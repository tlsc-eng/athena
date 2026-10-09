use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use athena_editor::{DiffEvent, DiffView, EditorView, HunkActions};
use athena_workspace::{DiffBase, ItemId, ItemKind, Workspace};
use gpui::{AnyWindowHandle, AppContext as _, Context, Entity, EntityId, Task, Window};
use serde_json::{Value, json};

use super::Shell;
use super::item::ItemView;
use super::notices::ToastAction;
use super::review::{self, diff_title};
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
    /// What the selection was last built from, so a poll that finds nothing new copies no text.
    seen: Option<SelectionKey>,
    /// Where proposal tabs live, to close one answered from its own toolbar.
    window: Option<AnyWindowHandle>,
}

#[derive(PartialEq)]
struct SelectionKey {
    editor: EntityId,
    version: Option<u64>,
    head: Option<(u32, u32)>,
    selected: Option<usize>,
}

struct Proposal {
    key: DiffKey,
    contents: String,
    /// The tab showing it; closing that tab rejects the change.
    view: Option<EntityId>,
}

/// Tabs that mean nothing to a later run: Claude's proposals, replace previews and conflict compares.
fn never_saved(kind: &ItemKind) -> bool {
    matches!(
        kind,
        ItemKind::Diff {
            base: DiffBase::Proposal { .. } | DiffBase::SearchReplace | DiffBase::Conflict,
            ..
        }
    )
}

/// The file on disk as diff text, in the encoding the editor would read it in; a FIFO or device
/// is refused before opening it would block.
fn read_text(path: &Path) -> Result<String, String> {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(format!("Could not read {}: {e}", path.display())),
        Ok(meta) if !meta.is_file() => return Err("This is not a regular file.".into()),
        Ok(meta) if meta.len() > review::MAX_DIFF_BYTES as u64 => {
            return Err("The file is larger than 20 MB.".into());
        }
        Ok(_) => {}
    }
    review::read_file(path)
        .and_then(|bytes| review::decoded(bytes.as_deref(), None))
        .map(|d| d.map(|d| d.text).unwrap_or_default())
        .map_err(|e| format!("{e:#}"))
}

/// `path` with symlinks and `..` resolved as the OS would, as far as it exists.
fn resolve(path: &Path) -> PathBuf {
    let parts: Vec<Component> = path.components().collect();
    for n in (1..=parts.len()).rev() {
        let Ok(mut real) = parts[..n].iter().collect::<PathBuf>().canonicalize() else {
            continue;
        };
        for part in &parts[n..] {
            match part {
                Component::ParentDir => {
                    real.pop();
                }
                Component::Normal(name) => real.push(name),
                _ => {}
            }
        }
        return real;
    }
    path.to_path_buf()
}

/// The project a proposed file really lands in, and the file's path under that project's root.
fn proposal_target(roots: &[&Path], path: &Path) -> Option<(usize, PathBuf)> {
    if !path.is_absolute() {
        return None;
    }
    let real = resolve(path);
    roots.iter().enumerate().find_map(|(i, root)| {
        let inside = real.strip_prefix(resolve(root)).ok()?;
        Some((i, root.join(inside)))
    })
}

/// A copy of `workspace` without proposal tabs, or `None` when it has none.
fn without_proposals(workspace: &Workspace) -> Option<Workspace> {
    let any = workspace
        .projects
        .iter()
        .filter_map(|p| p.layout.as_ref())
        .any(|l| l.items().any(|i| never_saved(&i.kind)));
    if !any {
        return None;
    }
    let mut saved = workspace.clone();
    for project in &mut saved.projects {
        let Some(layout) = project.layout.as_mut() else {
            continue;
        };
        let ids: Vec<ItemId> = layout
            .items()
            .filter(|i| never_saved(&i.kind))
            .map(|i| i.id)
            .collect();
        for id in ids {
            if !layout.close_item(id) {
                project.layout = None;
                break;
            }
        }
    }
    Some(saved)
}

