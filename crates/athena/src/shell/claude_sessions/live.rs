use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use athena_proto::{PaneId, TodoInfo};
use athena_term::ClaudeState;
use athena_workspace::git;
use gpui::App;

use super::{SessionRow, short_id};
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

/// Sessions that need the user: waiting for an answer, or finished and not yet seen.
pub(crate) fn attention_count(sessions: &[LiveSession]) -> usize {
    sessions
        .iter()
        .filter(|s| match s.state {
            Some(ClaudeState::NeedsInput) => true,
            Some(ClaudeState::Finished) => s.unread,
            _ => false,
        })
        .count()
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
    match cwd
        .ancestors()
        .find_map(|dir| Some((dir, git::main_worktree(dir)?)))
    {
        Some((tree, main)) => (main, branch_of(tree)),
        None => (cwd.to_path_buf(), None),
    }
}

/// Read from HEAD on disk, as this runs on every draw and must not start git.
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

impl Shell {
    /// One entry per session this window knows: those its terminals run, then its project's recent ones.
    pub(crate) fn live_sessions(&self, cx: &App) -> Vec<LiveSession> {
        let state = &self.review.sessions;
        let mut running: HashMap<&str, Running> = HashMap::new();
        for (pane, id) in &state.panes {
            let Some((root, item)) = self.find_session(*pane) else {
                continue;
            };
            let Some(view @ ItemView::Terminal(_)) = self.items.get(&(root.clone(), item)) else {
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
        let rows: HashMap<&str, &SessionRow> =
            state.rows.iter().map(|r| (r.id.as_str(), r)).collect();
        let ids = running
            .keys()
            .copied()
            .chain(rows.keys().copied().filter(|id| !running.contains_key(id)));
        let mut out: Vec<LiveSession> = ids
            .map(|id| {
                let (row, term) = (rows.get(id), running.get(id));
                let cwd = state
                    .cwds
                    .get(id)
                    .or(term.map(|t| &t.root))
                    .or(state.root.as_ref())
                    .cloned()
                    .unwrap_or_default();
                let (main_root, branch) = locate(&cwd);
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
                    unread: term.is_some_and(|t| {
                        t.bell && matches!(t.state, Some(s) if s != ClaudeState::Running)
                    }),
                    todos: live.and_then(|l| todo_counts(&l.todos)),
                    files: row.map_or(0, |r| r.files.len()),
                    usage: row.map(|r| r.tokens.clone()).unwrap_or_default(),
                    modified: row.map_or(SystemTime::UNIX_EPOCH, |r| r.modified),
                }
            })
            .collect();
        sort(&mut out);
        out
    }

    /// Sessions needing you across all open windows, this one included.
    pub(crate) fn attention_total(&self, cx: &App) -> usize {
        attention_count(&self.live_sessions(cx))
            + crate::shell::windows::attention_elsewhere(self.window_handle, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::super::SessionsState;
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

        assert_eq!(locate(&tree), (main.clone(), Some("claude/fix".into())));
        assert_eq!(locate(&inside), (main.clone(), Some("claude/fix".into())));
        assert_eq!(locate(&main), (main.clone(), None));
        assert_eq!(locate(&main.join("nowhere")), (main.join("nowhere"), None));

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
}
