//! `athena mcp-stdio`: an MCP server for Claude Code that answers from the running Athena window.
//! It holds no state of its own; every tool asks the window over `app.sock`, which works out
//! from the process tree which pane Claude is running in.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use athena_proto::{AppMsg, AppReply};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

const DEFAULT_LINES: u32 = 200;
const MAX_FILES: usize = 20_000;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct OpenFileArgs {
    /// Absolute path of a file inside one of the open projects.
    path: String,
    /// 1-based line to put the cursor on.
    line: Option<u32>,
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
        let AppReply::Projects(projects) = ask(AppMsg::ListProjects).await? else {
            return Err(unexpected());
        };
        let root = match args.project {
            Some(p) => projects
                .into_iter()
                .find(|x| {
                    let asked = Path::new(&p);
                    x.root == asked
                        || asked.canonicalize().is_ok_and(|c| c == x.root)
                        || x.name == p
                })
                .map(|x| x.root),
            None => match ask(AppMsg::WhoAmI).await? {
                AppReply::Caller {
                    project: Some(p), ..
                } => Some(p),
                _ => projects.into_iter().find(|x| x.active).map(|x| x.root),
            },
        }
        .ok_or_else(|| ErrorData::invalid_params("no such open project", None))?;
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
