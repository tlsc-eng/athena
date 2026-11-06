use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use daedalus_proto::{PaneId, TodoInfo};
use daedalus_term::{ClaudeState, StyledCell};
use gpui::App;

use super::{dialog, suggestion};
use crate::companion::protocol::{
    Dialog, Project, Session, SessionState, Terminal, Todo, UsageProfile, UsageStatus, UsageWindow,
};
use crate::companion::server::Snapshot;
use crate::shell::Shell;
use crate::shell::claude_sessions::live::LiveSession;
use crate::shell::item::ItemView;
use crate::shell::windows;
use crate::usage::{Profile, Status};

const SUGGESTION_ROWS: usize = 24;

/// What one window shows, read from memory so the main thread never waits on the disk.
pub(super) struct WindowInput {
    pub id: u64,
    pub projects: Vec<(PathBuf, String)>,
    pub terminals: Vec<TerminalInput>,
    pub sessions: Vec<SessionInput>,
    /// Empty while the usage indicator is off or before the first fetch.
    pub usage: Vec<(Profile, Status)>,
}

pub(super) struct TerminalInput {
    pub pane: Option<PaneId>,
    pub project: PathBuf,
    pub title: String,
    pub program: Option<String>,
    pub cwd: Option<PathBuf>,
    pub claude: Option<ClaudeState>,
    pub rows: u16,
    pub cols: u16,
    pub fitted: bool,
    pub dialog: Option<Dialog>,
    pub suggestion: Option<String>,
    pub background: Option<u16>,
}

pub(super) struct SessionInput {
    pub live: LiveSession,
    pub todos: Vec<TodoInfo>,
    pub plan: Option<String>,
    /// The last question its pane asked, kept until the pane runs again.
    pub needs: Option<String>,
}

pub(super) fn to_snapshot(windows: Vec<WindowInput>) -> Snapshot {
    let mut snapshot = Snapshot::default();
    // Profiles are global, so the first window that has readings speaks for all.
    let usage = windows
        .iter()
        .find(|w| !w.usage.is_empty())
        .map(|w| w.usage.clone())
        .unwrap_or_default();
    for window in windows {
        let roots: Vec<&Path> = window
            .projects
            .iter()
            .map(|(root, _)| root.as_path())
            .collect();
        snapshot
            .projects
            .extend(window.projects.iter().map(|(root, name)| Project {
                root: path_string(root),
                name: name.clone(),
                window: window.id,
            }));
        snapshot
            .terminals
            .extend(window.terminals.iter().filter_map(terminal));
        snapshot.sessions.extend(
            window
                .sessions
                .into_iter()
                .map(|input| session(input, &roots)),
        );
    }
    snapshot.terminals.sort_by_key(|t| t.pane);
    dedup_sessions(&mut snapshot.sessions);
    snapshot.usage = usage
        .iter()
        .map(|(profile, status)| usage_profile(profile, status))
        .collect();
    snapshot
}

fn usage_profile(profile: &Profile, status: &Status) -> UsageProfile {
    let (status, windows, message) = match status {
        Status::Windows(windows) => (
            UsageStatus::Ok,
            windows
                .iter()
                .map(|w| UsageWindow {
                    label: w.label.clone(),
                    used: w.used,
                    resets_at: w.resets_at,
                })
                .collect(),
            None,
        ),
        Status::Stale => (UsageStatus::Stale, Vec::new(), None),
        Status::NoAccess => (UsageStatus::NoAccess, Vec::new(), None),
        Status::RateLimited(_) => (UsageStatus::RateLimited, Vec::new(), None),
        Status::Unavailable(message) => {
            (UsageStatus::Unavailable, Vec::new(), Some(message.clone()))
        }
    };
    UsageProfile {
        profile: profile.name.clone(),
        status,
        windows,
        message,
    }
}

/// Windows can both list one session; keep the copy that runs in a pane, else the first.
fn dedup_sessions(sessions: &mut Vec<Session>) {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut kept: Vec<Session> = Vec::with_capacity(sessions.len());
    for session in sessions.drain(..) {
        match seen.get(&session.id) {
            Some(&at) if kept[at].pane.is_none() && session.pane.is_some() => kept[at] = session,
            Some(_) => {}
            None => {
                seen.insert(session.id.clone(), kept.len());
                kept.push(session);
            }
        }
    }
    *sessions = kept;
}

