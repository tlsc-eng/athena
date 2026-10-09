use std::io::{IsTerminal, Read};

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;

use athena_proto::{AppMsg, ClientMsg, ConnectError, NoticeKind, ServerMsg};

const USAGE: &str = "usage: athena [<folder> | --version | mcp-stdio | mux status | mux stop | notify --event <claude-stop|claude-needs-input|claude-running|claude-will-edit|claude-edited|claude-plan> | notify --edited <file> [--session <id>] | notify --title <t> [--body <b>]]";

/// Hook input past this is cut off; a Write of a file up to the snapshot limit fits easily.
const MAX_HOOK_INPUT: u64 = 256 * 1024 * 1024;

/// Todos and plan text past these are cut off before they reach the window.
const MAX_TODOS: usize = 200;
const MAX_PLAN: usize = 64 * 1024;

/// The fields Athena reads from a Claude Code hook's input; the rest, such as a Write's whole
/// file content, is skipped while parsing rather than held in memory.
#[derive(Debug, Default, serde::Deserialize)]
struct HookInput {
    session_id: Option<String>,
    cwd: Option<String>,
    message: Option<String>,
    tool_name: Option<String>,
    tool_input: Option<ToolInput>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ToolInput {
    file_path: Option<String>,
    todos: Option<Vec<Todo>>,
    plan: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct Todo {
    #[serde(default)]
    content: String,
    #[serde(default)]
    status: String,
    #[serde(rename = "activeForm")]
    active_form: Option<String>,
}

/// What a TodoWrite or ExitPlanMode hook tells the window, if the input is one of those.
fn plan_message(input: HookInput) -> Option<AppMsg> {
    let session = input
        .session_id
        .filter(|s| crate::snapshots::valid_session(s))?;
    let tool = input.tool_input?;
    match input.tool_name.as_deref()? {
        "TodoWrite" => Some(AppMsg::ClaudeTodos {
            session,
            todos: tool
                .todos?
                .into_iter()
                .take(MAX_TODOS)
                .map(|t| athena_proto::TodoInfo {
                    content: t.content,
                    status: t.status,
                    active_form: t.active_form,
                })
                .collect(),
        }),
        "ExitPlanMode" => {
            let mut plan = tool.plan?;
            plan.truncate(plan.floor_char_boundary(MAX_PLAN));
            Some(AppMsg::ClaudePlan { session, plan })
        }
        _ => None,
    }
}

fn read_hook_input(input: impl Read) -> HookInput {
    serde_json::from_reader(std::io::BufReader::new(input.take(MAX_HOOK_INPUT))).unwrap_or_default()
}

/// Handles command-line subcommands; `None` means start the app.
pub fn run(args: Vec<String>) -> Option<i32> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        [] if launched_by_launchd() => return None,
        [] => return open_app(),
        ["--version" | "-V"] => {
            println!("athena {}", env!("CARGO_PKG_VERSION"));
            return Some(0);
        }
        ["mux", "status"] => mux_status(),
        ["mux", "stop"] => mux_stop(),
        ["notify", rest @ ..] => notify(rest),
        ["mcp-stdio"] => crate::mcp::run(),
        // Started by LaunchServices (`open --args`): the window itself reads the folder.
        _ if launched_by_launchd() => return None,
        [path] if !path.starts_with('-') => return open_folder(Path::new(path)),
        _ => {
            eprintln!("{USAGE}");
            return Some(2);
        }
    };
    Some(match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("athena: {err:#}");
            1
        }
    })
}

