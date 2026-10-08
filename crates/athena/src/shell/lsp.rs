use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use athena_editor::{EditorView, Lang, Marker, MarkerSeverity};
use athena_lsp::{Client, Diagnostic, Event, Position, ServerKind, Severity};
use athena_proto::{DiagnosticInfo, NoticeKind};
use gpui::{Context, Entity, Task, Window};

use super::Shell;
use super::item::ItemView;

/// Typing pauses this long before the server gets the new text.
const CHANGE_DELAY: Duration = Duration::from_millis(300);

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
            return;
        };
        cx.spawn(async move |this, cx| {
            let found = client.definition(&doc, at).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(target) = found.into_iter().next() {
                    this.lsp.jump = Some((target.path, target.range.start));
                    cx.notify();
                }
            });
        })
        .detach();
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
