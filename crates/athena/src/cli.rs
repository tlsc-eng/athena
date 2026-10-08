use std::io::{IsTerminal, Read};

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;

use athena_proto::{AppMsg, ClientMsg, ConnectError, NoticeKind, ServerMsg};

const USAGE: &str = "usage: athena [<folder> | --version | mcp-stdio | mux status | mux stop | notify --event <claude-stop|claude-needs-input|claude-running> | notify --title <t> [--body <b>]]";

/// Hook input larger than this is ignored; Claude Code sends a small JSON object.
const MAX_HOOK_INPUT: u64 = 64 * 1024;

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
    let hook_message = || {
        let mut input = String::new();
        if !std::io::stdin().is_terminal() {
            let _ = std::io::stdin()
                .take(MAX_HOOK_INPUT)
                .read_to_string(&mut input);
        }
        serde_json::from_str::<serde_json::Value>(&input)
            .ok()
            .and_then(|v| {
                v.get("message")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
            })
    };
    let kind = match (flag("--event").as_deref(), flag("--title")) {
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
    let running = athena_proto::app_socket_path().is_ok_and(|p| UnixStream::connect(p).is_ok());
    let mut open = Command::new("/usr/bin/open");
    if running {
        open.args(["-b", "io.tlsc.athena"]);
    } else {
        // Homebrew links only the executable, so follow the link to find the bundle.
        let exe = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .ok()?;
        let bundle = exe
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))?;
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