fn mux_status() -> anyhow::Result<()> {
    let (conn, mut reader) = match athena_proto::connect(&athena_proto::socket_path()?) {
        Ok(pair) => pair,
        Err(ConnectError::NotRunning) => {
            println!("session daemon: not running");
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };
    conn.send(&ClientMsg::ListPanes)?;
    let Some(ServerMsg::Panes { panes }) = athena_proto::read_frame(&mut reader)? else {
        anyhow::bail!("unexpected reply from session daemon");
    };
    println!(
        "session daemon: pid {}, {} sessions",
        conn.daemon_pid,
        panes.len()
    );
    for pane in panes {
        let state = if pane.alive { "running" } else { "exited" };
        println!(
            "  {}  {}x{}  {:8}  {}",
            pane.id,
            pane.cols,
            pane.rows,
            state,
            pane.cwd.display()
        );
    }
    Ok(())
}

fn mux_stop() -> anyhow::Result<()> {
    if athena_proto::stop_daemon(&athena_proto::socket_path()?)? {
        println!("session daemon stopped; its shells were hung up");
    } else {
        println!("session daemon: not running");
    }
    Ok(())
}

/// Raises a notice in Athena. Never fails loudly: it runs inside Claude Code hooks, which must not
/// break because Athena is closed.
fn notify(args: &[&str]) -> anyhow::Result<()> {
    let flag = |name: &str| {
        args.iter()
            .position(|a| *a == name)
            .and_then(|i| args.get(i + 1))
            .map(|s| s.to_string())
    };
    // Only hook events read stdin; a script's `--title` call may have a pipe left open there.
    let hook_input = || {
        if std::io::stdin().is_terminal() {
            HookInput::default()
        } else {
            read_hook_input(std::io::stdin().lock())
        }
    };
    let hook_message = || hook_input().message;
    let event = flag("--event");
    if event.as_deref() == Some("claude-will-edit") {
        // A failing PreToolUse hook would get in Claude's way; a missed snapshot only costs the diff.
        if let (Some((path, session)), Ok(store)) =
            (edited_file(&hook_input(), None), crate::snapshots::store())
            && let Some(session) = session
        {
            let _ = crate::snapshots::take(&store, &session, &path);
        }
        return Ok(());
    }
    if event.as_deref() == Some("claude-plan") {
        if let Some(msg) = plan_message(hook_input()) {
            let _ = tell_window(msg);
        }
        return Ok(());
    }
    if event.as_deref() == Some("claude-edited") || flag("--edited").is_some() {
        let input = if event.is_some() {
            hook_input()
        } else {
            HookInput::default()
        };
        if let Some((path, session)) = edited_file(&input, flag("--edited")) {
            let session = flag("--session").or(session).unwrap_or_default();
            let _ = tell_window(AppMsg::ClaudeEdited { path, session });
        }
        return Ok(());
    }
    let kind = match (event.as_deref(), flag("--title")) {
        (Some("claude-stop"), _) => NoticeKind::ClaudeStopped,
        (Some("claude-running"), _) => NoticeKind::ClaudeRunning,
        (Some("claude-needs-input"), _) => NoticeKind::ClaudeNeedsInput {
            message: flag("--message").or_else(hook_message).unwrap_or_default(),
        },
        (None, Some(title)) => NoticeKind::Message {
            title,
            body: flag("--body").unwrap_or_default(),
        },
        _ => anyhow::bail!("{USAGE}"),
    };
    let pane = std::env::var("ATHENA_PANE_ID")
        .ok()
        .and_then(|p| p.parse().ok());
    if let Ok((conn, _)) = athena_proto::connect(&athena_proto::socket_path()?) {
        conn.send(&ClientMsg::Notify { pane, kind })?;
    }
    Ok(())
}

/// The file a Claude Code edit hook is about, made absolute against the session's folder, and
/// the session id; `given` (from `--edited`) wins over the hook input.
fn edited_file(input: &HookInput, given: Option<String>) -> Option<(PathBuf, Option<String>)> {
    let file = given.or_else(|| input.tool_input.as_ref()?.file_path.clone())?;
    let mut path = PathBuf::from(file);
    if path.is_relative() {
        let cwd = input
            .cwd
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())?;
        path = cwd.join(path);
    }
    // Projects are opened by their real path (/private/tmp, not /tmp); a new file has none yet.
    if let (Some(dir), Some(name)) = (path.parent(), path.file_name())
        && let Ok(real) = dir.canonicalize()
    {
        path = real.join(name);
    }
    let session = input.session_id.clone();
    Some((path, session))
}

