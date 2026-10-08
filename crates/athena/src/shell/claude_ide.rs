use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use athena_editor::{DiffEvent, DiffView, EditorView, HunkActions};
use athena_workspace::{DiffBase, ItemId, ItemKind, Workspace};
use gpui::{AnyWindowHandle, AppContext as _, Context, Entity, EntityId, Task, Window};
use serde_json::{Value, json};

use super::Shell;
use super::item::ItemView;
use super::notices::ToastAction;
use super::review::diff_title;
use crate::ide::{self, DiffKey, Event, Verdict};

/// How often the focused editor's selection is checked while Claude Code is connected; this
/// also debounces a drag or a held arrow key into one update.
const SELECTION_POLL: Duration = Duration::from_millis(150);

const WAITING_NOTE: &str = "Claude Code is waiting for your answer. Accept (⌘↵) lets it write \
                            this change; Reject (⌘⌫) or closing the tab declines it.";

#[derive(Default)]
pub(super) struct IdeState {
    server: Option<ide::Server>,
    _events: Option<Task<()>>,
    /// Proposals Claude Code waits on, by the id their tab carries.
    proposals: HashMap<String, Proposal>,
    /// Connected sessions and the pid each reported.
    clients: HashMap<u64, Option<i32>>,
    _selection: Option<Task<()>>,
    /// The selection last sent, so an unchanged one is not sent again.
    sent: Option<Value>,
    /// Where proposal tabs live, to close one answered from its own toolbar.
    window: Option<AnyWindowHandle>,
}

struct Proposal {
    key: DiffKey,
    contents: String,
    /// The tab showing it; closing that tab rejects the change.
    view: Option<EntityId>,
}

fn is_proposal(kind: &ItemKind) -> bool {
    matches!(
        kind,
        ItemKind::Diff {
            base: DiffBase::Proposal { .. },
            ..
        }
    )
}

fn read_text(path: &Path) -> Result<String, String> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).map_err(|_| "This file is not UTF-8 text.".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("Could not read {}: {e}", path.display())),
    }
}

/// `selection_changed` as VS Code sends it; Claude Code reads the 0-based lines and the text.
fn selection_json(path: &Path, start: (u32, u32), end: (u32, u32), text: String) -> Value {
    let position = |(line, character): (u32, u32)| json!({"line": line, "character": character});
    json!({
        "text": text,
        "filePath": path,
        "fileUrl": format!("file://{}", path.display()),
        "selection": {"start": position(start), "end": position(end), "isEmpty": start == end},
    })
}

/// `at_mentioned` for a file, with 0-based lines when something is selected; a selection ending
/// at the start of a line does not take that line in.
fn mention_json(path: &Path, start: (u32, u32), end: (u32, u32)) -> Value {
    let mut mention = json!({ "filePath": path });
    if start != end {
        let last = if end.1 == 0 && end.0 > start.0 {
            end.0 - 1
        } else {
            end.0
        };
        mention["lineStart"] = json!(start.0);
        mention["lineEnd"] = json!(last);
    }
    mention
}

/// The selection to send, if it differs from the one sent last.
fn changed(sent: &Option<Value>, next: Value) -> Option<Value> {
    (sent.as_ref() != Some(&next)).then_some(next)
}

fn severity(s: athena_lsp::Severity) -> &'static str {
    match s {
        athena_lsp::Severity::Error => "Error",
        athena_lsp::Severity::Warning => "Warning",
        athena_lsp::Severity::Information => "Info",
        athena_lsp::Severity::Hint => "Hint",
    }
}

fn diagnostic(d: &athena_lsp::Diagnostic) -> ide::Diagnostic {
    let position = |p: athena_lsp::Position| ide::Position {
        line: p.line,
        character: p.character,
    };
    ide::Diagnostic {
        message: d.message.clone(),
        severity: severity(d.severity),
        source: d.source.clone(),
        code: d.raw.get("code").cloned(),
        range: ide::Range {
            start: position(d.range.start),
            end: position(d.range.end),
        },
    }
}

