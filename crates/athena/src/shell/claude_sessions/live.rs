use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use athena_proto::{PaneId, TodoInfo};
use athena_term::ClaudeState;
use athena_workspace::ItemId;
use athena_workspace::git;
use gpui::App;

use super::{SessionRow, SessionsState, short_id};
use crate::shell::Shell;
use crate::shell::item::ItemView;
use crate::transcripts::Tokens;

/// A Claude session one window knows, whether or not a terminal runs it now.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LiveSession {
    pub id: String,
    pub label: String,
    /// The terminal running it; `None` for one only a transcript or a snapshot remembers.
    pub pane: Option<PaneId>,
    pub cwd: PathBuf,
    /// The main checkout of `cwd`, so a worktree's sessions group under their repository.
    pub main_root: PathBuf,
    /// Set only when `cwd` is a linked worktree.
    pub branch: Option<String>,
    pub state: Option<ClaudeState>,
    /// It stopped or asked while nobody was looking, and has not been looked at since.
    pub unread: bool,
    /// Todos done out of all of them.
    pub todos: Option<(usize, usize)>,
    pub files: usize,
    /// By model, for pricing; `tokens()` sums them.
    pub usage: BTreeMap<String, Tokens>,
    pub modified: SystemTime,
}

impl LiveSession {
    #[allow(dead_code)] // No caller yet; the list shows cost, not a token count.
    pub fn tokens(&self) -> u64 {
        self.usage.values().map(Tokens::total).sum()
    }

    pub fn cost_label(&self, prices: &HashMap<String, crate::transcripts::Price>) -> String {
        super::cost_label(&self.usage, prices)
    }

    fn rank(&self) -> u8 {
        match self.state {
            Some(ClaudeState::NeedsInput) => 0,
            Some(ClaudeState::Finished) if self.unread => 1,
            Some(ClaudeState::Running) => 2,
            _ => 3,
        }
    }
}

/// Whether a session waits on the user: asking, or finished and not yet looked at.
fn needs_you(state: Option<ClaudeState>, unread: bool) -> bool {
    match state {
        Some(ClaudeState::NeedsInput) => true,
        Some(ClaudeState::Finished) => unread,
        _ => false,
    }
}

/// A session that needs the user, placed so the next one to visit can be told.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Waiting {
    /// 2 while it asks, 1 once it has finished unseen.
    pub urgency: u8,
    pub window: usize,
    pub project: usize,
    pub root: PathBuf,
    pub item: ItemId,
}

/// The session after `here` in the order questions, then unseen finishes, each by window, project
/// and tab; the first when `here` is not one of them.
pub(crate) fn next_waiting(
    waiting: &[Waiting],
    here: Option<(usize, usize, ItemId)>,
) -> Option<&Waiting> {
    let mut order: Vec<&Waiting> = waiting.iter().collect();
    order.sort_by_key(|w| (std::cmp::Reverse(w.urgency), w.window, w.project, w.item));
    let current = here.and_then(|h| {
        order
            .iter()
            .position(|w| (w.window, w.project, w.item) == h)
    });
    order
        .get(current.map_or(0, |at| (at + 1) % order.len()))
        .copied()
}

/// The project roots whose sessions to read: the active one and those of the bound sessions.
pub(super) fn row_roots(
    active: Option<&Path>,
    bound: impl IntoIterator<Item = PathBuf>,
) -> BTreeSet<PathBuf> {
    bound
        .into_iter()
        .chain(active.map(Path::to_path_buf))
        .collect()
}

/// Sessions that need the user: waiting for an answer, or finished and not yet seen.
#[allow(dead_code)] // The windows count from their terminals instead; this one defines the rule over a list.
pub(crate) fn attention_count(sessions: &[LiveSession]) -> usize {
    sessions
        .iter()
        .filter(|s| needs_you(s.state, s.unread))
        .count()
}

/// A session stopped or asked while nobody looked, and the terminal has not been looked at since;
/// the bell is what records that, and a running session has not stopped yet.
fn unread(state: Option<ClaudeState>, bell: bool) -> bool {
    bell && matches!(state, Some(s) if s != ClaudeState::Running)
}

