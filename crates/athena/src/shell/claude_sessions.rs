use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use athena_editor::{EditorView, diff};
use athena_proto::{AppMsg, PaneId, TodoInfo};
use athena_ui::{ButtonKind, MenuItem, Space, Theme, Tooltip};
use athena_workspace::{DiffBase, SideMode};
use gpui::{
    AnyElement, App, ClipboardItem, Context, Entity, MouseButton, MouseDownEvent, PromptLevel,
    Task, Window, actions, div, prelude::*,
};

use super::Shell;
use super::item::{ItemView, file_label};
use super::lsp::{CONFIRMED, confirm_buttons};
use super::menus::shell_item;
use super::review::backup_dir;
use crate::snapshots::{self, Before};
use crate::transcripts::{self, Profile};

mod list;
pub(super) mod live;
mod session_view;

actions!(
    athena,
    [
        ShowClaudeSessions,
        ClaudeReviewNextFile,
        ClaudeReviewPreviousFile,
        ClaudeOpenSessionView
    ]
);

const SESSIONS: usize = 30;
/// A list older than this is read again the next time the tab draws.
const STALE_AFTER: Duration = Duration::from_secs(10);
/// Files bigger than this get no line counts; diffing them would hold the list up.
const MAX_COUNTED: u64 = 2 * 1024 * 1024;
const ROW_HEIGHT: f32 = 26.;
const PLAN_LINES: usize = 4;

/// Sessions mode: recent sessions of the active project, with what each changed.
#[derive(Default)]
pub(super) struct SessionsState {
    root: Option<PathBuf>,
    rows: Rc<Vec<SessionRow>>,
    /// The sessions of the other projects that terminals here run in; `rows` holds the active one's.
    elsewhere: Rc<Vec<SessionRow>>,
    /// The project roots the rows were last asked for.
    loaded_roots: BTreeSet<PathBuf>,
    hooks_on: bool,
    loading: Option<Task<()>>,
    loaded_at: Option<Instant>,
    expanded: HashSet<String>,
    /// Todo lists and plans the hooks reported, by session id.
    live: HashMap<String, LivePlan>,
    /// The session each terminal runs, as its hooks said.
    panes: HashMap<PaneId, String>,
    /// Where each session started, as its SessionStart hook said.
    cwds: HashMap<String, PathBuf>,
    /// `live::locate` per cwd, read by a load in the background so drawing never touches the disk.
    places: HashMap<PathBuf, live::Place>,
    review: Option<Review>,
    views: session_view::Views,
}

/// What follows a terminal's sessions when its project moves to another window.
#[derive(Default)]
pub(super) struct Bindings {
    panes: HashMap<PaneId, String>,
    cwds: HashMap<String, PathBuf>,
    live: HashMap<String, LivePlan>,
}

impl Bindings {
    pub(super) fn extend(&mut self, other: Bindings) {
        self.panes.extend(other.panes);
        self.cwds.extend(other.cwds);
        self.live.extend(other.live);
    }
}

impl SessionsState {
    /// Makes `session` the one `pane` runs; a session lives in one pane, the latest to claim it.
    pub(super) fn bind(&mut self, pane: PaneId, session: String) {
        if session.is_empty() {
            return;
        }
        self.panes.retain(|p, s| *p == pane || *s != session);
        self.panes.insert(pane, session);
    }

    /// Takes out what is known of the sessions the `moved` panes run.
    pub(super) fn split_off(&mut self, moved: &[PaneId]) -> Bindings {
        let mut out = Bindings::default();
        for pane in moved {
            let Some(session) = self.panes.remove(pane) else {
                continue;
            };
            if let Some(cwd) = self.cwds.remove(&session) {
                out.cwds.insert(session.clone(), cwd);
            }
            if let Some(live) = self.live.remove(&session) {
                out.live.insert(session.clone(), live);
            }
            out.panes.insert(*pane, session);
        }
        out
    }

    pub(super) fn absorb(&mut self, moved: Bindings) {
        for (pane, session) in moved.panes {
            self.bind(pane, session);
        }
        self.cwds.extend(moved.cwds);
        self.live.extend(moved.live);
    }

    /// Forgets which pane runs `session`, as another window now does.
    pub(super) fn unbind(&mut self, session: &str) -> bool {
        let before = self.panes.len();
        self.panes.retain(|_, s| s != session);
        self.panes.len() != before
    }

    /// The row read for `id`, from whichever project's list holds it.
    fn row(&self, id: &str) -> Option<&SessionRow> {
        let all = self.rows.iter().chain(self.elsewhere.iter());
        pick_row(
            all.filter(|r| r.id == id),
            self.cwds.get(id).map(PathBuf::as_path),
            self.root.as_deref(),
        )
    }