fn terminal(input: &TerminalInput) -> Option<Terminal> {
    Some(Terminal {
        pane: input.pane?,
        project: path_string(&input.project),
        title: input.title.clone(),
        program: input.program.clone(),
        cwd: input.cwd.as_deref().map(path_string),
        claude: input.claude.map(|state| state_of(Some(state))),
        rows: input.rows,
        cols: input.cols,
        fitted: input.fitted,
        dialog: input.dialog.clone(),
        suggestion: input.suggestion.clone(),
        background: input.background,
    })
}

/// Reads the screen only while Claude waits, so a running pane never pays for it.
fn open_dialog(
    claude: Option<ClaudeState>,
    screen: impl FnOnce() -> Vec<String>,
) -> Option<Dialog> {
    if claude != Some(ClaudeState::NeedsInput) {
        return None;
    }
    dialog::parse(&screen())
}

/// Reads the screen only after a finished turn, so a running pane never pays for it.
fn open_suggestion(
    claude: Option<ClaudeState>,
    rows: impl FnOnce() -> Vec<Vec<StyledCell>>,
) -> Option<String> {
    if claude != Some(ClaudeState::Finished) {
        return None;
    }
    suggestion::parse(&rows())
}

fn session(input: SessionInput, roots: &[&Path]) -> Session {
    let SessionInput {
        live,
        todos,
        plan,
        needs,
    } = input;
    let state = state_of(live.state);
    Session {
        project: owner(roots, &live.cwd)
            .or_else(|| owner(roots, &live.main_root))
            .map(path_string),
        id: live.id,
        label: live.label,
        pane: live.pane,
        cwd: path_string(&live.cwd),
        main_root: path_string(&live.main_root),
        branch: live.branch,
        state,
        unread: live.unread,
        todos: todos
            .into_iter()
            .map(|t| Todo {
                content: t.content,
                status: t.status,
                active_form: t.active_form,
            })
            .collect(),
        plan,
        needs: needs.filter(|_| state == SessionState::NeedsInput),
        files: live.files as u32,
        modified_ms: live
            .modified
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
    }
}

/// The deepest open root that is `path` or holds it.
fn owner<'a>(roots: &[&'a Path], path: &Path) -> Option<&'a Path> {
    roots
        .iter()
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.components().count())
        .copied()
}

