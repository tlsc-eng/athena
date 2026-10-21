use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use athena_term::ClaudeState;
use athena_workspace::{ItemId, git};
use gpui::{Context, Task, Window};
use serde_json::{Value, json};

use super::Shell;
use super::claude_sessions::live::LiveSession;
use super::item::{ItemView, file_label};
use super::palette::Mode;
use super::windows;

/// How long a mention waits for a new session to come up before it is dropped.
const PENDING_FOR: Duration = Duration::from_secs(30);
const RETRY: Duration = Duration::from_millis(500);

/// Lines of a file to hand to a Claude session, 1-based and inclusive.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Mention {
    pub(super) path: PathBuf,
    first: u32,
    last: u32,
}

impl Mention {
    /// From a selection's 0-based UTF-16 ends; one ending at the start of a line leaves that line out.
    pub(super) fn from_selection(path: &Path, start: (u32, u32), end: (u32, u32)) -> Self {
        let last = if end.1 == 0 && end.0 > start.0 {
            end.0 - 1
        } else {
            end.0
        };
        Self {
            path: path.to_path_buf(),
            first: start.0 + 1,
            last: last + 1,
        }
    }

    /// What goes into a prompt: the lines of `shown`, then a space to type on after.
    fn text(&self, shown: &str) -> String {
        match self.first == self.last {
            true => format!("@{shown}#L{} ", self.first),
            false => format!("@{shown}#L{}-{} ", self.first, self.last),
        }
    }

    /// `at_mentioned`'s params, whose lines are 0-based.
    fn json(&self) -> Value {
        json!({
            "filePath": self.path,
            "lineStart": self.first - 1,
            "lineEnd": self.last - 1,
        })
    }

    fn at(&self, path: PathBuf) -> Self {
        Self {
            path,
            ..self.clone()
        }
    }
}

/// `file` moved from the checkout topped at `from` to the one topped at `to`.
fn rewrite(file: &Path, from: &Path, to: &Path) -> PathBuf {
    file.strip_prefix(from)
        .map_or_else(|_| file.to_path_buf(), |rel| to.join(rel))
}

/// `file` as a session running in `cwd` would name it: below `cwd` when it is.
fn shown(cwd: &Path, file: &Path) -> String {
    file.strip_prefix(cwd)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned()
}

/// Whether `moved` is missing or a different size from `file`; a size is all that is compared, so a
/// same-length edit goes unnoticed.
fn differs(file: &Path, moved: &Path) -> bool {
    let size = |p: &Path| std::fs::metadata(p).ok().map(|m| m.len());
    size(moved).is_none() || size(moved) != size(file)
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum SendTo {
    Session(String),
    New,
    NewInWorktree,
}

pub(super) struct SendRow {
    pub(super) label: String,
    pub(super) key: String,
    pub(super) detail: Option<String>,
    pub(super) kind: Option<&'static str>,
    pub(super) to: SendTo,
}

fn state_word(state: Option<ClaudeState>) -> &'static str {
    match state {
        Some(ClaudeState::Running) => "running",
        Some(ClaudeState::NeedsInput) => "needs input",
        Some(ClaudeState::Finished) => "done",
        None => "idle",
    }
}

/// The running sessions, most recently active first, then the two ways to start one.
fn picker_rows(mut sessions: Vec<LiveSession>) -> Vec<SendRow> {
    sessions.retain(|s| s.pane.is_some());
    sessions.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
    let mut rows: Vec<SendRow> = sessions
        .into_iter()
        .map(|s| SendRow {
            key: match &s.branch {
                Some(branch) => format!("{branch} {}", s.label),
                None => s.label.clone(),
            },
            kind: Some(state_word(s.state)),
            detail: s.branch,
            label: s.label,
            to: SendTo::Session(s.id),
        })
        .collect();
    for (label, to) in [
        ("New Claude Session", SendTo::New),
        ("New Claude Session in Worktree…", SendTo::NewInWorktree),
    ] {
        rows.push(SendRow {
            label: label.into(),
            key: label.into(),
            detail: None,
            kind: None,
            to,
        });
    }
    rows
}

/// The IDE client whose Claude Code runs in `target`, if one is connected.
fn client_for<T: PartialEq>(
    clients: &HashMap<u64, Option<i32>>,
    terminal_of: impl Fn(i32) -> Option<T>,
    target: &T,
) -> Option<u64> {
    let mut ids: Vec<(&u64, &Option<i32>)> = clients.iter().collect();
    ids.sort_unstable_by_key(|(id, _)| **id);
    ids.into_iter()
        .find_map(|(id, pid)| (terminal_of((*pid)?).as_ref() == Some(target)).then_some(*id))
}

