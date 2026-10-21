use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use athena_proto::PaneId;
use athena_term::{ClaudeState, TerminalView};
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
const COMPARE_UP_TO: u64 = 2 * 1024 * 1024;

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

/// Whether `moved` is missing or unlike `file`: by contents up to `COMPARE_UP_TO` bytes, by size above.
fn differs(file: &Path, moved: &Path) -> bool {
    let size = |p: &Path| std::fs::metadata(p).ok().map(|m| m.len());
    let (Some(a), Some(b)) = (size(file), size(moved)) else {
        return true;
    };
    if a != b {
        return true;
    }
    if a > COMPARE_UP_TO {
        return false;
    }
    match (std::fs::read(file), std::fs::read(moved)) {
        (Ok(x), Ok(y)) => x != y,
        _ => true,
    }
}

/// Why Claude's prompt cannot take pasted text now.
#[derive(Debug, PartialEq)]
enum Refusal {
    NotClaude,
    Asking,
    NoBracketedPaste,
}

impl Refusal {
    fn why(&self) -> &'static str {
        match self {
            Self::NotClaude => "Claude is no longer running in that terminal.",
            Self::Asking => "It is asking a question; answer it, then send again.",
            Self::NoBracketedPaste => {
                "Its terminal is not taking pasted text; connect Claude Code with /ide, then send again."
            }
        }
    }
}

/// What stops a paste into a terminal: a plain shell would run it, a dialog would take it as an
/// answer, and without bracketed paste it would be typed.
fn paste_refusal(
    is_claude: bool,
    state: Option<ClaudeState>,
    bracketed_paste: bool,
) -> Option<Refusal> {
    if !is_claude {
        Some(Refusal::NotClaude)
    } else if state == Some(ClaudeState::NeedsInput) {
        Some(Refusal::Asking)
    } else if !bracketed_paste {
        Some(Refusal::NoBracketedPaste)
    } else {
        None
    }
}

fn terminal_refusal(view: &TerminalView) -> Option<Refusal> {
    paste_refusal(
        view.is_claude(),
        view.claude_state(),
        view.bracketed_paste(),
    )
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum SendTo {
    Terminal(PaneId),
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
    sessions.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.pane.cmp(&b.pane))
    });
    let mut rows: Vec<SendRow> = Vec::new();
    for s in sessions {
        let Some(pane) = s.pane else {
            continue;
        };
        // A terminal no hook has named is told apart by its project.
        let detail = s
            .branch
            .or_else(|| s.id.is_empty().then(|| file_label(&s.cwd)));
        rows.push(SendRow {
            key: match &detail {
                Some(detail) => format!("{detail} {}", s.label),
                None => s.label.clone(),
            },
            kind: Some(state_word(s.state)),
            detail,
            label: s.label,
            to: SendTo::Terminal(pane),
        });
    }
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

/// A mention waiting for the terminal of a session that is still starting; the pane, once the
/// terminal has one, finds it again if its tab moves.
struct Pending {
    root: PathBuf,
    item: ItemId,
    pane: Option<PaneId>,
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
            SendTo::Terminal(pane) => self.send_to_terminal(pane, &mention, window, cx),
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

    fn send_to_terminal(
        &mut self,
        pane: PaneId,
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
            .find(|s| s.pane == Some(pane));
        let terminal = self.find_session(pane).and_then(|(root, item)| {
            match self.items.get(&(root.clone(), item)) {
                Some(ItemView::Terminal(view)) => Some((root, item, view.clone())),
                _ => None,
            }
        });
        let (Some(session), Some((troot, item, view))) = (running, terminal) else {
            return self.transient_notice(
                "Claude is not running there",
                "Its terminal was closed or left Claude; pick another one.",
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
                if let Some(refusal) = terminal_refusal(view.read(cx)) {
                    return self.transient_notice(
                        format!("Could not send the selection to {}", session.label),
                        refusal.why(),
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
            pane: None,
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
        for mut pending in std::mem::take(&mut self.ide.send.pending) {
            match self.try_deliver(&mut pending, cx) {
                Outcome::Sent => self.transient_notice(
                    "Sent selection to the new session",
                    pending.text.trim_end().to_string(),
                    cx,
                ),
                Outcome::Waiting => waiting.push(pending),
                Outcome::Closed => self.transient_notice(
                    "Selection not sent",
                    format!(
                        "The new Claude session's terminal was closed; send {} again.",
                        file_label(&pending.mention.path)
                    ),
                    cx,
                ),
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

    fn try_deliver(&self, pending: &mut Pending, cx: &mut Context<Self>) -> Outcome {
        let here = (pending.root.clone(), pending.item);
        let found = match self.items.get(&here) {
            Some(_) => Some(here),
            None => pending.pane.and_then(|pane| self.find_session(pane)),
        };
        let Some((root, item)) = found else {
            return Outcome::Closed;
        };
        let Some(ItemView::Terminal(view)) = self.items.get(&(root.clone(), item)) else {
            return Outcome::Closed;
        };
        (pending.root, pending.item) = (root.clone(), item);
        pending.pane = view.read(cx).session().or(pending.pane);
        let target = (root, item);
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
        if !started || terminal_refusal(view.read(cx)).is_some() {
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
        let mut unbound = session("", Some(3), None, 20);
        unbound.label = "Claude".into();
        unbound.cwd = PathBuf::from("/code/athena");
        let rows = picker_rows(vec![
            session("old", Some(1), None, 50),
            session("closed", None, None, 0),
            unbound,
            session("new", Some(2), Some("claude/fix"), 5),
        ]);
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "label new",
                "Claude",
                "label old",
                "New Claude Session",
                "New Claude Session in Worktree…"
            ],
            "the newest comes first, where the palette preselects, and one with no terminal is left out"
        );
        assert_eq!(rows[1].to, SendTo::Terminal(3));
        assert_eq!(
            rows[1].detail.as_deref(),
            Some("athena"),
            "an unbound Claude is told apart by its project"
        );
        assert_eq!(rows[0].to, SendTo::Terminal(2));
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
    fn a_moved_file_differs_when_missing_or_unlike_by_contents_up_to_a_size() {
        let dir = std::env::temp_dir().join(format!("athena-send-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let a = file("a", b"abc");
        assert!(!differs(&a, &file("copy", b"abc")));
        assert!(differs(&a, &file("same-size", b"abd")));
        assert!(differs(&a, &file("longer", b"abcd")));
        assert!(differs(&a, &dir.join("missing")));
        let big = |last: u8| {
            let mut bytes = vec![b'x'; COMPARE_UP_TO as usize + 1];
            bytes[0] = last;
            bytes
        };
        assert!(
            !differs(&file("big", &big(b'a')), &file("big-same-size", &big(b'b'))),
            "above the cap only the size is compared"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_paste_is_refused_for_a_shell_a_question_or_a_terminal_that_would_type_it() {
        use ClaudeState::*;
        let refusal = paste_refusal;
        assert_eq!(refusal(false, None, true), Some(Refusal::NotClaude));
        assert_eq!(
            refusal(false, Some(NeedsInput), false),
            Some(Refusal::NotClaude),
            "a plain shell is the first thing checked"
        );
        assert_eq!(refusal(true, Some(NeedsInput), true), Some(Refusal::Asking));
        assert_eq!(
            refusal(true, Some(Running), false),
            Some(Refusal::NoBracketedPaste)
        );
        assert_eq!(refusal(true, None, true), None, "an idle Claude takes it");
        assert_eq!(refusal(true, Some(Finished), true), None);
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
            pane: None,
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
