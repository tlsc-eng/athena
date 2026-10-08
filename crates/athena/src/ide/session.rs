use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, CustomNotification, Implementation, ServerCapabilities,
    ServerConfig,
};
use rmcp::service::{NotificationContext, RequestContext};
use rmcp::{ErrorData, RoleServer, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use tokio::sync::oneshot;

use super::{DiffKey, Event, FileDiagnostics, Shared, Verdict};

/// Claude Code gives up on a per-file diagnostics baseline after 500 ms.
const DIAGNOSTICS_BUDGET: Duration = Duration::from_millis(450);

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct OpenDiffArgs {
    /// The file being changed.
    old_file_path: String,
    /// Where the proposed contents would go; Claude Code passes the same path.
    #[allow(dead_code)]
    new_file_path: String,
    /// The whole proposed file.
    new_file_contents: String,
    /// Claude Code's name for the tab, used again by close_tab.
    tab_name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CloseTabArgs {
    tab_name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DiagnosticsArgs {
    /// A `file://` URI; leave out for every open file.
    uri: Option<String>,
}

/// One Claude Code connection.
#[derive(Clone)]
pub(super) struct Session {
    pub client: u64,
    pub shared: Arc<Shared>,
}

/// Counts an openDiff that has a verdict but has not returned it yet, so quitting can wait for it.
struct Answering<'a>(&'a Shared);

impl Drop for Answering<'_> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.answering -= 1;
        self.0.settled.notify_all();
    }
}

fn text(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(s.into())])
}

#[tool_router]
impl Session {
    #[tool(
        name = "openDiff",
        description = "Show the user a proposed change to a file and wait until they accept or reject it."
    )]
    async fn open_diff(
        &self,
        Parameters(args): Parameters<OpenDiffArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let key = DiffKey {
            client: self.client,
            tab: args.tab_name,
        };
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.shared.lock();
            if state.stopping {
                return Ok(rejected(&key));
            }
            // The same tab asked for again replaces the old request, which then counts as rejected.
            state.pending.insert(key.clone(), tx);
            state.answering += 1;
        }
        let _answering = Answering(&self.shared);
        self.shared.send(Event::OpenDiff {
            key: key.clone(),
            path: PathBuf::from(args.old_file_path),
            contents: args.new_file_contents,
        });
        let verdict = tokio::select! {
            verdict = rx => verdict.unwrap_or(Verdict::Rejected),
            () = context.ct.cancelled() => {
                if self.shared.lock().pending.remove(&key).is_some() {
                    self.shared.send(Event::CloseDiff { key: key.clone() });
                }
                Verdict::Rejected
            }
        };
        Ok(match verdict {
            Verdict::Accepted(contents) => CallToolResult::success(vec![
                ContentBlock::text("FILE_SAVED"),
                ContentBlock::text(contents),
            ]),
            Verdict::Rejected => rejected(&key),
            Verdict::Unavailable => {
                return Err(ErrorData::internal_error(
                    "Athena has no project open to show this change in",
                    None,
                ));
            }
        })
    }

    #[tool(description = "Close a diff tab opened by openDiff.")]
    async fn close_tab(
        &self,
        Parameters(args): Parameters<CloseTabArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let key = DiffKey {
            client: self.client,
            tab: args.tab_name,
        };
        self.shared.drop_diffs(|k| *k == key);
        Ok(text("TAB_CLOSED"))
    }

    #[tool(
        name = "closeAllDiffTabs",
        description = "Close every diff tab this session opened."
    )]
    async fn close_all_diff_tabs(&self) -> Result<CallToolResult, ErrorData> {
        let client = self.client;
        let closed = self.shared.drop_diffs(|k| k.client == client);
        Ok(text(format!("CLOSED_{closed}_DIFF_TABS")))
    }

    #[tool(
        name = "getDiagnostics",
        description = "Errors and warnings from Athena's language servers, for one file or all open ones."
    )]
    async fn get_diagnostics(
        &self,
        Parameters(args): Parameters<DiagnosticsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let path = args
            .uri
            .as_deref()
            .map(|uri| PathBuf::from(uri.strip_prefix("file://").unwrap_or(uri)));
        let (reply, answer) = oneshot::channel();
        self.shared.send(Event::Diagnostics { path, reply });
        let files = tokio::time::timeout(DIAGNOSTICS_BUDGET, answer)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let json = diagnostics_json(args.uri, files);
        Ok(text(json.to_string()))
    }
}

fn rejected(key: &DiffKey) -> CallToolResult {
    CallToolResult::success(vec![
        ContentBlock::text("DIFF_REJECTED"),
        ContentBlock::text(key.tab.clone()),
    ])
}

/// Claude Code drops an answer whose `uri` differs from the one it asked about, so that is echoed.
pub(super) fn diagnostics_json(
    uri: Option<String>,
    files: Vec<FileDiagnostics>,
) -> serde_json::Value {
    let entry = |uri: String, diagnostics: Vec<super::Diagnostic>| serde_json::json!({ "uri": uri, "diagnostics": diagnostics });
    match uri {
        Some(uri) => {
            let diagnostics = files.into_iter().flat_map(|f| f.diagnostics).collect();
            serde_json::json!([entry(uri, diagnostics)])
        }
        None => serde_json::Value::Array(
            files
                .into_iter()
                .map(|f| entry(format!("file://{}", f.path.display()), f.diagnostics))
                .collect(),
        ),
    }
}

#[tool_handler]
impl ServerHandler for Session {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("athena", env!("CARGO_PKG_VERSION")))
    }

    async fn on_custom_notification(
        &self,
        notification: CustomNotification,
        _: NotificationContext<RoleServer>,
    ) {
        if notification.method != "ide_connected" {
            return;
        }
        let pid = notification
            .params
            .as_ref()
            .and_then(|p| p.get("pid"))
            .and_then(serde_json::Value::as_i64)
            .and_then(|p| i32::try_from(p).ok());
        if let Some(client) = self.shared.lock().clients.get_mut(&self.client) {
            client.pid = pid;
        }
        self.shared.send(Event::Client {
            client: self.client,
            pid,
        });
    }
}