    /// The sessions bound to a pane here, each with when it last changed (`UNIX_EPOCH` while the
    /// list has not loaded it).
    pub(super) fn bound_sessions(&self) -> Vec<(String, SystemTime)> {
        let modified = |id: &String| {
            let row = self.rows.iter().find(|r| r.id == *id);
            row.map_or(SystemTime::UNIX_EPOCH, |r| r.modified)
        };
        self.panes
            .values()
            .map(|s| (s.clone(), modified(s)))
            .collect()
    }

    /// The pane that runs `session` here.
    pub(super) fn pane_of(&self, session: &str) -> Option<PaneId> {
        self.panes
            .iter()
            .find_map(|(pane, s)| (s == session).then_some(*pane))
    }

    /// The session `pane` runs here.
    pub(super) fn session_of(&self, pane: PaneId) -> Option<&str> {
        self.panes.get(&pane).map(String::as_str)
    }

    /// Where `session` started, as its SessionStart hook said.
    pub(super) fn cwd_of(&self, session: &str) -> Option<&Path> {
        self.cwds.get(session).map(PathBuf::as_path)
    }
}

#[derive(Clone, Debug, Default)]
struct LivePlan {
    todos: Vec<TodoInfo>,
    plan: Option<String>,
}

/// "Review All": the session's files opened one after another.
struct Review {
    session: String,
    label: String,
    files: Vec<FileChange>,
    index: usize,
}

impl Review {
    /// A review of `row`'s files from the first; `None` when it changed none.
    fn of(row: &SessionRow) -> Option<Self> {
        (!row.files.is_empty()).then(|| Self {
            session: row.id.clone(),
            label: row.label.clone(),
            files: row.files.clone(),
            index: 0,
        })
    }

    /// Moves `by` files, stopping at either end, and returns the file now shown.
    fn step(&mut self, by: isize) -> &FileChange {
        let last = self.files.len().saturating_sub(1) as isize;
        self.index = (self.index as isize + by).clamp(0, last) as usize;
        &self.files[self.index]
    }
}

