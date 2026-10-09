use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use athena_editor::{
    Completion, EditorView, HoverBlock, Inlay, Lang, LspRequest, Marker, MarkerSeverity,
    Occurrence, Resolved, ServerEdit, Signature,
};
use athena_lsp::{
    Client, CompletionItem, Config, Diagnostic, Event, Location, MarkupBlock, Position, Range,
    ServerKind, Severity,
};
use athena_proto::{DiagnosticInfo, NoticeKind};
use athena_ui::ActiveTheme;
use athena_workspace::LinterTrust;
use gpui::{
    AnyElement, Context, Entity, EntityId, FontWeight, PromptButton, PromptLevel, Task, WeakEntity,
    Window, actions, div, prelude::*, px, uniform_list,
};

use super::Shell;
use super::drawer::DrawerTab;
use super::item::ItemView;

/// Typing pauses this long before the server gets the new text.
const CHANGE_DELAY: Duration = Duration::from_millis(300);

/// A slow formatter never holds up Cmd+S longer than this; the file saves unformatted.
const FORMAT_TIMEOUT: Duration = Duration::from_secs(1);

/// gopls reads its settings again a moment after being told they changed, and sends no refresh.
const SETTINGS_SETTLE: Duration = Duration::from_millis(300);

/// Longest line excerpt shown for a reference.
const SNIPPET_CHARS: usize = 160;

pub(super) const NO_SERVER: &str = "No language server runs for this file.";

actions!(athena, [AllowProjectLinters, DisallowProjectLinters]);

const PROJECT_LINTERS: [ServerKind; 2] = [ServerKind::Eslint, ServerKind::Biome];

/// A server that stops this many times within `CRASH_WINDOW` is left stopped, as in VS Code.
const MAX_CRASHES: usize = 5;
const CRASH_WINDOW: Duration = Duration::from_secs(180);

type ServerKey = (PathBuf, ServerKind);

/// The answer index of [`confirm_buttons`]' `action`.
pub(super) const CONFIRMED: usize = 1;

/// Buttons for a prompt whose `action` is risky: Escape cancels and Return picks neither, as
/// gpui gives Return to the first button unless it is a Cancel one.
pub(super) fn confirm_buttons(cancel: &str, action: &str) -> [PromptButton; 2] {
    [
        PromptButton::cancel(cancel.to_string()),
        PromptButton::ok(action.to_string()),
    ]
}

struct Server {
    client: Rc<Client>,
    config: Config,
    ready: bool,
    _events: Task<()>,
}

#[derive(Default)]
pub(super) struct LspState {
    servers: HashMap<ServerKey, Server>,
    /// Servers that failed to start, not retried until Athena restarts.
    failed: HashMap<ServerKey, String>,
    /// Each file's diagnostics, by the server that published them.
    pub(super) diagnostics: HashMap<PathBuf, Vec<(ServerKey, Vec<Diagnostic>)>>,
    /// Documents the servers have open, and which server has each.
    documents: HashMap<PathBuf, ServerKey>,
    /// Documents the project's linters (ESLint, Biome) have open beside their main server.
    linters: HashMap<PathBuf, Vec<ServerKey>>,
    changes: HashMap<PathBuf, Task<()>>,
    /// When each server last stopped unexpectedly, within `CRASH_WINDOW`.
    crashes: HashMap<ServerKey, Vec<Instant>>,
    restarts: HashMap<ServerKey, Task<()>>,
    pub(super) jump: Option<(PathBuf, Position)>,
    /// The document, cursor and editor a rename field is open for.
    pub(super) renaming: Option<(PathBuf, Position, WeakEntity<EditorView>)>,
    /// A file name a terminal link gave that matched several files, for Go to File to narrow.
    find_file: Option<String>,
    references: References,
    /// The project the references were asked from; other projects show the drawer tab empty.
    references_root: Option<PathBuf>,
    /// Bumped per lookup so a slow answer cannot replace a newer one.
    references_asked: u64,
    reference_opened: Option<usize>,
    /// What the References tab lists: `reference` or `implementation`.
    references_noun: &'static str,
    /// The References tab shows the call hierarchy rather than the last lookup.
    pub(super) showing_calls: bool,
    pub(super) calls: Option<super::calls::Calls>,
    pub(super) calls_client: Option<Rc<Client>>,
    /// Project roots whose "Run this project's code?" question is on screen.
    asking_trust: HashSet<PathBuf>,
    /// Projects whose own TypeScript waits for trust with no global one to run meanwhile.
    held_back: HashSet<PathBuf>,
    /// Each editor's last suggestions as the server sent them, to resolve one on request.
    completions: HashMap<EntityId, (u64, Rc<Client>, Vec<CompletionItem>)>,
}

impl LspState {
    /// Whether an answer `client` gave about `doc` still applies: that instance of the server
    /// still runs and still has the file open, so a late answer cannot revive either.
    fn answer_is_current(&self, key: &ServerKey, client: &Rc<Client>, doc: &Path) -> bool {
        let open = self.documents.get(doc) == Some(key)
            || self.linters.get(doc).is_some_and(|keys| keys.contains(key));
        open && self
            .servers
            .get(key)
            .is_some_and(|s| Rc::ptr_eq(&s.client, client))
    }
}

#[derive(Default)]
enum References {
    #[default]
    Idle,
    Loading,
    Found(Rc<Vec<Reference>>),
    Failed(String),
}

pub(super) struct Reference {
    path: PathBuf,
    at: Position,
    snippet: String,
}

/// Reads each file once, off the main thread, for the line every reference sits on.
fn with_snippets(mut found: Vec<Location>) -> Vec<Reference> {
    found.sort_by(|a, b| (&a.path, a.range.start).cmp(&(&b.path, b.range.start)));
    let mut text: Option<(PathBuf, Vec<String>)> = None;
    found
        .into_iter()
        .map(|l| {
            if text.as_ref().is_none_or(|(p, _)| *p != l.path) {
                let lines = std::fs::read_to_string(&l.path)
                    .map(|t| t.lines().map(str::to_string).collect())
                    .unwrap_or_default();
                text = Some((l.path.clone(), lines));
            }
            let snippet = text
                .as_ref()
                .and_then(|(_, lines)| lines.get(l.range.start.line as usize))
                .map(|line| line.trim().chars().take(SNIPPET_CHARS).collect())
                .unwrap_or_default();
            Reference {
                path: l.path,
                at: l.range.start,
                snippet,
            }
        })
        .collect()
}

/// Files under `root` whose path ends with `tail`, skipping ignored ones; at most a few.
fn files_ending_with(root: &Path, tail: &Path) -> Vec<PathBuf> {
    ignore::WalkBuilder::new(root)
        .hidden(true)
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()) && e.path().ends_with(tail))
        .map(ignore::DirEntry::into_path)
        .take(20)
        .collect()
}

/// How long to wait before restarting a server that has stopped `crashes` times in the window.
fn restart_delay(crashes: usize) -> Option<Duration> {
    (crashes < MAX_CRASHES).then(|| Duration::from_millis(500) * (1 << crashes.saturating_sub(1)))
}

/// The linters that run beside a file's main server when the project installs them.
fn linters_for(lang: Lang) -> &'static [ServerKind] {
    match lang {
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => &[ServerKind::Eslint, ServerKind::Biome],
        Lang::Json | Lang::Css => &[ServerKind::Biome],
        _ => &[],
    }
}

fn linter_name(kind: ServerKind) -> &'static str {
    match kind {
        ServerKind::Eslint => "ESLint",
        _ => "Biome",
    }
}

/// Settings a server reads only as it starts: which TypeScript and plugins tsserver loads, and
/// the Go toolchain gopls runs with.
fn needs_restart(kind: ServerKind, old: &serde_json::Value, new: &serde_json::Value) -> bool {
    let pointers: &[&str] = match kind {
        ServerKind::TypeScript => &["/tsserver", "/plugins"],
        ServerKind::Go => &["/env/GOTOOLCHAIN"],
        _ => &[],
    };
    pointers.iter().any(|p| old.pointer(p) != new.pointer(p))
}

fn project_name(root: &Path) -> String {
    root.file_name().map_or_else(
        || root.display().to_string(),
        |n| n.to_string_lossy().into(),
    )
}

/// The question and its detail for a project bringing `found` (ESLint, Biome, TypeScript) and,
/// with `settings`, settings that choose what language servers run.
fn trust_question(project: &str, found: &[&str], settings: bool) -> (String, String) {
    let names = match found {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    let message = match found.is_empty() {
        true => format!("Use {project}'s language server settings?"),
        false => format!("Run {names} from {project}?"),
    };
    let mut brings = Vec::new();
    if !found.is_empty() {
        brings.push(format!("installs {names} in node_modules"));
    }
    if settings {
        brings.push("has settings that choose what its language servers run".to_string());
    }
    let risk = match found.is_empty() {
        true => "Those settings can name programs, plugins, flags and toolchains",
        false => {
            "Running them runs the project's own code, with its config and plugins, on this Mac"
        }
    };
    let meanwhile = match found.contains(&"TypeScript") {
        true => {
            "Until then TypeScript uses your global install, and the project's editor settings \
                 still apply"
        }
        false => "Until then the project's editor settings still apply",
    };
    let detail = format!(
        "{project} {}. {risk}; allow it only for a project you trust. {meanwhile}. The command \
         palette can change this later.",
        brings.join(" and ")
    );
    (message, detail)
}

/// The servers a trust answer for `root` starts afresh: settings a server reads only at start,
/// such as gopls' environment, would otherwise outlive the trust that chose them, and
/// TypeScript goes back through the held-back check even when it is not running.
fn restarted_on_trust_change<'a>(
    running: impl Iterator<Item = &'a ServerKey>,
    root: &Path,
) -> Vec<ServerKey> {
    let mut keys: Vec<ServerKey> = running
        .filter(|(r, kind)| r == root && !kind.is_project_local())
        .cloned()
        .collect();
    let typescript = (root.to_path_buf(), ServerKind::TypeScript);
    if !keys.contains(&typescript) {
        keys.push(typescript);
    }
    keys
}

