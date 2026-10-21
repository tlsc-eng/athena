use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use athena_proto::{PaneId, TodoInfo};
use athena_term::ClaudeState;
use athena_workspace::ItemId;
use athena_workspace::git;
use gpui::{App, Context};

use super::{SessionRow, SessionsState, pick_row, short_id};
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
    /// `cwd` below the top of its own work tree, so the same package matches across worktrees.
    pub rel: PathBuf,
    /// Set only when `cwd` is a linked worktree.
    pub branch: Option<String>,
    pub state: Option<ClaudeState>,
    /// It stopped or asked while nobody was looking, and has not been looked at since.
    pub unread: bool,
    /// Todos done out of all of them.
    pub todos: Option<(usize, usize)>,
    pub files: usize,
    /// By model, for pricing.
    pub usage: BTreeMap<String, Tokens>,
    pub modified: SystemTime,
}

impl LiveSession {
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

/// How urgently a terminal wants you: 2 while it asks, 1 once it has finished unseen.
fn urgency(state: Option<ClaudeState>, bell: bool) -> Option<u8> {
    match state {
        Some(ClaudeState::NeedsInput) => Some(2),
        _ => needs_you(state, unread(state, bell)).then_some(1),
    }
}

/// The terminals, as (project root, tab, Claude state, bell), that need you; bound to a session or not.
fn needing(
    terminals: impl IntoIterator<Item = (PathBuf, ItemId, Option<ClaudeState>, bool)>,
) -> Vec<(PathBuf, ItemId, u8)> {
    terminals
        .into_iter()
        .filter_map(|(root, item, state, bell)| Some((root, item, urgency(state, bell)?)))
        .collect()
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

/// Where a folder sits among the checkouts of a repository.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Place {
    /// The main checkout the folder belongs to.
    pub main_root: PathBuf,
    /// Set only when the folder is in a linked worktree.
    pub branch: Option<String>,
    /// The folder below the top of its own work tree.
    pub rel: PathBuf,
}

fn locate(cwd: &Path) -> Place {
    let top = git::work_tree(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let rel = cwd
        .strip_prefix(&top)
        .unwrap_or(Path::new(""))
        .to_path_buf();
    match git::main_worktree(&top) {
        Some(main) => Place {
            branch: branch_of(&top),
            main_root: main,
            rel,
        },
        None => Place {
            main_root: top,
            branch: None,
            rel,
        },
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

/// A terminal whose foreground program is Claude, whether or not a hook bound a session to it.
struct ClaudeTerminal {
    root: PathBuf,
    pane: PaneId,
    label: String,
    state: Option<ClaudeState>,
    input: Option<Instant>,
    output: Option<Instant>,
}

/// `sessions` narrowed to those whose terminal runs Claude now, plus an entry for each such
/// terminal no session is bound to, all dated by the terminal's own last activity.
fn with_terminals(
    mut sessions: Vec<LiveSession>,
    claude: &[ClaudeTerminal],
    place: impl Fn(&Path) -> Place,
    now: SystemTime,
    at: Instant,
) -> Vec<LiveSession> {
    sessions.retain(|s| s.pane.is_some_and(|p| claude.iter().any(|c| c.pane == p)));
    for c in claude {
        if sessions.iter().any(|s| s.pane == Some(c.pane)) {
            continue;
        }
        let place = place(&c.root);
        sessions.push(LiveSession {
            id: String::new(),
            label: c.label.clone(),
            pane: Some(c.pane),
            cwd: c.root.clone(),
            main_root: place.main_root,
            rel: place.rel,
            branch: place.branch,
            state: c.state,
            unread: false,
            todos: None,
            files: 0,
            usage: BTreeMap::new(),
            modified: SystemTime::UNIX_EPOCH,
        });
    }
    // Typed-in terminals outrank the rest however busy those are, and a restart forgets typing.
    let floor = claude
        .iter()
        .filter(|c| c.input.is_none())
        .filter_map(|c| c.output)
        .min();
    for s in &mut sessions {
        let Some(c) = claude.iter().find(|c| Some(c.pane) == s.pane) else {
            continue;
        };
        match (c.input, c.output.zip(floor)) {
            (Some(input), _) => s.modified = now - at.saturating_duration_since(input),
            (None, Some((output, floor))) => {
                s.modified = SystemTime::UNIX_EPOCH + output.saturating_duration_since(floor);
            }
            (None, None) => {}
        }
    }
    sessions
}

/// What a terminal running a session reports.
struct Running {
    pane: PaneId,
    root: PathBuf,
    state: Option<ClaudeState>,
    /// The terminal's bell, which a notice raised while nobody watched also sets.
    bell: bool,
}

/// `locate` for each of `cwds`, which reads the disk and so is for a background thread.
pub(super) fn locate_all(cwds: &BTreeSet<PathBuf>) -> HashMap<PathBuf, Place> {
    cwds.iter().map(|cwd| (cwd.clone(), locate(cwd))).collect()
}

impl SessionsState {
    /// Where `cwd` was last found to belong; until a load has looked, it is its own main checkout.
    pub(super) fn place(&self, cwd: &Path) -> Place {
        self.places.get(cwd).cloned().unwrap_or_else(|| Place {
            main_root: cwd.to_path_buf(),
            branch: None,
            rel: PathBuf::new(),
        })
    }
}

impl Shell {
    /// The project root, tab and terminal view of `pane`, if this window holds it.
    fn pane_view(&self, pane: PaneId) -> Option<(PathBuf, ItemId, &ItemView)> {
        let (root, item) = self.find_session(pane)?;
        let view = self.items.get(&(root.clone(), item))?;
        matches!(view, ItemView::Terminal(_)).then_some((root, item, view))
    }

    /// This window's terminals that need you, whether or not a session is bound to them, so it is
    /// cheap per frame; bindings are lost with Athena, and the shells outlive it.
    fn needing_you(&self, cx: &App) -> Vec<(PathBuf, ItemId, u8)> {
        let terminals = self
            .items
            .iter()
            .filter(|(_, view)| matches!(view, ItemView::Terminal(_)));
        needing(terminals.map(|((root, item), view)| {
            (
                root.clone(),
                *item,
                view.claude_state(cx),
                view.has_bell(cx),
            )
        }))
    }

    pub(crate) fn attention_here(&self, cx: &App) -> usize {
        self.needing_you(cx).len()
    }

    /// The project roots of this window's terminals that need you, one per terminal.
    pub(in crate::shell) fn attention_roots(&self, cx: &App) -> Vec<PathBuf> {
        self.needing_you(cx)
            .into_iter()
            .map(|(root, ..)| root)
            .collect()
    }

    /// This window's sessions that need you, as window number `window` of those open.
    pub(in crate::shell) fn waiting_here(&self, window: usize, cx: &App) -> Vec<Waiting> {
        self.needing_you(cx)
            .into_iter()
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

    /// The project roots to read sessions for: the active one and each folder a bound session ran
    /// in, as Claude files its transcripts by that.
    pub(in crate::shell) fn session_row_roots(&self) -> BTreeSet<PathBuf> {
        let state = &self.review.sessions;
        let bound = state.panes.iter().filter_map(|(pane, id)| {
            let terminal = || self.find_session(*pane).map(|(root, _)| root);
            state.cwds.get(id).cloned().or_else(terminal)
        });
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
        let mut rows: HashMap<&str, Vec<&SessionRow>> = HashMap::new();
        for row in state.rows.iter().chain(state.elsewhere.iter()) {
            rows.entry(row.id.as_str()).or_default().push(row);
        }
        let ids = running
            .keys()
            .copied()
            .chain(rows.keys().copied().filter(|id| !running.contains_key(id)));
        let now = SystemTime::now();
        let mut out: Vec<LiveSession> = ids
            .map(|id| {
                let term = running.get(id);
                let started_in = state.cwds.get(id).or(term.map(|t| &t.root));
                let row = rows.get(id).and_then(|found| {
                    pick_row(
                        found.iter().copied(),
                        started_in.map(PathBuf::as_path),
                        state.root.as_deref(),
                    )
                });
                let cwd = started_in
                    .or(row.map(|r| &r.root))
                    .or(state.root.as_ref())
                    .cloned()
                    .unwrap_or_default();
                let place = state.place(&cwd);
                let live = state.live.get(id);
                LiveSession {
                    id: id.to_string(),
                    label: row
                        .map_or_else(|| format!("Session {}", short_id(id)), |r| r.label.clone()),
                    pane: term.map(|t| t.pane),
                    cwd,
                    main_root: place.main_root,
                    rel: place.rel,
                    branch: place.branch,
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

    /// The Claude terminals in `root`'s project and worktrees, as the Sessions list counts them,
    /// each enriched by its session when a hook bound one; the order is left to the caller.
    pub(in crate::shell) fn project_sessions(
        &mut self,
        root: &Path,
        cx: &mut Context<Shell>,
    ) -> Vec<LiveSession> {
        self.refresh_sessions(root, cx);
        let claude: Vec<ClaudeTerminal> = self
            .items
            .iter()
            .filter_map(|((root, _), view)| {
                let ItemView::Terminal(view) = view else {
                    return None;
                };
                let view = view.read(cx);
                let pane = view.session().filter(|_| view.is_claude())?;
                Some(ClaudeTerminal {
                    root: root.clone(),
                    pane,
                    label: view.label(),
                    state: view.claude_state(),
                    input: view.last_input(),
                    output: view.last_activity(),
                })
            })
            .collect();
        let mut sessions = self.live_sessions(cx);
        // Read from disk now, as the list's own background load may not have run.
        let unplaced: BTreeSet<PathBuf> = sessions
            .iter()
            .map(|s| s.cwd.clone())
            .chain(claude.iter().map(|c| c.root.clone()))
            .chain([root.to_path_buf()])
            .filter(|cwd| !self.review.sessions.places.contains_key(cwd))
            .collect();
        if !unplaced.is_empty() {
            self.review.sessions.places.extend(locate_all(&unplaced));
            sessions = self.live_sessions(cx);
        }
        let state = &self.review.sessions;
        let sessions = with_terminals(
            sessions,
            &claude,
            |cwd| state.place(cwd),
            SystemTime::now(),
            Instant::now(),
        );
        let active = state.place(root);
        sessions
            .into_iter()
            .filter(|s| super::list::in_project(s, &active))
            .collect()
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
            rel: PathBuf::new(),
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
    fn a_typed_in_terminal_outranks_a_never_typed_one_however_fresh_its_output() {
        let (now, at) = (SystemTime::now(), Instant::now());
        let terminal = |pane: PaneId, input: Option<u64>, output: u64| ClaudeTerminal {
            root: PathBuf::from("/p"),
            pane,
            label: "Claude".into(),
            state: None,
            input: input.map(|s| at - Duration::from_secs(s)),
            output: Some(at - Duration::from_secs(output)),
        };
        let place = |cwd: &Path| Place {
            main_root: cwd.to_path_buf(),
            branch: None,
            rel: PathBuf::new(),
        };
        let claude = [
            terminal(1, Some(600), 0),
            terminal(2, None, 1),
            terminal(3, None, 30),
        ];
        let listed = with_terminals(Vec::new(), &claude, place, now, at);
        let modified = |pane: PaneId| {
            listed
                .iter()
                .find(|s| s.pane == Some(pane))
                .unwrap()
                .modified
        };
        assert!(modified(1) > modified(2), "older typing beats newer output");
        assert!(modified(2) > modified(3), "without typing, output decides");
    }

    #[test]
    fn claude_terminals_are_listed_bound_or_not_and_dated_by_their_activity() {
        let (now, at) = (SystemTime::now(), Instant::now());
        let terminal = |pane: PaneId, label: &str, secs: u64| ClaudeTerminal {
            root: PathBuf::from("/p"),
            pane,
            label: label.into(),
            state: None,
            input: Some(at - Duration::from_secs(secs)),
            output: None,
        };
        let place = |cwd: &Path| Place {
            main_root: cwd.to_path_buf(),
            branch: None,
            rel: PathBuf::new(),
        };
        let bound = vec![
            session("quit", Some(ClaudeState::Running), false, 0),
            session("live", Some(ClaudeState::Running), false, 0),
        ];
        let mut bound = bound;
        bound[0].pane = Some(1);
        bound[1].pane = Some(2);
        let claude = [terminal(2, "Claude", 10), terminal(3, "Claude", 4)];
        let listed = with_terminals(bound, &claude, place, now, at);
        let found: Vec<(&str, Option<PaneId>)> =
            listed.iter().map(|s| (s.id.as_str(), s.pane)).collect();
        assert_eq!(
            found,
            [("live", Some(2)), ("", Some(3))],
            "a bound session whose terminal left Claude is dropped, an unbound Claude kept"
        );
        assert_eq!(listed[1].label, "Claude");
        assert!(
            listed[1].modified > listed[0].modified,
            "activity decides the order"
        );
    }

    #[test]
    fn attention_counts_questions_and_unseen_finishes_only() {
        let count =
            |all: &[LiveSession]| all.iter().filter(|s| needs_you(s.state, s.unread)).count();
        assert_eq!(count(&mixed()), 3);
        assert_eq!(count(&[]), 0);
        let quiet = [session("r", Some(ClaudeState::Running), true, 0)];
        assert_eq!(count(&quiet), 0, "a running session is never unread");
    }

    #[test]
    fn a_waiting_terminal_counts_with_no_session_bound_to_it() {
        use ClaudeState::*;
        let terminal = |item: u64, state, bell| (PathBuf::from("/a"), ItemId(item), state, bell);
        let found = needing([
            terminal(1, Some(NeedsInput), false),
            terminal(2, Some(Finished), true),
            terminal(3, Some(Finished), false),
            terminal(4, Some(Running), true),
            terminal(5, None, true),
        ]);
        let found: Vec<(u64, u8)> = found.into_iter().map(|(_, item, u)| (item.0, u)).collect();
        assert_eq!(found, [(1, 2), (2, 1)]);
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

        let (rel_in_tree, rel_in_main) = (locate(&inside).rel, locate(&main.join("nowhere")).rel);
        assert_eq!(rel_in_tree, Path::new("src"));
        assert_eq!(locate(&tree).rel, Path::new(""));
        assert_eq!(rel_in_main, Path::new("nowhere"));
        let locate = |dir: &Path| {
            let place = locate(dir);
            (place.main_root, place.branch)
        };
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
