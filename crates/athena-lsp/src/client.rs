use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::call::{Call, CallItem, parse_calls, parse_items};
use crate::code_action::{self, CodeAction, parse_code_action, parse_code_actions};
use crate::completion::{
    CompletionItem, CompletionList, TextEdit, parse_completions, parse_text_edits,
};
use crate::edit::{WorkspaceEdit, parse_workspace_edit};
use crate::file_ops;
use crate::markup::{Hover, parse_hover};
use crate::protocol::{self, Diagnostic, Highlight, InlayHint, Location, Position, Range};
use crate::ranges::{LinkedRanges, parse_linked_ranges, parse_selection_ranges};
use crate::signature::{SignatureHelp, parse_signature_help};
use crate::symbol::{Symbol, parse_symbols};
use crate::{ServerKind, env};

const MAX_MESSAGE: usize = 64 * 1024 * 1024;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// What a server reports, in arrival order.
#[derive(Debug)]
pub enum Event {
    Ready,
    Diagnostics {
        path: PathBuf,
        list: Vec<Diagnostic>,
    },
    /// The server could not start, or stopped; the text says why.
    Stopped(String),
    /// Inlay hints shown so far are out of date, as after its settings changed.
    RefreshInlayHints,
    /// Diagnostics pulled so far are out of date; ask for them again.
    RefreshDiagnostics,
    /// The server asks for an edit, usually while running a command; answer through `reply`.
    ApplyEdit {
        label: Option<String>,
        edit: WorkspaceEdit,
        reply: EditReply,
    },
}

/// The answer to a server's `workspace/applyEdit`; dropping it unanswered reports a failure.
pub struct EditReply {
    writer: Frames,
    id: Value,
    answered: bool,
}

impl EditReply {
    pub fn send(mut self, result: Result<(), String>) {
        self.answer(result);
    }

    fn answer(&mut self, result: Result<(), String>) {
        self.answered = true;
        let result = match result {
            Ok(()) => json!({"applied": true}),
            Err(why) => json!({"applied": false, "failureReason": why}),
        };
        let _ = send(
            &self.writer,
            json!({"jsonrpc": "2.0", "id": self.id, "result": result}),
        );
    }
}

impl Drop for EditReply {
    fn drop(&mut self) {
        if !self.answered {
            self.answer(Err("Athena closed before applying the edit".into()));
        }
    }
}

impl std::fmt::Debug for EditReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EditReply({})", self.id)
    }
}

/// How a watched file changed, for `workspace/didChangeWatchedFiles`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileEvent {
    Created = 1,
    Changed = 2,
    Deleted = 3,
}

/// What `textDocument/prepareRename` said about the place a rename starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameTarget {
    /// The range to rename, and the text to offer when the server gives one.
    Range(Range, Option<String>),
    /// Rename the word at the cursor.
    Word,
}