/// Sessions that need you first, then the running ones, then the rest by recency.
fn sort(sessions: &mut [LiveSession]) {
    sessions.sort_by(|a, b| {
        (a.rank(), std::cmp::Reverse(a.modified), &a.id).cmp(&(
            b.rank(),
            std::cmp::Reverse(b.modified),
            &b.id,
        ))
    });
}

/// The main checkout `cwd` belongs to, and its branch when `cwd` is a linked worktree.
fn locate(cwd: &Path) -> (PathBuf, Option<String>) {
    let top = git::work_tree(cwd).unwrap_or_else(|| cwd.to_path_buf());
    match git::main_worktree(&top) {
        Some(main) => {
            let branch = branch_of(&top);
            (main, branch)
        }
        None => (top, None),
    }
}

/// Read from HEAD on disk, as this must not start git.
fn branch_of(tree: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git::git_dirs(tree)?.git_dir.join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string)
}

fn todo_counts(todos: &[TodoInfo]) -> Option<(usize, usize)> {
    let done = todos.iter().filter(|t| t.status == "completed").count();
    (!todos.is_empty()).then_some((done, todos.len()))
}

/// What a terminal running a session reports.
struct Running {
    pane: PaneId,
    root: PathBuf,
    state: Option<ClaudeState>,
    /// The terminal's bell, which a notice raised while nobody watched also sets.
    bell: bool,
}

impl SessionsState {
    pub(in crate::shell) fn forget_places(&mut self) {
        self.places.get_mut().clear();
    }

    /// `locate(cwd)`, remembered until a hook or a reload clears it.
    pub(super) fn place(&self, cwd: &Path) -> (PathBuf, Option<String>) {
        let mut places = self.places.borrow_mut();
        places
            .entry(cwd.to_path_buf())
            .or_insert_with(|| locate(cwd))
            .clone()
    }
}

impl Shell {
    /// The project root, tab and terminal view of `pane`, if this window holds it.
    fn pane_view(&self, pane: PaneId) -> Option<(PathBuf, ItemId, &ItemView)> {
        let (root, item) = self.find_session(pane)?;
        let view = self.items.get(&(root.clone(), item))?;
        matches!(view, ItemView::Terminal(_)).then_some((root, item, view))
    }

    /// This window's sessions that need you, from its terminals alone so it is cheap per frame.
    fn needing_you<'a>(&'a self, cx: &'a App) -> impl Iterator<Item = (PathBuf, ItemId, u8)> + 'a {
        self.review
            .sessions
            .panes
            .keys()
            .filter_map(|pane| self.pane_view(*pane))
            .filter_map(|(root, item, view)| {
                let state = view.claude_state(cx);
                let urgency = match state {
                    Some(ClaudeState::NeedsInput) => 2,
                    _ if needs_you(state, unread(state, view.has_bell(cx))) => 1,
                    _ => return None,
                };
                Some((root, item, urgency))
            })
    }

    pub(crate) fn attention_here(&self, cx: &App) -> usize {
        self.needing_you(cx).count()
    }

    /// The project roots of this window's sessions that need you, one per session.
    pub(in crate::shell) fn attention_roots(&self, cx: &App) -> Vec<PathBuf> {
        self.needing_you(cx).map(|(root, ..)| root).collect()
    }

    /// This window's sessions that need you, as window number `window` of those open.
    pub(in crate::shell) fn waiting_here(&self, window: usize, cx: &App) -> Vec<Waiting> {
        self.needing_you(cx)
            .filter_map(|(root, item, urgency)| {
                let project = self
                    .workspace
                    .projects
                    .iter()
                    .position(|p| p.root == root)?;
                Some(Waiting {
                    urgency,
                    window,
                    project,
                    root,
                    item,
                })
            })
            .collect()
    }

    /// The project roots to read sessions for: the active one and each a bound session runs in.
    pub(in crate::shell) fn session_row_roots(&self) -> BTreeSet<PathBuf> {
        let bound = self
            .review
            .sessions
            .panes
            .keys()
            .filter_map(|pane| self.find_session(*pane).map(|(root, _)| root));
        row_roots(
            self.workspace.active_project().map(|p| p.root.as_path()),
            bound,
        )
    }