fn server_for(lang: Lang) -> Option<(ServerKind, &'static str)> {
    Some(match lang {
        Lang::Go => (ServerKind::Go, "go"),
        Lang::TypeScript => (ServerKind::TypeScript, "typescript"),
        Lang::Tsx => (ServerKind::TypeScript, "typescriptreact"),
        Lang::JavaScript => (ServerKind::TypeScript, "javascript"),
        _ => return None,
    })
}

/// Servers may report a file by its real path while the editor holds a symlinked one (/tmp).
pub(super) fn document_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn server_edit(e: athena_lsp::TextEdit) -> ServerEdit {
    ServerEdit {
        start: (e.range.start.line, e.range.start.character),
        end: (e.range.end.line, e.range.end.character),
        text: e.text,
    }
}

fn position((line, character): (u32, u32)) -> Position {
    Position { line, character }
}

fn hover_blocks(blocks: Vec<MarkupBlock>) -> Vec<HoverBlock> {
    blocks
        .into_iter()
        .map(|b| match b {
            MarkupBlock::Text(t) => HoverBlock::Text(t),
            MarkupBlock::Code(c) => HoverBlock::Code(c),
        })
        .collect()
}

fn completion(item: CompletionItem, resolve: bool) -> Completion {
    let pos = |p: Position| (p.line, p.character);
    Completion {
        range: item.range.map(|r| (pos(r.start), pos(r.end))),
        additional_edits: item.additional_edits.into_iter().map(server_edit).collect(),
        documentation: hover_blocks(item.documentation),
        resolve,
        label: item.label,
        kind: item.kind,
        detail: item.detail,
        filter_text: item.filter_text,
        sort_text: item.sort_text,
        text: item.text,
        select: item.select,
        stops: item.stops,
        preselect: item.preselect,
    }
}

/// Tells an editor which typed characters ask its server for suggestions and signature help.
fn push_triggers(editor: &Entity<EditorView>, client: &Client, cx: &mut Context<Shell>) {
    let completion = client.completion_triggers().to_vec();
    let signature = client.signature_triggers().to_vec();
    editor.update(cx, |e, cx| {
        e.set_completion_triggers(Some(completion));
        e.set_signature_triggers(signature);
        // A file shown before its server was ready asks for its hints now.
        e.refresh_inlay_hints(cx);
    });
}

fn marker(d: &Diagnostic) -> Marker {
    Marker {
        start: (d.range.start.line, d.range.start.character),
        end: (d.range.end.line, d.range.end.character),
        severity: match d.severity {
            Severity::Error => MarkerSeverity::Error,
            Severity::Warning => MarkerSeverity::Warning,
            Severity::Information | Severity::Hint => MarkerSeverity::Info,
        },
        message: match &d.source {
            Some(source) => format!("{} ({source})", d.message),
            None => d.message.clone(),
        },
    }
}

impl Shell {
    /// Opens a new editor's file in its language server, starting the server on first use.
    pub(super) fn lsp_opened(
        &mut self,
        root: &Path,
        editor: &Entity<EditorView>,
        cx: &mut Context<Self>,
    ) {
        let (path, lang, version, text) = {
            let e = editor.read(cx);
            (e.path().to_path_buf(), e.lang(), e.version(), e.text())
        };
        let doc = document_key(&path);
        self.push_markers(editor, &doc, cx);
        self.watch_for_lightbulb(editor, cx);
        let (Some(lang), Some(version), Some(text)) = (lang, version, text) else {
            return;
        };
        // Another tab may have opened this file in its servers already.
        if let Some((kind, language_id)) = server_for(lang)
            && !self.lsp.documents.contains_key(&doc)
        {
            let key = (root.to_path_buf(), kind);
            if let Some(client) = self.lsp_client(&key, cx) {
                client.did_open(&doc, language_id, version as i64, text.clone());
                self.lsp.documents.insert(doc.clone(), key);
            }
        }
        for &kind in linters_for(lang) {
            let key = (root.to_path_buf(), kind);
            if self.lsp.linters.get(&doc).is_some_and(|k| k.contains(&key)) {
                continue;
            }
            let Some(client) = self.lsp_client(&key, cx) else {
                continue;
            };
            let language_id = crate::settings::language_id(lang);
            client.did_open(&doc, language_id, version as i64, text.clone());
            self.pull_diagnostics(&key, &doc, cx);
            self.lsp.linters.entry(doc.clone()).or_default().push(key);
        }
        if let Some(client) = self.document_client(&doc) {
            push_triggers(editor, &client, cx);
        }
    }

    /// Every server with `doc` open, its main one first, and what each published for it.
    pub(super) fn document_servers(&self, doc: &Path) -> Vec<(Rc<Client>, Vec<&Diagnostic>)> {
        let published = |key: &ServerKey| -> Vec<&Diagnostic> {
            self.lsp
                .diagnostics
                .get(doc)
                .into_iter()
                .flatten()
                .filter(|(k, _)| k == key)
                .flat_map(|(_, list)| list)
                .collect()
        };
        self.document_keys(doc)
            .filter_map(|key| Some((self.lsp.servers.get(key)?.client.clone(), published(key))))
            .collect()
    }