/// `selection_changed` as VS Code sends it; Claude Code reads the 0-based lines and the text.
fn selection_json(path: &Path, start: (u32, u32), end: (u32, u32), text: String) -> Value {
    let position = |(line, character): (u32, u32)| json!({"line": line, "character": character});
    json!({
        "text": text,
        "filePath": path,
        "fileUrl": ide::file_url(path),
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
            self.write_setting(&["ide_integration"], false.into(), cx);
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
        self.write_setting(&["ide_integration"], true.into(), cx);
        self.transient_notice(
            "Claude Code integration is on",
            "Claude sessions started in new terminals connect to Athena: proposed edits open as \
             diffs to accept or reject, and your selection goes with each prompt. In a session \
             already running, type /ide.",
            cx,
        );
    }

    pub(super) fn stop_ide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        without_proposals(&self.workspace).map_or(Cow::Borrowed(&self.workspace), Cow::Owned)
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
        let roots: Vec<&Path> = self
            .workspace
            .projects
            .iter()
            .map(|p| p.root.as_path())
            .collect();
        let Some((project, path)) = proposal_target(&roots, &path) else {
            // Claude Code then asks in the terminal instead.
            if let Some(server) = &self.ide.server {
                server.resolve(&key, Verdict::Unavailable);
            }
            return;
        };
        self.switch_to(project, cx);
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
            .map(|(file, published)| ide::FileDiagnostics {
                path: file.clone(),
                diagnostics: published
                    .iter()
                    .flat_map(|(_, list)| list)
                    .map(diagnostic)
                    .collect(),
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
        let key = {
            let e = editor.read(cx);
            SelectionKey {
                editor: editor.entity_id(),
                version: e.version(),
                head: e.cursor_utf16(),
                selected: e.status().map(|s| s.selected),
            }
        };
        if self.ide.sent.is_some() && self.ide.seen.as_ref() == Some(&key) {
            return;
        }
        self.ide.seen = Some(key);
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
            && athena_workspace::Panel::holds(item)
        {
            self.activate_panel_terminal(item, window, cx);
        } else if let Some((_, item)) = terminal
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
    fn proposal_tabs_are_left_out_of_the_saved_workspace_and_focus_stays_valid() {
        use athena_workspace::{Axis, Layout};
        let proposal = ItemKind::Diff {
            path: PathBuf::from("/p/a.rs"),
            base: DiffBase::Proposal { id: "1:t".into() },
        };
        let mut workspace = Workspace::default();
        workspace.add_project(PathBuf::from("/p"));
        let mut layout = Layout::new(ItemKind::Terminal { session: None });
        let terminal_pane = layout.focused;
        let pane = layout
            .split(terminal_pane, Axis::Horizontal, proposal.clone())
            .unwrap();
        assert_eq!(layout.focused, pane);
        workspace.projects[0].layout = Some(layout);
        assert!(without_proposals(&Workspace::default()).is_none());

        let saved = without_proposals(&workspace).unwrap();
        let layout = saved.projects[0].layout.clone().unwrap();
        assert!(layout.items().all(|i| !never_saved(&i.kind)));
        assert_eq!(layout.focused, terminal_pane);
        assert!(layout.validated().is_some());

        let mut alone = Workspace::default();
        alone.add_project(PathBuf::from("/q"));
        alone.projects[0].layout = Some(Layout::new(proposal));
        assert!(
            without_proposals(&alone).unwrap().projects[0]
                .layout
                .is_none()
        );
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
    fn the_file_on_disk_is_read_only_when_it_is_small_regular_text() {
        let dir = std::env::temp_dir().join(format!("athena-ide-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str| dir.join(name);
        assert_eq!(read_text(&file("missing.rs")), Ok(String::new()));
        std::fs::write(file("a.rs"), "fn a() {}\n").unwrap();
        assert_eq!(read_text(&file("a.rs")).as_deref(), Ok("fn a() {}\n"));
        std::fs::write(file("a.png"), [0x89, b'P', 0, 1]).unwrap();
        assert!(read_text(&file("a.png")).unwrap_err().contains("binary"));

        let fifo =
            std::ffi::CString::new(file("pipe").into_os_string().into_encoded_bytes()).unwrap();
        // SAFETY: fifo is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(read_text(&file("pipe")).unwrap_err().contains("regular"));
        assert!(read_text(&dir).unwrap_err().contains("regular"));

        let big = std::fs::File::create(file("big.log")).unwrap();
        big.set_len(review::MAX_DIFF_BYTES as u64 + 1).unwrap();
        assert!(read_text(&file("big.log")).unwrap_err().contains("20 MB"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_file_on_disk_is_read_in_the_encoding_the_editor_would_use() {
        let dir = std::env::temp_dir().join(format!("athena-ide-enc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("a = \"é\"\n".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        std::fs::write(dir.join("utf16.txt"), utf16).unwrap();
        assert_eq!(
            read_text(&dir.join("utf16.txt")).as_deref(),
            Ok("a = \"é\"\n")
        );
        // こんにちは in Shift JIS.
        let sjis = b"\x82\xb1\x82\xf1\x82\xc9\x82\xbf\x82\xcd\n";
        std::fs::write(dir.join("sjis.txt"), sjis).unwrap();
        assert_eq!(
            read_text(&dir.join("sjis.txt")).as_deref(),
            Ok("こんにちは\n")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_proposal_lands_in_the_project_its_resolved_path_is_in() {
        let dir = std::env::temp_dir().join(format!("athena-ide-target-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (proj, other) = (dir.join("proj"), dir.join("other"));
        std::fs::create_dir_all(proj.join("src")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(&proj, dir.join("link")).unwrap();
        std::os::unix::fs::symlink(&other, proj.join("out")).unwrap();
        let roots = [other.as_path(), proj.as_path()];
        let target = |p: PathBuf| proposal_target(&roots, &p);

        assert_eq!(
            target(proj.join("src/new.rs")),
            Some((1, proj.join("src/new.rs")))
        );
        assert_eq!(
            target(proj.join("gone/../a.rs")),
            Some((1, proj.join("a.rs")))
        );
        assert_eq!(target(dir.join("link/a.rs")), Some((1, proj.join("a.rs"))));
        assert_eq!(target(proj.join("out/x.rs")), Some((0, other.join("x.rs"))));
        assert_eq!(target(proj.join("../../etc/passwd")), None);
        assert_eq!(target(PathBuf::from("proj/a.rs")), None);
        let linked = [dir.join("link")];
        let linked: Vec<&Path> = linked.iter().map(PathBuf::as_path).collect();
        assert_eq!(
            proposal_target(&linked, &proj.join("a.rs")),
            Some((0, dir.join("link/a.rs")))
        );
        std::fs::remove_dir_all(&dir).unwrap();
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