    /// One entry per session this window knows: those its terminals run, then its project's recent ones.
    pub(crate) fn live_sessions(&self, cx: &App) -> Vec<LiveSession> {
        let state = &self.review.sessions;
        let mut running: HashMap<&str, Running> = HashMap::new();
        for (pane, id) in &state.panes {
            let Some((root, _, view)) = self.pane_view(*pane) else {
                continue;
            };
            running.insert(
                id,
                Running {
                    pane: *pane,
                    root,
                    state: view.claude_state(cx),
                    bell: view.has_bell(cx),
                },
            );
        }
        // The other projects' rows come last, so a session's own project wins over one that only
        // left edits under another's folder.
        let rows: HashMap<&str, &SessionRow> = state
            .rows
            .iter()
            .chain(state.elsewhere.iter())
            .map(|r| (r.id.as_str(), r))
            .collect();
        let ids = running
            .keys()
            .copied()
            .chain(rows.keys().copied().filter(|id| !running.contains_key(id)));
        let now = SystemTime::now();
        let mut out: Vec<LiveSession> = ids
            .map(|id| {
                let (row, term) = (rows.get(id), running.get(id));
                let cwd = state
                    .cwds
                    .get(id)
                    .or(term.map(|t| &t.root))
                    .or(row.map(|r| &r.root))
                    .or(state.root.as_ref())
                    .cloned()
                    .unwrap_or_default();
                let (main_root, branch) = state.place(&cwd);
                let live = state.live.get(id);
                LiveSession {
                    id: id.to_string(),
                    label: row
                        .map_or_else(|| format!("Session {}", short_id(id)), |r| r.label.clone()),
                    pane: term.map(|t| t.pane),
                    cwd,
                    main_root,
                    branch,
                    state: term.and_then(|t| t.state),
                    unread: term.is_some_and(|t| unread(t.state, t.bell)),
                    todos: live.and_then(|l| todo_counts(&l.todos)),
                    files: row.map_or(0, |r| r.files.len()),
                    usage: row.map(|r| r.tokens.clone()).unwrap_or_default(),
                    // A live session with no transcript row yet is current, not the oldest.
                    modified: row.map_or_else(
                        || {
                            if term.is_some() {
                                now
                            } else {
                                SystemTime::UNIX_EPOCH
                            }
                        },
                        |r| r.modified,
                    ),
                }
            })
            .collect();
        sort(&mut out);
        out
    }