/// What a session's file is compared with: the commit named for it, else its snapshot.
fn change_base(session: &str, base: Option<DiffBase>) -> DiffBase {
    base.unwrap_or_else(|| DiffBase::Snapshot {
        session: session.to_string(),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct SessionRow {
    id: String,
    /// The project it was read for.
    root: PathBuf,
    label: String,
    /// The config folder its transcript is in; `None` when only edits were recorded.
    profile: Option<Profile>,
    modified: SystemTime,
    messages: usize,
    files: Vec<FileChange>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct FileChange {
    path: PathBuf,
    /// Lines added and removed since before the session, when known.
    counts: Option<(usize, usize)>,
    created: bool,
    deleted: bool,
    /// What the diff compares for a file no snapshot covers; `None` compares with the snapshot.
    base: Option<DiffBase>,
}

type CountKey = (String, PathBuf, u64, SystemTime);
type Counts = HashMap<CountKey, Option<(usize, usize)>>;
static COUNTS: Mutex<Option<Counts>> = Mutex::new(None);

/// Lines added and removed in `path` since `session` first edited it.
fn line_counts(store: &Path, session: &str, path: &Path) -> Option<(usize, usize)> {
    let meta = std::fs::metadata(path).ok();
    let key = (
        session.to_string(),
        path.to_path_buf(),
        meta.as_ref().map_or(0, |m| m.len()),
        meta.as_ref()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH),
    );
    if let Some(found) = COUNTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .get(&key)
    {
        return *found;
    }
    let counted = count(store, session, path, key.2);
    COUNTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .insert(key, counted);
    counted
}

fn count(store: &Path, session: &str, path: &Path, len: u64) -> Option<(usize, usize)> {
    let old = match snapshots::read(store, session, path) {
        Before::Text(t) => t,
        Before::Absent => Vec::new(),
        Before::Skipped | Before::Unknown => return None,
    };
    if len > MAX_COUNTED || old.len() as u64 > MAX_COUNTED {
        return None;
    }
    let new = std::fs::read(path).unwrap_or_default();
    let (old, new) = (String::from_utf8(old).ok()?, String::from_utf8(new).ok()?);
    let (a, b) = (diff::lines(&old), diff::lines(&new));
    Some(
        diff::diff_lines(&a, &b)
            .iter()
            .fold((0, 0), |(add, del), c| {
                (add + c.new.len(), del + c.old.len())
            }),
    )
}

/// The project's recent sessions: those with a transcript in any profile, and those that only
/// left edits in the snapshot store; newest first.
pub(super) fn load(home: &Path, store: &Path, root: &Path) -> Vec<SessionRow> {
    let mut rows: Vec<SessionRow> = transcripts::sessions(home, root, SESSIONS)
        .into_iter()
        .map(|s| SessionRow {
            label: s
                .summary
                .label()
                .map(str::to_string)
                .unwrap_or_else(|| format!("Session {}", short_id(&s.id))),
            id: s.id,
            root: root.to_path_buf(),
            profile: Some(s.profile),
            modified: s.modified,
            messages: s.summary.messages,
            files: Vec::new(),
        })
        .collect();
    for stored in snapshots::sessions_under(store, root) {
        let files = stored
            .files
            .iter()
            .map(|path| FileChange {
                counts: line_counts(store, &stored.session, path),
                created: snapshots::created(store, &stored.session, path),
                deleted: false,
                path: path.clone(),
                base: None,
            })
            .collect();
        match rows.iter_mut().find(|r| r.id == stored.session) {
            Some(row) => row.files = files,
            None => rows.push(SessionRow {
                label: format!("Session {}", short_id(&stored.session)),
                id: stored.session,
                root: root.to_path_buf(),
                profile: None,
                modified: stored.modified,
                messages: 0,
                files,
            }),
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.modified));
    rows.truncate(SESSIONS);
    rows
}

/// Of the rows read for one session, the one with a transcript, then the one read for the folder
/// the session ran in, then the active project's; a project's list also holds sessions that only
/// edited files under it. Rows are given in the same order everywhere, so a tie falls the same way.
fn pick_row<'a>(
    rows: impl IntoIterator<Item = &'a SessionRow>,
    cwd: Option<&Path>,
    active: Option<&Path>,
) -> Option<&'a SessionRow> {
    rows.into_iter().max_by_key(|r| {
        (
            r.profile.is_some(),
            cwd == Some(r.root.as_path()),
            active == Some(r.root.as_path()),
        )
    })
}

/// Whether two spellings of a path name the same file.
fn same_file(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// `3/7`: finished todos out of all of them.
fn progress(todos: &[TodoInfo]) -> Option<String> {
    let done = todos.iter().filter(|t| t.status == "completed").count();
    (!todos.is_empty()).then(|| format!("{done}/{}", todos.len()))
}

/// The todo list as a tooltip shows it, one marked line per todo.
fn todo_lines(todos: &[TodoInfo]) -> String {
    todos
        .iter()
        .map(|t| match t.status.as_str() {
            "completed" => format!("✓ {}", t.content),
            "in_progress" => format!("◐ {}", t.active_form.as_deref().unwrap_or(&t.content)),
            _ => format!("○ {}", t.content),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What Resume types: the profile's config folder, then Claude Code resuming the session.
pub(super) fn resume_command(id: &str, config_dir: Option<&Path>) -> Option<String> {
    let typable = |dir: &Path| !dir.to_string_lossy().chars().any(char::is_control);
    if !snapshots::valid_session(id) || !config_dir.is_none_or(typable) {
        return None;
    }
    let env = config_dir.map(|dir| {
        let quoted = dir.to_string_lossy().replace('\'', r"'\''");
        format!("CLAUDE_CONFIG_DIR='{quoted}' ")
    });
    Some(format!("{}claude --resume {id}", env.unwrap_or_default()))
}

impl Shell {
    /// A TodoWrite or ExitPlanMode hook reported in; the terminal it came from shows its progress.
    pub(super) fn claude_plan_event(
        &mut self,
        msg: AppMsg,
        caller: Option<PaneId>,
        cx: &mut Context<Self>,
    ) {
        let state = &mut self.review.sessions;
        let session = match msg {
            AppMsg::ClaudeTodos { session, todos } => {
                state.live.entry(session.clone()).or_default().todos = todos;
                session
            }
            AppMsg::ClaudePlan { session, plan } => {
                state.live.entry(session.clone()).or_default().plan = Some(plan);
                session
            }
            _ => return,
        };
        if let Some(pane) = caller {
            self.bind_session(pane, session, cx);
        }
        cx.notify();
    }

    /// A SessionStart hook reported in: the terminal it came from now runs that session.
    pub(super) fn claude_session_started(
        &mut self,
        session: String,
        cwd: PathBuf,
        caller: Option<PaneId>,
        cx: &mut Context<Self>,
    ) {
        if let Some(pane) = caller {
            // A relative path would resolve against Athena's own folder when located.
            let cwd = cwd.is_absolute().then_some(cwd);
            if let Some(cwd) = cwd {
                self.review.sessions.cwds.insert(session.clone(), cwd);
            }
            self.bind_session(pane, session, cx);
            self.deliver_pending(cx);
            cx.notify();
        }
    }

    /// Binds `session` to `pane` here and drops it from every other window, which cannot run it too.
    pub(super) fn bind_session(&mut self, pane: PaneId, session: String, cx: &mut Context<Self>) {
        super::windows::unbind_elsewhere(session.clone(), self.window_handle, cx);
        let changed = self.review.sessions.panes.get(&pane) != Some(&session);
        self.review.sessions.bind(pane, session);
        if changed {
            self.claude_sessions_stale(cx);
        }
    }

    /// Something changed on disk or in a session: read the list again when it next draws.
    pub(super) fn claude_sessions_stale(&mut self, cx: &mut Context<Self>) {
        self.review.sessions.loaded_at = None;
        self.refresh_session_views(cx);
        if self.sessions_visible() {
            cx.notify();
        }
    }

    /// Whether the session list or a session's panel is on screen.
    fn sessions_visible(&self) -> bool {
        (self.workspace.ui.tree_visible && self.workspace.ui.side_mode == SideMode::Sessions)
            || !self.open_session_views().is_empty()
    }

    fn terminal_session(&self, view: &ItemView, cx: &gpui::App) -> Option<&LivePlan> {
        let ItemView::Terminal(t) = view else {
            return None;
        };
        let state = &self.review.sessions;
        let session = state.panes.get(&t.read(cx).session()?)?;
        state.live.get(session)
    }

    /// A terminal's Claude todo progress, `3/7`, and the list for its tooltip.
    pub(super) fn terminal_todos(
        &self,
        view: &ItemView,
        cx: &gpui::App,
    ) -> Option<(String, String)> {
        let live = self.terminal_session(view, cx)?;
        Some((progress(&live.todos)?, todo_lines(&live.todos)))
    }

    /// Whether a terminal open now is running the session.
    fn session_running(&self, id: &str, cx: &Context<Self>) -> bool {
        self.review.sessions.panes.iter().any(|(pane, session)| {
            session == id
                && self.items.values().any(|v| match v {
                    ItemView::Terminal(t) => {
                        t.read(cx).session() == Some(*pane) && t.read(cx).is_claude()
                    }
                    _ => false,
                })
        })
    }

    /// Reads the sessions of `root` and of every other project a terminal here runs a session in.
    fn load_claude_sessions(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let mut roots = self.session_row_roots();
        roots.insert(root.clone());
        let state = &mut self.review.sessions;
        if state.root.as_ref() != Some(&root) {
            state.rows = Rc::default();
            state.expanded.clear();
        }
        state.root = Some(root.clone());
        state.loaded_at = Some(Instant::now());
        state.loaded_roots = roots.clone();
        let mut cwds = roots.clone();
        cwds.extend(state.cwds.values().cloned());
        let (Some(home), Ok(store)) = (std::env::home_dir(), snapshots::store()) else {
            return;
        };
        state.loading = Some(cx.spawn(async move |this, cx| {
            let (loaded, places, hooks_on) = cx
                .background_executor()
                .spawn(async move {
                    let loaded: Vec<SessionRow> =
                        roots.iter().flat_map(|r| load(&home, &store, r)).collect();
                    let hooks = crate::claude_hooks::global_settings(&home);
                    (
                        loaded,
                        live::locate_all(&cwds),
                        hooks.iter().any(|p| crate::claude_hooks::enabled(p)),
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let state = &mut this.review.sessions;
                if state.root.as_ref() == Some(&root) {
                    let (rows, elsewhere): (Vec<_>, Vec<_>) =
                        loaded.into_iter().partition(|r| r.root == root);
                    let mut changed = *state.elsewhere != elsewhere;
                    if changed {
                        state.elsewhere = Rc::new(elsewhere);
                    }
                    state.loading = None;
                    state.hooks_on = hooks_on;
                    state.places = places;
                    // The newest session opens on first view: it is usually the one to review.
                    if state.rows.is_empty()
                        && state.expanded.is_empty()
                        && let Some(first) = rows.first()
                    {
                        state.expanded.insert(first.id.clone());
                    }
                    if *state.rows != rows {
                        state.rows = Rc::new(rows);
                        changed = true;
                    }
                    if changed {
                        this.refresh_session_views(cx);
                    }
                    this.refresh_send_palette(cx);
                    cx.notify();
                }
            });
        }));
    }

    /// Reads the lists again when they are stale, or a terminal now runs a session in a project
    /// they have not covered.
    fn refresh_sessions(&mut self, root: &Path, cx: &mut Context<Self>) {
        let state = &self.review.sessions;
        let stale = state.loaded_at.is_none_or(|at| at.elapsed() > STALE_AFTER);
        let idle = state.loading.is_none();
        let reload = stale || self.session_row_roots() != state.loaded_roots;
        if state.root.as_deref() != Some(root) || (reload && idle) {
            self.load_claude_sessions(root.to_path_buf(), cx);
        }
    }

    pub(super) fn resume_claude_session(
        &mut self,
        id: &str,
        config_dir: Option<&Path>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(command) = resume_command(id, config_dir) {
            // Claude finds a session by the folder it ran in, so a worktree's is resumed from there.
            let home = self.review.sessions.row(id).map(|r| r.root.clone());
            let index =
                home.and_then(|root| self.workspace.projects.iter().position(|p| p.root == root));
            if let Some(index) = index.filter(|i| Some(*i) != self.workspace.active) {
                self.switch_to(index, cx);
            }
            self.run_in_new_terminal(command, window, cx);
        }
    }

    fn open_session_file(
        &mut self,
        session: &str,
        path: &Path,
        base: Option<DiffBase>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_diff(path.to_path_buf(), change_base(session, base), window, cx);
    }

    fn start_review(&mut self, row: &SessionRow, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = Review::of(row) else {
            return;
        };
        let first = review.files[0].clone();
        self.review.sessions.review = Some(review);
        self.open_session_file(&row.id, &first.path, first.base, window, cx);
        cx.notify();
    }

    /// Moves the review to another file; with none running, starts one on the newest session
    /// that changed files.
    fn step_review(&mut self, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.sessions.review.is_none() {
            let rows = self.review.sessions.rows.clone();
            if let Some(row) = rows.iter().find(|r| !r.files.is_empty()) {
                self.start_review(row, window, cx);
            }
            return;
        }
        let Some(review) = self.review.sessions.review.as_mut() else {
            return;
        };
        let session = review.session.clone();
        let file = review.step(by).clone();
        self.open_session_file(&session, &file.path, file.base, window, cx);
        cx.notify();
    }

    fn revert_session_file(
        &mut self,
        session: String,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = &self.review.sessions;
        let Some(root) = state
            .row(&session)
            .map(|r| r.root.clone())
            .or(state.root.clone())
        else {
            return;
        };
        if self.refuse_unsaved_revert(&path, cx) {
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!(
                "Revert {} to before this Claude session?",
                file_label(&path)
            ),
            Some(
                "Athena keeps a copy of the current file in Application Support/athena/discarded \
                 for 30 days.",
            ),
            &confirm_buttons("Cancel", "Revert"),
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(CONFIRMED) {
                return;
            }
            let unsaved = this.update(cx, |this, cx| this.refuse_unsaved_revert(&path, cx));
            if unsaved.unwrap_or(true) {
                return;
            }
            let Ok(store) = snapshots::store() else {
                return;
            };
            let (job_root, job_path) = (root.clone(), path.clone());
            let done = cx
                .background_executor()
                .spawn(async move {
                    snapshots::revert(&store, &session, &job_root, &job_path, &backup_dir()?)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match done {
                    Err(err) => {
                        this.transient_notice("Could not revert the file", format!("{err:#}"), cx)
                    }
                    Ok(()) => {
                        for editor in this.editors_of(&path, cx) {
                            editor.update(cx, |e, cx| e.check_disk(cx));
                        }
                    }
                }
                this.forget_gutter_marks(&path);
                this.reload_diffs(&root, Some(&path), cx);
                this.git_kick(cx);
                this.claude_sessions_stale(cx);
            });
        })
        .detach();
    }

    /// Editors open on `path`, under whatever spelling of it they were opened by.
    fn editors_of(&self, path: &Path, cx: &App) -> Vec<Entity<EditorView>> {
        self.items
            .values()
            .filter_map(|view| match view {
                ItemView::Editor(e) if same_file(e.read(cx).path(), path) => Some(e.clone()),
                _ => None,
            })
            .collect()
    }

    /// Says so and returns true when `path` has unsaved edits, which a revert would lose and
    /// the next save would undo.
    fn refuse_unsaved_revert(&mut self, path: &Path, cx: &mut Context<Self>) -> bool {
        let unsaved = self
            .editors_of(path, cx)
            .iter()
            .any(|e| e.read(cx).is_dirty());
        if unsaved {
            self.transient_notice(
                format!("{} has unsaved changes", file_label(path)),
                "Save or revert them in the editor before reverting the session's edits.",
                cx,
            );
        }
        unsaved
    }

    fn open_session_menu(
        &mut self,
        row: SessionRow,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut items = Vec::new();
        if let Some(profile) = row.profile.clone()
            && !self.session_running(&row.id, cx)
        {
            let id = row.id.clone();
            items.push(shell_item(
                "Resume in New Terminal",
                cx,
                move |this, w, cx| {
                    this.resume_claude_session(&id, profile.config_dir.as_deref(), w, cx)
                },
            ));
        }
        if !row.files.is_empty() {
            let review = row.clone();
            items.push(shell_item("Review All Changes", cx, move |this, w, cx| {
                this.start_review(&review, w, cx)
            }));
        }
        let view_id = row.id.clone();
        items.push(shell_item("Open Session View", cx, move |this, w, cx| {
            this.open_session_view(view_id.clone(), w, cx)
        }));
        if !items.is_empty() {
            items.push(MenuItem::separator());
        }
        let id = row.id.clone();
        items.push(MenuItem::new("Copy Session ID", move |_, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(id.clone()))
        }));
        self.open_context_menu(position, items, window, cx);
    }

    fn open_session_file_menu(
        &mut self,
        session: String,
        path: PathBuf,
        base: Option<DiffBase>,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (s1, p1, p2, s3, p3) = (
            session.clone(),
            path.clone(),
            path.clone(),
            session,
            path.clone(),
        );
        let b1 = base.clone();
        let mut items = vec![
            shell_item("Open Changes", cx, move |this, w, cx| {
                this.open_session_file(&s1, &p1, b1.clone(), w, cx)
            }),
            shell_item("Open File", cx, move |this, w, cx| {
                this.open_file(p2.clone(), w, cx)
            }),
        ];
        // Only a snapshot can put a file back to how it was before the session.
        if base.is_none() {
            items.push(MenuItem::separator());
            items.push(shell_item("Revert File…", cx, move |this, w, cx| {
                this.revert_session_file(s3.clone(), p3.clone(), w, cx)
            }));
        }
        self.open_context_menu(position, items, window, cx);
    }

    fn render_review_bar(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let review = self.review.sessions.review.as_ref()?;
        let n = review.files.len();
        let at = review.index;
        let button = |id: &'static str, label: &'static str| {
            athena_ui::Button::new(id, label, ButtonKind::Ghost)
        };
        let text = format!(
            "Reviewing {} · file {} of {n}: {}",
            review.label,
            at + 1,
            file_label(&review.files[at].path)
        );
        let tip = text.clone();
        Some(
            div()
                .h(t.ui(ROW_HEIGHT + 4.))
                .px(t.space(Space::Lg))
                .flex()
                .items_center()
                .gap(t.space(Space::Md))
                .bg(t.color.surface_active)
                .child(
                    div()
                        .id("claude-review-label")
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(t.color.content)
                        .tooltip(move |_, cx| Tooltip::view(tip.clone(), cx))
                        .child(text),
                )
                .when(at > 0, |el| {
                    el.child(
                        button("claude-review-prev", "‹")
                            .on_click(cx.listener(|this, _, w, cx| this.step_review(-1, w, cx))),
                    )
                })
                .when(at + 1 < n, |el| {
                    el.child(
                        button("claude-review-next", "›")
                            .on_click(cx.listener(|this, _, w, cx| this.step_review(1, w, cx))),
                    )
                })
                .child(button("claude-review-done", "Done").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.review.sessions.review = None;
                        cx.notify();
                    },
                )))
                .into_any_element(),
        )
    }

    fn render_live_plan(&self, i: usize, row: &SessionRow, t: &Theme) -> Vec<AnyElement> {
        let Some(live) = self.review.sessions.live.get(&row.id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (j, todo) in live.todos.iter().enumerate() {
            let line = todo_lines(std::slice::from_ref(todo));
            let color = match todo.status.as_str() {
                "completed" => t.color.content_disabled,
                "in_progress" => t.color.content,
                _ => t.color.content_secondary,
            };
            out.push(
                detail_line(("claude-todo", i * 10_000 + j), line, t)
                    .text_color(color)
                    .into_any_element(),
            );
        }
        if let Some(plan) = &live.plan {
            let lines: Vec<&str> = plan.lines().filter(|l| !l.trim().is_empty()).collect();
            let full: String = plan.chars().take(4000).collect();
            let mut shown: Vec<String> = lines
                .iter()
                .take(PLAN_LINES)
                .map(|l| l.trim().to_string())
                .collect();
            if lines.len() > PLAN_LINES {
                shown.push(format!("… {} more lines", lines.len() - PLAN_LINES));
            }
            out.push(
                detail_line(
                    ("claude-plan", i),
                    format!("Plan: {}", shown.join(" · ")),
                    t,
                )
                .tooltip(move |_, cx| Tooltip::view(full.clone(), cx))
                .into_any_element(),
            );
        }
        out
    }

    fn render_file_row(
        &self,
        id: usize,
        session: &str,
        file: &FileChange,
        root: &Path,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rel = file
            .path
            .strip_prefix(root)
            .unwrap_or(&file.path)
            .display()
            .to_string();
        let tip = rel.clone();
        let (s_open, p_open, b_open) = (session.to_string(), file.path.clone(), file.base.clone());
        let (s_menu, p_menu, b_menu) = (session.to_string(), file.path.clone(), file.base.clone());
        let reviewing = self.review.sessions.review.as_ref().is_some_and(|r| {
            r.session == session && r.files.get(r.index).map(|f| &f.path) == Some(&file.path)
        });
        div()
            .id(("claude-file", id))
            .h(t.ui(ROW_HEIGHT - 2.))
            .pl(t.ui(48.))
            .pr(t.space(Space::Lg))
            .flex()
            .items_center()
            .gap(t.space(Space::Md))
            .cursor_pointer()
            .when(reviewing, |el| el.bg(t.color.surface_active))
            .hover(|s| s.bg(t.color.surface_hover))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_session_file(&s_open, &p_open, b_open.clone(), window, cx)
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_session_file_menu(
                        s_menu.clone(),
                        p_menu.clone(),
                        b_menu.clone(),
                        ev.position,
                        window,
                        cx,
                    )
                }),
            )
            .child(
                div()
                    .w(t.ui(10.))
                    .flex_none()
                    .text_color(match (file.created, file.deleted) {
                        (_, true) => t.color.danger,
                        (true, _) => t.color.success,
                        _ => t.color.warning,
                    })
                    .child(match (file.created, file.deleted) {
                        (_, true) => "D",
                        (true, _) => "A",
                        _ => "M",
                    }),
            )
            .child(
                div()
                    .id(("claude-file-path", id))
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(t.color.content)
                    .tooltip(move |_, cx| Tooltip::view(tip.clone(), cx))
                    .child(rel),
            )
            .children(file.counts.map(|(added, removed)| {
                div()
                    .flex_none()
                    .flex()
                    .gap(t.space(Space::Sm))
                    .child(div().text_color(t.color.success).child(format!("+{added}")))
                    .child(
                        div()
                            .text_color(t.color.danger)
                            .child(format!("−{removed}")),
                    )
            }))
            .into_any_element()
    }
}