/// One request to the window over app.sock, giving up quickly: hooks must not stall Claude.
fn tell_window(msg: AppMsg) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(athena_proto::app_socket_path()?)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;
    if let Some(session) = std::env::var("ATHENA_PANE_ID")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        athena_proto::write_frame(&mut stream, &AppMsg::Identify { session })?;
        athena_proto::read_frame::<_, athena_proto::AppReply>(&mut stream)?;
    }
    athena_proto::write_frame(&mut stream, &msg)?;
    athena_proto::read_frame::<_, athena_proto::AppReply>(&mut stream)?;
    Ok(())
}

/// `athena <folder>`: hands the folder to a running Athena, else starts the installed app with it.
/// Returns `None` to start the window in this process (a development build with no app installed).
fn open_folder(path: &Path) -> Option<i32> {
    let Ok(path) = path
        .canonicalize()
        .map_err(|e| eprintln!("athena: {}: {e}", path.display()))
    else {
        return Some(1);
    };
    if !path.is_dir() {
        eprintln!("athena: {} is not a folder", path.display());
        return Some(2);
    }
    if send_to_window(&path).is_ok() {
        return Some(0);
    }
    let launched = Command::new("/usr/bin/open")
        .args(["-b", "io.tlsc.athena", "--args"])
        .arg(&path)
        .status()
        .is_ok_and(|s| s.success());
    if launched { Some(0) } else { None }
}

/// Bare `athena` from a shell: brings the running window forward, or starts the app this
/// executable belongs to. Returns `None` to start the window in this process (a development build).
fn open_app() -> Option<i32> {
    // Homebrew links only the executable, so follow the link to find the bundle.
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .ok()?;
    let bundle = exe
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))?;
    let running = athena_proto::app_socket_path().is_ok_and(|p| UnixStream::connect(p).is_ok());
    let mut open = Command::new("/usr/bin/open");
    if running {
        open.args(["-b", "io.tlsc.athena"]);
    } else {
        open.arg(bundle);
    }
    open.status().is_ok_and(|s| s.success()).then_some(0)
}

fn send_to_window(path: &Path) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(athena_proto::app_socket_path()?)?;
    athena_proto::write_frame(
        &mut stream,
        &AppMsg::OpenProject {
            path: path.to_path_buf(),
        },
    )?;
    Ok(())
}

/// A folder given on the command line when this process is the window itself.
pub fn startup_folder() -> Option<PathBuf> {
    let arg = std::env::args().nth(1)?;
    let path = PathBuf::from(arg).canonicalize().ok()?;
    path.is_dir().then_some(path)
}

