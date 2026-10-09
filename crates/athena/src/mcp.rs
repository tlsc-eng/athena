//! `athena mcp-stdio`: an MCP server for Claude Code that answers from the running Athena window.
//! It holds no state of its own; every tool asks the window over `app.sock`, which works out
//! from the process tree which pane Claude is running in.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use athena_proto::{AppMsg, AppReply, SourcePosition};
use athena_workspace::git;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

const DEFAULT_LINES: u32 = 200;
const MAX_FILES: usize = 20_000;
const MAX_STATUS: usize = 2_000;
const DEFAULT_BUFFER: u32 = 256 * 1024;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct OpenFileArgs {
    /// Absolute path of a file inside one of the open projects.
    path: String,
    /// 1-based line to put the cursor on.
    line: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct OpenDiffArgs {
    /// Absolute path of a file inside one of the open projects.
    path: String,
    /// Show the staged changes (HEAD against the index) instead of the unstaged ones.
    staged: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReadTerminalArgs {
    /// The `session` of a terminal from list_terminals.
    session: u64,
    /// How many of the most recent lines to return (default 200, at most 2000).
    last_n_lines: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RunArgs {
    /// The `session` of a terminal from list_terminals.
    session: u64,
    /// What to type. The user sees it in full and must approve it.
    text: String,
    /// Press Return after typing (default true).
    newline: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DiagnosticsArgs {
    /// Absolute path of one file; leave out for every open project.
    path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ProjectFilesArgs {
    /// Project root from list_projects; defaults to the project this session runs in.
    project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PositionArgs {
    /// Absolute path of a file open in Athena's editor.
    path: String,
    /// 1-based line.
    line: u32,
    /// The symbol's text on that line, such as `parseConfig`; preferred over `column`.
    symbol: Option<String>,
    /// 1-based column in UTF-16 units, when `symbol` is not given.
    column: Option<u32>,
}

impl PositionArgs {
    fn position(self) -> SourcePosition {
        SourcePosition {
            path: PathBuf::from(self.path),
            line: self.line,
            column: self.column,
            symbol: self.symbol,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PathArgs {
    /// Absolute path of a file open in Athena's editor.
    path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReadBufferArgs {
    /// Absolute path of a file open in Athena's editor.
    path: String,
    /// Return at most this many bytes of the text (default 262144, at most 524288).
    max_bytes: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RunTestsArgs {
    /// Absolute path of a Go package folder, a Go file (runs its package) or a JavaScript test
    /// file; leave out to run all of the project's tests.
    path: Option<String>,
    /// One test to run: a Go test function or a Vitest/Jest test or describe title. Needs `path`.
    name: Option<String>,
}

#[derive(Clone)]
struct Bridge;

/// One request to the window, on a blocking thread so the MCP loop keeps serving.
async fn ask(msg: AppMsg) -> Result<AppReply, ErrorData> {
    let reply = tokio::task::spawn_blocking(move || -> Result<AppReply, String> {
        let path = athena_proto::app_socket_path().map_err(|e| e.to_string())?;
        let mut stream =
            UnixStream::connect(path).map_err(|_| "Athena is not running".to_string())?;
        if let Some(session) = std::env::var("ATHENA_PANE_ID")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            let identify = AppMsg::Identify { session };
            athena_proto::write_frame(&mut stream, &identify).map_err(|e| e.to_string())?;
            athena_proto::read_frame::<_, AppReply>(&mut stream).map_err(|e| e.to_string())?;
        }
        athena_proto::write_frame(&mut stream, &msg).map_err(|e| e.to_string())?;
        athena_proto::read_frame::<_, AppReply>(&mut stream)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "Athena closed the connection".to_string())
    })
    .await
    .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
    .map_err(|e| ErrorData::internal_error(e, None))?;
    match reply {
        AppReply::Error(e) => Err(ErrorData::invalid_params(e, None)),
        reply => Ok(reply),
    }
}

fn json(value: &impl serde::Serialize) -> Result<CallToolResult, ErrorData> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

fn text(s: impl Into<String>) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::success(vec![ContentBlock::text(s.into())]))
}

fn unexpected() -> ErrorData {
    ErrorData::internal_error("unexpected reply from Athena", None)
}

/// The open project `asked` names (root path or name), else the caller's, else the active one.
async fn project_root(asked: Option<String>) -> Result<PathBuf, ErrorData> {
    let AppReply::Projects(projects) = ask(AppMsg::ListProjects).await? else {
        return Err(unexpected());
    };
    match asked {
        Some(p) => projects
            .into_iter()
            .find(|x| {
                let asked = Path::new(&p);
                x.root == asked || asked.canonicalize().is_ok_and(|c| c == x.root) || x.name == p
            })
            .map(|x| x.root),
        None => match ask(AppMsg::WhoAmI).await? {
            AppReply::Caller {
                project: Some(p), ..
            } => Some(p),
            _ => projects.into_iter().find(|x| x.active).map(|x| x.root),
        },
    }
    .ok_or_else(|| ErrorData::invalid_params("no such open project", None))
}

/// `git status` of the project at `root` as the tool returns it, paths relative to the root.
fn git_status(root: &Path) -> Result<serde_json::Value, String> {
    if !git::available() {
        return Err("git needs the Xcode Command Line Tools (xcode-select --install)".into());
    }
    let prefix =
        git::prefix(root).map_err(|_| format!("{} is not in a git repository", root.display()))?;
    let snapshot = git::status(root, &prefix, true).map_err(|e| format!("{e:#}"))?;
    let letter = |s: Option<git::FileStatus>| s.map(git::FileStatus::letter);
    let changed: Vec<_> = snapshot
        .entries
        .iter()
        .filter(|(_, e)| e.unstaged != Some(git::FileStatus::Ignored))
        .collect();
    let files: Vec<_> = changed
        .iter()
        .take(MAX_STATUS)
        .map(|(path, e)| {
            serde_json::json!({
                "path": path.strip_prefix(root).unwrap_or(path),
                "staged": letter(e.staged),
                "unstaged": letter(e.unstaged),
                "renamed_from": e.orig,
            })
        })
        .collect();
    Ok(serde_json::json!({
        "branch": snapshot.branch,
        "upstream": snapshot.tracking.as_ref().map(|t| &t.upstream),
        "ahead": snapshot.tracking.as_ref().map(|t| t.ahead),
        "behind": snapshot.tracking.as_ref().map(|t| t.behind),
        "files": files,
        "truncated": changed.len() > MAX_STATUS,
    }))
}

#[tool_router]
impl Bridge {
    #[tool(
        description = "List the projects open in Athena. `current` marks the project this Claude session runs in."
    )]
    async fn list_projects(&self) -> Result<CallToolResult, ErrorData> {
        let AppReply::Projects(projects) = ask(AppMsg::ListProjects).await? else {
            return Err(unexpected());
        };
        let current = match ask(AppMsg::WhoAmI).await? {
            AppReply::Caller { project, .. } => project,
            _ => None,
        };
        let list: Vec<_> = projects
            .into_iter()
            .map(|p| {
                serde_json::json!({
                    "root": p.root,
                    "name": p.name,
                    "active": p.active,
                    "current": current.as_ref() == Some(&p.root),
                })
            })
            .collect();
        json(&list)
    }

    #[tool(
        description = "The file open in Athena's focused editor: path, 1-based cursor line and column, selected text and whether it has unsaved changes."
    )]
    async fn get_active_file(&self) -> Result<CallToolResult, ErrorData> {
        match ask(AppMsg::ActiveFile).await? {
            AppReply::ActiveFile(Some(file)) => json(&file),
            AppReply::ActiveFile(None) => text("No file is focused in Athena."),
            _ => Err(unexpected()),
        }
    }

    #[tool(
        description = "Open a file from one of the open projects in Athena's editor, optionally at a 1-based line."
    )]
    async fn open_file(
        &self,
        Parameters(args): Parameters<OpenFileArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        ask(AppMsg::OpenFile {
            path: PathBuf::from(&args.path),
            line: args.line,
        })
        .await?;
        text(format!("Opened {}", args.path))
    }

    #[tool(
        description = "Show the user a file's uncommitted changes in Athena's diff viewer (unstaged by default), where they can stage or revert each change."
    )]
    async fn open_diff(
        &self,
        Parameters(args): Parameters<OpenDiffArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        ask(AppMsg::OpenDiff {
            path: PathBuf::from(&args.path),
            staged: args.staged.unwrap_or(false),
        })
        .await?;
        text(format!("Showing the changes to {}", args.path))
    }

    #[tool(
        description = "List terminals in every open project with their session id, title, working directory, running program and Claude Code state."
    )]
    async fn list_terminals(&self) -> Result<CallToolResult, ErrorData> {
        let AppReply::Terminals(list) = ask(AppMsg::ListTerminals).await? else {
            return Err(unexpected());
        };
        json(&list)
    }

    #[tool(
        description = "Read the most recent output of a terminal open in Athena, as plain text."
    )]
    async fn read_terminal(
        &self,
        Parameters(args): Parameters<ReadTerminalArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let lines = args.last_n_lines.unwrap_or(DEFAULT_LINES);
        let AppReply::Lines(lines) = ask(AppMsg::ReadTerminal {
            session: args.session,
            lines,
        })
        .await?
        else {
            return Err(unexpected());
        };
        text(lines.join("\n"))
    }

    #[tool(
        description = "Type a command into one of the user's terminals. Athena shows the user the exact text and runs it only if they approve within 60 seconds."
    )]
    async fn run_in_terminal(
        &self,
        Parameters(args): Parameters<RunArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let msg = AppMsg::RunInTerminal {
            session: args.session,
            text: args.text,
            newline: args.newline.unwrap_or(true),
        };
        ask(msg).await?;
        text("The user approved it; it was typed into the terminal.")
    }

    #[tool(
        description = "List a project's files (gitignore honoured, secrets such as .env and keys left out)."
    )]
    async fn list_project_files(
        &self,
        Parameters(args): Parameters<ProjectFilesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = project_root(args.project).await?;
        let files = tokio::task::spawn_blocking(move || project_files(&root))
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        text(files.join("\n"))
    }

    #[tool(
        description = "Errors and warnings from Athena's language servers (gopls, typescript-language-server). Covers files open in Athena's editor and, for Go, the rest of their packages. Lines are 1-based; columns count UTF-16 units."
    )]
    async fn get_diagnostics(
        &self,
        Parameters(args): Parameters<DiagnosticsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let AppReply::Diagnostics(list) = ask(AppMsg::Diagnostics {
            path: args.path.map(PathBuf::from),
        })
        .await?
        else {
            return Err(unexpected());
        };
        json(&list)
    }

    #[tool(
        description = "Where the symbol at a position is defined, from the language server that has the file open in Athena (unsaved edits included). Name the position by 1-based line plus the symbol's text on that line, or a 1-based UTF-16 column. Returns up to 200 locations with 1-based lines and columns and the source line."
    )]
    async fn lsp_definition(
        &self,
        Parameters(args): Parameters<PositionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let AppReply::Locations(list) = ask(AppMsg::LspDefinition {
            at: args.position(),
        })
        .await?
        else {
            return Err(unexpected());
        };
        json(&list)
    }

    #[tool(
        description = "Every reference to the symbol at a position, from the language server that has the file open in Athena (unsaved edits included). Same position arguments as lsp_definition; returns up to 200 locations."
    )]
    async fn lsp_references(
        &self,
        Parameters(args): Parameters<PositionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let AppReply::Locations(list) = ask(AppMsg::LspReferences {
            at: args.position(),
        })
        .await?
        else {
            return Err(unexpected());
        };
        json(&list)
    }

    #[tool(
        description = "The symbols (functions, types, methods, fields…) of a file open in Athena, as its language server sees the unsaved text: name, kind, container and the 1-based lines each spans. At most 2000."
    )]
    async fn document_symbols(
        &self,
        Parameters(args): Parameters<PathArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let msg = AppMsg::DocumentSymbols {
            path: PathBuf::from(args.path),
        };
        let AppReply::Symbols(list) = ask(msg).await? else {
            return Err(unexpected());
        };
        json(&list)
    }

    #[tool(
        description = "Files open in Athena's editor tabs across all projects: path, project, whether it has unsaved changes (`dirty`) and whether it is the focused tab."
    )]
    async fn get_open_editors(&self) -> Result<CallToolResult, ErrorData> {
        let AppReply::Editors(list) = ask(AppMsg::OpenEditors).await? else {
            return Err(unexpected());
        };
        json(&list)
    }

    #[tool(
        description = "The text of a file open in Athena's editor, including changes not saved to disk yet. Cut off after max_bytes; `total_bytes` gives the full size."
    )]
    async fn read_buffer(
        &self,
        Parameters(args): Parameters<ReadBufferArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let msg = AppMsg::ReadBuffer {
            path: PathBuf::from(args.path),
            max_bytes: args.max_bytes.unwrap_or(DEFAULT_BUFFER),
        };
        let AppReply::Buffer(buffer) = ask(msg).await? else {
            return Err(unexpected());
        };
        json(&buffer)
    }

    #[tool(
        description = "Start tests in Athena's Tests panel for this session's project: go test where go.mod is, Vitest or Jest from package.json (only once the user has allowed the project's code). `path` must lie in this session's project. Returns at once; poll get_test_results until `running` is false."
    )]
    async fn run_tests(
        &self,
        Parameters(args): Parameters<RunTestsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let msg = AppMsg::RunTests {
            path: args.path.map(PathBuf::from),
            name: args.name,
        };
        let AppReply::Lines(lines) = ask(msg).await? else {
            return Err(unexpected());
        };
        text(lines.join("\n"))
    }

    #[tool(
        description = "Results of the tests run in Athena for this session's project, later runs folded into earlier ones: whether a run is still going, counts, and each failed test with its output (shortened)."
    )]
    async fn get_test_results(&self) -> Result<CallToolResult, ErrorData> {
        let AppReply::Tests(results) = ask(AppMsg::TestResults).await? else {
            return Err(unexpected());
        };
        json(&results)
    }

    #[tool(
        description = "Read-only state of the debugger in Athena for this session's project: whether a Go program is being debugged (status not_debugging, starting, running or paused) and, while paused, why it stopped, where (file and 1-based line of the selected frame), the call stack (up to 20 frames) and that frame's local variables (up to 50, values shortened to 200 characters). Use it to help the user while their program is paused at a breakpoint."
    )]
    async fn debug_state(&self) -> Result<CallToolResult, ErrorData> {
        let AppReply::Debug(state) = ask(AppMsg::DebugState).await? else {
            return Err(unexpected());
        };
        json(&state)
    }

    #[tool(
        description = "git status of an open project (default: the one this session runs in): branch, upstream with ahead/behind counts from the last fetch, and changed files with one-letter staged and unstaged states (M modified, A added, D deleted, R renamed, U untracked, ! conflict). Ignored files are left out; at most 2000 files."
    )]
    async fn git_status(
        &self,
        Parameters(args): Parameters<ProjectFilesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = project_root(args.project).await?;
        let status = tokio::task::spawn_blocking(move || git_status(&root))
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        json(&status)
    }
}