fn detail_line(
    id: (&'static str, usize),
    text: impl Into<gpui::SharedString>,
    t: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(t.ui(ROW_HEIGHT - 4.))
        .pl(t.ui(48.))
        .pr(t.space(Space::Lg))
        .flex()
        .items_center()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_color(t.color.content_muted)
        .child(text.into())
}

/// Binds the Claude sessions commands on the shell's root element.
pub(super) fn bind_claude_actions(el: gpui::Div, cx: &mut Context<Shell>) -> gpui::Div {
    el.on_action(cx.listener(|this, _: &ShowClaudeSessions, _, cx| {
        this.select_side_mode(SideMode::Sessions, cx)
    }))
    .on_action(
        cx.listener(|this, _: &ClaudeOpenSessionView, w, cx| this.open_current_session_view(w, cx)),
    )
    .on_action(cx.listener(|this, _: &ClaudeReviewNextFile, w, cx| this.step_review(1, w, cx)))
    .on_action(cx.listener(|this, _: &ClaudeReviewPreviousFile, w, cx| this.step_review(-1, w, cx)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, root: &str, transcript: bool) -> SessionRow {
        SessionRow {
            id: id.into(),
            root: root.into(),
            label: format!("{root} {transcript}"),
            profile: transcript.then(|| Profile {
                name: "claude".into(),
                config_dir: None,
                dir: PathBuf::new(),
            }),
            modified: SystemTime::UNIX_EPOCH,
            messages: 0,
            files: Vec::new(),
        }
    }

    #[test]
    fn a_session_in_two_lists_takes_the_row_with_a_transcript_then_the_one_for_its_folder() {
        let (edits_only, own) = (row("s", "/main", false), row("s", "/wt", false));
        let transcript = row("s", "/wt", true);
        let picked = |rows: &[&SessionRow], cwd: &str| {
            pick_row(rows.iter().copied(), Some(Path::new(cwd)), None).map(|r| r.label.clone())
        };
        assert_eq!(
            picked(&[&edits_only, &transcript], "/main"),
            Some("/wt true".into()),
            "a transcript beats the folder match"
        );
        assert_eq!(
            picked(&[&edits_only, &own], "/wt"),
            Some("/wt false".into())
        );
        assert_eq!(
            picked(&[&own, &edits_only], "/main"),
            Some("/main false".into())
        );
        assert!(pick_row([], None, None).is_none());
        let (here, there) = (row("s", "/a", false), row("s", "/b", false));
        let by_active = |active: &str| {
            let rows = [&there, &here];
            pick_row(rows, None, Some(Path::new(active))).map(|r| r.root.clone())
        };
        assert_eq!(by_active("/a"), Some(PathBuf::from("/a")));
        assert_eq!(by_active("/b"), Some(PathBuf::from("/b")));

        let mut state = SessionsState {
            rows: Rc::new(vec![edits_only]),
            elsewhere: Rc::new(vec![own]),
            ..SessionsState::default()
        };
        state.cwds.insert("s".into(), "/wt".into());
        assert_eq!(
            state.row("s").map(|r| r.root.as_path()),
            Some(Path::new("/wt"))
        );
        state.cwds.insert("s".into(), "/main".into());
        assert_eq!(
            state.row("s").map(|r| r.root.as_path()),
            Some(Path::new("/main"))
        );
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("athena-cs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn bound(state: &SessionsState) -> Vec<(PaneId, &str)> {
        let mut all: Vec<_> = state.panes.iter().map(|(p, s)| (*p, s.as_str())).collect();
        all.sort();
        all
    }

    #[test]
    fn a_pane_runs_one_session_and_a_session_lives_in_one_pane() {
        let mut state = SessionsState::default();
        state.bind(1, "a".into());
        state.bind(1, "b".into());
        assert_eq!(
            bound(&state),
            [(1, "b")],
            "rebinding a pane replaces its session"
        );
        state.bind(2, "b".into());
        assert_eq!(
            bound(&state),
            [(2, "b")],
            "resuming elsewhere moves the session"
        );
        state.bind(1, "a".into());
        state.bind(2, "".into());
        assert_eq!(
            bound(&state),
            [(1, "a"), (2, "b")],
            "an empty id binds nothing"
        );
    }

    #[test]
    fn the_changeset_joins_transcripts_with_the_snapshot_store() {
        let dir = temp("join");
        let (home, store, root) = (dir.join("home"), dir.join("store"), dir.join("app"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let talked = "0b6c1e3a-1f2d-4c1b-9d55-1f0d2c3b4a5e";
        let silent = "1c7d2f4b-2a3e-4d2c-8e66-2a1e3d4c5b6f";
        let folder = home
            .join(".claude-work/projects")
            .join(transcripts::project_folder(&root));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join(format!("{talked}.jsonl")),
            "{\"type\":\"user\",\"message\":{\"content\":\"rename the handler\"}}\n",
        )
        .unwrap();

        let edited = root.join("src/main.go");
        std::fs::write(&edited, "a\nb\nc\n").unwrap();
        snapshots::take(&store, talked, &edited).unwrap();
        std::fs::write(&edited, "a\nB\nc\nd\n").unwrap();
        let created = root.join("new.go");
        snapshots::take(&store, silent, &created).unwrap();
        std::fs::write(&created, "x\ny\n").unwrap();

        let rows = load(&home, &store, &root);
        assert_eq!(rows.len(), 2);
        let talked_row = rows.iter().find(|r| r.id == talked).unwrap();
        assert_eq!(talked_row.label, "rename the handler");
        assert_eq!(
            talked_row.profile.as_ref().map(|p| p.name.as_str()),
            Some("claude-work")
        );
        assert_eq!(
            talked_row.files,
            [FileChange {
                path: edited,
                counts: Some((2, 1)),
                created: false,
                deleted: false,
                base: None
            }]
        );
        let silent_row = rows.iter().find(|r| r.id == silent).unwrap();
        assert_eq!(silent_row.label, "Session 1c7d2f4b");
        assert_eq!(silent_row.profile, None);
        assert_eq!(silent_row.files[0].counts, Some((2, 0)));
        assert!(silent_row.files[0].created);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resume_types_the_profiles_folder_quoted_and_only_for_a_real_session_id() {
        let id = "0b6c1e3a-1f2d-4c1b-9d55-1f0d2c3b4a5e";
        assert_eq!(
            resume_command(id, None).unwrap(),
            format!("claude --resume {id}")
        );
        assert_eq!(
            resume_command(id, Some(Path::new("/Users/me/.claude-work"))).unwrap(),
            format!("CLAUDE_CONFIG_DIR='/Users/me/.claude-work' claude --resume {id}")
        );
        assert_eq!(
            resume_command(id, Some(Path::new("/x/it's"))).unwrap(),
            format!("CLAUDE_CONFIG_DIR='/x/it'\\''s' claude --resume {id}")
        );
        assert_eq!(resume_command("x; rm -rf ~", None), None);
        assert_eq!(
            resume_command("--dangerously-skip-permissions", None),
            None,
            "a transcript named like a flag"
        );
        assert_eq!(
            resume_command(id, Some(Path::new("/x/a\nrm -rf ~"))),
            None,
            "a newline would run what follows"
        );
    }

    #[test]
    fn an_editor_opened_through_a_link_is_the_file_a_session_reverts() {
        let dir = temp("same");
        std::fs::write(dir.join("a.go"), "x").unwrap();
        std::os::unix::fs::symlink(dir.join("a.go"), dir.join("link.go")).unwrap();
        assert!(same_file(&dir.join("link.go"), &dir.join("a.go")));
        assert!(same_file(&dir.join("gone.go"), &dir.join("gone.go")));
        assert!(!same_file(&dir.join("a.go"), &dir.join("gone.go")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_review_opens_each_file_against_the_base_its_change_carries() {
        let fork = DiffBase::Revision {
            rev: "abcd".into(),
            at: "src/b.rs".into(),
        };
        let file = |name: &str, base| FileChange {
            path: PathBuf::from(name),
            counts: None,
            created: false,
            deleted: false,
            base,
        };
        let mut session = row("s-1", "/p", true);
        session.files = vec![
            file("/p/a.rs", None),
            file("/p/src/b.rs", Some(fork.clone())),
        ];
        let mut review = Review::of(&session).unwrap();
        let first = review.files[0].clone();
        assert_eq!(
            change_base("s-1", first.base),
            DiffBase::Snapshot {
                session: "s-1".into()
            }
        );
        let second = review.step(1).clone();
        assert_eq!(second.path, PathBuf::from("/p/src/b.rs"));
        assert_eq!(change_base("s-1", second.base), fork);
        assert_eq!(
            review.step(1).path,
            PathBuf::from("/p/src/b.rs"),
            "stops at the end"
        );
        session.files.clear();
        assert!(Review::of(&session).is_none());
    }

    #[test]
    fn todo_progress_counts_finished_ones() {
        let todo = |status: &str| TodoInfo {
            content: format!("do {status}"),
            status: status.into(),
            active_form: Some(format!("doing {status}")),
        };
        let todos = [todo("completed"), todo("in_progress"), todo("pending")];
        assert_eq!(progress(&todos).as_deref(), Some("1/3"));
        assert_eq!(progress(&[]), None);
        assert_eq!(
            todo_lines(&todos),
            "✓ do completed\n◐ doing in_progress\n○ do pending"
        );
    }
}