impl Shell {
    /// Starts the server if the setting is on; otherwise withdraws a port an earlier run left.
    pub(super) fn start_ide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.workspace.ide_integration {
            if let Some(env) = ide::env_file() {
                ide::forget_env(&env);
            }
            return;
        }
        let Some(lock_dir) = ide::default_lock_dir() else {
            return;
        };
        let config = ide::Config {
            lock_dir,
            env_file: ide::env_file(),
            folders: self.ide_folders(),
        };
        match ide::Server::start(config) {
            Ok((server, events)) => {
                self.ide.server = Some(server);
                self.ide.window = Some(window.window_handle());
                self.ide._events = Some(cx.spawn_in(window, async move |this, cx| {
                    while let Ok(event) = events.recv().await {
                        let handled = this
                            .update_in(cx, |this, window, cx| this.ide_event(event, window, cx));
                        if handled.is_err() {
                            return;
                        }
                    }
                }));
            }
            Err(e) => {
                tracing::error!("could not start the Claude Code IDE server: {e}");
                self.transient_notice(
                    "Claude Code integration could not start",
                    format!("{e}. Turn it off and on again to retry."),
                    cx,
                );
            }
        }
    }

    pub(super) fn toggle_ide_integration(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.ide_integration = !self.workspace.ide_integration;
        self.schedule_save(cx);
        if !self.workspace.ide_integration {
            self.stop_ide(window, cx);
            return self.transient_notice(
                "Claude Code integration is off",
                "Claude Code asks about edits in the terminal again.",
                cx,
            );
        }
        self.start_ide(window, cx);
        if self.ide.server.is_none() {
            self.workspace.ide_integration = false;
            return;
        }
        self.transient_notice(
            "Claude Code integration is on",
            "Claude sessions started in new terminals connect to Athena: proposed edits open as \
             diffs to accept or reject, and your selection goes with each prompt. In a session \
             already running, type /ide.",
            cx,
        );
    }

    fn stop_ide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ide._events = None;
        self.ide._selection = None;
        self.ide.clients.clear();
        self.ide.sent = None;
        if let Some(server) = self.ide.server.take() {
            server.turn_off();
        }
        let ids: Vec<String> = self.ide.proposals.drain().map(|(id, _)| id).collect();
        for id in ids {
            self.close_proposal_tab(&id, window, cx);
        }
    }

    /// Claude Code hears that pending proposals were rejected, and the lock file goes.
    pub(super) fn ide_quit(&mut self) {
        self.ide.server = None;
    }

    fn ide_folders(&self) -> Vec<PathBuf> {
        self.workspace
            .projects
            .iter()
            .map(|p| p.root.clone())
            .collect()
    }

    /// Keeps the lock file's folders in step with the open projects.
    pub(super) fn sync_ide_folders(&mut self) {
        let folders = self.ide_folders();
        if let Some(server) = self.ide.server.as_mut() {
            server.set_folders(folders);
        }
    }

    /// The workspace as saved: a proposal cannot outlive the request that opened it.
    pub(super) fn persisted_workspace(&self) -> Cow<'_, Workspace> {
        let any = self
            .workspace
            .projects
            .iter()
            .filter_map(|p| p.layout.as_ref())
            .any(|l| l.items().any(|i| is_proposal(&i.kind)));
        if !any {
            return Cow::Borrowed(&self.workspace);
        }
        let mut saved = self.workspace.clone();
        for project in &mut saved.projects {
            let Some(layout) = project.layout.as_mut() else {
                continue;
            };
            let ids: Vec<ItemId> = layout
                .items()
                .filter(|i| is_proposal(&i.kind))
                .map(|i| i.id)
                .collect();
            for id in ids {
                if !layout.close_item(id) {
                    project.layout = None;
                    break;
                }
            }
        }
        Cow::Owned(saved)
    }

    fn ide_event(&mut self, event: Event, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            Event::OpenDiff {
                key,
                path,
                contents,
            } => self.show_proposal(key, path, contents, window, cx),
            Event::CloseDiff { key } => {
                let id = key.id();
                self.ide.proposals.remove(&id);
                self.close_proposal_tab(&id, window, cx);
            }
            Event::Diagnostics { path, reply } => {
                let _ = reply.send(self.ide_diagnostics(path.as_deref()));
            }
            Event::Client { client, pid } => {
                self.ide.clients.insert(client, pid);
                self.watch_selection(cx);
            }
            Event::Disconnected { client } => {
                self.ide.clients.remove(&client);
                if self.ide.clients.is_empty() {
                    self.ide._selection = None;
                }
            }
        }
    }

    fn show_proposal(
        &mut self,
        key: DiffKey,
        path: PathBuf,
        contents: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self
            .workspace
            .projects
            .iter()
            .position(|p| path.starts_with(&p.root))
        {
            Some(i) => self.switch_to(i, cx),
            None if self.workspace.active.is_none() => {
                // Claude Code then asks in the terminal instead.
                if let Some(server) = &self.ide.server {
                    server.resolve(&key, Verdict::Unavailable);
                }
                return;
            }
            None => {}
        }
        let id = key.id();
        self.close_proposal_tab(&id, window, cx);
        self.ide.proposals.insert(
            id.clone(),
            Proposal {
                key,
                contents,
                view: None,
            },
        );
        self.open_diff(path, DiffBase::Proposal { id }, window, cx);
    }

    pub(super) fn new_proposal_view(
        &mut self,
        path: &Path,
        id: &str,
        cx: &mut Context<Self>,
    ) -> ItemView {
        let base = DiffBase::Proposal { id: id.to_string() };
        let title = diff_title(path, &base);
        let view = cx.new(|cx| {
            DiffView::new(
                path.to_path_buf(),
                title,
                "On Disk",
                "Claude's Proposal",
                HunkActions::default(),
                cx,
            )
            .as_proposal()
        });
        let entity = view.entity_id();
        match self.ide.proposals.get_mut(id) {
            Some(proposal) => {
                proposal.view = Some(entity);
                let new = proposal.contents.clone();
                view.update(cx, |v, cx| {
                    v.set_old_label("On Disk", Some(WAITING_NOTE), cx)
                });
                let (disk, weak) = (path.to_path_buf(), view.downgrade());
                cx.spawn(async move |_, cx| {
                    let old = cx
                        .background_executor()
                        .spawn(async move { read_text(&disk) })
                        .await;
                    if let Some(view) = weak.upgrade() {
                        let _ = view.update(cx, |v, cx| match old {
                            Ok(old) => v.set_texts(old, new, cx),
                            Err(e) => v.set_error(e, cx),
                        });
                    }
                })
                .detach();
            }
            None => view.update(cx, |v, cx| {
                v.set_error(
                    "Claude Code is no longer waiting for an answer to this change.",
                    cx,
                )
            }),
        }
        let (key, file) = (id.to_string(), path.to_path_buf());
        cx.subscribe(&view, move |this, _, event: &DiffEvent, cx| match event {
            DiffEvent::Accept => this.answer_proposal(&key, true, cx),
            DiffEvent::Reject => this.answer_proposal(&key, false, cx),
            DiffEvent::OpenFile => {
                this.pending_open = Some(file.clone());
                cx.notify();
            }
            _ => {}
        })
        .detach();
        let key = id.to_string();
        cx.observe_release(&view, move |this, _, cx| {
            if this
                .ide
                .proposals
                .get(&key)
                .is_some_and(|p| p.view == Some(entity))
            {
                this.answer_proposal(&key, false, cx);
            }
        })
        .detach();
        ItemView::Diff(view)
    }

    fn answer_proposal(&mut self, id: &str, accept: bool, cx: &mut Context<Self>) {
        let Some(proposal) = self.ide.proposals.remove(id) else {
            return;
        };
        let verdict = match accept {
            true => Verdict::Accepted(proposal.contents),
            false => Verdict::Rejected,
        };
        if let Some(server) = &self.ide.server {
            server.resolve(&proposal.key, verdict);
        }
        let Some(handle) = self.ide.window else {
            return;
        };
        // Answered from inside the tab's own event, while the window is busy dispatching it.
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            let _ = cx.update_window(handle, |_, window, cx| {
                this.update(cx, |this, cx| this.close_proposal_tab(&id, window, cx))
            });
        })
        .detach();
    }

    fn close_proposal_tab(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let tabs: Vec<(PathBuf, ItemId)> = self
            .workspace
            .projects
            .iter()
            .filter_map(|p| Some((&p.root, p.layout.as_ref()?)))
            .flat_map(|(root, layout)| {
                layout
                    .items()
                    .filter(|i| matches!(&i.kind, ItemKind::Diff { base: DiffBase::Proposal { id: x }, .. } if x == id))
                    .map(|i| (root.clone(), i.id))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (root, item) in tabs {
            self.remove_item_from(&root, item, window, cx);
        }
    }

    fn ide_diagnostics(&self, path: Option<&Path>) -> Vec<ide::FileDiagnostics> {
        let wanted = path.map(|p| (p.to_path_buf(), p.canonicalize().ok()));
        self.lsp
            .diagnostics
            .iter()
            .filter(|(file, _)| match &wanted {
                Some((asked, real)) => *file == asked || real.as_ref() == Some(*file),
                None => true,
            })
            .map(|(file, (_, list))| ide::FileDiagnostics {
                path: file.clone(),
                diagnostics: list.iter().map(diagnostic).collect(),
            })
            .collect()
    }

    fn watch_selection(&mut self, cx: &mut Context<Self>) {
        // A session that just connected has not seen the current selection.
        self.ide.sent = None;
        if self.ide._selection.is_some() {
            return;
        }
        self.ide._selection = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SELECTION_POLL).await;
                if this.update(cx, |this, cx| this.push_selection(cx)).is_err() {
                    return;
                }
            }
        }));
    }

    /// The focused editor with its project.
    fn focused_editor_in(&self) -> Option<(PathBuf, Entity<EditorView>)> {
        Some((self.active_root()?, self.focused_editor()?))
    }

    /// Sends the editor selection when it changed; a focused terminal keeps the last one, as in
    /// VS Code, so it stays attached while the user types to Claude.
    fn push_selection(&mut self, cx: &mut Context<Self>) {
        let Some((root, editor)) = self.focused_editor_in() else {
            return;
        };
        let editor = editor.read(cx);
        let Some((start, end, text)) = editor.selection_utf16() else {
            return;
        };
        let next = selection_json(editor.path(), start, end, text);
        let Some(next) = changed(&self.ide.sent, next) else {
            return;
        };
        let targets = self.ide_targets(&root, cx);
        if let Some(server) = &self.ide.server {
            server.notify(&targets, "selection_changed", next.clone());
        }
        self.ide.sent = Some(next);
    }

    /// The terminal a Claude Code process runs in, from the process tree.
    fn claude_terminal(&self, pid: i32, cx: &Context<Self>) -> Option<(PathBuf, ItemId)> {
        let above = crate::procinfo::ancestry(pid);
        self.items.iter().find_map(|((root, id), view)| {
            let ItemView::Terminal(t) = view else {
                return None;
            };
            let fg = t.read(cx).foreground_pid()?;
            (above.contains(&fg) || crate::procinfo::ancestry(fg).contains(&pid))
                .then(|| (root.clone(), *id))
        })
    }

    /// Sessions running in `root`'s terminals, plus any whose terminal is not known.
    fn ide_targets(&self, root: &Path, cx: &Context<Self>) -> Vec<u64> {
        let mut targets: Vec<u64> = self
            .ide
            .clients
            .iter()
            .filter(|(_, pid)| {
                pid.and_then(|pid| self.claude_terminal(pid, cx))
                    .is_none_or(|(r, _)| r == root)
            })
            .map(|(client, _)| *client)
            .collect();
        targets.sort_unstable();
        targets
    }

    /// Mentions the focused file and its selected lines in the Claude Code session of its project.
    pub(super) fn send_to_claude(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(server) = &self.ide.server else {
            let action = ToastAction {
                label: "Turn On",
                run: Rc::new(|this: &mut Shell, window, cx| {
                    this.toggle_ide_integration(window, cx)
                }),
            };
            self.action_toast(
                "Claude Code integration is off",
                "Turn it on to send files and selections to Claude Code sessions in Athena.",
                action,
                cx,
            );
            return;
        };
        let Some((root, editor)) = self.focused_editor_in() else {
            return self.transient_notice(
                "Nothing to send to Claude",
                "Focus a file in the editor, then press ⌘⌥K.",
                cx,
            );
        };
        let (path, selection) = {
            let e = editor.read(cx);
            (e.path().to_path_buf(), e.selection_utf16())
        };
        let Some((start, end, _)) = selection else {
            return;
        };
        let targets = self.ide_targets(&root, cx);
        if targets.is_empty() {
            return self.transient_notice(
                "No Claude Code session is connected",
                "Start claude in a new Athena terminal, or type /ide in a session already running.",
                cx,
            );
        }
        server.notify(&targets, "at_mentioned", mention_json(&path, start, end));
        // Typing goes on in the session that got the mention, as in VS Code.
        let terminal = targets
            .iter()
            .filter_map(|c| self.ide.clients.get(c).copied().flatten())
            .find_map(|pid| self.claude_terminal(pid, cx))
            .filter(|(r, _)| *r == root);
        if let Some((_, item)) = terminal
            && let Some((pane, index)) = self
                .workspace
                .active_project()
                .and_then(|p| p.layout.as_ref()?.find_item(item))
        {
            self.activate_tab(pane, index, window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selection_is_sent_as_vs_code_does_and_only_when_it_changed() {
        let path = Path::new("/p/src/main.rs");
        let sel = selection_json(path, (1, 4), (3, 0), "x".into());
        assert_eq!(
            sel,
            json!({
                "text": "x",
                "filePath": "/p/src/main.rs",
                "fileUrl": "file:///p/src/main.rs",
                "selection": {
                    "start": {"line": 1, "character": 4},
                    "end": {"line": 3, "character": 0},
                    "isEmpty": false,
                },
            })
        );
        let cursor = selection_json(path, (2, 0), (2, 0), String::new());
        assert_eq!(cursor["selection"]["isEmpty"], true);

        let mut sent = None;
        assert!(changed(&sent, sel.clone()).is_some());
        sent = Some(sel.clone());
        assert!(
            changed(&sent, sel).is_none(),
            "the same selection is not sent twice"
        );
        assert!(changed(&sent, cursor).is_some());
    }

    #[test]
    fn a_mention_names_the_selected_lines_and_drops_a_trailing_line_start() {
        let path = Path::new("/p/a.rs");
        assert_eq!(
            mention_json(path, (2, 0), (2, 0)),
            json!({"filePath": "/p/a.rs"})
        );
        assert_eq!(
            mention_json(path, (2, 3), (4, 1)),
            json!({"filePath": "/p/a.rs", "lineStart": 2, "lineEnd": 4})
        );
        assert_eq!(
            mention_json(path, (2, 0), (5, 0)),
            json!({"filePath": "/p/a.rs", "lineStart": 2, "lineEnd": 4})
        );
        assert_eq!(
            mention_json(path, (2, 1), (2, 6)),
            json!({"filePath": "/p/a.rs", "lineStart": 2, "lineEnd": 2})
        );
    }

    #[test]
    fn diagnostics_use_claude_codes_severity_names_and_keep_the_code() {
        let d = athena_lsp::Diagnostic {
            range: athena_lsp::Range {
                start: athena_lsp::Position {
                    line: 1,
                    character: 2,
                },
                end: athena_lsp::Position {
                    line: 1,
                    character: 5,
                },
            },
            severity: athena_lsp::Severity::Information,
            message: "m".into(),
            source: Some("gopls".into()),
            raw: json!({"code": "SA1000"}),
        };
        let out = diagnostic(&d);
        assert_eq!(out.severity, "Info");
        assert_eq!(out.code, Some(json!("SA1000")));
        assert_eq!((out.range.start.line, out.range.end.character), (1, 5));
        assert_eq!(severity(athena_lsp::Severity::Error), "Error");
        assert_eq!(severity(athena_lsp::Severity::Hint), "Hint");
    }
}
