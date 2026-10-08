use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use athena_editor::{
    Completion, EditorView, HoverBlock, Lang, Marker, MarkerSeverity, ServerEdit, Signature,
};
use athena_lsp::{
    Client, CompletionItem, Diagnostic, Event, Location, MarkupBlock, Position, ServerKind,
    Severity,
};
use athena_proto::{DiagnosticInfo, NoticeKind};
use athena_ui::ActiveTheme;
use gpui::{
    AnyElement, Context, Entity, FontWeight, Task, Window, div, prelude::*, px, uniform_list,
};

use super::Shell;
use super::drawer::DrawerTab;
use super::item::ItemView;

/// Typing pauses this long before the server gets the new text.
const CHANGE_DELAY: Duration = Duration::from_millis(300);

/// A slow formatter never holds up Cmd+S longer than this; the file saves unformatted.
const FORMAT_TIMEOUT: Duration = Duration::from_secs(1);

/// Longest line excerpt shown for a reference.
const SNIPPET_CHARS: usize = 160;

const NO_SERVER: &str = "No language server runs for this file.";

/// A server that stops this many times within `CRASH_WINDOW` is left stopped, as in VS Code.
const MAX_CRASHES: usize = 5;
const CRASH_WINDOW: Duration = Duration::from_secs(180);

type ServerKey = (PathBuf, ServerKind);

struct Server {
    client: Rc<Client>,
    ready: bool,
    _events: Task<()>,
}

#[derive(Default)]
pub(super) struct LspState {
    servers: HashMap<ServerKey, Server>,
    /// Servers that failed to start, not retried until Athena restarts.
    failed: HashMap<ServerKey, String>,
    /// Each file's diagnostics and the server that published them.
    diagnostics: HashMap<PathBuf, (ServerKey, Vec<Diagnostic>)>,
    /// Documents the servers have open, and which server has each.
    documents: HashMap<PathBuf, ServerKey>,
    changes: HashMap<PathBuf, Task<()>>,
    /// When each server last stopped unexpectedly, within `CRASH_WINDOW`.
    crashes: HashMap<ServerKey, Vec<Instant>>,
    restarts: HashMap<ServerKey, Task<()>>,
    pub(super) jump: Option<(PathBuf, Position)>,
    references: References,
    /// The project the references were asked from; other projects show the drawer tab empty.
    references_root: Option<PathBuf>,
    /// Bumped per lookup so a slow answer cannot replace a newer one.
    references_asked: u64,
    reference_opened: Option<usize>,
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

/// How long to wait before restarting a server that has stopped `crashes` times in the window.
fn restart_delay(crashes: usize) -> Option<Duration> {
    (crashes < MAX_CRASHES).then(|| Duration::from_millis(500) * (1 << crashes.saturating_sub(1)))
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

fn completion(item: CompletionItem) -> Completion {
    let pos = |p: Position| (p.line, p.character);
    Completion {
        range: item.range.map(|r| (pos(r.start), pos(r.end))),
        additional_edits: item
            .additional_edits
            .into_iter()
            .map(|e| ServerEdit {
                start: pos(e.range.start),
                end: pos(e.range.end),
                text: e.text,
            })
            .collect(),
        label: item.label,
        kind: item.kind,
        detail: item.detail,
        filter_text: item.filter_text,
        sort_text: item.sort_text,
        text: item.text,
        select: item.select,
        preselect: item.preselect,
    }
}

/// Tells an editor which typed characters ask its server for suggestions and signature help.
fn push_triggers(editor: &Entity<EditorView>, client: &Client, cx: &mut Context<Shell>) {
    let completion = client.completion_triggers().to_vec();
    let signature = client.signature_triggers().to_vec();
    editor.update(cx, |e, _| {
        e.set_completion_triggers(Some(completion));
        e.set_signature_triggers(signature);
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
        let (Some(lang), Some(version), Some(text)) = (lang, version, text) else {
            return;
        };
        if let Some(client) = self.document_client(&doc) {
            // Another tab already opened this file in its server.
            push_triggers(editor, &client, cx);
            return;
        }
        let Some((kind, language_id)) = server_for(lang) else {
            return;
        };
        let key = (root.to_path_buf(), kind);
        let Some(client) = self.lsp_client(&key, cx) else {
            return;
        };
        client.did_open(&doc, language_id, version as i64, text);
        push_triggers(editor, &client, cx);
        self.lsp.documents.insert(doc, key);
    }

    fn document_client(&self, doc: &Path) -> Option<Rc<Client>> {
        let key = self.lsp.documents.get(doc)?;
        Some(self.lsp.servers.get(key)?.client.clone())
    }

    fn lsp_client(&mut self, key: &ServerKey, cx: &mut Context<Self>) -> Option<Rc<Client>> {
        if self.lsp.failed.contains_key(key) {
            return None;
        }
        if let Some(server) = self.lsp.servers.get(key) {
            return Some(server.client.clone());
        }
        tracing::info!(root = %key.0.display(), "starting {}", key.1.program());
        let (client, events) = Client::start(key.1, document_key(&key.0));
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
                ready: false,
                _events: task,
            },
        );
        Some(client)
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
            }
            Event::Diagnostics { path, list } => {
                let doc = document_key(&path);
                self.lsp.diagnostics.insert(doc.clone(), (key, list));
                for editor in self.editors_showing(&doc, cx) {
                    self.push_markers(&editor, &doc, cx);
                }
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
        }
    }