enum Outgoing {
    Notify(&'static str, Value),
    Request(i64, &'static str, Value),
    Shutdown,
}

/// A reply's `result`, or the message of its `error`.
type Reply = Result<Value, String>;
type Pending = Arc<Mutex<HashMap<i64, async_channel::Sender<Reply>>>>;
/// Frames for the one thread that writes to the server, so no writer waits on another.
type Frames = mpsc::Sender<Value>;
/// A server's settings, which the user may change while it runs.
pub type Config = Arc<RwLock<Value>>;

/// One running language server. Calls never block: messages queue until `initialize` has finished.
pub struct Client {
    outgoing: async_channel::Sender<Outgoing>,
    pending: Pending,
    next_id: AtomicI64,
    child: Arc<Mutex<Option<Child>>>,
    /// Characters after which the server offers completions, known once it has started.
    triggers: Arc<OnceLock<Vec<String>>>,
    signature_triggers: Arc<OnceLock<Vec<String>>>,
    capabilities: Arc<OnceLock<Value>>,
}

impl Client {
    pub fn start(kind: ServerKind, root: PathBuf) -> (Self, async_channel::Receiver<Event>) {
        Self::start_with(kind, root, Config::default())
    }

    /// Starts a server configured with `config`: its `initializationOptions`, and the answer to
    /// its `workspace/configuration` requests.
    pub fn start_with(
        kind: ServerKind,
        root: PathBuf,
        config: Config,
    ) -> (Self, async_channel::Receiver<Event>) {
        Self::spawn(kind, None, root, config)
    }

    /// Starts the server at `program`, a path [`crate::project_server`] vetted, not one on PATH.
    pub fn start_local(
        kind: ServerKind,
        program: PathBuf,
        root: PathBuf,
        config: Config,
    ) -> (Self, async_channel::Receiver<Event>) {
        Self::spawn(kind, Some(program), root, config)
    }

    fn spawn(
        kind: ServerKind,
        program: Option<PathBuf>,
        root: PathBuf,
        config: Config,
    ) -> (Self, async_channel::Receiver<Event>) {
        let (out_tx, out_rx) = async_channel::unbounded();
        let (events_tx, events_rx) = async_channel::unbounded();
        let pending: Pending = Arc::default();
        let child: Arc<Mutex<Option<Child>>> = Arc::default();
        let triggers: Arc<OnceLock<Vec<String>>> = Arc::default();
        let signature_triggers: Arc<OnceLock<Vec<String>>> = Arc::default();
        let capabilities: Arc<OnceLock<Value>> = Arc::default();
        let session = Session {
            triggers: triggers.clone(),
            signature_triggers: signature_triggers.clone(),
            capabilities: capabilities.clone(),
            kind,
            program,
            root,
            config,
            outgoing: out_rx.clone(),
            events: events_tx.clone(),
            pending: pending.clone(),
            child: child.clone(),
        };
        let waiters = pending.clone();
        let spawned = thread::Builder::new()
            .name(format!("lsp-{}", kind.program()))
            .spawn(move || {
                if let Err(e) = session.run() {
                    let _ = events_tx.send_blocking(Event::Stopped(format!("{e:#}")));
                }
                // Requests sent while it failed to start are answered rather than left waiting.
                out_rx.close();
                waiters.lock().expect("pending lock").clear();
            });
        if spawned.is_err() {
            let _ = events_rx.close();
        }
        let client = Self {
            outgoing: out_tx,
            pending,
            next_id: AtomicI64::new(1),
            child,
            triggers,
            signature_triggers,
            capabilities,
        };
        (client, events_rx)
    }

    /// The server's process id, once it is running.
    pub fn pid(&self) -> Option<u32> {
        self.child
            .lock()
            .expect("child lock")
            .as_ref()
            .map(Child::id)
    }

    /// The server's completion trigger characters; empty until it has started.
    pub fn completion_triggers(&self) -> &[String] {
        self.triggers.get().map_or(&[], Vec::as_slice)
    }

    /// Characters that open or move signature help, such as `(` and `,`; empty until started.
    pub fn signature_triggers(&self) -> &[String] {
        self.signature_triggers.get().map_or(&[], Vec::as_slice)
    }

    /// Whether the started server declared the capability at `pointer`, e.g.
    /// `/renameProvider/prepareProvider`; a bare `true` or an options object both count.
    pub fn supports(&self, pointer: &str) -> bool {
        self.capabilities
            .get()
            .and_then(|c| c.pointer(pointer))
            .is_some_and(|v| v.as_bool().unwrap_or(v.is_object()))
    }

    fn notify(&self, method: &'static str, params: Value) {
        let _ = self.outgoing.try_send(Outgoing::Notify(method, params));
    }

    fn request(&self, method: &'static str, params: Value) -> async_channel::Receiver<Reply> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(id, method, "lsp request");
        let (tx, rx) = async_channel::bounded(1);
        self.pending.lock().expect("pending lock").insert(id, tx);
        // Once the server has gone the queue is closed, and the waiter is told so at once.
        if self
            .outgoing
            .try_send(Outgoing::Request(id, method, params))
            .is_err()
        {
            self.pending.lock().expect("pending lock").remove(&id);
        }
        rx
    }

    pub fn did_open(&self, path: &Path, language_id: &str, version: i64, text: String) {
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {"uri": protocol::uri_from_path(path), "languageId": language_id,
                                    "version": version, "text": text}}),
        );
    }

    /// Sends the whole document; valid whichever sync kind the server asked for.
    pub fn did_change(&self, path: &Path, version: i64, text: String) {
        self.notify(
            "textDocument/didChange",
            json!({"textDocument": {"uri": protocol::uri_from_path(path), "version": version},
                   "contentChanges": [{"text": text}]}),
        );
    }

    pub fn did_save(&self, path: &Path) {
        self.notify(
            "textDocument/didSave",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}}),
        );
    }

    pub fn did_close(&self, path: &Path) {
        self.notify(
            "textDocument/didClose",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}}),
        );
    }

    /// Where the symbol at `at` is defined; `Err` carries the server's own reason.
    pub async fn definition(&self, path: &Path, at: Position) -> Result<Vec<Location>, String> {
        let reply = self.request(
            "textDocument/definition",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at}),
        );
        locations(reply).await
    }

    /// The concrete types or methods behind the interface or method at `at`.
    pub async fn implementation(&self, path: &Path, at: Position) -> Result<Vec<Location>, String> {
        let reply = self.request(
            "textDocument/implementation",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at}),
        );
        locations(reply).await
    }

    /// Where the type of the expression at `at` is defined.
    pub async fn type_definition(
        &self,
        path: &Path,
        at: Position,
    ) -> Result<Vec<Location>, String> {
        let reply = self.request(
            "textDocument/typeDefinition",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at}),
        );
        locations(reply).await
    }

    /// Every use of the symbol at `at`, its declaration included.
    pub async fn references(&self, path: &Path, at: Position) -> Result<Vec<Location>, String> {
        let reply = self.request(
            "textDocument/references",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at,
                   "context": {"includeDeclaration": true}}),
        );
        locations(reply).await
    }

    /// Where the symbol at `at` is used in its file, as the editor marks it while the cursor rests.
    pub async fn document_highlights(
        &self,
        path: &Path,
        at: Position,
    ) -> Result<Vec<Highlight>, String> {
        let reply = self.request(
            "textDocument/documentHighlight",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at}),
        );
        Ok(protocol::parse_highlights(&answer(reply).await?))
    }

    /// The notes to draw inside `range` of the file, such as parameter names and inferred types.
    pub async fn inlay_hints(&self, path: &Path, range: Range) -> Result<Vec<InlayHint>, String> {
        let reply = self.request(
            "textDocument/inlayHint",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "range": range}),
        );
        Ok(protocol::parse_inlay_hints(&answer(reply).await?))
    }

    /// Documentation for the symbol at `at`; `None` when the server has nothing to say.
    pub async fn hover(&self, path: &Path, at: Position) -> Result<Option<Hover>, String> {
        let reply = self.request(
            "textDocument/hover",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at}),
        );
        Ok(parse_hover(&answer(reply).await?))
    }

    /// The signature of the call around `at`; `None` outside any call.
    pub async fn signature_help(
        &self,
        path: &Path,
        at: Position,
    ) -> Result<Option<SignatureHelp>, String> {
        let reply = self.request(
            "textDocument/signatureHelp",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at}),
        );
        Ok(parse_signature_help(&answer(reply).await?))
    }

    /// The edits that format the whole document, indenting with tabs or `tab_size` spaces.
    pub async fn formatting(
        &self,
        path: &Path,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<Vec<TextEdit>, String> {
        let reply = self.request(
            "textDocument/formatting",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)},
                   "options": {"tabSize": tab_size, "insertSpaces": insert_spaces}}),
        );
        Ok(parse_text_edits(&answer(reply).await?))
    }

    /// Whether the symbol at `at` can be renamed, and which text a rename replaces.
    /// `Ok(None)` means nothing renameable is there.
    pub async fn prepare_rename(
        &self,
        path: &Path,
        at: Position,
    ) -> Result<Option<RenameTarget>, String> {
        let reply = self.request(
            "textDocument/prepareRename",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at}),
        );
        let result = answer(reply).await?;
        if result.get("defaultBehavior").is_some() {
            return Ok(Some(RenameTarget::Word));
        }
        let range = result.get("range").unwrap_or(&result);
        let placeholder = result
            .get("placeholder")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(serde_json::from_value(range.clone())
            .ok()
            .map(|range| RenameTarget::Range(range, placeholder)))
    }

    /// The edit that renames the symbol at `at` to `name` everywhere it is used.
    pub async fn rename(
        &self,
        path: &Path,
        at: Position,
        name: &str,
    ) -> Result<WorkspaceEdit, String> {
        let reply = self.request(
            "textDocument/rename",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at,
                   "newName": name}),
        );
        let result = answer(reply).await?;
        if result.is_null() {
            return Ok(WorkspaceEdit::default());
        }
        parse_workspace_edit(&result).ok_or_else(|| "the server sent an unreadable edit".into())
    }

    /// Fixes and refactorings for `range`, given the diagnostics there as the server published
    /// them; `only` narrows to kinds such as `quickfix`.
    pub async fn code_actions(
        &self,
        path: &Path,
        range: Range,
        diagnostics: Vec<Value>,
        only: Option<&[&str]>,
    ) -> Result<Vec<CodeAction>, String> {
        let mut context = json!({"diagnostics": diagnostics, "triggerKind": 1});
        if let Some(only) = only {
            context["only"] = json!(only);
        }
        let reply = self.request(
            "textDocument/codeAction",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "range": range,
                   "context": context}),
        );
        Ok(parse_code_actions(&answer(reply).await?))
    }

    /// Fills in the edit of an action the server sent without one.
    pub async fn resolve_code_action(&self, action: &CodeAction) -> Result<CodeAction, String> {
        let reply = self.request("codeAction/resolve", action.raw.clone());
        parse_code_action(&answer(reply).await?)
            .ok_or_else(|| "the server sent an unreadable action".into())
    }

    /// Runs a server command; any edit it makes arrives as [`Event::ApplyEdit`].
    pub async fn execute_command(&self, command: &code_action::Command) -> Result<Value, String> {
        let mut params = json!({"command": command.command});
        if let Some(arguments) = &command.arguments {
            params["arguments"] = arguments.clone();
        }
        let reply = self.request("workspace/executeCommand", params);
        answer(reply).await
    }

    /// The symbols declared in a file, outermost first.
    pub async fn document_symbols(&self, path: &Path) -> Result<Vec<Symbol>, String> {
        let reply = self.request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}}),
        );
        Ok(parse_symbols(&answer(reply).await?, Some(path)))
    }

    /// The function or method at `at`, to ask for its callers and callees.
    pub async fn prepare_call_hierarchy(
        &self,
        path: &Path,
        at: Position,
    ) -> Result<Vec<CallItem>, String> {
        let reply = self.request(
            "textDocument/prepareCallHierarchy",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at}),
        );
        Ok(parse_items(&answer(reply).await?))
    }

    /// The functions that call `item`, each with its calls to it.
    pub async fn incoming_calls(&self, item: &CallItem) -> Result<Vec<Call>, String> {
        let reply = self.request("callHierarchy/incomingCalls", json!({"item": item.raw}));
        Ok(parse_calls(&answer(reply).await?, "from"))
    }

    /// The functions `item` calls, each with where `item` calls it.
    pub async fn outgoing_calls(&self, item: &CallItem) -> Result<Vec<Call>, String> {
        let reply = self.request("callHierarchy/outgoingCalls", json!({"item": item.raw}));
        Ok(parse_calls(&answer(reply).await?, "to"))
    }

    /// The diagnostics a pull-model server, such as vscode-eslint-language-server 3, finds in
    /// the file; it publishes none by itself.
    pub async fn pull_diagnostics(&self, path: &Path) -> Result<Vec<Diagnostic>, String> {
        let uri = protocol::uri_from_path(path);
        let reply = self.request(
            "textDocument/diagnostic",
            json!({"textDocument": {"uri": uri}}),
        );
        let result = answer(reply).await?;
        let items = result.get("items").cloned().unwrap_or_else(|| json!([]));
        Ok(
            protocol::parse_diagnostics(&json!({"uri": uri, "diagnostics": items}))
                .map(|(_, list)| list)
                .unwrap_or_default(),
        )
    }

    /// Symbols anywhere in the workspace whose names match `query`, as the server matches.
    pub async fn workspace_symbols(&self, query: &str) -> Result<Vec<Symbol>, String> {
        let reply = self.request("workspace/symbol", json!({"query": query}));
        Ok(parse_symbols(&answer(reply).await?, None))
    }

    /// Tells the server its settings changed; servers such as gopls then ask for them again.
    pub fn did_change_configuration(&self, settings: Value) {
        self.notify(
            "workspace/didChangeConfiguration",
            json!({"settings": settings}),
        );
    }

    /// Tells the server files changed on disk outside the documents it has open.
    pub fn did_change_watched_files(&self, changes: &[(PathBuf, FileEvent)]) {
        let changes: Vec<Value> = changes
            .iter()
            .map(|(path, kind)| json!({"uri": protocol::uri_from_path(path), "type": *kind as u8}))
            .collect();
        self.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes": changes}),
        );
    }

    /// Suggestions at `at`; `trigger` is the character typed that asked for them, if any.
    pub async fn completion(
        &self,
        path: &Path,
        at: Position,
        trigger: Option<&str>,
    ) -> Result<CompletionList, String> {
        let context = match trigger {
            Some(c) => json!({"triggerKind": 2, "triggerCharacter": c}),
            None => json!({"triggerKind": 1}),
        };
        let reply = self.request(
            "textDocument/completion",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at,
                   "context": context}),
        );
        Ok(parse_completions(&answer(reply).await?))
    }

    /// Fills in what the server left out of a suggestion, such as its documentation or the
    /// import TypeScript adds with it; the item comes back unchanged if it resolves nothing.
    pub async fn resolve_completion(
        &self,
        item: &CompletionItem,
    ) -> Result<CompletionItem, String> {
        if !self.supports("/completionProvider/resolveProvider") {
            return Ok(item.clone());
        }
        let reply = self.request("completionItem/resolve", item.raw.clone());
        Ok(item.resolved_with(&answer(reply).await?))
    }

    /// Whether the server asked to hear of a rename of `path`: before it happens (`will`), so it
    /// can answer with edits such as updated imports, or after.
    pub fn wants_rename(&self, will: bool, path: &Path, is_dir: bool) -> bool {
        let pointer = match will {
            true => "/workspace/fileOperations/willRename/filters",
            false => "/workspace/fileOperations/didRename/filters",
        };
        self.capabilities
            .get()
            .and_then(|c| c.pointer(pointer))
            .is_some_and(|filters| file_ops::matches_filters(filters, path, is_dir))
    }

    /// The edit to make before `renames` (from, to) happen on disk.
    pub async fn will_rename_files(
        &self,
        renames: &[(PathBuf, PathBuf)],
    ) -> Result<WorkspaceEdit, String> {
        let reply = self.request(
            "workspace/willRenameFiles",
            file_ops::rename_params(renames),
        );
        let result = answer(reply).await?;
        if result.is_null() {
            return Ok(WorkspaceEdit::default());
        }
        parse_workspace_edit(&result).ok_or_else(|| "the server sent an unreadable edit".into())
    }

    /// Tells the server files were renamed on disk.
    pub fn did_rename_files(&self, renames: &[(PathBuf, PathBuf)]) {
        self.notify("workspace/didRenameFiles", file_ops::rename_params(renames));
    }

    /// The type at `at`, to ask for its supertypes and subtypes.
    pub async fn prepare_type_hierarchy(
        &self,
        path: &Path,
        at: Position,
    ) -> Result<Vec<CallItem>, String> {
        let reply = self.request(
            "textDocument/prepareTypeHierarchy",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at}),
        );
        Ok(parse_items(&answer(reply).await?))
    }

    /// The types `item` extends or implements (`supertypes`), or those extending it.
    pub async fn type_hierarchy(
        &self,
        item: &CallItem,
        supertypes: bool,
    ) -> Result<Vec<CallItem>, String> {
        let method = match supertypes {
            true => "typeHierarchy/supertypes",
            false => "typeHierarchy/subtypes",
        };
        let reply = self.request(method, json!({"item": item.raw}));
        Ok(parse_items(&answer(reply).await?))
    }

    /// For each of `positions`, the ranges around it that selection grows through, innermost
    /// first.
    pub async fn selection_ranges(
        &self,
        path: &Path,
        positions: &[Position],
    ) -> Result<Vec<Vec<Range>>, String> {
        let reply = self.request(
            "textDocument/selectionRange",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "positions": positions}),
        );
        Ok(parse_selection_ranges(&answer(reply).await?))
    }

    /// The edits that format `range` of the document.
    pub async fn range_formatting(
        &self,
        path: &Path,
        range: Range,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<Vec<TextEdit>, String> {
        let reply = self.request(
            "textDocument/rangeFormatting",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "range": range,
                   "options": {"tabSize": tab_size, "insertSpaces": insert_spaces}}),
        );
        Ok(parse_text_edits(&answer(reply).await?))
    }

    /// The ranges edited together with the one at `at`, such as a tag's matching tag name.
    pub async fn linked_editing_ranges(
        &self,
        path: &Path,
        at: Position,
    ) -> Result<LinkedRanges, String> {
        let reply = self.request(
            "textDocument/linkedEditingRange",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at}),
        );
        Ok(parse_linked_ranges(&answer(reply).await?))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.outgoing.try_send(Outgoing::Shutdown);
        let child = self.child.clone();
        // A server that ignores shutdown, or never finished starting, is killed with what it
        // started: biome's node wrapper leaves a native lsp-proxy holding the pipes otherwise.
        let _ = thread::Builder::new()
            .name("lsp-reaper".into())
            .spawn(move || {
                thread::sleep(SHUTDOWN_GRACE);
                if let Some(mut c) = child.lock().expect("child lock").take() {
                    // SAFETY: the unreaped child keeps its group id from being reused.
                    unsafe { libc::killpg(c.id() as libc::pid_t, libc::SIGKILL) };
                    let _ = c.wait();
                }
            });
    }
}