/// A mention waiting for the terminal of a session that is still starting.
struct Pending {
    root: PathBuf,
    item: ItemId,
    mention: Mention,
    text: String,
    expires: Instant,
}

/// Takes out of `pending` the mentions whose time is up.
fn take_expired(pending: &mut Vec<Pending>, now: Instant) -> Vec<Pending> {
    let (gone, kept) = std::mem::take(pending)
        .into_iter()
        .partition(|p| p.expires <= now);
    *pending = kept;
    gone
}

#[derive(Default)]
pub(super) struct SendState {
    /// The selection the open picker will send.
    picking: Option<Mention>,
    /// The selection the New Worktree picker will take to the Claude session it starts.
    pub(super) worktree: Option<Mention>,
    pending: Vec<Pending>,
    retry: Option<Task<()>>,
}

enum Outcome {
    Sent,
    Waiting,
    Closed,
}

impl Shell {
    /// Opens the picker for the selection `mention` names.
    pub(super) fn pick_send_target(
        &mut self,
        mention: Mention,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ide.send.picking = Some(mention);
        self.open_palette(Mode::SendSelection, window, cx);
    }

    /// The picker's rows with the selection each one sends.
    pub(super) fn send_rows(&mut self, cx: &mut Context<Self>) -> Vec<(SendRow, Mention)> {
        let (Some(mention), Some(root)) = (self.ide.send.picking.clone(), self.active_root())
        else {
            return Vec::new();
        };
        let sessions = self.project_sessions(&root, cx);
        picker_rows(sessions)
            .into_iter()
            .map(|row| (row, mention.clone()))
            .collect()
    }

    pub(super) fn send_selection(
        &mut self,
        to: SendTo,
        mention: Mention,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match to {
            SendTo::Session(id) => self.send_to_session(&id, &mention, window, cx),
            SendTo::New => self.send_to_new_session(mention, window, cx),
            SendTo::NewInWorktree => {
                self.open_worktrees(Mode::NewWorktree, true, window, cx);
                // Set after the call, which clears a mention left by a picker nobody finished.
                self.ide.send.worktree = Some(mention);
            }
        }
    }

    /// `mention` as a session in the checkout topped at `checkout` sees it, and the warning to
    /// show when its file is missing there or differs.
    fn relocate(
        &self,
        mention: &Mention,
        checkout: &Path,
        branch: Option<&str>,
    ) -> (Mention, Option<String>) {
        let from = git::work_tree(&mention.path).unwrap_or_else(|| checkout.to_path_buf());
        if from == checkout {
            return (mention.clone(), None);
        }
        let moved = mention.at(rewrite(&mention.path, &from, checkout));
        let warning = differs(&mention.path, &moved.path).then(|| {
            let place = branch.map_or_else(|| file_label(checkout), str::to_string);
            format!("{} differs in {place}", file_label(&mention.path))
        });
        (moved, warning)
    }

    fn send_to_session(
        &mut self,
        id: &str,
        mention: &Mention,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root() else {
            return;
        };
        let running = self
            .project_sessions(&root, cx)
            .into_iter()
            .find(|s| s.id == id);
        let terminal = running
            .as_ref()
            .and_then(|s| self.find_session(s.pane?))
            .and_then(|(root, item)| match self.items.get(&(root.clone(), item)) {
                Some(ItemView::Terminal(view)) => Some((root, item, view.clone())),
                _ => None,
            });
        let (Some(session), Some((troot, item, view))) = (running, terminal) else {
            return self.transient_notice(
                "That session is not running",
                "Its terminal was closed; pick another one.",
                cx,
            );
        };
        let checkout = git::work_tree(&session.cwd).unwrap_or_else(|| session.cwd.clone());
        let (mention, warning) = self.relocate(mention, &checkout, session.branch.as_deref());
        let text = mention.text(&shown(&session.cwd, &mention.path));
        let target = (troot.clone(), item);
        let client = windows::ide(cx).and_then(|_| {
            client_for(
                &windows::ide_clients(cx),
                |pid| self.claude_terminal(pid, cx),
                &target,
            )
        });
        match client {
            Some(client) => {
                if let Some(server) = windows::ide(cx) {
                    server.notify(&[client], "at_mentioned", mention.json());
                }
            }
            None => {
                let asking = view.read(cx).claude_state() == Some(ClaudeState::NeedsInput);
                if asking || !view.read(cx).bracketed_paste() {
                    let why = match asking {
                        true => "It is asking a question; answer it, then send again.",
                        false => {
                            "Its terminal is not taking pasted text; connect Claude Code \
                                  with /ide, then send again."
                        }
                    };
                    return self.transient_notice(
                        format!("Could not send the selection to {}", session.label),
                        why,
                        cx,
                    );
                }
                view.update(cx, |v, cx| v.paste_text(&text, cx));
            }
        }
        if let Some(warning) = warning {
            self.transient_notice("Selection sent from another checkout", warning, cx);
        }
        self.transient_notice(
            format!("Sent selection to {}", session.label),
            text.trim_end().to_string(),
            cx,
        );
        self.focus_item(&troot, item, window, cx);
    }