    /// Drops what a stopped server reported; its replacement publishes afresh.
    fn clear_diagnostics(&mut self, key: &ServerKey, cx: &mut Context<Self>) {
        let files: Vec<PathBuf> = self
            .lsp
            .diagnostics
            .iter()
            .filter(|(_, (k, _))| k == key)
            .map(|(file, _)| file.clone())
            .collect();
        for file in files {
            self.lsp.diagnostics.remove(&file);
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
                e.read(cx)
                    .lang()
                    .and_then(server_for)
                    .is_some_and(|(kind, _)| kind == key.1)
            })
            .collect();
        tracing::info!(root = %key.0.display(), files = editors.len(), "restarting {}", key.1.program());
        for editor in editors {
            self.lsp_opened(&key.0, &editor, cx);
        }
    }

    fn editors_showing(&self, doc: &Path, cx: &Context<Self>) -> Vec<Entity<EditorView>> {
        self.items
            .values()
            .filter_map(|view| match view {
                ItemView::Editor(e) if document_key(e.read(cx).path()) == doc => Some(e.clone()),
                _ => None,
            })
            .collect()
    }

    fn push_markers(&self, editor: &Entity<EditorView>, doc: &Path, cx: &mut Context<Self>) {
        let markers = self
            .lsp
            .diagnostics
            .get(doc)
            .map(|(_, list)| list.iter().map(marker).collect())
            .unwrap_or_default();
        editor.update(cx, |e, cx| e.set_markers(markers, cx));
    }

    pub(super) fn lsp_edited(&mut self, editor: &Entity<EditorView>, cx: &mut Context<Self>) {
        let doc = document_key(editor.read(cx).path());
        if !self.lsp.documents.contains_key(&doc) {
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
        let server = self
            .lsp
            .documents
            .get(doc)
            .and_then(|k| self.lsp.servers.get(k));
        if let (Some(server), Some(text), Some(version)) = (server, e.text(), e.version()) {
            server.client.did_change(doc, version as i64, text);
        }
    }

    /// Sends a pending edit now, so the server answers about what is on screen.
    fn flush_change(&mut self, doc: &Path, editor: &Entity<EditorView>, cx: &Context<Self>) {
        if self.lsp.changes.remove(doc).is_some() {
            self.send_change(doc, editor, cx);
        }
    }

    pub(super) fn lsp_saved(&mut self, editor: &Entity<EditorView>, cx: &mut Context<Self>) {
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, editor, cx);
        if let Some(server) = self
            .lsp
            .documents
            .get(&doc)
            .and_then(|k| self.lsp.servers.get(k))
        {
            server.client.did_save(&doc);
        }
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
                Ok(Some(hover)) => hover
                    .blocks
                    .into_iter()
                    .map(|b| match b {
                        MarkupBlock::Text(t) => HoverBlock::Text(t),
                        MarkupBlock::Code(c) => HoverBlock::Code(c),
                    })
                    .collect(),
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

    /// Asks the server to format the file before Cmd+S saves it; the editor always gets an
    /// answer, empty when the server is missing, fails or is too slow.
    pub(super) fn lsp_format(
        &mut self,
        editor: &Entity<EditorView>,
        request: u64,
        (tab_size, insert_spaces): (u32, bool),
        cx: &mut Context<Self>,
    ) {
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, editor, cx);
        let Some(client) = self.document_client(&doc) else {
            editor.update(cx, |e, cx| e.format_and_save(request, Vec::new(), cx));
            return;
        };
        let weak = editor.downgrade();
        let late = weak.clone();
        cx.spawn(async move |_, cx| {
            cx.background_executor().timer(FORMAT_TIMEOUT).await;
            let _ = late.update(cx, |e, cx| e.format_and_save(request, Vec::new(), cx));
        })
        .detach();
        cx.spawn(async move |_, cx| {
            let edits = match client.formatting(&doc, tab_size, insert_spaces).await {
                Ok(edits) => edits,
                Err(why) => {
                    tracing::warn!("formatting failed: {why}");
                    Vec::new()
                }
            };
            tracing::debug!("formatting → {} edits", edits.len());
            let edits = edits
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
        cx.spawn(async move |_, cx| {
            let (items, incomplete) = match client.completion(&doc, at, trigger.as_deref()).await {
                Ok(list) => (
                    list.items.into_iter().map(completion).collect(),
                    list.incomplete,
                ),
                Err(why) => {
                    tracing::debug!("completion failed: {why}");
                    (Vec::new(), false)
                }
            };
            tracing::debug!(
                "completion → {} items, incomplete {incomplete}",
                items.len()
            );
            let _ = weak.update(cx, |e, cx| {
                e.show_completions(request, items, incomplete, cx)
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
        self.lsp.reference_opened = None;
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
        let Some(References::Found(list)) = self.active_references() else {
            return None;
        };
        let t = cx.theme();
        let count = match list.len() {
            1 => "1 reference".to_string(),
            n => format!("{n} references"),
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
                    "Press Shift+F12 or ⌘⌥R on a symbol to list where it is used.".into(),
                );
            }
            References::Loading => return message("Finding references…".into()),
            References::Failed(why) => return message(why.clone()),
            References::Found(list) if list.is_empty() => {
                return message("No references found.".into());
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
    fn lsp_failed(&mut self, title: &str, body: String, cx: &mut Context<Self>) {
        self.transient_notice(title, body, cx);
    }

    /// Opens a definition found since the last frame; opening a tab needs the window.
    pub(super) fn take_lsp_jump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        if !self.editors_showing(&doc, cx).is_empty() {
            return;
        }
        self.lsp.changes.remove(&doc);
        if let Some(key) = self.lsp.documents.remove(&doc)
            && let Some(server) = self.lsp.servers.get(&key)
        {
            server.client.did_close(&doc);
        }
    }

    pub(super) fn lsp_project_closed(&mut self, root: &Path) {
        self.lsp.servers.retain(|(r, _), _| r != root);
        self.lsp.failed.retain(|(r, _), _| r != root);
        self.lsp.documents.retain(|_, (r, _)| r != root);
        self.lsp.crashes.retain(|(r, _), _| r != root);
        self.lsp.restarts.retain(|(r, _), _| r != root);
        self.lsp.diagnostics.retain(|_, ((r, _), _)| r != root);
        let under = document_key(root);
        self.lsp.changes.retain(|doc, _| !doc.starts_with(&under));
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
            .flat_map(|(file, (_, list))| {
                list.iter().map(move |d| DiagnosticInfo {
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