fn launched_by_launchd() -> bool {
    // SAFETY: getppid has no preconditions.
    unsafe { libc::getppid() == 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook_input(value: serde_json::Value) -> HookInput {
        read_hook_input(value.to_string().as_bytes())
    }

    #[test]
    fn edit_hook_input_names_an_absolute_file_and_the_session() {
        let input = hook_input(serde_json::json!({
            "session_id": "abc-123",
            "cwd": "/nonexistent-athena-test/app",
            "tool_name": "Edit",
            "tool_input": { "file_path": "src/main.go", "old_string": "a", "new_string": "b" }
        }));
        let (path, session) = edited_file(&input, None).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/nonexistent-athena-test/app/src/main.go")
        );
        let tmp =
            hook_input(serde_json::json!({ "tool_input": { "file_path": "/tmp/new-file.go" } }));
        let (real, _) = edited_file(&tmp, None).unwrap();
        assert_eq!(
            real,
            Path::new("/tmp")
                .canonicalize()
                .unwrap()
                .join("new-file.go")
        );
        assert_eq!(session.as_deref(), Some("abc-123"));
        let (path, _) = edited_file(&input, Some("/x/y.go".into())).unwrap();
        assert_eq!(path, PathBuf::from("/x/y.go"));
        assert!(edited_file(&hook_input(serde_json::json!({})), None).is_none());
    }

    #[test]
    fn todo_and_plan_hooks_become_window_messages() {
        let todos = hook_input(serde_json::json!({
            "session_id": "abc-123",
            "tool_name": "TodoWrite",
            "tool_input": { "todos": [
                { "content": "Parse transcripts", "status": "completed", "activeForm": "Parsing transcripts" },
                { "content": "Draw the tab", "status": "in_progress" }
            ] },
            "tool_response": { "oldTodos": [] }
        }));
        let Some(AppMsg::ClaudeTodos { session, todos }) = plan_message(todos) else {
            panic!("expected todos");
        };
        assert_eq!(session, "abc-123");
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].active_form.as_deref(), Some("Parsing transcripts"));
        assert_eq!(todos[1].status, "in_progress");

        let plan = hook_input(serde_json::json!({
            "session_id": "abc-123",
            "tool_name": "ExitPlanMode",
            "tool_input": { "plan": "# Plan\n1. do it", "planFilePath": "/x/plan.md" }
        }));
        assert_eq!(
            plan_message(plan),
            Some(AppMsg::ClaudePlan {
                session: "abc-123".into(),
                plan: "# Plan\n1. do it".into()
            })
        );
        for odd in [
            serde_json::json!({ "session_id": "../x", "tool_name": "ExitPlanMode", "tool_input": { "plan": "p" } }),
            serde_json::json!({ "session_id": "s", "tool_name": "Bash", "tool_input": { "command": "ls" } }),
            serde_json::json!({ "session_id": "s", "tool_name": "TodoWrite", "tool_input": {} }),
        ] {
            assert_eq!(plan_message(hook_input(odd)), None);
        }
    }

    #[test]
    fn long_todo_lists_and_plans_are_cut_off() {
        let todos: Vec<_> = (0..MAX_TODOS + 5)
            .map(|i| serde_json::json!({ "content": format!("t{i}"), "status": "pending" }))
            .collect();
        let input = hook_input(serde_json::json!({
            "session_id": "abc-123",
            "tool_name": "TodoWrite",
            "tool_input": { "todos": todos }
        }));
        let Some(AppMsg::ClaudeTodos { todos, .. }) = plan_message(input) else {
            panic!("expected todos");
        };
        assert_eq!(todos.len(), MAX_TODOS);
        let input = hook_input(serde_json::json!({
            "session_id": "abc-123",
            "tool_name": "ExitPlanMode",
            "tool_input": { "plan": format!("a{}", "é".repeat(MAX_PLAN)) }
        }));
        let Some(AppMsg::ClaudePlan { plan, .. }) = plan_message(input) else {
            panic!("expected a plan");
        };
        assert_eq!(plan.len(), MAX_PLAN - 1, "cut on a character boundary");
    }

    #[test]
    fn a_write_hook_with_a_large_file_still_records_the_baseline() {
        let dir = std::env::temp_dir().join(format!("athena-cli-large-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let file = dir.join("big.txt");
        std::fs::write(&file, "before\n").unwrap();
        let content: String = std::iter::repeat_n('x', 200 * 1024).collect();
        // The file path comes after the content, so a cut-off read would lose it.
        let text = format!(
            r#"{{"session_id":"s-big","cwd":"{}","tool_name":"Write","tool_input":{{"content":"{content}","file_path":"big.txt"}}}}"#,
            dir.display()
        );
        assert!(text.len() > 64 * 1024);
        let input = read_hook_input(text.as_bytes());
        let (path, session) = edited_file(&input, None).unwrap();
        assert_eq!(path, file);
        let store = dir.join("store");
        crate::snapshots::take(&store, session.as_deref().unwrap(), &path).unwrap();
        std::fs::write(&file, &content).unwrap();
        assert_eq!(
            crate::snapshots::read(&store, "s-big", &file),
            crate::snapshots::Before::Text(b"before\n".to_vec())
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