    /// Starts `claude` in a new terminal of the active project and keeps `mention` for it until
    /// it can take it.
    pub(super) fn send_to_new_session(
        &mut self,
        mention: Mention,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((root, item)) = self.run_in_new_terminal_tab("claude".into(), window, cx) else {
            return;
        };
        let checkout = git::work_tree(&root).unwrap_or_else(|| root.clone());
        let (mention, warning) = self.relocate(&mention, &checkout, None);
        if let Some(warning) = warning {
            self.transient_notice("Selection sent from another checkout", warning, cx);
        }
        let text = mention.text(&shown(&root, &mention.path));
        self.ide.send.pending.push(Pending {
            root,
            item,
            mention,
            text,
            expires: Instant::now() + PENDING_FOR,
        });
        self.deliver_pending(cx);
    }

    /// Hands each pending mention to its terminal once Claude Code there has connected as an IDE or
    /// its session has started, and drops those that waited too long.
    pub(super) fn deliver_pending(&mut self, cx: &mut Context<Self>) {
        let mut waiting = Vec::new();
        for pending in std::mem::take(&mut self.ide.send.pending) {
            match self.try_deliver(&pending, cx) {
                Outcome::Sent => self.transient_notice(
                    "Sent selection to the new session",
                    pending.text.trim_end().to_string(),
                    cx,
                ),
                Outcome::Waiting => waiting.push(pending),
                Outcome::Closed => {}
            }
        }
        for gone in take_expired(&mut waiting, Instant::now()) {
            self.transient_notice(
                "Selection not sent",
                format!(
                    "The new Claude session was not ready within {}s; send {} again.",
                    PENDING_FOR.as_secs(),
                    file_label(&gone.mention.path)
                ),
                cx,
            );
        }
        self.ide.send.pending = waiting;
        if self.ide.send.pending.is_empty() {
            self.ide.send.retry = None;
        } else if self.ide.send.retry.is_none() {
            self.ide.send.retry = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(RETRY).await;
                    if this
                        .update(cx, |this, cx| this.deliver_pending(cx))
                        .is_err()
                    {
                        return;
                    }
                }
            }));
        }
    }

    fn try_deliver(&self, pending: &Pending, cx: &mut Context<Self>) -> Outcome {
        let Some(ItemView::Terminal(view)) = self.items.get(&(pending.root.clone(), pending.item))
        else {
            return Outcome::Closed;
        };
        let target = (pending.root.clone(), pending.item);
        let client = windows::ide(cx).and_then(|_| {
            client_for(
                &windows::ide_clients(cx),
                |pid| self.claude_terminal(pid, cx),
                &target,
            )
        });
        if let Some(client) = client {
            if let Some(server) = windows::ide(cx) {
                server.notify(&[client], "at_mentioned", pending.mention.json());
            }
            return Outcome::Sent;
        }
        let started = view
            .read(cx)
            .session()
            .is_some_and(|pane| self.review.sessions.session_of(pane).is_some());
        let ready = started
            && view.read(cx).claude_state() != Some(ClaudeState::NeedsInput)
            && view.read(cx).bracketed_paste();
        if !ready {
            return Outcome::Waiting;
        }
        view.clone()
            .update(cx, |v, cx| v.paste_text(&pending.text, cx));
        Outcome::Sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn session(id: &str, pane: Option<u64>, branch: Option<&str>, age: u64) -> LiveSession {
        LiveSession {
            id: id.into(),
            label: format!("label {id}"),
            pane,
            cwd: PathBuf::new(),
            main_root: PathBuf::new(),
            rel: PathBuf::new(),
            branch: branch.map(str::to_string),
            state: pane.map(|_| ClaudeState::Running),
            unread: false,
            todos: None,
            files: 0,
            usage: Default::default(),
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1000 - age),
        }
    }

    #[test]
    fn the_picker_lists_running_sessions_newest_first_then_the_two_new_ones() {
        let rows = picker_rows(vec![
            session("old", Some(1), None, 50),
            session("closed", None, None, 0),
            session("new", Some(2), Some("claude/fix"), 5),
        ]);
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "label new",
                "label old",
                "New Claude Session",
                "New Claude Session in Worktree…"
            ],
            "the newest comes first, where the palette preselects, and one with no terminal is left out"
        );
        assert_eq!(rows[0].to, SendTo::Session("new".into()));
        assert_eq!(rows[0].detail.as_deref(), Some("claude/fix"));
        assert_eq!(rows[0].kind, Some("running"));
        for row in &rows {
            assert!(
                row.key.ends_with(&row.label),
                "the palette highlights the label inside the key"
            );
        }
        assert_eq!(picker_rows(Vec::new()).len(), 2);
    }

    #[test]
    fn a_mention_names_a_line_or_a_range_and_drops_a_trailing_line_start() {
        let path = Path::new("/p/a.rs");
        let text = |start, end| Mention::from_selection(path, start, end).text("src/a.rs");
        assert_eq!(text((2, 0), (2, 0)), "@src/a.rs#L3 ");
        assert_eq!(text((2, 1), (2, 6)), "@src/a.rs#L3 ");
        assert_eq!(text((2, 3), (4, 1)), "@src/a.rs#L3-5 ");
        assert_eq!(text((2, 0), (5, 0)), "@src/a.rs#L3-5 ");
        assert_eq!(
            Mention::from_selection(path, (2, 3), (4, 1)).json(),
            json!({"filePath": "/p/a.rs", "lineStart": 2, "lineEnd": 4})
        );
        assert_eq!(
            Mention::from_selection(path, (0, 0), (0, 0)).json(),
            json!({"filePath": "/p/a.rs", "lineStart": 0, "lineEnd": 0})
        );
    }

    #[test]
    fn a_file_keeps_its_place_when_it_moves_between_checkouts() {
        let file = Path::new("/repo/src/a.rs");
        let main = Path::new("/repo");
        let tree = Path::new("/repo-wt/fix");
        assert_eq!(
            rewrite(file, main, tree),
            PathBuf::from("/repo-wt/fix/src/a.rs")
        );
        assert_eq!(
            rewrite(Path::new("/elsewhere/a.rs"), main, tree),
            PathBuf::from("/elsewhere/a.rs"),
            "a file outside the checkout is left as it is"
        );
        assert_eq!(shown(Path::new("/repo/src"), file), "a.rs");
        assert_eq!(
            shown(Path::new("/repo/src"), Path::new("/repo/b.rs")),
            "/repo/b.rs"
        );
    }

    #[test]
    fn a_moved_file_differs_when_missing_or_another_size() {
        let dir = std::env::temp_dir().join(format!("athena-send-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, same, longer) = (dir.join("a"), dir.join("same"), dir.join("longer"));
        std::fs::write(&a, "abc").unwrap();
        std::fs::write(&same, "xyz").unwrap();
        std::fs::write(&longer, "abcd").unwrap();
        assert!(!differs(&a, &same));
        assert!(differs(&a, &longer));
        assert!(differs(&a, &dir.join("missing")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_client_is_the_one_whose_claude_runs_in_the_target_terminal() {
        let clients = HashMap::from([(7, Some(700)), (3, Some(300)), (5, None), (9, Some(900))]);
        let terminal_of = |pid: i32| match pid {
            300 | 700 => Some("a"),
            900 => Some("b"),
            _ => None,
        };
        assert_eq!(client_for(&clients, terminal_of, &"b"), Some(9));
        assert_eq!(
            client_for(&clients, terminal_of, &"a"),
            Some(3),
            "the lowest id when two clients share a terminal"
        );
        assert_eq!(client_for(&clients, terminal_of, &"c"), None);
        assert_eq!(client_for(&HashMap::new(), terminal_of, &"a"), None);
    }

    #[test]
    fn a_pending_mention_expires_after_its_time() {
        let now = Instant::now();
        let pending = |item: u64, left: u64| Pending {
            root: PathBuf::from("/p"),
            item: ItemId(item),
            mention: Mention::from_selection(Path::new("/p/a.rs"), (0, 0), (0, 0)),
            text: String::new(),
            expires: now + Duration::from_secs(left),
        };
        let mut all = vec![pending(1, 0), pending(2, 30), pending(3, 1)];
        let gone = take_expired(&mut all, now + Duration::from_secs(1));
        let ids = |v: &[Pending]| v.iter().map(|p| p.item.0).collect::<Vec<_>>();
        assert_eq!(ids(&gone), [1, 3]);
        assert_eq!(ids(&all), [2]);
        assert!(take_expired(&mut all, now).is_empty());
        assert!(PENDING_FOR == Duration::from_secs(30));
    }
}