fn state_of(state: Option<ClaudeState>) -> SessionState {
    match state {
        Some(ClaudeState::Running) => SessionState::Running,
        Some(ClaudeState::NeedsInput) => SessionState::NeedsInput,
        Some(ClaudeState::Finished) => SessionState::Finished,
        None => SessionState::Idle,
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Every open window's state, in the order the windows were opened.
pub(super) fn gather(cx: &App) -> Vec<WindowInput> {
    let needs = windows::companion_needs(cx);
    windows::handles(cx)
        .into_iter()
        .filter_map(|handle| {
            let shell = handle.read(cx).ok()?;
            Some(shell.companion_input(handle.window_id().as_u64(), needs, cx))
        })
        .collect()
}

impl Shell {
    fn companion_input(&self, id: u64, needs: &HashMap<PaneId, String>, cx: &App) -> WindowInput {
        let mut claude_panes = HashSet::new();
        let terminals = self
            .items
            .iter()
            .filter_map(|((root, _), view)| match view {
                ItemView::Terminal(v) => Some((root, v.read(cx))),
                _ => None,
            })
            .map(|(root, view)| {
                if view.is_claude() {
                    claude_panes.extend(view.session());
                }
                let foreground = view.foreground();
                let (rows, cols) = view.grid_size();
                let claude = view.claude_state();
                let dialog = open_dialog(claude, || view.text_lines(rows as usize));
                let suggestion = open_suggestion(claude, || view.styled_rows(SUGGESTION_ROWS));
                TerminalInput {
                    pane: view.session(),
                    project: root.clone(),
                    title: view.label(),
                    cwd: foreground.as_ref().and_then(|(_, cwd)| cwd.clone()),
                    program: foreground.map(|(name, _)| name),
                    claude,
                    rows,
                    cols,
                    fitted: view.is_leased(),
                    dialog,
                    suggestion,
                    background: view.claude_waiting(),
                }
            })
            .collect();
        let sessions = self
            .live_sessions(cx)
            .into_iter()
            .map(|live| detach_exited(live, &claude_panes))
            .map(|live| {
                let plan = self.review.sessions.plan_of(&live.id);
                SessionInput {
                    todos: plan.map(|(todos, _)| todos.to_vec()).unwrap_or_default(),
                    plan: plan.and_then(|(_, plan)| plan.map(str::to_string)),
                    needs: live.pane.and_then(|pane| needs.get(&pane).cloned()),
                    live,
                }
            })
            .collect();
        WindowInput {
            id,
            projects: self
                .workspace
                .projects
                .iter()
                .map(|p| (p.root.clone(), p.name()))
                .collect(),
            terminals,
            sessions,
            usage: self.usage_readings(),
        }
    }
}

/// A pane whose Claude exited keeps its shell and binding, but the phone must not type prompts into it.
fn detach_exited(mut live: LiveSession, claude: &HashSet<PaneId>) -> LiveSession {
    if live.pane.is_some_and(|pane| !claude.contains(&pane)) {
        live.pane = None;
        live.state = None;
    }
    live
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn live(id: &str, state: Option<ClaudeState>) -> LiveSession {
        LiveSession {
            id: id.into(),
            label: format!("label {id}"),
            pane: Some(7),
            cwd: "/code/app".into(),
            main_root: "/code/app".into(),
            rel: PathBuf::new(),
            branch: None,
            state,
            unread: false,
            todos: None,
            files: 2,
            modified: UNIX_EPOCH + Duration::from_millis(1_500),
        }
    }

    fn input(live: LiveSession) -> SessionInput {
        SessionInput {
            live,
            todos: Vec::new(),
            plan: None,
            needs: Some("Allow the edit?".into()),
        }
    }

    fn term(pane: Option<PaneId>) -> TerminalInput {
        TerminalInput {
            pane,
            project: "/code/app".into(),
            title: "zsh".into(),
            program: None,
            cwd: None,
            claude: None,
            rows: 24,
            cols: 80,
            fitted: false,
            dialog: None,
            suggestion: None,
            background: None,
        }
    }

    fn window(sessions: Vec<SessionInput>, terminals: Vec<TerminalInput>) -> WindowInput {
        WindowInput {
            id: 3,
            projects: vec![("/code/app".into(), "app".into())],
            terminals,
            sessions,
            usage: Vec::new(),
        }
    }

    #[test]
    fn states_map_to_the_protocol_and_none_is_idle() {
        let sessions = [
            Some(ClaudeState::Running),
            Some(ClaudeState::NeedsInput),
            Some(ClaudeState::Finished),
            None,
        ]
        .into_iter()
        .enumerate()
        .map(|(i, state)| input(live(&format!("s{i}"), state)))
        .collect();
        let got = to_snapshot(vec![window(sessions, Vec::new())]);
        let states: Vec<_> = got.sessions.iter().map(|s| s.state).collect();
        assert_eq!(
            states,
            [
                SessionState::Running,
                SessionState::NeedsInput,
                SessionState::Finished,
                SessionState::Idle
            ]
        );
    }

    #[test]
    fn a_terminal_claude_state_stays_unset_without_one() {
        let mut running = term(Some(1));
        running.claude = Some(ClaudeState::Running);
        let got = to_snapshot(vec![window(Vec::new(), vec![running, term(Some(2))])]);
        assert_eq!(got.terminals[0].claude, Some(SessionState::Running));
        assert_eq!(got.terminals[1].claude, None);
    }

    #[test]
    fn a_leased_terminal_is_reported_fitted_at_the_leased_grid() {
        let mut leased = term(Some(1));
        (leased.rows, leased.cols, leased.fitted) = (44, 57, true);
        let got = to_snapshot(vec![window(Vec::new(), vec![leased, term(Some(2))])]);
        assert!(got.terminals[0].fitted && !got.terminals[1].fitted);
        assert_eq!((got.terminals[0].rows, got.terminals[0].cols), (44, 57));
    }

    #[test]
    fn only_a_terminal_that_needs_input_carries_a_dialog() {
        let screen = || {
            [" Run it?", " ❯ 1. Yes", "   2. No", " Esc to cancel"]
                .map(String::from)
                .to_vec()
        };
        let mut waiting = term(Some(1));
        waiting.claude = Some(ClaudeState::NeedsInput);
        waiting.dialog = open_dialog(waiting.claude, screen);
        let mut running = term(Some(2));
        running.claude = Some(ClaudeState::Running);
        running.dialog = open_dialog(running.claude, || panic!("a running pane is not read"));
        let got = to_snapshot(vec![window(
            Vec::new(),
            vec![waiting, running, term(Some(3))],
        )]);
        let dialog = got.terminals[0].dialog.as_ref().expect("a dialog");
        assert_eq!(dialog.options.len(), 2);
        assert!(got.terminals[1].dialog.is_none() && got.terminals[2].dialog.is_none());
    }

    #[test]
    fn only_a_finished_terminal_carries_a_suggestion() {
        let rule = "─".repeat(40);
        let screen = || {
            let dim = |c: char| StyledCell {
                c,
                dim: true,
                inverse: false,
            };
            let plain = |c: char| StyledCell {
                c,
                dim: false,
                inverse: false,
            };
            vec![
                rule.chars().map(plain).collect(),
                "❯ run it"
                    .chars()
                    .enumerate()
                    .map(|(i, c)| if i < 2 { plain(c) } else { dim(c) })
                    .collect(),
                rule.chars().map(plain).collect(),
            ]
        };
        let mut done = term(Some(1));
        done.claude = Some(ClaudeState::Finished);
        done.suggestion = open_suggestion(done.claude, screen);
        let mut running = term(Some(2));
        running.claude = Some(ClaudeState::Running);
        running.suggestion =
            open_suggestion(running.claude, || panic!("a running pane is not read"));
        let mut waiting = term(Some(3));
        waiting.claude = Some(ClaudeState::NeedsInput);
        waiting.suggestion =
            open_suggestion(waiting.claude, || panic!("a waiting pane is not read"));
        let got = to_snapshot(vec![window(Vec::new(), vec![done, running, waiting])]);
        assert_eq!(got.terminals[0].suggestion.as_deref(), Some("run it"));
        assert!(got.terminals[1].suggestion.is_none() && got.terminals[2].suggestion.is_none());
    }

    #[test]
    fn terminals_without_a_pane_are_left_out_and_the_rest_sorted() {
        let got = to_snapshot(vec![window(
            Vec::new(),
            vec![term(Some(9)), term(None), term(Some(4))],
        )]);
        let panes: Vec<_> = got.terminals.iter().map(|t| t.pane).collect();
        assert_eq!(panes, [4, 9]);
        assert_eq!((got.terminals[0].rows, got.terminals[0].cols), (24, 80));
    }

    #[test]
    fn a_worktree_session_belongs_to_the_open_project_holding_its_main_checkout() {
        let mut worktree = live("w", Some(ClaudeState::Running));
        worktree.cwd = "/code/app-worktrees/fix".into();
        worktree.main_root = "/code/app".into();
        let mut stray = live("x", None);
        stray.cwd = "/elsewhere".into();
        stray.main_root = "/elsewhere".into();
        let got = to_snapshot(vec![window(
            vec![input(worktree), input(stray)],
            Vec::new(),
        )]);
        assert_eq!(got.sessions[0].project.as_deref(), Some("/code/app"));
        assert_eq!(got.sessions[1].project, None);
        assert_eq!(got.projects[0].window, 3);
    }

    #[test]
    fn the_deepest_open_root_holding_the_cwd_wins() {
        let mut nested = window(vec![input(live("n", None))], Vec::new());
        nested
            .projects
            .push(("/code/app/crates/inner".into(), "inner".into()));
        nested.sessions[0].live.cwd = "/code/app/crates/inner/src".into();
        let got = to_snapshot(vec![nested]);
        assert_eq!(
            got.sessions[0].project.as_deref(),
            Some("/code/app/crates/inner")
        );
    }

    #[test]
    fn a_worktree_opened_as_its_own_project_owns_its_sessions() {
        let mut open = window(vec![input(live("w", None))], Vec::new());
        open.projects
            .push(("/code/app-worktrees/fix".into(), "fix".into()));
        open.sessions[0].live.cwd = "/code/app-worktrees/fix/src".into();
        open.sessions[0].live.main_root = "/code/app".into();
        let got = to_snapshot(vec![open]);
        assert_eq!(
            got.sessions[0].project.as_deref(),
            Some("/code/app-worktrees/fix")
        );
    }

    #[test]
    fn a_session_two_windows_list_appears_once_with_its_pane() {
        let mut idle = live("dup", None);
        idle.pane = None;
        let mut first = window(vec![input(idle), input(live("only", None))], Vec::new());
        first.id = 1;
        let mut second = window(
            vec![input(live("dup", Some(ClaudeState::Running)))],
            Vec::new(),
        );
        second.id = 2;
        let got = to_snapshot(vec![first, second]);
        let ids: Vec<_> = got.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["dup", "only"]);
        assert_eq!(got.sessions[0].pane, Some(7));

        let mut a = live("same", None);
        a.label = "first".into();
        let mut b = live("same", None);
        b.label = "second".into();
        let got = to_snapshot(vec![window(vec![input(a), input(b)], Vec::new())]);
        assert_eq!(got.sessions.len(), 1);
        assert_eq!(got.sessions[0].label, "first");
    }

    #[test]
    fn needs_is_sent_only_while_the_session_needs_input() {
        let sessions = vec![
            input(live("a", Some(ClaudeState::NeedsInput))),
            input(live("b", Some(ClaudeState::Running))),
            input(live("c", None)),
        ];
        let got = to_snapshot(vec![window(sessions, Vec::new())]);
        assert_eq!(got.sessions[0].needs.as_deref(), Some("Allow the edit?"));
        assert_eq!(got.sessions[1].needs, None);
        assert_eq!(got.sessions[2].needs, None);
    }

    #[test]
    fn todos_plan_and_modified_time_carry_over() {
        let mut with_plan = input(live("p", Some(ClaudeState::Running)));
        with_plan.todos = vec![TodoInfo {
            content: "Write tests".into(),
            status: "in_progress".into(),
            active_form: Some("Writing tests".into()),
        }];
        with_plan.plan = Some("1. Do it".into());
        let got = to_snapshot(vec![window(vec![with_plan], Vec::new())]);
        let s = &got.sessions[0];
        assert_eq!(s.todos[0].active_form.as_deref(), Some("Writing tests"));
        assert_eq!(s.plan.as_deref(), Some("1. Do it"));
        assert_eq!(s.modified_ms, 1_500);
        assert_eq!((s.files, s.pane), (2, Some(7)));
    }

    #[test]
    fn equal_input_gives_equal_snapshots() {
        let build = || {
            to_snapshot(vec![window(
                vec![input(live("a", Some(ClaudeState::NeedsInput)))],
                vec![term(Some(2)), term(Some(1))],
            )])
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn usage_statuses_map_to_the_protocol() {
        let profile = Profile::named("claude");
        let windows = Status::Windows(vec![crate::usage::Window {
            label: "5h".into(),
            used: 42.5,
            resets_at: Some(100),
        }]);
        let ok = usage_profile(&profile, &windows);
        assert_eq!(ok.status, UsageStatus::Ok);
        assert_eq!(ok.windows[0].label, "5h");
        assert_eq!(ok.windows[0].resets_at, Some(100));
        let cases = [
            (Status::Stale, UsageStatus::Stale),
            (Status::NoAccess, UsageStatus::NoAccess),
            (
                Status::RateLimited(Duration::from_secs(30)),
                UsageStatus::RateLimited,
            ),
        ];
        for (status, want) in cases {
            let got = usage_profile(&profile, &status);
            assert_eq!(
                (got.status, got.windows.len(), got.message),
                (want, 0, None)
            );
        }
        let down = usage_profile(&profile, &Status::Unavailable("offline".into()));
        assert_eq!(down.status, UsageStatus::Unavailable);
        assert_eq!(down.message.as_deref(), Some("offline"));
    }

    #[test]
    fn the_first_window_with_readings_supplies_the_usage() {
        let empty = window(Vec::new(), Vec::new());
        let mut full = window(Vec::new(), Vec::new());
        full.usage = vec![(Profile::named("claude"), Status::Stale)];
        let got = to_snapshot(vec![empty, full]);
        assert_eq!(got.usage.len(), 1);
        assert_eq!(got.usage[0].profile, "claude");
        assert!(
            to_snapshot(vec![window(Vec::new(), Vec::new())])
                .usage
                .is_empty()
        );
    }

    #[test]
    fn a_session_whose_claude_exited_is_sent_without_its_pane() {
        let got = detach_exited(live("a", Some(ClaudeState::Running)), &HashSet::new());
        assert_eq!((got.pane, got.state), (None, None));
    }

    #[test]
    fn a_session_still_running_claude_keeps_its_pane() {
        let got = detach_exited(live("a", Some(ClaudeState::Running)), &HashSet::from([7]));
        assert_eq!((got.pane, got.state), (Some(7), Some(ClaudeState::Running)));
    }

    #[test]
    fn a_history_row_is_unchanged() {
        let mut past = live("a", None);
        past.pane = None;
        let got = detach_exited(past, &HashSet::from([7]));
        assert_eq!((got.pane, got.state), (None, None));
    }
}
