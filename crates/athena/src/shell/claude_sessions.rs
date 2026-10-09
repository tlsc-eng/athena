use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use athena_editor::diff;
use athena_proto::{AppMsg, PaneId, TodoInfo};
use athena_ui::{ActiveTheme, ButtonKind, MenuItem, Theme, Tooltip, empty_state};
use athena_workspace::DiffBase;
use athena_workspace::git::relative_time;
use gpui::{
    AnyElement, ClipboardItem, Context, FontWeight, MouseButton, MouseDownEvent, PromptLevel, Task,
    Window, actions, div, prelude::*, px,
};

use super::Shell;
use super::drawer::DrawerTab;
use super::git_view::row_button;
use super::item::{ItemView, file_label};
use super::lsp::{CONFIRMED, confirm_buttons};
use super::menus::shell_item;
use super::review::backup_dir;
use crate::snapshots::{self, Before};
use crate::transcripts::{self, Profile, Tokens};

actions!(
    athena,
    [
        ShowClaudeSessions,
        ClaudeReviewNextFile,
        ClaudeReviewPreviousFile
    ]
);

const SESSIONS: usize = 30;
/// A list older than this is read again the next time the tab draws.
const STALE_AFTER: Duration = Duration::from_secs(10);
/// Files bigger than this get no line counts; diffing them would hold the list up.
const MAX_COUNTED: u64 = 2 * 1024 * 1024;
const ROW_HEIGHT: f32 = 26.;
const PLAN_LINES: usize = 4;