struct Session {
    kind: ServerKind,
    program: Option<PathBuf>,
    root: PathBuf,
    config: Config,
    outgoing: async_channel::Receiver<Outgoing>,
    events: async_channel::Sender<Event>,
    pending: Pending,
    child: Arc<Mutex<Option<Child>>>,
    triggers: Arc<OnceLock<Vec<String>>>,
    signature_triggers: Arc<OnceLock<Vec<String>>>,
    capabilities: Arc<OnceLock<Value>>,
}

impl Session {
    fn run(self) -> Result<()> {
        let program = self
            .program
            .clone()
            .or_else(|| env::find_program(self.kind.program()))
            .with_context(|| {
                format!(
                    "{} is not installed. Install it with: {}",
                    self.kind.program(),
                    self.kind.install_hint()
                )
            })?;
        let mut child = Command::new(&program)
            .args(self.kind.args())
            .env_clear()
            .envs(env::server_env(
                self.program.is_some(),
                self.toolchain().as_deref(),
            ))
            .current_dir(&self.root)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("could not start {}", program.display()))?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        *self.child.lock().expect("child lock") = Some(child);
        self.serve(stdin, stdout)
    }

    /// The Go toolchain gopls runs with when its settings choose one, as `env.GOTOOLCHAIN`.
    fn toolchain(&self) -> Option<String> {
        if self.kind != ServerKind::Go {
            return None;
        }
        let config = self.config.read().ok()?;
        Some(config.pointer("/env/GOTOOLCHAIN")?.as_str()?.to_string())
    }

    fn serve(
        self,
        stdin: impl Write + Send + 'static,
        stdout: impl Read + Send + 'static,
    ) -> Result<()> {
        let (writer, frames) = mpsc::channel();
        thread::Builder::new()
            .name("lsp-writer".into())
            .spawn(move || write_frames(BufWriter::new(stdin), frames))?;
        let reader = Reader {
            program: self.kind.program(),
            config: self.config.clone(),
            writer: writer.clone(),
            outgoing: self.outgoing.clone(),
            pending: self.pending.clone(),
            events: self.events.clone(),
        };
        thread::Builder::new()
            .name("lsp-reader".into())
            .spawn(move || reader.run(stdout))?;

        let init = {
            let (tx, rx) = async_channel::bounded(1);
            self.pending.lock().expect("pending lock").insert(0, tx);
            rx
        };
        send(&writer, request(0, "initialize", self.initialize_params()))?;
        let result = init
            .recv_blocking()
            .context("the server stopped while starting")?
            .map_err(anyhow::Error::msg)?;
        if result.is_null() {
            bail!("{} refused to start", self.kind.program());
        }
        let strings = |pointer: &str| -> Vec<String> {
            result
                .pointer(pointer)
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        let _ = self.triggers.set(strings(
            "/capabilities/completionProvider/triggerCharacters",
        ));
        let mut signature = strings("/capabilities/signatureHelpProvider/triggerCharacters");
        signature.extend(strings(
            "/capabilities/signatureHelpProvider/retriggerCharacters",
        ));
        let _ = self.signature_triggers.set(signature);
        let _ = self
            .capabilities
            .set(result.get("capabilities").cloned().unwrap_or(Value::Null));
        send(&writer, notification("initialized", json!({})))?;
        let _ = self.events.send_blocking(Event::Ready);

        while let Ok(message) = self.outgoing.recv_blocking() {
            let frame = match message {
                Outgoing::Notify(method, params) => notification(method, params),
                Outgoing::Request(id, method, params) => request(id, method, params),
                Outgoing::Shutdown => break,
            };
            send(&writer, frame)?;
        }
        let _ = send(&writer, request(i64::MAX, "shutdown", Value::Null));
        let _ = send(&writer, notification("exit", Value::Null));
        Ok(())
    }

    fn initialize_params(&self) -> Value {
        let uri = protocol::uri_from_path(&self.root);
        let name = self
            .root
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let options = self.config.read().map_or(Value::Null, |c| c.clone());
        let mut params = json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "athena", "version": env!("CARGO_PKG_VERSION")},
            "rootUri": uri,
            "workspaceFolders": [{"uri": uri, "name": name}],
            "capabilities": {
                "general": {"positionEncodings": ["utf-16"]},
                "workspace": {
                    "configuration": true,
                    "workspaceFolders": true,
                    "applyEdit": true,
                    // Deleting files for a server is refused, so it is not offered.
                    "workspaceEdit": {
                        "documentChanges": true,
                        "resourceOperations": ["create", "rename"],
                        "failureHandling": "abort"
                    },
                    "executeCommand": {},
                    "inlayHint": {"refreshSupport": true},
                    "symbol": {},
                    "didChangeWatchedFiles": {},
                    "fileOperations": {"willRename": true, "didRename": true}
                },
                "textDocument": {
                    "synchronization": {"didSave": true},
                    "publishDiagnostics": {},
                    "definition": {"linkSupport": true},
                    "implementation": {"linkSupport": true},
                    "typeDefinition": {"linkSupport": true},
                    "references": {},
                    "documentHighlight": {},
                    "inlayHint": {},
                    "rename": {"prepareSupport": true},
                    "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                    "callHierarchy": {},
                    "typeHierarchy": {},
                    "selectionRange": {},
                    "rangeFormatting": {},
                    "linkedEditingRange": {},
                    "codeAction": {
                        "codeActionLiteralSupport": {
                            "codeActionKind": {"valueSet": [
                                "", "quickfix", "refactor", "refactor.extract", "refactor.inline",
                                "refactor.rewrite", "source", "source.organizeImports",
                                "source.fixAll"
                            ]}
                        },
                        "isPreferredSupport": true,
                        "disabledSupport": true,
                        "dataSupport": true,
                        "resolveSupport": {"properties": ["edit"]}
                    },
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "formatting": {},
                    "signatureHelp": {
                        "signatureInformation": {
                            "documentationFormat": ["markdown", "plaintext"],
                            "parameterInformation": {"labelOffsetSupport": true}
                        }
                    },
                    "completion": {
                        "completionItem": {
                            "snippetSupport": true,
                            "documentationFormat": ["markdown", "plaintext"],
                            "resolveSupport": {
                                "properties": ["documentation", "detail", "additionalTextEdits"]
                            }
                        },
                        "contextSupport": true
                    }
                }
            }
        });
        if !options.is_null() {
            params["initializationOptions"] = options;
        }
        // Only ESLint is asked to be pulled from; gopls and others keep publishing.
        if self.kind == ServerKind::Eslint {
            params["capabilities"]["textDocument"]["diagnostic"] = json!({});
            params["capabilities"]["workspace"]["diagnostics"] = json!({"refreshSupport": true});
        }
        params
    }
}