    fn document_keys<'a>(&'a self, doc: &Path) -> impl Iterator<Item = &'a ServerKey> {
        self.lsp
            .documents
            .get(doc)
            .into_iter()
            .chain(self.lsp.linters.get(doc).into_iter().flatten())
    }

    /// Every diagnostic published for `doc`, whichever server sent it.
    pub(super) fn file_diagnostics(&self, doc: &Path) -> Vec<&Diagnostic> {
        self.lsp
            .diagnostics
            .get(doc)
            .into_iter()
            .flatten()
            .flat_map(|(_, list)| list)
            .collect()
    }

    /// The settings a server starts with and is sent again when settings change: the user's
    /// `lsp.<program>` section with the project's laid over it, on top of what ESLint needs to
    /// run at all; TypeScript is pinned to a global install until the project is trusted.
    fn server_settings(&self, key: &ServerKey) -> serde_json::Value {
        let user = self
            .settings_for(Some(&key.0))
            .server_config(key.1.program());
        match key.1 {
            ServerKind::Eslint => {
                let mut settings = athena_lsp::eslint_settings(&document_key(&key.0));
                crate::settings::merge_json(&mut settings, &user);
                settings
            }
            ServerKind::TypeScript if self.linter_trust(&key.0) != Some(LinterTrust::Allowed) => {
                let mut settings = user;
                // typescript-language-server would load the project's node_modules/typescript,
                // or one in a folder above it, and the tsconfig plugins beside it.
                if let Some(global) = athena_lsp::global_typescript(&key.0) {
                    crate::settings::merge_json(
                        &mut settings,
                        &serde_json::json!({"tsserver": {"path": global}}),
                    );
                }
                settings
            }
            _ => user,
        }
    }

    pub(super) fn document_client(&self, doc: &Path) -> Option<Rc<Client>> {
        let key = self.lsp.documents.get(doc)?;
        Some(self.lsp.servers.get(key)?.client.clone())
    }

    /// Every running server.
    pub(super) fn all_clients(&self) -> Vec<Rc<Client>> {
        self.lsp
            .servers
            .values()
            .filter(|s| s.ready)
            .map(|s| s.client.clone())
            .collect()
    }

    /// Whether a language server has `doc` open, so knows its version.
    pub(super) fn lsp_knows(&self, doc: &Path) -> bool {
        self.lsp.documents.contains_key(doc)
    }

    /// `path` as the project that holds it spells it; servers report real paths, and a project
    /// opened through a symlink (/tmp) names its tabs through the link.
    pub(super) fn project_spelling(&self, path: &Path) -> PathBuf {
        for project in &self.workspace.projects {
            if let Ok(rest) = path.strip_prefix(document_key(&project.root)) {
                return project.root.join(rest);
            }
        }
        path.to_path_buf()
    }

    /// The running main servers of the project at `root`, linters left out.
    pub(super) fn project_clients(&self, root: &Path) -> Vec<Rc<Client>> {
        self.lsp
            .servers
            .iter()
            .filter(|((r, kind), s)| r == root && s.ready && !kind.is_project_local())
            .map(|(_, s)| s.client.clone())
            .collect()
    }

    fn lsp_client(&mut self, key: &ServerKey, cx: &mut Context<Self>) -> Option<Rc<Client>> {
        if self.lsp.failed.contains_key(key) {
            return None;
        }
        if let Some(server) = self.lsp.servers.get(key) {
            return Some(server.client.clone());
        }
        // A linter the project does not install is simply not run, and never reported.
        let local = match key.1.is_project_local() {
            true => {
                let program = athena_lsp::project_server(&key.0, key.1)?;
                match self.linter_trust(&key.0)? {
                    LinterTrust::Allowed => Some(program),
                    LinterTrust::NotAsked => {
                        self.ask_project_trust(&key.0, cx);
                        return None;
                    }
                    LinterTrust::Denied => return None,
                }
            }
            false => None,
        };
        if key.1 == ServerKind::TypeScript && !self.typescript_may_start(&key.0, cx) {
            return None;
        }
        tracing::info!(root = %key.0.display(), ?local, "starting {}", key.1.program());
        let config: Config = Arc::new(RwLock::new(self.server_settings(key)));
        let root = document_key(&key.0);
        let (client, events) = match local {
            Some(program) => Client::start_local(key.1, program, root, config.clone()),
            None => Client::start_with(key.1, root, config.clone()),
        };
        let client = Rc::new(client);
        let event_key = key.clone();
        let task = cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                let event_key = event_key.clone();
                if this
                    .update(cx, |this, cx| this.lsp_event(event_key, event, cx))
                    .is_err()
                {
                    return;
                }
            }
        });
        self.lsp.servers.insert(
            key.clone(),
            Server {
                client: client.clone(),
                config,
                ready: false,
                _events: task,
            },
        );
        Some(client)
    }

    pub(super) fn linter_trust(&self, root: &Path) -> Option<LinterTrust> {
        let project = self.workspace.projects.iter().find(|p| p.root == root)?;
        Some(project.linters)
    }

    /// Whether typescript-language-server may start for the project at `root`: always once the
    /// project is trusted, else only when a global TypeScript can stand in for any it could reach.
    fn typescript_may_start(&mut self, root: &Path, cx: &mut Context<Self>) -> bool {
        let trust = self.linter_trust(root);
        if trust == Some(LinterTrust::Allowed) || athena_lsp::reachable_typescript(root).is_none() {
            return true;
        }
        if trust == Some(LinterTrust::NotAsked) {
            self.ask_project_trust(root, cx);
        }
        if athena_lsp::global_typescript(root).is_some() {
            return true;
        }
        if self.lsp.held_back.insert(root.to_path_buf()) {
            let name = project_name(root);
            self.transient_notice(
                "TypeScript waits for this project to be allowed",
                format!(
                    "{name} has its own TypeScript, which runs only once {name} is allowed. \
                     Install one for Athena to use meanwhile: npm install -g typescript"
                ),
                cx,
            );
        }
        false
    }

    /// Asks once per project whether the code it brings may run: the linters and TypeScript it
    /// installs, and settings that choose what language servers run, as VS Code asks before
    /// trusting a workspace.
    pub(super) fn ask_project_trust(&mut self, root: &Path, cx: &mut Context<Self>) {
        if !self.lsp.asking_trust.insert(root.to_path_buf()) {
            return;
        }
        let mut found: Vec<&str> = PROJECT_LINTERS
            .into_iter()
            .filter(|&kind| athena_lsp::project_server(root, kind).is_some())
            .map(linter_name)
            .collect();
        if athena_lsp::reachable_typescript(root).is_some() {
            found.push("TypeScript");
        }
        let settings = self.project_changes_programs(root);
        let project = project_name(root);
        let (message, detail) = trust_question(&project, &found, settings);
        tracing::info!(root = %root.display(), "asking whether {project}'s code may run");
        let shell = cx.entity();
        let root = root.to_path_buf();
        // Deferred: files restored at launch open before the window exists.
        cx.defer(move |cx| {
            let window = cx
                .active_window()
                .or_else(|| cx.windows().into_iter().next());
            let asked = window.map(|window| {
                window.update(cx, |_, window, cx| {
                    let answer = window.prompt(
                        PromptLevel::Warning,
                        &message,
                        Some(&detail),
                        &confirm_buttons("Don't Allow", "Allow"),
                        cx,
                    );
                    let root = root.clone();
                    shell.update(cx, |_, cx| {
                        cx.spawn_in(window, async move |this, cx| {
                            let answer = answer.await;
                            let _ = this.update(cx, |this, cx| {
                                this.lsp.asking_trust.remove(&root);
                                match answer {
                                    Ok(CONFIRMED) => {
                                        this.set_linter_trust(&root, LinterTrust::Allowed, cx)
                                    }
                                    Ok(_) => this.set_linter_trust(&root, LinterTrust::Denied, cx),
                                    // The window closed first; ask again next time.
                                    Err(_) => {}
                                }
                            });
                        })
                        .detach();
                    });
                })
            });
            if !matches!(asked, Some(Ok(()))) {
                shell.update(cx, |this, _| this.lsp.asking_trust.remove(&root));
            }
        });
    }

    /// Records the answer for the project at `root`, then starts or stops its linters and
    /// TypeScript, and applies or withholds its server settings, to match.
    fn set_linter_trust(&mut self, root: &Path, trust: LinterTrust, cx: &mut Context<Self>) {
        let Some(project) = self.workspace.projects.iter_mut().find(|p| p.root == root) else {
            return;
        };
        project.linters = trust;
        self.schedule_save(cx);
        self.lsp.held_back.remove(root);
        for key in restarted_on_trust_change(self.lsp.servers.keys(), root) {
            match self.lsp.servers.contains_key(&key) {
                true => self.replace_server(&key, cx),
                false => self.restart_lsp(&key, cx),
            }
        }
        self.project_settings_changed(root, cx);
        if trust != LinterTrust::Allowed {
            return self.stop_project_linters(root, cx);
        }
        for kind in PROJECT_LINTERS {
            self.restart_lsp(&(root.to_path_buf(), kind), cx);
        }
    }

    fn stop_project_linters(&mut self, root: &Path, cx: &mut Context<Self>) {
        let ours = |(r, kind): &ServerKey| r == root && kind.is_project_local();
        let keys: Vec<ServerKey> = self
            .lsp
            .servers
            .keys()
            .filter(|k| ours(k))
            .cloned()
            .collect();
        for key in &keys {
            self.lsp.servers.remove(key);
            self.clear_diagnostics(key, cx);
        }
        self.lsp.linters.retain(|_, keys| {
            keys.retain(|k| !ours(k));
            !keys.is_empty()
        });
        self.lsp.failed.retain(|k, _| !ours(k));
        self.lsp.crashes.retain(|k, _| !ours(k));
        self.lsp.restarts.retain(|k, _| !ours(k));
        cx.notify();
    }

    /// The palette's Allow/Disallow Project Linters, for the active project.
    pub(super) fn change_linter_trust(&mut self, allow: bool, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        let name = self
            .workspace
            .active_project()
            .map(|p| p.name())
            .unwrap_or_default();
        let (trust, title, body) = match allow {
            true => (
                LinterTrust::Allowed,
                "Project code allowed",
                format!(
                    "ESLint, Biome and TypeScript from {name}'s node_modules run for its files, \
                     and its settings choose what language servers run."
                ),
            ),
            false => (
                LinterTrust::Denied,
                "Project code disallowed",
                format!(
                    "Athena no longer runs ESLint, Biome or TypeScript from {name}'s \
                     node_modules, nor its settings for language servers."
                ),
            ),
        };
        self.set_linter_trust(&root, trust, cx);
        self.transient_notice(title, body, cx);
    }

    fn lsp_event(&mut self, key: ServerKey, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Ready => {
                let Some(server) = self.lsp.servers.get_mut(&key) else {
                    return;
                };
                server.ready = true;
                // Editors opened while the server started learn its trigger characters now.
                let client = server.client.clone();
                let docs: Vec<PathBuf> = self
                    .lsp
                    .documents
                    .iter()
                    .filter(|(_, k)| **k == key)
                    .map(|(doc, _)| doc.clone())
                    .collect();
                for doc in docs {
                    for editor in self.editors_showing(&doc, cx) {
                        push_triggers(&editor, &client, cx);
                    }
                }
                self.pull_all_diagnostics(&key, cx);
            }
            Event::RefreshDiagnostics => self.pull_all_diagnostics(&key, cx),
            Event::Diagnostics { path, mut list } => {
                if key.1.is_project_local() {
                    for d in &mut list {
                        d.source.get_or_insert_with(|| key.1.label().into());
                    }
                }
                let doc = document_key(&path);
                let published = self.lsp.diagnostics.entry(doc.clone()).or_default();
                published.retain(|(k, _)| *k != key);
                published.push((key, list));
                for editor in self.editors_showing(&doc, cx) {
                    self.push_markers(&editor, &doc, cx);
                }
                self.diagnostics_moved_lightbulb(&doc, cx);
                // The title bar counter and the Problems tab follow every report.
                cx.notify();
            }
            Event::Stopped(why) => {
                let Some(server) = self.lsp.servers.remove(&key) else {
                    return;
                };
                self.lsp.documents.retain(|doc, k| {
                    let keep = *k != key;
                    if !keep {
                        self.lsp.changes.remove(doc);
                    }
                    keep
                });
                self.lsp.linters.retain(|_, keys| {
                    keys.retain(|k| *k != key);
                    !keys.is_empty()
                });
                self.clear_diagnostics(&key, cx);
                let program = key.1.program();
                tracing::warn!("{program} stopped: {why}");
                let crashes = {
                    let times = self.lsp.crashes.entry(key.clone()).or_default();
                    times.retain(|t| t.elapsed() < CRASH_WINDOW);
                    times.push(Instant::now());
                    times.len()
                };
                let title = match restart_delay(crashes).filter(|_| server.ready) {
                    Some(delay) => {
                        self.schedule_lsp_restart(key, delay, cx);
                        format!("{program} stopped; restarting it")
                    }
                    None if server.ready => {
                        self.lsp.failed.insert(key, why.clone());
                        format!("{program} stopped {crashes} times in 3 minutes; not restarting it")
                    }
                    None => {
                        self.lsp.failed.insert(key, why.clone());
                        format!("{program} could not start")
                    }
                };
                self.local_notice(NoticeKind::Message { title, body: why }, cx);
            }
            Event::RefreshInlayHints => self.refresh_inlay_hints(&key, cx),
            Event::ApplyEdit { label, edit, reply } => {
                tracing::debug!(
                    ?label,
                    changes = edit.changes.len(),
                    "server applies an edit"
                );
                let result = self.apply_workspace_edit(&edit, cx);
                if let Err(why) = &result {
                    let title = label.unwrap_or_else(|| "Edit not applied".into());
                    self.lsp_failed(&title, why.clone(), cx);
                }
                reply.send(result.map(drop));
            }
        }
    }

    /// Sends each running server its settings again where settings.json changed them, then asks
    /// its editors for inlay hints again.
    pub(super) fn lsp_settings_changed(&mut self, cx: &mut Context<Self>) {
        let mut changed = Vec::new();
        let mut restart = Vec::new();
        for (key, server) in &self.lsp.servers {
            let config = self.server_settings(key);
            let Ok(mut current) = server.config.write() else {
                continue;
            };
            if *current == config {
                continue;
            }
            if needs_restart(key.1, &current, &config) {
                restart.push(key.clone());
                continue;
            }
            *current = config.clone();
            drop(current);
            tracing::info!("{} settings changed", key.1.program());
            server.client.did_change_configuration(config);
            changed.push(key.clone());
        }
        for key in &restart {
            tracing::info!("{} restarts for its new settings", key.1.program());
            self.replace_server(key, cx);
        }
        if changed.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SETTINGS_SETTLE).await;
            let _ = this.update(cx, |this, cx| {
                for key in &changed {
                    this.refresh_inlay_hints(key, cx);
                }
            });
        })
        .detach();
    }

    /// Stops a running server and starts it again for the files it had open.
    fn replace_server(&mut self, key: &ServerKey, cx: &mut Context<Self>) {
        if self.lsp.servers.remove(key).is_none() {
            return;
        }
        self.lsp.documents.retain(|_, k| k != key);
        self.lsp.linters.retain(|_, keys| {
            keys.retain(|k| k != key);
            !keys.is_empty()
        });
        self.clear_diagnostics(key, cx);
        self.restart_lsp(key, cx);
    }

    fn refresh_inlay_hints(&mut self, key: &ServerKey, cx: &mut Context<Self>) {
        let docs: Vec<PathBuf> = self
            .lsp
            .documents
            .iter()
            .filter(|(_, k)| *k == key)
            .map(|(doc, _)| doc.clone())
            .collect();
        for doc in docs {
            for editor in self.editors_showing(&doc, cx) {
                editor.update(cx, |e, cx| e.refresh_inlay_hints(cx));
            }
        }
    }

    /// Asks the server for the inlay hints on `lines` of the editor's file.
    pub(super) fn lsp_inlay_hints(
        &mut self,
        editor: &Entity<EditorView>,
        request: u64,
        lines: std::ops::Range<u32>,
        cx: &mut Context<Self>,
    ) {
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, editor, cx);
        let Some(client) = self
            .document_client(&doc)
            .filter(|c| c.supports("/inlayHintProvider"))
        else {
            editor.update(cx, |e, cx| e.show_inlay_hints(request, Vec::new(), cx));
            return;
        };
        let range = Range {
            start: Position {
                line: lines.start,
                character: 0,
            },
            end: Position {
                line: lines.end,
                character: 0,
            },
        };
        let weak = editor.downgrade();
        cx.spawn(async move |_, cx| {
            let found = client.inlay_hints(&doc, range).await.unwrap_or_else(|why| {
                tracing::debug!("inlay hints failed: {why}");
                Vec::new()
            });
            tracing::debug!("inlay hints → {}", found.len());
            let hints = found
                .into_iter()
                .map(|h| Inlay {
                    position: (h.position.line, h.position.character),
                    text: format!(
                        "{}{}{}",
                        if h.padding_left { " " } else { "" },
                        h.label,
                        if h.padding_right { " " } else { "" }
                    ),
                    is_type: h.is_type,
                })
                .collect();
            let _ = weak.update(cx, |e, cx| e.show_inlay_hints(request, hints, cx));
        })
        .detach();
    }

    /// Drops what a stopped server reported; its replacement publishes afresh.
    fn clear_diagnostics(&mut self, key: &ServerKey, cx: &mut Context<Self>) {
        let files: Vec<PathBuf> = self
            .lsp
            .diagnostics
            .iter()
            .filter(|(_, published)| published.iter().any(|(k, _)| k == key))
            .map(|(file, _)| file.clone())
            .collect();
        for file in files {
            if let Some(published) = self.lsp.diagnostics.get_mut(&file) {
                published.retain(|(k, _)| k != key);
            }
            for editor in self.editors_showing(&file, cx) {
                self.push_markers(&editor, &file, cx);
            }
        }
    }

    fn schedule_lsp_restart(&mut self, key: ServerKey, delay: Duration, cx: &mut Context<Self>) {
        let task_key = key.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| this.restart_lsp(&task_key, cx));
        });
        self.lsp.restarts.insert(key, task);
    }

    /// Starts a stopped server again by reopening the files of its language that its project shows.
    fn restart_lsp(&mut self, key: &ServerKey, cx: &mut Context<Self>) {
        self.lsp.restarts.remove(key);
        let editors: Vec<Entity<EditorView>> = self
            .items
            .iter()
            .filter(|((root, _), _)| *root == key.0)
            .filter_map(|(_, view)| match view {
                ItemView::Editor(e) => Some(e.clone()),
                _ => None,
            })
            .filter(|e| {
                e.read(cx).lang().is_some_and(|lang| {
                    server_for(lang).is_some_and(|(kind, _)| kind == key.1)
                        || linters_for(lang).contains(&key.1)
                })
            })
            .collect();
        tracing::info!(root = %key.0.display(), files = editors.len(), "restarting {}", key.1.program());
        for editor in editors {
            self.lsp_opened(&key.0, &editor, cx);
        }
    }

    pub(super) fn editors_showing(
        &self,
        doc: &Path,
        cx: &Context<Self>,
    ) -> Vec<Entity<EditorView>> {
        self.items
            .values()
            .filter_map(|view| match view {
                ItemView::Editor(e) if document_key(e.read(cx).path()) == doc => Some(e.clone()),
                _ => None,
            })
            .collect()
    }

    fn push_markers(&self, editor: &Entity<EditorView>, doc: &Path, cx: &mut Context<Self>) {
        let markers = self.file_diagnostics(doc).into_iter().map(marker).collect();
        editor.update(cx, |e, cx| e.set_markers(markers, cx));
    }

    pub(super) fn lsp_edited(&mut self, editor: &Entity<EditorView>, cx: &mut Context<Self>) {
        let doc = document_key(editor.read(cx).path());
        if self.document_keys(&doc).next().is_none() {
            return;
        }
        let weak = editor.downgrade();
        let task_doc = doc.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CHANGE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.lsp.changes.remove(&task_doc);
                if let Some(editor) = weak.upgrade() {
                    this.send_change(&task_doc, &editor, cx);
                }
            });
        });
        self.lsp.changes.insert(doc, task);
    }

    fn send_change(&self, doc: &Path, editor: &Entity<EditorView>, cx: &Context<Self>) {
        let e = editor.read(cx);
        let (Some(text), Some(version)) = (e.text(), e.version()) else {
            return;
        };
        for key in self.document_keys(doc) {
            if let Some(server) = self.lsp.servers.get(key) {
                server.client.did_change(doc, version as i64, text.clone());
                self.pull_diagnostics(key, doc, cx);
            }
        }
    }

    /// Asks a pull-model server (ESLint) for `doc`'s diagnostics, delivered as if published.
    fn pull_diagnostics(&self, key: &ServerKey, doc: &Path, cx: &Context<Self>) {
        let Some(server) = self.lsp.servers.get(key) else {
            return;
        };
        if key.1 != ServerKind::Eslint || !server.client.supports("/diagnosticProvider") {
            return;
        }
        let (client, key, path) = (server.client.clone(), key.clone(), doc.to_path_buf());
        cx.spawn(async move |this, cx| {
            let list = match client.pull_diagnostics(&path).await {
                Ok(list) => list,
                Err(why) => return tracing::debug!("{} diagnostics failed: {why}", key.1.label()),
            };
            let _ = this.update(cx, |this, cx| {
                if this.lsp.answer_is_current(&key, &client, &path) {
                    this.lsp_event(key, Event::Diagnostics { path, list }, cx);
                }
            });
        })
        .detach();
    }

    fn pull_all_diagnostics(&self, key: &ServerKey, cx: &Context<Self>) {
        let docs: Vec<&PathBuf> = self
            .lsp
            .linters
            .iter()
            .filter(|(_, keys)| keys.contains(key))
            .map(|(doc, _)| doc)
            .collect();
        for doc in docs {
            self.pull_diagnostics(key, doc, cx);
        }
    }

    /// Sends a pending edit now, so the server answers about what is on screen.
    pub(super) fn flush_change(
        &mut self,
        doc: &Path,
        editor: &Entity<EditorView>,
        cx: &Context<Self>,
    ) {
        if self.lsp.changes.remove(doc).is_some() {
            self.send_change(doc, editor, cx);
        }
    }

    pub(super) fn lsp_saved(&mut self, editor: &Entity<EditorView>, cx: &mut Context<Self>) {
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, editor, cx);
        for key in self.document_keys(&doc) {
            if let Some(server) = self.lsp.servers.get(key) {
                server.client.did_save(&doc);
            }
        }
        let path = editor.read(cx).path().to_path_buf();
        self.project_settings_saved(&path, cx);
    }

    pub(super) fn lsp_definition(
        &mut self,
        editor: &Entity<EditorView>,
        at: Position,
        cx: &mut Context<Self>,
    ) {
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, editor, cx);
        let Some(client) = self
            .lsp
            .documents
            .get(&doc)
            .and_then(|k| self.lsp.servers.get(k))
            .map(|s| s.client.clone())
        else {
            return self.lsp_failed("No definition found", NO_SERVER.into(), cx);
        };
        tracing::debug!(path = %doc.display(), line = at.line, character = at.character, "definition");
        cx.spawn(async move |this, cx| {
            let found = client.definition(&doc, at).await;
            match &found {
                Ok(list) => tracing::debug!("definition → {} locations", list.len()),
                Err(why) => tracing::warn!("definition failed: {why}"),
            }
            let _ = this.update(cx, |this, cx| {
                match found.map(|list| list.into_iter().next()) {
                    Ok(Some(target)) => {
                        this.lsp.jump = Some((target.path, target.range.start));
                        cx.notify();
                    }
                    Ok(None) => this.lsp_failed(
                        "No definition found",
                        "Nothing is defined under the cursor.".into(),
                        cx,
                    ),
                    Err(why) => this.lsp_failed("Go to definition failed", why, cx),
                }
            });
        })
        .detach();
    }

    /// Cmd+F12 (`implementation`) or Go to Type Definition: one result opens, several are listed
    /// in the References tab.
    pub(super) fn lsp_implementation(&mut self, type_definition: bool, cx: &mut Context<Self>) {
        let Some(editor) = self.focused_editor() else {
            return;
        };
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, &editor, cx);
        let (noun, title) = match type_definition {
            true => ("type definition", "No type definition found"),
            false => ("implementation", "No implementation found"),
        };
        let Some(client) = self.document_client(&doc) else {
            return self.lsp_failed(title, NO_SERVER.into(), cx);
        };
        let Some((line, character)) = editor.read(cx).cursor_utf16() else {
            return;
        };
        let at = Position { line, character };
        tracing::debug!(path = %doc.display(), line, character, "{noun}");
        let root = self.workspace.active_project().map(|p| p.root.clone());
        cx.spawn(async move |this, cx| {
            let found = match type_definition {
                true => client.type_definition(&doc, at).await,
                false => client.implementation(&doc, at).await,
            };
            let found = match found {
                Ok(list) if list.len() > 1 => Ok(cx
                    .background_executor()
                    .spawn(async move { with_snippets(list) })
                    .await),
                Ok(list) => Err(list.into_iter().next()),
                Err(why) => {
                    tracing::debug!("{noun} failed: {why}");
                    Err(None)
                }
            };
            let _ = this.update(cx, |this, cx| match found {
                Ok(list) => {
                    this.lsp.references_asked += 1;
                    this.lsp.showing_calls = false;
                    this.lsp.reference_opened = None;
                    this.lsp.references_noun = noun;
                    this.lsp.references_root = root;
                    this.lsp.references = References::Found(Rc::new(list));
                    this.show_drawer_tab(DrawerTab::References, cx);
                }
                Err(Some(target)) => {
                    this.lsp.jump = Some((target.path, target.range.start));
                    cx.notify();
                }
                Err(None) => this.lsp_failed(
                    title,
                    format!("No {noun} was found for the symbol under the cursor."),
                    cx,
                ),
            });
        })
        .detach();
    }

    /// Asks the server about the symbol at `at`; an empty answer shows nothing, as in VS Code.
    pub(super) fn lsp_hover(
        &mut self,
        editor: &Entity<EditorView>,
        request: u64,
        at: (u32, u32),
        cx: &mut Context<Self>,
    ) {
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, editor, cx);
        let Some(client) = self.document_client(&doc) else {
            editor.update(cx, |e, cx| e.show_hover(request, Vec::new(), cx));
            return;
        };
        let at = Position {
            line: at.0,
            character: at.1,
        };
        tracing::debug!(path = %doc.display(), line = at.line, character = at.character, "hover");
        let weak = editor.downgrade();
        cx.spawn(async move |_, cx| {
            let found = client.hover(&doc, at).await;
            let blocks = match found {
                Ok(Some(hover)) => hover_blocks(hover.blocks),
                Ok(None) => Vec::new(),
                Err(why) => {
                    tracing::debug!("hover failed: {why}");
                    Vec::new()
                }
            };
            tracing::debug!("hover → {} blocks", blocks.len());
            let _ = weak.update(cx, |e, cx| e.show_hover(request, blocks, cx));
        })
        .detach();
    }

    /// Asks the server where else the symbol at `at` is used, to mark while the cursor rests.
    pub(super) fn lsp_highlight(
        &mut self,
        editor: &Entity<EditorView>,
        request: u64,
        at: (u32, u32),
        cx: &mut Context<Self>,
    ) {
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, editor, cx);
        let Some(client) = self
            .document_client(&doc)
            .filter(|c| c.supports("/documentHighlightProvider"))
        else {
            editor.update(cx, |e, cx| {
                e.show_document_highlights(request, Vec::new(), cx)
            });
            return;
        };
        let at = Position {
            line: at.0,
            character: at.1,
        };
        let weak = editor.downgrade();
        cx.spawn(async move |_, cx| {
            let found = client
                .document_highlights(&doc, at)
                .await
                .unwrap_or_else(|why| {
                    tracing::debug!("document highlight failed: {why}");
                    Vec::new()
                });
            let pos = |p: Position| (p.line, p.character);
            let ranges = found
                .into_iter()
                .map(|h| Occurrence {
                    start: pos(h.range.start),
                    end: pos(h.range.end),
                    write: h.write,
                })
                .collect();
            let _ = weak.update(cx, |e, cx| e.show_document_highlights(request, ranges, cx));
        })
        .detach();
    }

    /// Asks the server for the signature of the call around `at`.
    pub(super) fn lsp_signature(
        &mut self,
        editor: &Entity<EditorView>,
        request: u64,
        at: (u32, u32),
        cx: &mut Context<Self>,
    ) {
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, editor, cx);
        let Some(client) = self.document_client(&doc) else {
            editor.update(cx, |e, cx| e.show_signature(request, None, cx));
            return;
        };
        let at = Position {
            line: at.0,
            character: at.1,
        };
        let weak = editor.downgrade();
        cx.spawn(async move |_, cx| {
            let help = match client.signature_help(&doc, at).await {
                Ok(help) => help,
                Err(why) => {
                    tracing::debug!("signature help failed: {why}");
                    None
                }
            };
            tracing::debug!(found = help.is_some(), "signature help");
            let signature = help.map(|h| Signature {
                label: h.label,
                active: h.active,
                documentation: h.documentation,
            });
            let _ = weak.update(cx, |e, cx| e.show_signature(request, signature, cx));
        })
        .detach();
    }

    /// Asks the server to format the file before Cmd+S saves it, to organize its imports when
    /// `editor.codeActionsOnSave` asks, and ESLint to fix it when `eslint.fixOnSave` is on; the
    /// editor always gets an answer, empty when the servers are missing, fail or are too slow.
    pub(super) fn lsp_format(
        &mut self,
        editor: &Entity<EditorView>,
        request: u64,
        (tab_size, insert_spaces): (u32, bool),
        cx: &mut Context<Self>,
    ) {
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, editor, cx);
        let lang = editor.read(cx).lang();
        let root = self.project_root_of(editor.read(cx).path());
        let (wants_format, organize, fix_all) = {
            let file = self.settings_for(root.as_deref());
            let e = file.editor_for(lang);
            // Fix on save and organize imports route every save here, so formatting still
            // follows its own setting.
            (
                e.format_on_save
                    .or(self.workspace.format_on_save)
                    .unwrap_or(lang == Some(Lang::Go)),
                e.organize_imports_on_save == Some(true),
                e.fix_all_on_save.unwrap_or(file.eslint_fix_on_save()),
            )
        };
        let eslint = fix_all
            .then(|| {
                let key = self
                    .lsp
                    .linters
                    .get(&doc)?
                    .iter()
                    .find(|(_, k)| *k == ServerKind::Eslint)?;
                Some(self.lsp.servers.get(key)?.client.clone())
            })
            .flatten();
        let main = self.document_client(&doc);
        let client = main.clone().filter(|_| wants_format);
        let organizer = main.filter(|_| organize);
        if client.is_none() && organizer.is_none() && eslint.is_none() {
            editor.update(cx, |e, cx| e.format_and_save(request, Vec::new(), cx));
            return;
        }
        let weak = editor.downgrade();
        let late = weak.clone();
        cx.spawn(async move |_, cx| {
            cx.background_executor().timer(FORMAT_TIMEOUT).await;
            let _ = late.update(cx, |e, cx| e.format_and_save(request, Vec::new(), cx));
        })
        .detach();
        cx.spawn(async move |_, cx| {
            let source = |client: Option<Rc<Client>>, kind: &'static str| {
                let doc = doc.clone();
                async move {
                    match client {
                        Some(client) => {
                            super::code_actions::source_action_edits(&client, &doc, kind).await
                        }
                        None => Vec::new(),
                    }
                }
            };
            let imports = source(organizer, "source.organizeImports");
            let fixes = source(eslint, "source.fixAll.eslint");
            let formatting = async {
                match &client {
                    Some(client) => client.formatting(&doc, tab_size, insert_spaces).await,
                    None => Ok(Vec::new()),
                }
            };
            let (imports, fixes, formatted) = futures::join!(imports, fixes, formatting);
            let edits = match formatted {
                Ok(edits) => edits,
                Err(why) => {
                    tracing::warn!("formatting failed: {why}");
                    Vec::new()
                }
            };
            tracing::debug!(
                "formatting → {} edits, organize imports → {}, eslint fixes → {}",
                edits.len(),
                imports.len(),
                fixes.len()
            );
            // ESLint's fixes win where they overlap, as VS Code applies them before formatting.
            let edits = super::code_actions::merge_save_edits(imports, edits);
            let edits = super::code_actions::merge_save_edits(fixes, edits)
                .into_iter()
                .map(|e| ServerEdit {
                    start: (e.range.start.line, e.range.start.character),
                    end: (e.range.end.line, e.range.end.character),
                    text: e.text,
                })
                .collect();
            let _ = weak.update(cx, |e, cx| e.format_and_save(request, edits, cx));
        })
        .detach();
    }

    /// Asks the server for suggestions at `at`, sending any pending edit first so they fit.
    pub(super) fn lsp_complete(
        &mut self,
        editor: &Entity<EditorView>,
        request: u64,
        at: (u32, u32),
        trigger: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, editor, cx);
        let Some(client) = self.document_client(&doc) else {
            editor.update(cx, |e, cx| {
                e.show_completions(request, Vec::new(), false, cx)
            });
            return;
        };
        let at = Position {
            line: at.0,
            character: at.1,
        };
        tracing::debug!(path = %doc.display(), line = at.line, character = at.character, ?trigger, "completion");
        let weak = editor.downgrade();
        let id = editor.entity_id();
        let resolves = client.supports("/completionProvider/resolveProvider");
        cx.spawn(async move |this, cx| {
            let (list, incomplete) = match client.completion(&doc, at, trigger.as_deref()).await {
                Ok(list) => (list.items, list.incomplete),
                Err(why) => {
                    tracing::debug!("completion failed: {why}");
                    (Vec::new(), false)
                }
            };
            tracing::debug!("completion → {} items, incomplete {incomplete}", list.len());
            let items = list
                .iter()
                .map(|item| completion(item.clone(), resolves))
                .collect();
            if resolves {
                let _ = this.update(cx, |this, _| {
                    this.lsp.completions.insert(id, (request, client, list));
                });
            }
            let _ = weak.update(cx, |e, cx| {
                e.show_completions(request, items, incomplete, cx)
            });
        })
        .detach();
    }

    /// Answers the editor's requests for the newer language features.
    pub(super) fn lsp_editor_request(
        &mut self,
        editor: &Entity<EditorView>,
        request: LspRequest,
        cx: &mut Context<Self>,
    ) {
        match request {
            LspRequest::ResolveCompletion { request, index } => {
                self.lsp_resolve_completion(editor, request, index, cx)
            }
            LspRequest::SelectionRanges { request, positions } => {
                self.lsp_selection_ranges(editor, request, positions, cx)
            }
            LspRequest::FormatSelection {
                request,
                start,
                end,
                tab_size,
                insert_spaces,
            } => {
                let range = Range {
                    start: position(start),
                    end: position(end),
                };
                self.lsp_format_selection(editor, request, range, (tab_size, insert_spaces), cx)
            }
            LspRequest::LinkedEditing {
                request,
                line,
                character,
            } => self.lsp_linked_editing(editor, request, position((line, character)), cx),
        }
    }

    /// Asks the server for the ranges selection grows through; without one the editor grows by
    /// words and lines.
    fn lsp_selection_ranges(
        &mut self,
        editor: &Entity<EditorView>,
        request: u64,
        positions: Vec<(u32, u32)>,
        cx: &mut Context<Self>,
    ) {
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, editor, cx);
        let client = self
            .document_client(&doc)
            .filter(|c| c.supports("/selectionRangeProvider"));
        let weak = editor.downgrade();
        cx.spawn(async move |_, cx| {
            let positions: Vec<Position> = positions.into_iter().map(position).collect();
            let chains = match client {
                Some(client) => match client.selection_ranges(&doc, &positions).await {
                    Ok(chains) => Some(chains),
                    Err(why) => {
                        tracing::debug!("selection ranges failed: {why}");
                        None
                    }
                },
                None => None,
            };
            let pos = |p: Position| (p.line, p.character);
            let chains = chains.map(|all| {
                all.into_iter()
                    .map(|chain| {
                        chain
                            .into_iter()
                            .map(|r| (pos(r.start), pos(r.end)))
                            .collect()
                    })
                    .collect()
            });
            let _ = weak.update(cx, |e, cx| e.show_selection_ranges(request, chains, cx));
        })
        .detach();
    }

    /// Cmd+K Cmd+F: the server's edits for the selection, or a notice when it formats only
    /// whole files.
    fn lsp_format_selection(
        &mut self,
        editor: &Entity<EditorView>,
        request: u64,
        range: Range,
        (tab_size, insert_spaces): (u32, bool),
        cx: &mut Context<Self>,
    ) {
        const TITLE: &str = "Format Selection";
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, editor, cx);
        let Some(client) = self.document_client(&doc) else {
            return self.lsp_failed(TITLE, NO_SERVER.into(), cx);
        };
        if !client.supports("/documentRangeFormattingProvider") {
            let program = self
                .lsp
                .documents
                .get(&doc)
                .map_or("The language server", |(_, kind)| kind.program());
            return self.lsp_failed(
                TITLE,
                format!("{program} formats whole files only; ⌘S formats on save."),
                cx,
            );
        }
        let weak = editor.downgrade();
        cx.spawn(async move |this, cx| {
            match client
                .range_formatting(&doc, range, tab_size, insert_spaces)
                .await
            {
                Ok(edits) => {
                    let edits = edits.into_iter().map(server_edit).collect();
                    let _ = weak.update(cx, |e, cx| e.formatted_selection(request, edits, cx));
                }
                Err(why) => {
                    let _ = this.update(cx, |this, cx| this.lsp_failed(TITLE, why, cx));
                }
            }
        })
        .detach();
    }

    /// The ranges typed together with the one at `at`, for linked editing.
    fn lsp_linked_editing(
        &mut self,
        editor: &Entity<EditorView>,
        request: u64,
        at: Position,
        cx: &mut Context<Self>,
    ) {
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, editor, cx);
        let client = self
            .document_client(&doc)
            .filter(|c| c.supports("/linkedEditingRangeProvider"));
        let weak = editor.downgrade();
        cx.spawn(async move |_, cx| {
            let linked = match client {
                Some(client) => {
                    client
                        .linked_editing_ranges(&doc, at)
                        .await
                        .unwrap_or_else(|why| {
                            tracing::debug!("linked editing failed: {why}");
                            Default::default()
                        })
                }
                None => Default::default(),
            };
            let pos = |p: Position| (p.line, p.character);
            let ranges = linked
                .ranges
                .into_iter()
                .map(|r| (pos(r.start), pos(r.end)))
                .collect();
            let _ = weak.update(cx, |e, cx| {
                e.show_linked_editing(request, ranges, linked.word_pattern, cx)
            });
        })
        .detach();
    }

    fn lsp_resolve_completion(
        &mut self,
        editor: &Entity<EditorView>,
        request: u64,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some((asked, client, items)) = self.lsp.completions.get(&editor.entity_id()) else {
            return;
        };
        let Some(item) = items.get(index).filter(|_| *asked == request).cloned() else {
            return;
        };
        let client = client.clone();
        let weak = editor.downgrade();
        cx.spawn(async move |_, cx| {
            let item = match client.resolve_completion(&item).await {
                Ok(item) => item,
                Err(why) => return tracing::debug!("completion resolve failed: {why}"),
            };
            let resolved = Resolved {
                detail: item.detail,
                documentation: hover_blocks(item.documentation),
                additional_edits: item.additional_edits.into_iter().map(server_edit).collect(),
            };
            let _ = weak.update(cx, |e, cx| {
                e.resolved_completion(request, index, resolved, cx)
            });
        })
        .detach();
    }

    pub(super) fn lsp_references(
        &mut self,
        editor: &Entity<EditorView>,
        at: Position,
        cx: &mut Context<Self>,
    ) {
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, editor, cx);
        self.lsp.references_asked += 1;
        self.lsp.showing_calls = false;
        self.lsp.reference_opened = None;
        self.lsp.references_noun = "reference";
        self.lsp.references_root = self.workspace.active_project().map(|p| p.root.clone());
        self.show_drawer_tab(DrawerTab::References, cx);
        let Some(client) = self
            .lsp
            .documents
            .get(&doc)
            .and_then(|k| self.lsp.servers.get(k))
            .map(|s| s.client.clone())
        else {
            self.lsp.references = References::Failed(NO_SERVER.into());
            return;
        };
        self.lsp.references = References::Loading;
        let asked = self.lsp.references_asked;
        tracing::debug!(path = %doc.display(), line = at.line, character = at.character, "references");
        cx.spawn(async move |this, cx| {
            // gopls answers a lookup on whitespace or a keyword with an error; that is the user's miss.
            let found = client.references(&doc, at).await.map_err(|why| {
                if why.contains("no identifier found") {
                    tracing::debug!("references: {why}");
                    "No symbol at cursor".to_string()
                } else {
                    tracing::warn!("references failed: {why}");
                    why
                }
            });
            if let Ok(list) = &found {
                tracing::debug!("references → {} locations", list.len());
            }
            let found = match found {
                Ok(list) => Ok(cx
                    .background_executor()
                    .spawn(async move { with_snippets(list) })
                    .await),
                Err(why) => Err(why),
            };
            let _ = this.update(cx, |this, cx| {
                if this.lsp.references_asked != asked {
                    return;
                }
                this.lsp.references = match found {
                    Ok(list) => References::Found(Rc::new(list)),
                    Err(why) => References::Failed(why),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// The last lookup, if it was made in the active project.
    fn active_references(&self) -> Option<&References> {
        let root = self.workspace.active_project().map(|p| &p.root);
        (self.lsp.references_root.as_ref() == root).then_some(&self.lsp.references)
    }

    pub(super) fn render_references_count(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.lsp.showing_calls {
            return self.render_calls_header(cx);
        }
        let Some(References::Found(list)) = self.active_references() else {
            return None;
        };
        let t = cx.theme();
        let count = match list.len() {
            1 => format!("1 {}", self.lsp.references_noun),
            n => format!("{n} {}s", self.lsp.references_noun),
        };
        Some(
            div()
                .text_color(t.color.content_muted)
                .child(count)
                .into_any_element(),
        )
    }

    /// The References tab: `path:line:col` and the line's text, one row per use.
    pub(super) fn render_references(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.lsp.showing_calls
            && let Some(calls) = self.render_calls(cx)
        {
            return calls;
        }
        let t = cx.theme().clone();
        let message = |text: String| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(t.typography.caption)
                .text_color(t.color.content_muted)
                .child(text)
                .into_any_element()
        };
        let list = match self.active_references().unwrap_or(&References::Idle) {
            References::Idle => {
                return message(
                    "Press Shift+F12 or ⌘⌥R on a symbol to list where it is used, or \
                     Shift+Alt+H on a function for its calls."
                        .into(),
                );
            }
            References::Loading => return message("Finding references…".into()),
            References::Failed(why) => return message(why.clone()),
            References::Found(list) if list.is_empty() => {
                return message(format!("No {}s found.", self.lsp.references_noun));
            }
            References::Found(list) => list.clone(),
        };
        let roots: Vec<PathBuf> = self
            .workspace
            .active_project()
            .into_iter()
            .flat_map(|p| [p.root.canonicalize().ok(), Some(p.root.clone())])
            .flatten()
            .collect();
        let opened = self.lsp.reference_opened;
        uniform_list(
            "references",
            list.len(),
            cx.processor(move |_this, range: std::ops::Range<usize>, _window, cx| {
                range
                    .map(|i| {
                        let r = &list[i];
                        let shown = roots
                            .iter()
                            .find_map(|root| r.path.strip_prefix(root).ok())
                            .unwrap_or(&r.path);
                        let place = format!(
                            "{}:{}:{}",
                            shown.display(),
                            r.at.line + 1,
                            r.at.character + 1
                        );
                        let (path, at) = (r.path.clone(), r.at);
                        let selected = opened == Some(i);
                        div()
                            .id(("reference", i))
                            .w_full()
                            .h(px(28.))
                            .px(px(12.))
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .cursor_pointer()
                            .hover(|s| s.bg(t.color.surface_hover))
                            .when(selected, |el| el.bg(t.color.surface_active))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.lsp.reference_opened = Some(i);
                                this.lsp.jump = Some((path.clone(), at));
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(t.typography.caption)
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(if selected {
                                        t.color.accent
                                    } else {
                                        t.color.content_secondary
                                    })
                                    .child(place),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .font_family(t.typography.mono.clone())
                                    .text_size(t.typography.caption)
                                    .text_color(t.color.content_muted)
                                    .child(r.snippet.clone()),
                            )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .size_full()
        .into_any_element()
    }

    /// Says why a lookup went nowhere, so a click or key press is never silently ignored.
    pub(super) fn lsp_failed(&mut self, title: &str, body: String, cx: &mut Context<Self>) {
        self.transient_notice(title, body, cx);
    }

    /// Cmd+click on `path:line:col` in a terminal; a relative path no folder had is looked for
    /// in the project, as Go test output names files relative to their package.
    pub(super) fn open_file_link(
        &mut self,
        path: PathBuf,
        line: Option<u32>,
        column: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        let at = line.map(|line| Position {
            line: line.saturating_sub(1),
            character: column.unwrap_or(1).saturating_sub(1),
        });
        let open = move |this: &mut Self, path: PathBuf, cx: &mut Context<Self>| {
            match at {
                Some(at) => this.lsp.jump = Some((path, at)),
                None => this.pending_open = Some(path),
            }
            cx.notify();
        };
        if path.is_absolute() {
            return open(self, path, cx);
        }
        let Some(root) = self.active_root() else {
            return;
        };
        let walk = cx.background_executor().spawn({
            let path = path.clone();
            async move { files_ending_with(&root, &path) }
        });
        cx.spawn(async move |this, cx| {
            let found = walk.await;
            let _ = this.update(cx, |this, cx| match found.as_slice() {
                [only] => open(this, only.clone(), cx),
                [] => this.transient_notice(
                    "File not found",
                    format!("No {} in this project.", path.display()),
                    cx,
                ),
                _ => {
                    this.lsp.find_file = Some(path.display().to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Opens a definition found since the last frame; opening a tab needs the window.
    pub(super) fn take_lsp_jump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(query) = self.lsp.find_file.take() {
            self.open_palette_with(super::palette::Mode::Files, &query, window, cx);
        }
        let Some((path, at)) = self.lsp.jump.take() else {
            return;
        };
        self.open_file(path.clone(), window, cx);
        if let Some(ItemView::Editor(view)) = self.editor_for(&path, cx) {
            view.update(cx, |v, cx| v.go_to_position(at.line, at.character, cx));
        }
    }

    /// Tells the server a file is closed once no tab shows it.
    pub(super) fn lsp_closed(&mut self, path: &Path, cx: &Context<Self>) {
        let doc = document_key(path);
        let open: HashSet<EntityId> = self
            .items
            .values()
            .filter_map(|view| match view {
                ItemView::Editor(e) => Some(e.entity_id()),
                _ => None,
            })
            .collect();
        self.lsp.completions.retain(|id, _| open.contains(id));
        if !self.editors_showing(&doc, cx).is_empty() {
            return;
        }
        self.lsp.changes.remove(&doc);
        self.breadcrumbs_closed(&doc);
        let keys = self.lsp.documents.remove(&doc).into_iter();
        for key in keys.chain(self.lsp.linters.remove(&doc).into_iter().flatten()) {
            if let Some(server) = self.lsp.servers.get(&key) {
                server.client.did_close(&doc);
            }
        }
    }

    pub(super) fn lsp_project_closed(&mut self, root: &Path) {
        if self.lsp.calls.as_ref().is_some_and(|c| c.root == root) {
            self.lsp.calls = None;
            self.lsp.calls_client = None;
        }
        self.lsp.servers.retain(|(r, _), _| r != root);
        self.lsp.held_back.remove(root);
        self.settings.projects.remove(root);
        self.lsp.failed.retain(|(r, _), _| r != root);
        self.lsp.documents.retain(|_, (r, _)| r != root);
        self.lsp.linters.retain(|_, keys| {
            keys.retain(|(r, _)| r != root);
            !keys.is_empty()
        });
        self.lsp.crashes.retain(|(r, _), _| r != root);
        self.lsp.restarts.retain(|(r, _), _| r != root);
        self.lsp.diagnostics.retain(|_, published| {
            published.retain(|((r, _), _)| r != root);
            !published.is_empty()
        });
        let under = document_key(root);
        self.lsp.changes.retain(|doc, _| !doc.starts_with(&under));
    }

    /// Each file's diagnostics from the servers of the project at `root`, by canonical path.
    pub(super) fn lsp_diagnostics_of(&self, root: &Path) -> Vec<(&Path, Vec<&Diagnostic>)> {
        self.lsp
            .diagnostics
            .iter()
            .map(|(doc, published)| {
                let list = published
                    .iter()
                    .filter(|((r, _), _)| r == root)
                    .flat_map(|(_, list)| list)
                    .collect::<Vec<_>>();
                (doc.as_path(), list)
            })
            .filter(|(_, list)| !list.is_empty())
            .collect()
    }

    /// Whether a language server has started for the active project.
    pub(super) fn lsp_running_for_active(&self) -> bool {
        let root = self.active_root();
        self.lsp
            .servers
            .keys()
            .any(|(r, _)| Some(r) == root.as_ref())
    }

    /// Diagnostics for `path`, or for every file under `roots`, 1-based for people and Claude.
    pub(super) fn diagnostics_under(
        &self,
        path: Option<&Path>,
        roots: &[PathBuf],
    ) -> Vec<DiagnosticInfo> {
        let mut out: Vec<DiagnosticInfo> = self
            .lsp
            .diagnostics
            .iter()
            .filter(|(file, _)| match path {
                Some(p) => file.as_path() == p,
                None => roots.iter().any(|r| file.starts_with(r)),
            })
            .flat_map(|(file, published)| {
                published
                    .iter()
                    .flat_map(|(_, list)| list)
                    .map(move |d| DiagnosticInfo {
                        path: file.clone(),
                        line: d.range.start.line + 1,
                        column: d.range.start.character + 1,
                        severity: format!("{:?}", d.severity).to_lowercase(),
                        message: d.message.clone(),
                        source: d.source.clone(),
                    })
            })
            .collect();
        out.sort_by(|a, b| (&a.path, a.line, a.column).cmp(&(&b.path, b.line, b.column)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unstarted() -> (Rc<Client>, Server) {
        let (client, _) = Client::start_local(
            ServerKind::Eslint,
            PathBuf::from("/nonexistent/eslint"),
            PathBuf::from("/tmp"),
            Config::default(),
        );
        let client = Rc::new(client);
        let server = Server {
            client: client.clone(),
            config: Config::default(),
            ready: true,
            _events: Task::ready(()),
        };
        (client, server)
    }

    #[test]
    fn a_pulled_answer_from_a_crashed_instance_is_not_applied_to_its_restart() {
        let key: ServerKey = (PathBuf::from("/p"), ServerKind::Eslint);
        let doc = PathBuf::from("/p/a.ts");
        let mut lsp = LspState::default();
        let (crashed, server) = unstarted();
        lsp.servers.insert(key.clone(), server);
        lsp.linters.insert(doc.clone(), vec![key.clone()]);
        assert!(lsp.answer_is_current(&key, &crashed, &doc));
        let (restarted, server) = unstarted();
        lsp.servers.insert(key.clone(), server);
        assert!(!lsp.answer_is_current(&key, &crashed, &doc));
        assert!(lsp.answer_is_current(&key, &restarted, &doc));
        lsp.linters.clear();
        assert!(
            !lsp.answer_is_current(&key, &restarted, &doc),
            "the file closed"
        );
    }

    #[test]
    fn the_trust_question_names_what_the_project_brings() {
        let (message, detail) = trust_question("web", &["ESLint", "Biome", "TypeScript"], true);
        assert_eq!(message, "Run ESLint, Biome and TypeScript from web?");
        assert!(
            detail.starts_with(
                "web installs ESLint, Biome and TypeScript in node_modules and has settings"
            ),
            "{detail}"
        );
        let (message, detail) = trust_question("api", &["ESLint"], false);
        assert_eq!(message, "Run ESLint from api?");
        assert!(!detail.contains("settings that"), "{detail}");
        assert!(!detail.contains("TypeScript"), "{detail}");
        let (message, detail) = trust_question("go-svc", &[], true);
        assert_eq!(message, "Use go-svc's language server settings?");
        assert!(detail.starts_with("go-svc has settings"), "{detail}");
        assert!(detail.contains("can name programs"), "{detail}");
    }

    /// Return, Escape and Space as gpui 0.2.2's macOS `prompt` maps them onto the buttons.
    fn alert_keys(buttons: &[PromptButton]) -> [Option<usize>; 3] {
        let cancel = |i: &usize| matches!(buttons[*i], PromptButton::Cancel(_));
        let focused = (0..buttons.len())
            .rev()
            .find(|i| !cancel(i))
            .filter(|&i| i > 0);
        let mut added: Vec<usize> = (0..buttons.len()).filter(|&i| Some(i) != focused).collect();
        added.extend(focused);
        // A Cancel button's Escape replaces the Return NSAlert gives its first button.
        let enter = added.first().copied().filter(|i| !cancel(i));
        let escape = added.iter().copied().find(cancel);
        [enter, escape, added.last().copied()]
    }

    #[test]
    fn a_trust_answer_restarts_every_main_server_of_the_project_and_typescript() {
        let root = Path::new("/p");
        let running = [
            (root.to_path_buf(), ServerKind::Go),
            (root.to_path_buf(), ServerKind::Eslint),
            (PathBuf::from("/q"), ServerKind::Go),
        ];
        assert_eq!(
            restarted_on_trust_change(running.iter(), root),
            [
                (root.to_path_buf(), ServerKind::Go),
                (root.to_path_buf(), ServerKind::TypeScript),
            ]
        );
        let running = [(root.to_path_buf(), ServerKind::TypeScript)];
        assert_eq!(
            restarted_on_trust_change(running.iter(), root),
            running,
            "once"
        );
    }

    #[test]
    fn risky_prompts_leave_return_unanswered_and_escape_cancels() {
        for (cancel, action) in [
            ("Don't Allow", "Allow"),
            ("Cancel", "Delete Anyway"),
            ("Don't Update", "Update Imports"),
            ("Cancel", "Revert"),
        ] {
            let buttons = confirm_buttons(cancel, action);
            assert_eq!(buttons[CONFIRMED].label().as_ref(), action);
            assert_eq!(
                alert_keys(&buttons),
                [None, Some(0), Some(CONFIRMED)],
                "{action}"
            );
        }
        let before = [
            PromptButton::ok("Allow"),
            PromptButton::cancel("Don't Allow"),
        ];
        assert_eq!(alert_keys(&before)[0], Some(0), "Return used to allow");
    }

    #[test]
    fn only_settings_read_at_start_restart_a_server() {
        use serde_json::json;
        let pinned =
            json!({"tsserver": {"path": "/g/typescript/lib/tsserver.js"}, "preferences": {}});
        let project = json!({"preferences": {"quoteStyle": "single"}});
        assert!(needs_restart(ServerKind::TypeScript, &pinned, &project));
        assert!(!needs_restart(
            ServerKind::TypeScript,
            &project,
            &json!({"preferences": {}})
        ));
        assert!(needs_restart(
            ServerKind::Go,
            &json!({}),
            &json!({"env": {"GOTOOLCHAIN": "auto"}})
        ));
        assert!(!needs_restart(
            ServerKind::Go,
            &json!({}),
            &json!({"staticcheck": true})
        ));
    }

    #[test]
    fn a_crashed_server_restarts_later_each_time_and_then_stays_stopped() {
        let delays: Vec<_> = (1..=MAX_CRASHES).map(restart_delay).collect();
        assert_eq!(
            delays,
            [
                Some(Duration::from_millis(500)),
                Some(Duration::from_secs(1)),
                Some(Duration::from_secs(2)),
                Some(Duration::from_secs(4)),
                None,
            ]
        );
    }
}

/// A file's language server as the status bar shows it.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum LspStatus {
    Starting(&'static str),
    Ready(&'static str),
    Failed(&'static str, String),
}

impl Shell {
    /// The server for `lang` files in the project at `root`; `None` when none runs for them.
    pub(super) fn lsp_status(&self, root: &Path, lang: Lang) -> Option<LspStatus> {
        let (kind, _) = server_for(lang)?;
        let key = (root.to_path_buf(), kind);
        let program = kind.program();
        if let Some(error) = self.lsp.failed.get(&key) {
            return Some(LspStatus::Failed(program, error.clone()));
        }
        let server = self.lsp.servers.get(&key)?;
        Some(if server.ready {
            LspStatus::Ready(program)
        } else {
            LspStatus::Starting(program)
        })
    }
}