/// The Claude tab: recent sessions of the active project, with what each changed.
#[derive(Default)]
pub(super) struct SessionsState {
    root: Option<PathBuf>,
    rows: Rc<Vec<SessionRow>>,
    hooks_on: bool,
    loading: Option<Task<()>>,
    loaded_at: Option<Instant>,
    expanded: HashSet<String>,
    /// Todo lists and plans the hooks reported, by session id.
    live: HashMap<String, LivePlan>,
    /// The session each terminal runs, as its hooks said.
    panes: HashMap<PaneId, String>,
    review: Option<Review>,
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
    files: Vec<PathBuf>,
    index: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct SessionRow {
    id: String,
    label: String,
    /// The config folder its transcript is in; `None` when only edits were recorded.
    profile: Option<Profile>,
    modified: SystemTime,
    messages: usize,
    tokens: BTreeMap<String, Tokens>,
    files: Vec<FileChange>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct FileChange {
    path: PathBuf,
    /// Lines added and removed since before the session, when known.
    counts: Option<(usize, usize)>,
    created: bool,
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
            profile: Some(s.profile),
            modified: s.modified,
            messages: s.summary.messages,
            tokens: s.summary.tokens,
            files: Vec::new(),
        })
        .collect();
    for stored in snapshots::sessions_under(store, root) {
        let files = stored
            .files
            .iter()
            .map(|path| FileChange {
                counts: line_counts(store, &stored.session, path),
                created: snapshots::read(store, &stored.session, path) == Before::Absent,
                path: path.clone(),
            })
            .collect();
        match rows.iter_mut().find(|r| r.id == stored.session) {
            Some(row) => row.files = files,
            None => rows.push(SessionRow {
                label: format!("Session {}", short_id(&stored.session)),
                id: stored.session,
                profile: None,
                modified: stored.modified,
                messages: 0,
                tokens: BTreeMap::new(),
                files,
            }),
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.modified));
    rows.truncate(SESSIONS);
    rows
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

fn cost_label(
    tokens: &BTreeMap<String, Tokens>,
    prices: &HashMap<String, transcripts::Price>,
) -> String {
    if tokens.is_empty() {
        return String::new();
    }
    match transcripts::cost(tokens, prices) {
        (_, unpriced) if unpriced.len() == tokens.len() => "cost n/a".into(),
        (usd, unpriced) if unpriced.is_empty() => format!("≈ ${usd:.2}"),
        (usd, _) => format!("≈ ${usd:.2}+"),
    }
}

fn usage_tooltip(row: &SessionRow, prices: &HashMap<String, transcripts::Price>) -> String {
    let mut lines = vec![format!("Session {}", row.id)];
    if let Some(profile) = &row.profile {
        lines.push(format!("Profile: {}", profile.name));
    }
    let mut sum = Tokens::default();
    for t in row.tokens.values() {
        sum.input += t.input;
        sum.output += t.output;
        sum.cache_write_5m += t.cache_write_5m + t.cache_write_1h;
        sum.cache_read += t.cache_read;
    }
    if !row.tokens.is_empty() {
        let n = transcripts::short_count;
        lines.push(format!(
            "Tokens: {} in · {} out · {} cache write · {} cache read",
            n(sum.input),
            n(sum.output),
            n(sum.cache_write_5m),
            n(sum.cache_read)
        ));
        let (usd, unpriced) = transcripts::cost(&row.tokens, prices);
        lines.push(format!(
            "Estimated cost ${usd:.2} at list prices; not a bill. Set prices with \"claude.prices\" in settings.json."
        ));
        if !unpriced.is_empty() {
            lines.push(format!("No price for {}", unpriced.join(", ")));
        }
    }
    lines.join("\n")
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
            state.panes.insert(pane, session);
        }
        cx.notify();
    }

    /// Something changed on disk or in a session: read the list again when it next draws.
    pub(super) fn claude_sessions_stale(&mut self, cx: &mut Context<Self>) {
        self.review.sessions.loaded_at = None;
        if self.drawer == Some(DrawerTab::Claude) {
            cx.notify();
        }
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
                        t.read(cx).session() == Some(*pane) && v.claude_state(cx).is_some()
                    }
                    _ => false,
                })
        })
    }

    fn load_claude_sessions(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let state = &mut self.review.sessions;
        if state.root.as_ref() != Some(&root) {
            state.rows = Rc::default();
            state.expanded.clear();
        }
        state.root = Some(root.clone());
        state.loaded_at = Some(Instant::now());
        let (Some(home), Ok(store)) = (std::env::home_dir(), snapshots::store()) else {
            return;
        };
        state.loading = Some(cx.spawn(async move |this, cx| {
            let task_root = root.clone();
            let (rows, hooks_on) = cx
                .background_executor()
                .spawn(async move {
                    let rows = load(&home, &store, &task_root);
                    (rows, crate::claude_hooks::enabled(&task_root))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let state = &mut this.review.sessions;
                if state.root.as_ref() == Some(&root) {
                    state.loading = None;
                    state.hooks_on = hooks_on;
                    // The newest session opens on first view: it is usually the one to review.
                    if state.rows.is_empty()
                        && state.expanded.is_empty()
                        && let Some(first) = rows.first()
                    {
                        state.expanded.insert(first.id.clone());
                    }
                    if *state.rows != rows {
                        state.rows = Rc::new(rows);
                    }
                    cx.notify();
                }
            });
        }));
    }

    pub(super) fn resume_claude_session(
        &mut self,
        id: &str,
        config_dir: Option<&Path>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(command) = resume_command(id, config_dir) {
            self.run_in_new_terminal(command, window, cx);
        }
    }

    fn open_session_file(
        &mut self,
        session: &str,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let base = DiffBase::Snapshot {
            session: session.to_string(),
        };
        self.open_diff(path.to_path_buf(), base, window, cx);
    }

    fn start_review(&mut self, row: &SessionRow, window: &mut Window, cx: &mut Context<Self>) {
        let files: Vec<PathBuf> = row.files.iter().map(|f| f.path.clone()).collect();
        let Some(first) = files.first().cloned() else {
            return;
        };
        self.review.sessions.review = Some(Review {
            session: row.id.clone(),
            label: row.label.clone(),
            files,
            index: 0,
        });
        self.open_session_file(&row.id, &first, window, cx);
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
        let last = review.files.len().saturating_sub(1) as isize;
        review.index = (review.index as isize + by).clamp(0, last) as usize;
        let (session, path) = (review.session.clone(), review.files[review.index].clone());
        self.open_session_file(&session, &path, window, cx);
        cx.notify();
    }

    fn revert_session_file(
        &mut self,
        session: String,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.review.sessions.root.clone() else {
            return;
        };
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
                if let Err(err) = done {
                    this.transient_notice("Could not revert the file", format!("{err:#}"), cx);
                }
                this.forget_gutter_marks(&path);
                this.reload_diffs(&root, Some(&path), cx);
                this.git_kick(cx);
                this.claude_sessions_stale(cx);
            });
        })
        .detach();
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
        let items = vec![
            shell_item("Open Changes", cx, move |this, w, cx| {
                this.open_session_file(&s1, &p1, w, cx)
            }),
            shell_item("Open File", cx, move |this, w, cx| {
                this.open_file(p2.clone(), w, cx)
            }),
            MenuItem::separator(),
            shell_item("Revert File…", cx, move |this, w, cx| {
                this.revert_session_file(s3.clone(), p3.clone(), w, cx)
            }),
        ];
        self.open_context_menu(position, items, window, cx);
    }

    /// The session count and Refresh, beside the drawer tabs.
    pub(super) fn render_claude_sessions_action(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let t = cx.theme().clone();
        let state = &self.review.sessions;
        let count = match (state.loading.is_some(), state.rows.len()) {
            (true, 0) => "Reading sessions…".to_string(),
            (_, 1) => "1 session".into(),
            (_, n) => format!("{n} sessions"),
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(div().text_color(t.color.content_muted).child(count))
                .child(
                    athena_ui::Button::new("claude-refresh", "Refresh", ButtonKind::Ghost)
                        .on_click(cx.listener(|this, _, _, cx| this.claude_sessions_stale(cx))),
                )
                .into_any_element(),
        )
    }

    pub(super) fn render_claude_sessions(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let centered = |el: gpui::Div| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(el)
                .into_any_element()
        };
        let Some(root) = self.active_root() else {
            return centered(empty_state(
                "No project open",
                "Open a folder to see its Claude sessions.",
                None,
                cx,
            ));
        };
        let state = &self.review.sessions;
        let stale = state.loaded_at.is_none_or(|at| at.elapsed() > STALE_AFTER);
        if state.root.as_ref() != Some(&root) || (stale && state.loading.is_none()) {
            self.load_claude_sessions(root.clone(), cx);
        }
        let state = &self.review.sessions;
        if state.rows.is_empty() {
            if state.loading.is_some() {
                return centered(
                    div()
                        .text_size(t.typography.caption)
                        .text_color(t.color.content_muted)
                        .child("Reading sessions…"),
                );
            }
            let enable = (!state.hooks_on).then(|| {
                athena_ui::Button::new("claude-enable-hooks", "Enable Hooks", ButtonKind::Primary)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.set_claude_hooks(true, window, cx)),
                    )
            });
            return centered(empty_state(
                "No Claude sessions yet",
                "Sessions run in this project are listed here with their token use. With Claude \
                 Code hooks on, each also lists the files it changed.",
                enable,
                cx,
            ));
        }
        let rows = state.rows.clone();
        let prices = self.settings.file.claude_prices.clone();
        let mut out: Vec<AnyElement> = Vec::new();
        if let Some(bar) = self.render_review_bar(&t, cx) {
            out.push(bar);
        }
        let now = SystemTime::now();
        for (i, row) in rows.iter().enumerate() {
            out.push(self.render_session_row(i, row, &prices, now, &t, cx));
            if !self.review.sessions.expanded.contains(&row.id) {
                continue;
            }
            out.extend(self.render_live_plan(i, row, &t));
            if row.files.is_empty() {
                let hint = match self.review.sessions.hooks_on {
                    true => "No edits recorded for this session.",
                    false => {
                        "No edits recorded. Enable Claude Code hooks to record the next session's."
                    }
                };
                out.push(detail_line(("claude-nofiles", i), hint, &t).into_any_element());
            }
            for (j, file) in row.files.iter().enumerate() {
                out.push(self.render_file_row(i * 10_000 + j, &row.id, file, &root, &t, cx));
            }
        }
        div()
            .id("claude-sessions")
            .size_full()
            .overflow_y_scroll()
            .text_size(t.typography.caption)
            .children(out)
            .into_any_element()
    }

    fn render_review_bar(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let review = self.review.sessions.review.as_ref()?;
        let n = review.files.len();
        let at = review.index;
        let button = |id: &'static str, label: &'static str| {
            athena_ui::Button::new(id, label, ButtonKind::Ghost)
        };
        Some(
            div()
                .h(t.ui(ROW_HEIGHT + 4.))
                .px(px(12.))
                .flex()
                .items_center()
                .gap(px(8.))
                .bg(t.color.surface_active)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(t.color.content)
                        .child(format!(
                            "Reviewing {} · file {} of {n}: {}",
                            review.label,
                            at + 1,
                            file_label(&review.files[at])
                        )),
                )
                .when(at > 0, |el| {
                    el.child(
                        button("claude-review-prev", "‹ Previous")
                            .on_click(cx.listener(|this, _, w, cx| this.step_review(-1, w, cx))),
                    )
                })
                .when(at + 1 < n, |el| {
                    el.child(
                        button("claude-review-next", "Next ›")
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

    fn render_session_row(
        &self,
        i: usize,
        row: &SessionRow,
        prices: &HashMap<String, transcripts::Price>,
        now: SystemTime,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let group = format!("claude-session-{i}");
        let open = self.review.sessions.expanded.contains(&row.id);
        let running = self.session_running(&row.id, cx);
        let live = self.review.sessions.live.get(&row.id);
        let todo_progress = live.and_then(|l| progress(&l.todos));
        let tip = usage_tooltip(row, prices);
        let id = row.id.clone();
        let menu = row.clone();
        let review = row.clone();
        let resume = row.profile.clone().filter(|_| !running);
        let resume_id = row.id.clone();
        let ago = now
            .duration_since(row.modified)
            .unwrap_or_default()
            .as_secs() as i64;
        let muted = |text: String| {
            div()
                .flex_none()
                .text_color(t.color.content_muted)
                .child(text)
        };
        div()
            .id(("claude-session", i))
            .group(group.clone())
            .h(t.ui(ROW_HEIGHT))
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(8.))
            .cursor_pointer()
            .hover(|s| s.bg(t.color.surface_hover))
            .tooltip(move |_, cx| Tooltip::view(tip.clone(), cx))
            .on_click(cx.listener(move |this, _, _, cx| {
                let expanded = &mut this.review.sessions.expanded;
                if !expanded.remove(&id) {
                    expanded.insert(id.clone());
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_session_menu(menu.clone(), ev.position, window, cx)
                }),
            )
            .child(
                div()
                    .w(px(10.))
                    .flex_none()
                    .text_color(t.color.content_muted)
                    .child(if open { "▾" } else { "▸" }),
            )
            .child(div().size(px(6.)).flex_none().bg(if running {
                t.color.success
            } else {
                t.color.content_disabled
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.color.content)
                    .child(row.label.clone()),
            )
            .children(todo_progress.map(|p| {
                div()
                    .flex_none()
                    .px(px(6.))
                    .rounded(t.shape.radius_control)
                    .bg(t.color.surface_active)
                    .text_color(t.color.content_secondary)
                    .child(p)
            }))
            .when(!row.files.is_empty(), |el| {
                el.child(row_button(
                    ("claude-review-all", i),
                    "Review All",
                    group.clone(),
                    t,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.start_review(&review, window, cx)
                    }),
                ))
            })
            .children(resume.map(|profile| {
                row_button(
                    ("claude-resume", i),
                    "Resume",
                    group.clone(),
                    t,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.resume_claude_session(
                            &resume_id,
                            profile.config_dir.as_deref(),
                            window,
                            cx,
                        )
                    }),
                )
            }))
            .children(
                row.profile
                    .as_ref()
                    .filter(|p| p.config_dir.is_some())
                    .map(|p| muted(p.name.clone())),
            )
            .when(!row.files.is_empty(), |el| {
                let n = row.files.len();
                el.child(muted(format!("{n} file{}", if n == 1 { "" } else { "s" })))
            })
            .when(row.messages > 0, |el| {
                el.child(muted(format!("{} msgs", row.messages)))
            })
            .when(!row.tokens.is_empty(), |el| {
                let total = row.tokens.values().map(Tokens::total).sum();
                el.child(muted(format!("{} tok", transcripts::short_count(total))))
                    .child(muted(cost_label(&row.tokens, prices)))
            })
            .child(
                div()
                    .w(px(88.))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .text_color(t.color.content_muted)
                    .child(relative_time(ago)),
            )
            .into_any_element()
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
        let group = format!("claude-file-{id}");
        let rel = file
            .path
            .strip_prefix(root)
            .unwrap_or(&file.path)
            .display()
            .to_string();
        let (s_open, p_open) = (session.to_string(), file.path.clone());
        let (s_diff, p_diff) = (session.to_string(), file.path.clone());
        let (s_rev, p_rev) = (session.to_string(), file.path.clone());
        let (s_menu, p_menu) = (session.to_string(), file.path.clone());
        let reviewing = self
            .review
            .sessions
            .review
            .as_ref()
            .is_some_and(|r| r.session == session && r.files.get(r.index) == Some(&file.path));
        div()
            .id(("claude-file", id))
            .group(group.clone())
            .h(t.ui(ROW_HEIGHT - 2.))
            .pl(px(48.))
            .pr(px(12.))
            .flex()
            .items_center()
            .gap(px(8.))
            .cursor_pointer()
            .when(reviewing, |el| el.bg(t.color.surface_active))
            .hover(|s| s.bg(t.color.surface_hover))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_session_file(&s_open, &p_open, window, cx)
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_session_file_menu(
                        s_menu.clone(),
                        p_menu.clone(),
                        ev.position,
                        window,
                        cx,
                    )
                }),
            )
            .child(
                div()
                    .w(px(10.))
                    .flex_none()
                    .text_color(if file.created {
                        t.color.success
                    } else {
                        t.color.warning
                    })
                    .child(if file.created { "A" } else { "M" }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(t.color.content)
                    .child(rel),
            )
            .child(row_button(
                ("claude-file-diff", id),
                "Open Changes",
                group.clone(),
                t,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.open_session_file(&s_diff, &p_diff, window, cx)
                }),
            ))
            .child(row_button(
                ("claude-file-revert", id),
                "Revert",
                group,
                t,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.revert_session_file(s_rev.clone(), p_rev.clone(), window, cx)
                }),
            ))
            .children(file.counts.map(|(added, removed)| {
                div()
                    .flex_none()
                    .flex()
                    .gap(px(6.))
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
        .pl(px(48.))
        .pr(px(12.))
        .flex()
        .items_center()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_color(t.color.content_muted)
        .child(text.into())
}

/// Binds the Claude tab's commands on the shell's root element.
pub(super) fn bind_claude_actions(el: gpui::Div, cx: &mut Context<Shell>) -> gpui::Div {
    el.on_action(cx.listener(|this, _: &ShowClaudeSessions, _, cx| {
        this.toggle_drawer_tab(DrawerTab::Claude, cx)
    }))
    .on_action(cx.listener(|this, _: &ClaudeReviewNextFile, w, cx| this.step_review(1, w, cx)))
    .on_action(cx.listener(|this, _: &ClaudeReviewPreviousFile, w, cx| this.step_review(-1, w, cx)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("athena-cs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
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
                created: false
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