#[tool_handler]
impl ServerHandler for Bridge {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Athena is the user's editor and terminal multiplexer. Use these tools to see which file \
             they have open, open files for them, and read the output of their other terminals.",
        )
    }
}

fn project_files(root: &Path) -> Vec<String> {
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|e| e.path().strip_prefix(root).ok().map(Path::to_path_buf))
        .filter(|rel| !athena_workspace::denied(rel))
        .take(MAX_FILES)
        .map(|rel| rel.to_string_lossy().into_owned())
        .collect()
}

/// Serves MCP on stdin/stdout until Claude Code closes it. Logs go to stderr only.
pub fn run() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    runtime.block_on(async {
        let server = Bridge.serve(rmcp::transport::stdio()).await?;
        server.waiting().await?;
        anyhow::Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_new_tools_are_listed_with_precise_schemas() {
        let tools = Bridge::tool_router().list_all();
        let tool = |name: &str| {
            tools
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("{name} is not listed"))
        };
        for name in [
            "lsp_definition",
            "lsp_references",
            "document_symbols",
            "get_open_editors",
            "read_buffer",
            "run_tests",
            "get_test_results",
            "git_status",
            "debug_state",
        ] {
            assert!(
                tool(name)
                    .description
                    .as_ref()
                    .is_some_and(|d| d.len() > 40)
            );
        }
        let schema = serde_json::Value::Object((*tool("lsp_definition").input_schema).clone());
        let mut required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        required.sort();
        assert_eq!(required, ["line", "path"]);
        for field in ["symbol", "column"] {
            assert!(schema["properties"][field].is_object(), "{field}");
        }
        let read = serde_json::Value::Object((*tool("read_buffer").input_schema).clone());
        assert!(read["properties"]["max_bytes"].is_object());
        let run = serde_json::Value::Object((*tool("run_tests").input_schema).clone());
        assert!(
            run.get("required")
                .is_none_or(|r| r.as_array().unwrap().is_empty())
        );
    }

    #[test]
    fn git_status_lists_changes_relative_to_the_project_without_ignored_files() {
        if !git::available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("athena-mcp-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let dir = dir.canonicalize().unwrap();
        let sh = |args: &[&str]| {
            let ok = std::process::Command::new("/usr/bin/git")
                .arg("-C")
                .arg(&dir)
                .args(["-c", "user.name=T", "-c", "user.email=t@x"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        std::fs::write(dir.join("sub/a.txt"), "one\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        sh(&["init", "-q", "-b", "main"]);
        sh(&["add", "-A"]);
        sh(&["commit", "-qm", "init"]);
        std::fs::write(dir.join("sub/a.txt"), "two\n").unwrap();
        std::fs::write(dir.join("sub/new.txt"), "x\n").unwrap();
        std::fs::write(dir.join("sub/b.log"), "x\n").unwrap();
        std::fs::write(dir.join("top.txt"), "x\n").unwrap();
        sh(&["add", "top.txt"]);

        let whole = git_status(&dir).unwrap();
        assert_eq!(whole["branch"], "main");
        let files = whole["files"].as_array().unwrap();
        let find = |p: &str| files.iter().find(|f| f["path"] == p).cloned();
        assert_eq!(find("sub/a.txt").unwrap()["unstaged"], "M");
        assert_eq!(find("sub/new.txt").unwrap()["unstaged"], "U");
        assert_eq!(find("top.txt").unwrap()["staged"], "A");
        assert!(find("sub/b.log").is_none(), "ignored files are left out");

        let sub = git_status(&dir.join("sub")).unwrap();
        let paths: Vec<&str> = sub["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths.len(), 2, "{paths:?}");
        assert!(paths.contains(&"a.txt") && paths.contains(&"new.txt"));
        assert!(git_status(&std::env::temp_dir().join("athena-no-repo-here")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
