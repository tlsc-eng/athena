use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use athena_editor::{EditorView, Lang, Marker, MarkerSeverity};
use athena_lsp::{Client, Diagnostic, Event, Location, Position, ServerKind, Severity};
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

/// Longest line excerpt shown for a reference.
const SNIPPET_CHARS: usize = 160;

const NO_SERVER: &str = "No language server runs for this file.";

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
    diagnostics: HashMap<PathBuf, Vec<Diagnostic>>,
    /// Documents the servers have open, and which server has each.
    documents: HashMap<PathBuf, ServerKey>,
    changes: HashMap<PathBuf, Task<()>>,
    jump: Option<(PathBuf, Position)>,
    references: References,
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
fn document_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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
        if self.lsp.documents.contains_key(&doc) {
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
        self.lsp.documents.insert(doc, key);
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
                if let Some(server) = self.lsp.servers.get_mut(&key) {
                    server.ready = true;
                }
            }
            Event::Diagnostics { path, list } => {
                let doc = document_key(&path);
                self.lsp.diagnostics.insert(doc.clone(), list);
                for editor in self.editors_showing(&doc, cx) {
                    self.push_markers(&editor, &doc, cx);
                }
            }
            Event::Stopped(why) => {
                let Some(server) = self.lsp.servers.remove(&key) else {
                    return;
                };
                self.lsp.documents.retain(|_, k| *k != key);
                let program = key.1.program();
                tracing::warn!("{program} stopped: {why}");
                let title = if server.ready {
                    format!("{program} stopped; it restarts when you open a file")
                } else {
                    self.lsp.failed.insert(key, why.clone());
                    format!("{program} could not start")
                };
                self.local_notice(NoticeKind::Message { title, body: why }, cx);
            }
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
            .map(|list| list.iter().map(marker).collect())
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
            let found = client.references(&doc, at).await;
            match &found {
                Ok(list) => tracing::debug!("references → {} locations", list.len()),
                Err(why) => tracing::warn!("references failed: {why}"),
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

    pub(super) fn render_references_count(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let References::Found(list) = &self.lsp.references else {
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
        let list = match &self.lsp.references {
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
        let root = self
            .workspace
            .active_project()
            .and_then(|p| p.root.canonicalize().ok());
        let opened = self.lsp.reference_opened;
        uniform_list(
            "references",
            list.len(),
            cx.processor(move |_this, range: std::ops::Range<usize>, _window, cx| {
                range
                    .map(|i| {
                        let r = &list[i];
                        let shown = root
                            .as_ref()
                            .and_then(|root| r.path.strip_prefix(root).ok())
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
        let title = title.to_string();
        self.local_notice(NoticeKind::Message { title, body }, cx);
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
            .flat_map(|(file, list)| {
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