struct Reader {
    program: &'static str,
    config: Config,
    writer: Frames,
    outgoing: async_channel::Receiver<Outgoing>,
    pending: Pending,
    events: async_channel::Sender<Event>,
}

impl Reader {
    fn run(self, stdout: impl Read) {
        let mut input = BufReader::new(stdout);
        while let Ok(Some(message)) = read_message(&mut input) {
            self.handle(message);
        }
        // Waiters see a closed channel instead of hanging; closing the queue first means a
        // request that misses this clear is refused by the queue instead.
        self.outgoing.close();
        self.pending.lock().expect("pending lock").clear();
        let _ = self
            .events
            .send_blocking(Event::Stopped("the language server exited".into()));
    }

    fn handle(&self, message: Value) {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();
        match (method, id) {
            (None, Some(id)) => {
                let waiter = id
                    .as_i64()
                    .and_then(|id| self.pending.lock().expect("pending lock").remove(&id));
                if let Some(waiter) = waiter {
                    tracing::debug!(%id, failed = message.get("error").is_some(), "lsp reply");
                    let reply = match message.get("error") {
                        Some(error) => Err(error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("the request failed")
                            .to_string()),
                        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = waiter.try_send(reply);
                }
            }
            (Some("workspace/applyEdit"), Some(id)) => {
                let params = message.get("params");
                let label = params
                    .and_then(|p| p.get("label"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let mut reply = EditReply {
                    writer: self.writer.clone(),
                    id,
                    answered: false,
                };
                match params
                    .and_then(|p| p.get("edit"))
                    .and_then(parse_workspace_edit)
                {
                    Some(edit) => {
                        let _ = self
                            .events
                            .send_blocking(Event::ApplyEdit { label, edit, reply });
                    }
                    None => reply.answer(Err("Athena could not read this edit".into())),
                }
            }
            // Servers wait for these answers; gopls stalls without them.
            (Some(method), Some(id)) => {
                if method == "workspace/inlayHint/refresh" {
                    let _ = self.events.send_blocking(Event::RefreshInlayHints);
                }
                if method == "workspace/diagnostic/refresh" {
                    let _ = self.events.send_blocking(Event::RefreshDiagnostics);
                }
                let result = match method {
                    "workspace/configuration" => {
                        let config = self.config.read().map_or(Value::Null, |c| c.clone());
                        let items = message
                            .pointer("/params/items")
                            .and_then(Value::as_array)
                            .map_or(&[][..], Vec::as_slice);
                        Value::Array(
                            items
                                .iter()
                                .map(|item| {
                                    let section = item.get("section").and_then(Value::as_str);
                                    configuration_section(&config, self.program, section)
                                })
                                .collect(),
                        )
                    }
                    "workspace/workspaceFolders" => Value::Array(Vec::new()),
                    // Older ESLint servers ask before running the project's ESLint; 4 approves.
                    "eslint/confirmESLintExecution" => Value::from(4),
                    _ => Value::Null,
                };
                let _ = send(
                    &self.writer,
                    json!({"jsonrpc": "2.0", "id": id, "result": result}),
                );
            }
            (Some("textDocument/publishDiagnostics"), None) => {
                if let Some((path, list)) =
                    message.get("params").and_then(protocol::parse_diagnostics)
                {
                    let _ = self.events.send_blocking(Event::Diagnostics { path, list });
                }
            }
            _ => {}
        }
    }
}

/// The part of a server's settings a `workspace/configuration` item asks for: all of them for
/// no section or the server's own name (gopls asks for "gopls"), else the dotted path in them.
fn configuration_section(config: &Value, program: &str, section: Option<&str>) -> Value {
    match section {
        None | Some("") => config.clone(),
        Some(s) if s == program => config.clone(),
        Some(s) => config
            .pointer(&format!("/{}", s.replace('.', "/")))
            .cloned()
            .unwrap_or(Value::Null),
    }
}

async fn answer(reply: async_channel::Receiver<Reply>) -> Result<Value, String> {
    reply
        .recv()
        .await
        .unwrap_or_else(|_| Err("the language server exited".into()))
}

async fn locations(reply: async_channel::Receiver<Reply>) -> Result<Vec<Location>, String> {
    answer(reply).await.map(|v| protocol::parse_locations(&v))
}

fn request(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

fn send(writer: &Frames, message: Value) -> Result<()> {
    writer
        .send(message)
        .map_err(|_| anyhow!("the language server has exited"))
}

/// Writes queued frames in order until the server stops reading or every sender is gone.
fn write_frames(mut out: impl Write, frames: mpsc::Receiver<Value>) {
    for message in frames {
        let Ok(body) = serde_json::to_vec(&message) else {
            continue;
        };
        let written = write!(out, "Content-Length: {}\r\n\r\n", body.len())
            .and_then(|()| out.write_all(&body))
            .and_then(|()| out.flush());
        if written.is_err() {
            return;
        }
    }
}

fn read_message(input: &mut impl BufRead) -> Result<Option<Value>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.context("message without Content-Length")?;
    if length > MAX_MESSAGE {
        bail!("message of {length} bytes is too large");
    }
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::time::Instant;

    fn write_message(out: &mut impl Write, message: &Value) {
        let body = serde_json::to_vec(message).unwrap();
        write!(out, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        out.write_all(&body).unwrap();
    }

    #[test]
    fn dropping_a_server_kills_the_processes_it_started() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("athena-lsp-group-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("server");
        let pid_file = dir.join("helper.pid");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nexec sleep 30\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let alive = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;
        let (client, _events) =
            Client::start_local(ServerKind::Biome, program, dir.clone(), Config::default());
        let started = Instant::now();
        let helper: i32 = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                break pid;
            }
            assert!(started.elapsed() < Duration::from_secs(5), "never started");
            thread::sleep(Duration::from_millis(20));
        };
        assert!(alive(helper));
        drop(client);
        let dropped = Instant::now();
        while alive(helper) && dropped.elapsed() < SHUTDOWN_GRACE + Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(helper), "the server's own child outlived it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A server that floods configuration requests while the client sends a huge didChange.
    #[test]
    fn answering_server_requests_never_waits_behind_a_large_write() {
        const REQUESTS: i64 = 2000;
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let (out_tx, out_rx) = async_channel::unbounded();
        let (events_tx, events) = async_channel::unbounded();
        let session = Session {
            kind: ServerKind::Go,
            program: None,
            root: PathBuf::from("/tmp"),
            config: Arc::new(RwLock::new(json!({"hints": {"parameterNames": true}}))),
            outgoing: out_rx,
            events: events_tx,
            pending: Arc::default(),
            child: Arc::default(),
            triggers: Arc::default(),
            signature_triggers: Arc::default(),
            capabilities: Arc::default(),
        };
        let stdin = client_end.try_clone().unwrap();
        thread::spawn(move || session.serve(stdin, client_end));

        let (done_tx, done) = mpsc::channel();
        thread::spawn(move || {
            let mut input = BufReader::new(server_end.try_clone().unwrap());
            let mut output = server_end;
            let init = read_message(&mut input).unwrap().unwrap();
            let settings = json!({"hints": {"parameterNames": true}});
            assert_eq!(init["params"]["initializationOptions"], settings);
            write_message(
                &mut output,
                &json!({"jsonrpc": "2.0", "id": init["id"], "result": {"capabilities": {}}}),
            );
            let initialized = read_message(&mut input).unwrap().unwrap();
            assert_eq!(initialized["method"], "initialized");
            for id in 0..REQUESTS {
                write_message(
                    &mut output,
                    &json!({"jsonrpc": "2.0", "id": 1000 + id, "method": "workspace/configuration",
                            "params": {"items": [{"section": "gopls"}]}}),
                );
            }
            let (mut replies, mut changed) = (0, false);
            while replies < REQUESTS || !changed {
                let message = read_message(&mut input).unwrap().unwrap();
                match message["method"].as_str() {
                    Some("textDocument/didChange") => changed = true,
                    Some(_) => {}
                    None => {
                        assert_eq!(message["result"], json!([settings]));
                        replies += 1;
                    }
                }
            }
            let _ = done_tx.send(());
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(events.try_recv(), Ok(Event::Ready)) {
            assert!(Instant::now() < deadline, "never became ready");
            thread::sleep(Duration::from_millis(5));
        }
        let text = "x".repeat(4 * 1024 * 1024);
        out_tx
            .try_send(Outgoing::Notify(
                "textDocument/didChange",
                json!({"contentChanges": [{"text": text}]}),
            ))
            .unwrap();
        assert!(
            done.recv_timeout(Duration::from_secs(20)).is_ok(),
            "client and server deadlocked"
        );
    }

    /// A client wired to a scripted server: `answer` replies to each request by method, and every
    /// message the client sends after `initialized` is passed on to the returned channel.
    fn scripted(
        capabilities: Value,
        answer: impl Fn(&str, &Value) -> Value + Send + 'static,
    ) -> (Client, mpsc::Receiver<Value>) {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let (out_tx, out_rx) = async_channel::unbounded();
        let (events_tx, events) = async_channel::unbounded();
        let pending: Pending = Arc::default();
        let capabilities_slot: Arc<OnceLock<Value>> = Arc::default();
        let session = Session {
            kind: ServerKind::TypeScript,
            program: None,
            root: PathBuf::from("/tmp"),
            config: Config::default(),
            outgoing: out_rx,
            events: events_tx,
            pending: pending.clone(),
            child: Arc::default(),
            triggers: Arc::default(),
            signature_triggers: Arc::default(),
            capabilities: capabilities_slot.clone(),
        };
        let stdin = client_end.try_clone().unwrap();
        thread::spawn(move || session.serve(stdin, client_end));
        let (seen_tx, seen) = mpsc::channel();
        thread::spawn(move || {
            let mut input = BufReader::new(server_end.try_clone().unwrap());
            let mut output = server_end;
            let init = read_message(&mut input).unwrap().unwrap();
            let _ = seen_tx.send(init.clone());
            write_message(
                &mut output,
                &json!({"jsonrpc": "2.0", "id": init["id"], "result": {"capabilities": capabilities}}),
            );
            while let Ok(Some(message)) = read_message(&mut input) {
                if let (Some(method), Some(id)) = (message["method"].as_str(), message.get("id")) {
                    let result = answer(method, &message["params"]);
                    write_message(
                        &mut output,
                        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    );
                }
                let _ = seen_tx.send(message);
            }
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(events.try_recv(), Ok(Event::Ready)) {
            assert!(Instant::now() < deadline, "never became ready");
            thread::sleep(Duration::from_millis(5));
        }
        let client = Client {
            outgoing: out_tx,
            pending,
            next_id: AtomicI64::new(1),
            child: Arc::default(),
            triggers: Arc::default(),
            signature_triggers: Arc::default(),
            capabilities: capabilities_slot,
        };
        (client, seen)
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::task::{Context, Poll, Wake, Waker};
        struct Unpark(thread::Thread);
        impl Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(Unpark(thread::current())));
        let mut cx = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                return out;
            }
            thread::park_timeout(Duration::from_millis(50));
        }
    }

    fn sent(seen: &mpsc::Receiver<Value>, method: &str) -> Value {
        loop {
            let message = seen
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("{method} never sent"));
            if message["method"] == method {
                return message;
            }
        }
    }

    fn range(a: (u32, u32), b: (u32, u32)) -> Value {
        json!({"start": {"line": a.0, "character": a.1}, "end": {"line": b.0, "character": b.1}})
    }

    #[test]
    fn the_client_offers_the_newer_features_it_handles() {
        let (_client, seen) = scripted(json!({}), |_, _| Value::Null);
        let init = sent(&seen, "initialize");
        let caps = &init["params"]["capabilities"];
        assert_eq!(caps["workspace"]["fileOperations"]["willRename"], true);
        assert_eq!(caps["workspace"]["fileOperations"]["didRename"], true);
        for feature in [
            "typeHierarchy",
            "selectionRange",
            "rangeFormatting",
            "linkedEditingRange",
        ] {
            assert!(caps["textDocument"][feature].is_object(), "{feature}");
        }
        let completion = &caps["textDocument"]["completion"]["completionItem"];
        assert_eq!(
            completion["resolveSupport"]["properties"],
            json!(["documentation", "detail", "additionalTextEdits"])
        );
    }

    #[test]
    fn renames_reach_only_servers_whose_filters_match_and_their_edit_comes_back() {
        let filters = json!({"workspace": {"fileOperations": {
            "willRename": {"filters": [{"scheme": "file", "pattern": {"glob": "**/*.ts"}}]},
            "didRename": {"filters": [{"pattern": {"glob": "**", "matches": "folder"}}]}
        }}});
        let (client, seen) = scripted(filters, |method, params| match method {
            "workspace/willRenameFiles" => {
                assert_eq!(params["files"][0]["oldUri"], "file:///p/a.ts");
                json!({"changes": {"file:///p/main.ts": [
                    {"range": range((0, 20), (0, 23)), "newText": "./b"}
                ]}})
            }
            _ => Value::Null,
        });
        assert!(client.wants_rename(true, Path::new("/p/a.ts"), false));
        assert!(!client.wants_rename(true, Path::new("/p/a.go"), false));
        assert!(!client.wants_rename(false, Path::new("/p/a.ts"), false));
        assert!(client.wants_rename(false, Path::new("/p/src"), true));
        let renames = [(PathBuf::from("/p/a.ts"), PathBuf::from("/p/b.ts"))];
        let edit = block_on(client.will_rename_files(&renames)).unwrap();
        assert_eq!(edit.changes.len(), 1);
        assert!(
            matches!(&edit.changes[0], crate::FileChange::Edit { path, .. } if path == Path::new("/p/main.ts"))
        );
        client.did_rename_files(&renames);
        let note = sent(&seen, "workspace/didRenameFiles");
        assert_eq!(note["params"]["files"][0]["newUri"], "file:///p/b.ts");
        assert!(note.get("id").is_none(), "a notification");
    }

    #[test]
    fn completion_resolve_selection_and_range_formatting_and_linked_ranges_round_trip() {
        let caps = json!({"completionProvider": {"resolveProvider": true}});
        let (client, seen) = scripted(caps, |method, params| match method {
            "completionItem/resolve" => {
                assert_eq!(params["data"], json!({"id": 7}), "the item as sent");
                json!({"label": "useState", "documentation": "Returns state.",
                       "additionalTextEdits": [{"range": range((0, 0), (0, 0)),
                                                "newText": "import { useState } from 'react';\n"}]})
            }
            "textDocument/selectionRange" => {
                assert_eq!(params["positions"].as_array().unwrap().len(), 2);
                json!([{"range": range((1, 2), (1, 5)), "parent": {"range": range((1, 0), (1, 9))}},
                       {"range": range((3, 0), (3, 1))}])
            }
            "textDocument/rangeFormatting" => {
                assert_eq!(params["range"], range((1, 0), (2, 0)));
                assert_eq!(params["options"]["tabSize"], 2);
                json!([{"range": range((1, 0), (1, 4)), "newText": "  "}])
            }
            "textDocument/linkedEditingRange" => {
                json!({"ranges": [range((0, 1), (0, 4)), range((0, 8), (0, 11))]})
            }
            "textDocument/prepareTypeHierarchy" => json!([{"name": "Shape", "kind": 11,
                "uri": "file:///p/s.go", "range": range((2, 0), (4, 1)),
                "selectionRange": range((2, 5), (2, 10)), "data": 1}]),
            "typeHierarchy/subtypes" => {
                assert_eq!(params["item"]["data"], 1);
                json!([{"name": "Square", "kind": 23, "uri": "file:///p/q.go",
                        "range": range((0, 0), (1, 0)), "selectionRange": range((0, 5), (0, 11))}])
            }
            _ => Value::Null,
        });
        let list = crate::completion::parse_completions(
            &json!([{"label": "useState", "kind": 3, "data": {"id": 7}}]),
        );
        let item = block_on(client.resolve_completion(&list.items[0])).unwrap();
        assert_eq!(item.additional_edits.len(), 1);
        assert_eq!(
            item.documentation,
            [crate::MarkupBlock::Text("Returns state.".into())]
        );

        let doc = Path::new("/p/a.tsx");
        let at = |line, character| Position { line, character };
        let chains = block_on(client.selection_ranges(doc, &[at(1, 3), at(3, 0)])).unwrap();
        assert_eq!(chains.iter().map(Vec::len).collect::<Vec<_>>(), [2, 1]);

        let whole_lines = Range {
            start: at(1, 0),
            end: at(2, 0),
        };
        let edits = block_on(client.range_formatting(doc, whole_lines, 2, true)).unwrap();
        assert_eq!(edits[0].text, "  ");

        let linked = block_on(client.linked_editing_ranges(doc, at(0, 2))).unwrap();
        assert_eq!(linked.ranges.len(), 2);

        let types = block_on(client.prepare_type_hierarchy(doc, at(2, 6))).unwrap();
        assert_eq!(types[0].name, "Shape");
        let subtypes = block_on(client.type_hierarchy(&types[0], false)).unwrap();
        assert_eq!(subtypes[0].name, "Square");
        assert_eq!(
            sent(&seen, "typeHierarchy/subtypes")["params"]["item"]["name"],
            "Shape"
        );
    }

    #[test]
    fn a_server_without_resolve_is_not_asked_and_the_item_stays() {
        let (client, _seen) = scripted(json!({"completionProvider": {}}), |method, _| {
            panic!("{method} should not be sent")
        });
        let list = crate::completion::parse_completions(&json!([{"label": "x", "data": 1}]));
        assert_eq!(
            block_on(client.resolve_completion(&list.items[0])).unwrap(),
            list.items[0]
        );
    }

    #[test]
    fn configuration_requests_are_answered_from_the_settings_per_section() {
        let config = json!({"hints": {"parameterNames": true}, "typescript": {"format": {"semicolons": "remove"}}});
        assert_eq!(
            configuration_section(&config, "gopls", Some("gopls")),
            config
        );
        assert_eq!(configuration_section(&config, "gopls", None), config);
        assert_eq!(
            configuration_section(&config, "x", Some("typescript.format")),
            json!({"semicolons": "remove"})
        );
        assert_eq!(
            configuration_section(&config, "gopls", Some("nope")),
            Value::Null
        );
        assert_eq!(
            configuration_section(&Value::Null, "gopls", Some("gopls")),
            Value::Null
        );
    }

    #[test]
    fn frames_are_read_with_their_length() {
        let body = br#"{"jsonrpc":"2.0","method":"x"}"#;
        let mut raw =
            format!("Content-Length: {}\r\nContent-Type: x\r\n\r\n", body.len()).into_bytes();
        raw.extend_from_slice(body);
        raw.extend_from_slice(b"Content-Length: 2\r\n\r\n{}");
        let mut input = BufReader::new(&raw[..]);
        assert_eq!(read_message(&mut input).unwrap().unwrap()["method"], "x");
        assert_eq!(read_message(&mut input).unwrap().unwrap(), json!({}));
        assert!(read_message(&mut input).unwrap().is_none());
    }
}