    /// Sessions needing you across all open windows, this one included.
    pub(crate) fn attention_total(&self, cx: &App) -> usize {
        self.attention_here(cx) + crate::shell::windows::attention_elsewhere(self.window_handle, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::super::LivePlan;
    use super::*;
    use std::time::Duration;

    fn session(id: &str, state: Option<ClaudeState>, unread: bool, age: u64) -> LiveSession {
        LiveSession {
            id: id.into(),
            label: id.into(),
            pane: state.map(|_| 1),
            cwd: PathBuf::new(),
            main_root: PathBuf::new(),
            branch: None,
            state,
            unread,
            todos: None,
            files: 0,
            usage: BTreeMap::new(),
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1000 - age),
        }
    }

    fn mixed() -> Vec<LiveSession> {
        use ClaudeState::*;
        vec![
            session("idle-new", None, false, 1),
            session("running", Some(Running), false, 9),
            session("seen", Some(Finished), false, 2),
            session("asking-old", Some(NeedsInput), true, 8),
            session("unread", Some(Finished), true, 7),
            session("asking-new", Some(NeedsInput), false, 3),
            session("idle-old", None, false, 5),
        ]
    }

    #[test]
    fn attention_counts_questions_and_unseen_finishes_only() {
        assert_eq!(attention_count(&mixed()), 3);
        assert_eq!(attention_count(&[]), 0);
        let quiet = [session("r", Some(ClaudeState::Running), true, 0)];
        assert_eq!(
            attention_count(&quiet),
            0,
            "a running session is never unread"
        );
    }

    #[test]
    fn questions_come_first_then_unseen_finishes_then_running_then_by_recency() {
        let mut all = mixed();
        sort(&mut all);
        let ids: Vec<&str> = all.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "asking-new",
                "asking-old",
                "unread",
                "running",
                "idle-new",
                "seen",
                "idle-old"
            ]
        );
    }

    fn waiting(urgency: u8, window: usize, project: usize, item: u64) -> Waiting {
        Waiting {
            urgency,
            window,
            project,
            root: PathBuf::new(),
            item: ItemId(item),
        }
    }

    fn visited(
        waiting: &[Waiting],
        from: Option<(usize, usize, u64)>,
        steps: usize,
    ) -> Vec<(u8, usize, usize, u64)> {
        let mut here = from.map(|(w, p, i)| (w, p, ItemId(i)));
        let mut seen = Vec::new();
        for _ in 0..steps {
            let next = next_waiting(waiting, here).unwrap();
            seen.push((next.urgency, next.window, next.project, next.item.0));
            here = Some((next.window, next.project, next.item));
        }
        seen
    }

    #[test]
    fn cycling_visits_questions_before_unseen_finishes_across_windows_and_wraps() {
        let all = [
            waiting(1, 0, 0, 5),
            waiting(2, 1, 0, 2),
            waiting(2, 0, 1, 3),
            waiting(1, 1, 0, 1),
            waiting(2, 0, 0, 9),
        ];
        let order = [
            (2, 0, 0, 9),
            (2, 0, 1, 3),
            (2, 1, 0, 2),
            (1, 0, 0, 5),
            (1, 1, 0, 1),
        ];
        assert_eq!(
            visited(&all, None, 6),
            [order.as_slice(), &[order[0]]].concat()
        );
        assert_eq!(
            visited(&all, Some((0, 0, 4)), 1),
            [order[0]],
            "from a tab that is not waiting, the most urgent first"
        );
        assert_eq!(next_waiting(&[], None), None);
        let one = [waiting(2, 0, 0, 1)];
        assert_eq!(visited(&one, Some((0, 0, 1)), 2), [(2, 0, 0, 1); 2]);
    }

    #[test]
    fn rows_are_read_for_the_active_project_and_every_project_a_session_runs_in() {
        let roots = |active: Option<&str>, bound: &[&str]| {
            row_roots(active.map(Path::new), bound.iter().map(PathBuf::from))
                .into_iter()
                .collect::<Vec<_>>()
        };
        let path = |s: &str| PathBuf::from(s);
        assert_eq!(roots(Some("/a"), &[]), [path("/a")]);
        assert_eq!(
            roots(Some("/a"), &["/b", "/a", "/b", "/a-wt"]),
            [path("/a"), path("/a-wt"), path("/b")],
            "each root once, the active one included"
        );
        assert_eq!(roots(None, &["/b"]), [path("/b")]);
        assert!(roots(None, &[]).is_empty());
    }

    fn sh(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("/usr/bin/git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=T", "-c", "user.email=t@x"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    }

    #[test]
    fn a_worktree_session_belongs_to_the_main_checkout_and_names_its_branch() {
        if !git::available() {
            return;
        }
        let main = std::env::temp_dir().join(format!("athena-live-{}", std::process::id()));
        let tree = main.with_extension("wt");
        let _ = std::fs::remove_dir_all(&main);
        let _ = std::fs::remove_dir_all(&tree);
        std::fs::create_dir_all(&main).unwrap();
        let main = main.canonicalize().unwrap();
        sh(&main, &["init", "-q", "-b", "main"]);
        sh(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
        sh(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "claude/fix",
                tree.to_str().unwrap(),
            ],
        );
        let tree = tree.canonicalize().unwrap();
        let inside = tree.join("src");
        std::fs::create_dir_all(&inside).unwrap();

        let branch = Some("claude/fix".to_string());
        assert_eq!(locate(&tree), (main.clone(), branch.clone()));
        assert_eq!(locate(&inside), (main.clone(), branch.clone()));
        assert_eq!(locate(&main), (main.clone(), None));
        let sub = main.join("crates");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(locate(&sub), (main.clone(), None));
        assert_eq!(locate(&main.join("nowhere")), (main.clone(), None));

        let nested = tree.join("vendor").join("dep");
        std::fs::create_dir_all(&nested).unwrap();
        sh(&nested, &["init", "-q", "-b", "main"]);
        assert_eq!(locate(&nested), (nested.clone(), None));

        let loose = main.with_extension("loose");
        std::fs::create_dir_all(&loose).unwrap();
        assert_eq!(locate(&loose), (loose.clone(), None));

        sh(&tree, &["switch", "-q", "--detach"]);
        assert_eq!(
            locate(&tree),
            (main.clone(), None),
            "a detached HEAD has no branch"
        );

        sh(
            &main,
            &["worktree", "remove", "--force", tree.to_str().unwrap()],
        );
        std::fs::remove_dir_all(&main).unwrap();
        std::fs::remove_dir_all(&loose).unwrap();
    }

    #[test]
    fn a_session_bound_in_one_window_is_dropped_from_the_others() {
        let mut windows = [
            SessionsState::default(),
            SessionsState::default(),
            SessionsState::default(),
        ];
        windows[0].bind(1, "x".into());
        windows[1].bind(2, "x".into());
        windows[1].bind(3, "y".into());
        windows[2].bind(4, "z".into());

        let holder = 0;
        let dropped: Vec<bool> = windows
            .iter_mut()
            .enumerate()
            .filter(|(i, _)| *i != holder)
            .map(|(_, w)| w.unbind("x"))
            .collect();
        assert_eq!(dropped, [true, false]);
        assert_eq!(windows[0].panes.get(&1).map(String::as_str), Some("x"));
        assert!(!windows[1].panes.contains_key(&2));
        assert_eq!(windows[1].panes.get(&3).map(String::as_str), Some("y"));
        assert_eq!(windows[2].panes.get(&4).map(String::as_str), Some("z"));
    }

    #[test]
    fn todos_count_the_finished_ones() {
        let todo = |status: &str| TodoInfo {
            content: String::new(),
            status: status.into(),
            active_form: None,
        };
        assert_eq!(todo_counts(&[]), None);
        assert_eq!(
            todo_counts(&[todo("completed"), todo("pending"), todo("completed")]),
            Some((2, 3))
        );
    }

    #[test]
    fn only_a_session_that_has_stopped_or_asked_is_unread() {
        use ClaudeState::*;
        assert!(!unread(Some(Running), true));
        assert!(unread(Some(Finished), true));
        assert!(unread(Some(NeedsInput), true));
        assert!(!unread(Some(Finished), false));
        assert!(!unread(None, true));
    }

    #[test]
    fn moved_panes_take_their_sessions_and_leave_the_rest() {
        let plan = || LivePlan {
            todos: Vec::new(),
            plan: Some("p".into()),
        };
        let mut from = SessionsState::default();
        from.bind(1, "a".into());
        from.bind(2, "b".into());
        from.cwds.insert("a".into(), "/x".into());
        from.cwds.insert("b".into(), "/y".into());
        from.live.insert("a".into(), plan());
        from.live.insert("b".into(), plan());

        let moved = from.split_off(&[1, 9]);
        assert_eq!(from.panes.len(), 1);
        assert_eq!(from.panes.get(&2).map(String::as_str), Some("b"));
        assert!(!from.cwds.contains_key("a") && from.cwds.contains_key("b"));
        assert!(!from.live.contains_key("a") && from.live.contains_key("b"));

        let mut parked = SessionsState::default();
        parked.absorb(moved);
        let mut to = SessionsState::default();
        to.absorb(parked.split_off(&[1]));
        assert!(parked.panes.is_empty() && parked.cwds.is_empty() && parked.live.is_empty());
        assert_eq!(to.panes.get(&1).map(String::as_str), Some("a"));
        assert_eq!(to.cwds.get("a"), Some(&PathBuf::from("/x")));
        assert!(to.live.contains_key("a") && to.panes.len() == 1);
    }
}
