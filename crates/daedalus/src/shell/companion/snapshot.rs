use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use daedalus_proto::{PaneId, TodoInfo};
use daedalus_term::ClaudeState;
use gpui::App;

use crate::companion::protocol::{Project, Session, SessionState, Terminal, Todo};
use crate::companion::server::Snapshot;
use crate::shell::Shell;
use crate::shell::claude_sessions::live::LiveSession;
use crate::shell::item::ItemView;
use crate::shell::windows;

/// What one window shows, read from memory so the main thread never waits on the disk.
pub(super) struct WindowInput {
    pub id: u64,
    pub projects: Vec<(PathBuf, String)>,
    pub terminals: Vec<TerminalInput>,
    pub sessions: Vec<SessionInput>,
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
    snapshot
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
    })
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
        let terminals = self
            .items
            .iter()
            .filter_map(|((root, _), view)| match view {
                ItemView::Terminal(v) => Some((root, v.read(cx))),
                _ => None,
            })
            .map(|(root, view)| {
                let foreground = view.foreground();
                let (rows, cols) = view.grid_size();
                TerminalInput {
                    pane: view.session(),
                    project: root.clone(),
                    title: view.label(),
                    cwd: foreground.as_ref().and_then(|(_, cwd)| cwd.clone()),
                    program: foreground.map(|(name, _)| name),
                    claude: view.claude_state(),
                    rows,
                    cols,
                }
            })
            .collect();
        let sessions = self
            .live_sessions(cx)
            .into_iter()
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
        }
    }
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
        }
    }

    fn window(sessions: Vec<SessionInput>, terminals: Vec<TerminalInput>) -> WindowInput {
        WindowInput {
            id: 3,
            projects: vec![("/code/app".into(), "app".into())],
            terminals,
            sessions,
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
        .map(|state| input(live("s", state)))
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
}
