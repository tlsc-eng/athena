use std::collections::HashSet;
use std::f32::consts::{FRAC_PI_2, TAU};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use athena_proto::PaneId;
use athena_term::ClaudeState;
use athena_ui::motion::{Closing, Opening};
use athena_ui::{
    ActiveTheme, Button, ButtonKind, DotState, Ellipsis, IconButton, ListRow, Size, Space,
    StatusDot, Theme, empty_state, motion,
};
use athena_workspace::SideMode;
use gpui::{
    Animation, AnyElement, Context, Div, ElementId, FontWeight, MouseDownEvent, Svg, Task,
    Transformation, Window, div, prelude::*, px, radians, svg,
};

use super::live::{LiveSession, Place};
use super::{ROW_HEIGHT, SessionRow, SessionsState, Shell};
use crate::shell::sidebar::Tween;

/// Above this many child rows an expanding session only fades, so a long list does not sweep.
const MAX_SWEPT_ROWS: usize = 12;
const SPIN_TICK_MS: u128 = 125;
const SPIN_STEPS: u128 = 8;

/// What a row says besides its label, as shown in its detail line.
#[derive(Debug, PartialEq)]
pub(super) struct RowText {
    /// `3/7`: finished todos out of all of them.
    pub todos: Option<String>,
    pub files: Option<String>,
    pub branch: Option<String>,
}

pub(super) fn row_text(session: &LiveSession) -> RowText {
    RowText {
        todos: session.todos.map(|(done, all)| format!("{done}/{all}")),
        files: match session.files {
            0 => None,
            1 => Some("1 file".into()),
            n => Some(format!("{n} files")),
        },
        branch: session.branch.clone(),
    }
}

pub(super) fn dot_state(state: Option<ClaudeState>, unread: bool) -> DotState {
    match state {
        Some(ClaudeState::Running) => DotState::Running,
        Some(ClaudeState::NeedsInput) => DotState::NeedsInput,
        Some(ClaudeState::Finished) if unread => DotState::Unread,
        Some(ClaudeState::Finished) => DotState::Finished,
        None => DotState::Idle,
    }
}

const DAY: i64 = 86_400;

/// How long ago, in the fewest characters: `now`, `5m`, `3h`, `2d`, `4w`, `1y`.
fn short_age(secs: i64) -> String {
    if secs < 60 {
        "now".into()
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < DAY {
        format!("{}h", secs / 3_600)
    } else if secs < 7 * DAY {
        format!("{}d", secs / DAY)
    } else if secs < 365 * DAY {
        format!("{}w", secs / (7 * DAY))
    } else {
        format!("{}y", secs / (365 * DAY))
    }
}

/// A group of rows under one header, newest group first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Section {
    Active,
    Today,
    Yesterday,
    Week,
    Older,
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Section::Active => "Active",
            Section::Today => "Today",
            Section::Yesterday => "Yesterday",
            Section::Week => "Previous 7 Days",
            Section::Older => "Older",
        }
    }

    /// `flipped` holds the sections the user toggled away from their default, which is open except
    /// for the oldest.
    fn folded(self, flipped: &HashSet<Section>) -> bool {
        (self == Section::Older) != flipped.contains(&self)
    }
}

/// Where the date sections start, in unix seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cutoffs {
    today: i64,
    yesterday: i64,
    week: i64,
}

impl Cutoffs {
    fn at(now: SystemTime) -> Self {
        Self {
            today: local_midnight(now, 0),
            yesterday: local_midnight(now, 1),
            week: local_midnight(now, 7),
        }
    }
}

/// The section a session sits in.
fn section(live: bool, modified: i64, cuts: Cutoffs) -> Section {
    if live {
        Section::Active
    } else if modified >= cuts.today {
        Section::Today
    } else if modified >= cuts.yesterday {
        Section::Yesterday
    } else if modified >= cuts.week {
        Section::Week
    } else {
        Section::Older
    }
}

/// The start of the local day `days_back` before the one containing `now`, in unix seconds.
fn local_midnight(now: SystemTime, days_back: i32) -> i64 {
    let t = unix_secs(now) as libc::time_t;
    // SAFETY: localtime_r and mktime only touch the tm they are given.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm.tm_hour = 0;
        tm.tm_min = 0;
        tm.tm_sec = 0;
        tm.tm_mday -= days_back;
        // mktime picks the zone's offset for that day, so a DST change does not shift it.
        tm.tm_isdst = -1;
        libc::mktime(&mut tm) as i64
    }
}

fn unix_secs(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The disclosure arrow, pointing right and turned down once `turn` reaches 1.
fn chevron(turn: f32, t: &Theme) -> Svg {
    svg()
        .path("ui/chevron.svg")
        .size(t.ui(10.))
        .flex_none()
        .text_color(t.color.content_muted)
        .when(turn > 0., |s| {
            s.with_transformation(Transformation::rotate(radians(FRAC_PI_2 * turn)))
        })
}

/// The running glyph's turn after `elapsed`: one eighth of a revolution per tick.
fn spin_step(elapsed: Duration) -> f32 {
    ((elapsed.as_millis() / SPIN_TICK_MS) % SPIN_STEPS) as f32 * TAU / SPIN_STEPS as f32
}

fn should_spin(visible: bool, any_running: bool, reduced: bool) -> bool {
    visible && any_running && !reduced
}

/// Whether a tick goes on: only if the list drew since the last one, else nothing shows it.
fn keep_spinning(drawn: bool) -> bool {
    drawn
}

/// The wait to the next tick boundary after `elapsed`, so ticks stay on the grid `spin_step` reads.
fn until_next_tick(elapsed: Duration) -> Duration {
    let tick = SPIN_TICK_MS as u64;
    Duration::from_millis(tick - elapsed.as_millis() as u64 % tick)
}

/// The height the children of an expanding row reach, or None when there are too many to sweep.
fn expand_height(lines: usize, files: usize, scale: f32) -> Option<f32> {
    (lines + files <= MAX_SWEPT_ROWS)
        .then_some((lines as f32 * (ROW_HEIGHT - 4.) + files as f32 * (ROW_HEIGHT - 2.)) * scale)
}

fn section_key(section: Section) -> String {
    format!("section:{section:?}")
}

impl SessionsState {
    /// Turns the chevron toward `open`, from wherever a turn in flight had got to.
    fn turn(&mut self, key: String, open: bool, reduced: bool, fast: Duration) {
        let from = self.turning.get(&key).map_or(f32::from(!open), |t| {
            t.at(Instant::now(), fast, motion::ease_standard())
        });
        match Tween::start(reduced, open, from) {
            Some(tween) => self.turning.insert(key, tween),
            None => self.turning.remove(&key),
        };
    }

    fn chevron_turn(&self, key: &str, open: bool, fast: Duration) -> f32 {
        self.turning.get(key).map_or(f32::from(open), |t| {
            t.at(Instant::now(), fast, motion::ease_standard())
        })
    }
}

/// Wraps a session's child rows so they sweep open, or shut while `closing`.
fn animate_children(
    kids: Vec<AnyElement>,
    (lines, files): (usize, usize),
    (closing, entering): (Option<Closing>, bool),
    id: &str,
    t: &Theme,
) -> AnyElement {
    let height = expand_height(lines, files, t.ui_scale);
    let wrap = div().overflow_hidden().children(kids);
    let shown = move |el: Div, d: f32| {
        let el = el.opacity(d);
        match height {
            Some(h) => el.h(px(h * d)),
            None => el,
        }
    };
    match closing {
        Some(c) => motion::animate_exit(
            t.motion.reduced,
            wrap,
            ElementId::NamedInteger(format!("session-collapse-{id}").into(), c.generation),
            t.motion.fast,
            move |el, d| shown(el, 1. - d),
        ),
        None => motion::animate_enter(
            t.motion.reduced,
            entering,
            wrap,
            ElementId::Name(format!("session-expand-{id}").into()),
            Animation::new(t.motion.base).with_easing(motion::ease_standard()),
            shown,
        ),
    }
}

/// Whether a session shows in the project at `active`: it is in the same repository and below the
/// same folder of its work tree, so a project at a work tree's top lists every checkout's sessions
/// and a package lists that package's in any checkout, not its siblings'.
pub(super) fn in_project(session: &LiveSession, active: &Place) -> bool {
    session.main_root == active.main_root && session.rel.starts_with(&active.rel)
}

/// What a session with no row read yet offers a menu: its id.
pub(super) fn bare_row(session: &LiveSession) -> SessionRow {
    SessionRow {
        id: session.id.clone(),
        root: session.cwd.clone(),
        label: session.label.clone(),
        profile: None,
        modified: session.modified,
        messages: 0,
        files: Vec::new(),
    }
}

fn centered(el: impl IntoElement) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(el)
        .into_any_element()
}

impl Shell {
    /// Sessions mode: the active project's sessions, in the main checkout and every worktree.
    pub(in crate::shell) fn render_sessions_list(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        // Taken so every early return below stops the spinner; the end puts it back if it still spins.
        let spin = self.review.sessions.spin.take();
        let Some(root) = self.active_root() else {
            return centered(empty_state(
                "No project open",
                "Open a folder to see its Claude sessions.",
                None,
                cx,
            ));
        };
        self.refresh_sessions(&root, cx);
        let active = self.review.sessions.place(&root);
        let sessions: Vec<LiveSession> = self
            .live_sessions(cx)
            .into_iter()
            .filter(|s| in_project(s, &active))
            .collect();
        if sessions.is_empty() {
            if self.review.sessions.loading.is_some() {
                return centered(
                    div()
                        .text_size(t.typography.caption)
                        .text_color(t.color.content_muted)
                        .child("Reading sessions…"),
                );
            }
            let start = Button::new(
                "claude-new-session",
                "New Claude Session",
                ButtonKind::Secondary,
            )
            .on_click(cx.listener(|this, _, window, cx| this.new_claude_session(window, cx)));
            return centered(empty_state(
                "No Claude sessions yet",
                "Sessions run in this project are listed here.",
                Some(start),
                cx,
            ));
        }
        let mut out: Vec<AnyElement> = Vec::new();
        if let Some(bar) = self.render_review_bar(&t, cx) {
            out.push(bar);
        }
        let now = SystemTime::now();
        let (now_secs, cuts) = (unix_secs(now), Cutoffs::at(now));
        let (active, mut history): (Vec<_>, Vec<_>) =
            sessions.iter().partition(|s| s.state.is_some());
        history.sort_by_key(|s| std::cmp::Reverse(s.modified));
        let mut sections: Vec<(Section, Vec<(usize, &LiveSession)>)> = Vec::new();
        for (i, session) in active.into_iter().chain(history).enumerate() {
            let at = section(session.state.is_some(), unix_secs(session.modified), cuts);
            match sections.last_mut() {
                Some((last, rows)) if *last == at => rows.push((i, session)),
                _ => sections.push((at, vec![(i, session)])),
            }
        }
        self.track_looks(&sessions);
        let headed =
            sections.len() > 1 || sections.first().is_some_and(|(s, _)| *s != Section::Active);
        let current = self.current_session_id(cx);
        let muted = !window.is_window_active();
        let angle = spin
            .as_ref()
            .map_or(0., |(start, _)| spin_step(start.elapsed()));
        let mut any_running = false;
        for (n, (at, rows)) in sections.iter().enumerate() {
            let folded = headed && at.folded(&self.review.sessions.folded);
            if headed {
                out.push(self.render_section_header(n, *at, rows.len(), folded, &t, cx));
            }
            if folded {
                continue;
            }
            let mut body: Vec<AnyElement> = Vec::new();
            for &(i, session) in rows {
                any_running |= session.state == Some(ClaudeState::Running);
                let selected = current.as_deref() == Some(session.id.as_str());
                body.push(self.render_live_row(
                    i,
                    session,
                    (selected, muted),
                    angle,
                    now_secs - unix_secs(session.modified),
                    cx,
                ));
                let state = &self.review.sessions;
                let closing = state.collapsing.get(&session.id).copied();
                if !state.expanded.contains(&session.id) && closing.is_none() {
                    continue;
                }
                let Some(row) = state.row(&session.id).cloned() else {
                    continue;
                };
                let entering = state
                    .entering
                    .get(&session.id)
                    .is_some_and(|o| o.running(t.motion.base));
                let mut kids = self.render_live_plan(i, &row, &t);
                let lines = kids.len();
                for (j, file) in row.files.iter().enumerate() {
                    kids.push(self.render_file_row(
                        i * 10_000 + j,
                        &row.id,
                        file,
                        &row.root,
                        &t,
                        cx,
                    ));
                }
                body.push(animate_children(
                    kids,
                    (lines, row.files.len()),
                    (closing, entering),
                    &row.id,
                    &t,
                ));
            }
            if headed {
                let key = section_key(*at);
                let entering = self
                    .review
                    .sessions
                    .entering
                    .get(&key)
                    .is_some_and(|o| o.running(t.motion.base));
                out.push(motion::animate_enter(
                    t.motion.reduced,
                    entering,
                    div().children(body),
                    ElementId::Name(format!("section-rows-{key}").into()),
                    Animation::new(t.motion.base).with_easing(motion::ease_standard()),
                    |el, d| el.opacity(d),
                ));
            } else {
                out.extend(body);
            }
        }
        let spinning = should_spin(self.list_shown(), any_running, t.motion.reduced);
        self.review.sessions.spin_drawn = spinning;
        self.review.sessions.spin = if spinning {
            spin.or_else(|| Some(Self::start_spin(cx)))
        } else {
            None
        };
        div()
            .id("claude-sessions")
            .size_full()
            .overflow_y_scroll()
            .text_size(t.typography.caption)
            .children(out)
            .into_any_element()
    }

    fn render_section_header(
        &self,
        n: usize,
        section: Section,
        count: usize,
        folded: bool,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = if folded {
            format!("{} · {count}", section.label())
        } else {
            section.label().to_string()
        };
        div()
            .id(("claude-section", n))
            .h(t.size(Size::Row))
            .mx(t.space(Space::Sm))
            .pl(t.space(Space::Md))
            .pr(t.space(Space::Md))
            .flex()
            .items_center()
            .gap(t.space(Space::Sm))
            .cursor_pointer()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(t.color.content_muted)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_section(section, cx)))
            .child(chevron(
                self.review
                    .sessions
                    .chevron_turn(&section_key(section), !folded, t.motion.fast),
                t,
            ))
            .child(label)
            .into_any_element()
    }

    fn render_live_row(
        &self,
        i: usize,
        session: &LiveSession,
        (selected, muted): (bool, bool),
        angle: f32,
        age: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let c = &t.color;
        let text = row_text(session);
        let dot = dot_state(session.state, session.unread);
        let live = session.state.is_some();
        let expandable = session.files > 0;
        let open = expandable && self.review.sessions.expanded.contains(&session.id);
        let turn = self
            .review
            .sessions
            .chevron_turn(&session.id, open, t.motion.fast);
        let (toggle_id, open_id, view_id) =
            (session.id.clone(), session.id.clone(), session.id.clone());
        let group = format!("claude-live-session-{i}");
        let menu_for = session.clone();
        let pane = session.pane;
        let chevron_slot = div()
            .id(("session-chevron", i))
            .w(t.ui(10.))
            .flex_none()
            .when(expandable, |el| {
                el.cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_session(&toggle_id, cx);
                    }))
                    .child(
                        chevron(turn, &t).group_hover(group.clone(), |s| s.text_color(c.content)),
                    )
            });
        let phrase = session.state.map(|state| match state {
            ClaudeState::NeedsInput => ("Needs input", c.warning),
            ClaudeState::Finished if session.unread => ("Done", c.success),
            ClaudeState::Finished => ("Done", c.content_muted),
            ClaudeState::Running => ("Running", c.content_muted),
        });
        let detail = phrase.map(|(phrase, color)| {
            let dot_sep = || div().flex_none().child("·");
            div()
                .min_w_0()
                .flex()
                .items_center()
                .gap(t.space(Space::Sm))
                .child(div().flex_none().text_color(color).child(phrase))
                .children(text.todos.map(|todos| {
                    div()
                        .flex()
                        .gap(t.space(Space::Sm))
                        .child(dot_sep())
                        .child(div().flex_none().child(todos))
                }))
                .children(text.branch.map(|branch| {
                    div()
                        .min_w_0()
                        .flex()
                        .gap(t.space(Space::Sm))
                        .child(dot_sep())
                        .child(
                            div()
                                .min_w_0()
                                .max_w(t.ui(110.))
                                .ellipsis()
                                .px(t.space(Space::Sm))
                                .rounded(t.shape.radius_control)
                                .when(!selected, |el| el.bg(c.surface_active))
                                .text_color(c.content_secondary)
                                .child(branch),
                        )
                }))
                .children(text.files.as_ref().map(|files| {
                    div()
                        .flex()
                        .gap(t.space(Space::Sm))
                        .child(dot_sep())
                        .child(div().flex_none().child(files.clone()))
                }))
        });
        let tooltip = match (&text.files, live) {
            (Some(files), false) => format!("{}\n{files}", session.label),
            _ => session.label.clone(),
        };
        let trailing = div()
            .relative()
            .flex_none()
            .h_full()
            .min_w(t.ui(28.))
            .flex()
            .items_center()
            .justify_end()
            .child(
                div()
                    .text_color(c.content_muted)
                    .group_hover(group.clone(), |s| s.invisible())
                    .child(short_age(age)),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right_0()
                    .flex()
                    .items_center()
                    .invisible()
                    .group_hover(group.clone(), |s| s.visible())
                    .child(
                        IconButton::new(("session-view-open", i))
                            .label("◧")
                            .tooltip("Open Session View")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.open_session_view(view_id.clone(), window, cx)
                            })),
                    ),
            );
        let (generation, fresh) =
            self.review.sessions.looks.get(&session.id).map_or(
                (0, false),
                |(_, generation, since)| {
                    (*generation, *generation > 0 && since.running(t.motion.base))
                },
            );
        let glyph =
            motion::animate_enter(
                t.motion.reduced,
                fresh,
                div().flex_none().child(
                    StatusDot::new(dot).rotation(if dot == DotState::Running { angle } else { 0. }),
                ),
                ElementId::NamedInteger(format!("session-dot-{}", session.id).into(), generation),
                Animation::new(t.motion.base).with_easing(motion::ease_enter()),
                |el, d| el.opacity(d),
            );
        ListRow::new(("claude-live-session", i), session.label.clone())
            .group(group)
            .leading(
                div()
                    .flex()
                    .items_center()
                    .gap(t.space(Space::Sm))
                    .child(chevron_slot)
                    .child(glyph),
            )
            .emphasized(matches!(dot, DotState::NeedsInput | DotState::Unread))
            .bright(live)
            .selected(selected)
            .muted_selection(muted)
            .tooltip(tooltip)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_live_session(&open_id, pane, window, cx)
            }))
            .on_context_menu(cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                let row = this.review.sessions.row(&menu_for.id).cloned();
                let row = row.unwrap_or_else(|| bare_row(&menu_for));
                this.open_session_menu(row, ev.position, window, cx)
            }))
            .trailing(trailing)
            .when_some(detail, |row, detail| row.detail(detail))
            .into_any_element()
    }

    fn list_shown(&self) -> bool {
        self.workspace.active.is_some()
            && self.workspace.ui.tree_visible
            && self.workspace.ui.side_mode == SideMode::Sessions
    }

    /// Ticks the running glyph on a timer rather than an animation, so it costs eight frames a second.
    fn start_spin(cx: &mut Context<Self>) -> (Instant, Task<()>) {
        let start = Instant::now();
        let task = cx.spawn(async move |this, cx| {
            loop {
                let wait = until_next_tick(start.elapsed());
                cx.background_executor().timer(wait).await;
                let kept = this.update(cx, |this, cx| {
                    let state = &mut this.review.sessions;
                    let kept = keep_spinning(std::mem::take(&mut state.spin_drawn));
                    if kept {
                        cx.notify();
                    } else {
                        state.spin = None;
                    }
                    kept
                });
                if !kept.unwrap_or(false) {
                    break;
                }
            }
        });
        (start, task)
    }

    /// Notes each row's glyph state, so a change replays the fade under a new animation id.
    fn track_looks(&mut self, sessions: &[LiveSession]) {
        let looks = &mut self.review.sessions.looks;
        looks.retain(|id, _| sessions.iter().any(|s| s.id == *id));
        for s in sessions {
            let dot = dot_state(s.state, s.unread);
            match looks.get_mut(&s.id) {
                Some((seen, generation, since)) if *seen != dot => {
                    *seen = dot;
                    *generation += 1;
                    *since = Opening::now();
                }
                Some(_) => {}
                None => {
                    looks.insert(s.id.clone(), (dot, 0, Opening::now()));
                }
            }
        }
    }

    /// Drops finished turns and fades, stops the spinner once the list is off screen, and keeps
    /// frames coming while a chevron turns.
    pub(in crate::shell) fn sync_sessions_motion(
        &mut self,
        window: &mut Window,
        cx: &Context<Self>,
    ) {
        let (base, fast) = (cx.theme().motion.base, cx.theme().motion.fast);
        let shown = self.list_shown();
        let state = &mut self.review.sessions;
        if !shown {
            state.spin = None;
            state.looks.clear();
        }
        let now = Instant::now();
        state.turning.retain(|_, tween| tween.running(now, fast));
        state.entering.retain(|_, opening| opening.running(base));
        if !state.turning.is_empty() {
            window.request_animation_frame();
        }
    }

    fn toggle_session(&mut self, id: &str, cx: &mut Context<Self>) {
        let (reduced, fast) = (cx.theme().motion.reduced, cx.theme().motion.fast);
        let generation = self.next_generation();
        let state = &mut self.review.sessions;
        let open = !state.expanded.remove(id);
        if open {
            state.expanded.insert(id.to_string());
            state.collapsing.remove(id);
            state.entering.insert(id.to_string(), Opening::now());
        } else if !reduced {
            state.entering.remove(id);
            state
                .collapsing
                .insert(id.to_string(), Closing::new(generation));
            let id = id.to_string();
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(motion::exit_delay(false, fast))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    let closing = &mut this.review.sessions.collapsing;
                    if closing.get(&id).is_some_and(|c| c.generation == generation) {
                        closing.remove(&id);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        state.turn(id.to_string(), open, reduced, fast);
        cx.notify();
    }

    fn toggle_section(&mut self, section: Section, cx: &mut Context<Self>) {
        let (reduced, fast) = (cx.theme().motion.reduced, cx.theme().motion.fast);
        let state = &mut self.review.sessions;
        if !state.folded.remove(&section) {
            state.folded.insert(section);
        }
        let open = !section.folded(&state.folded);
        let key = section_key(section);
        if open {
            state.entering.insert(key.clone(), Opening::now());
        }
        state.turn(key, open, reduced, fast);
        cx.notify();
    }

    /// Focuses the terminal running the session, else resumes it in a new one.
    pub(super) fn open_live_session(
        &mut self,
        id: &str,
        pane: Option<PaneId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((root, item)) = pane.and_then(|pane| self.find_session(pane)) {
            return self.focus_item(&root, item, window, cx);
        }
        let profile = self.review.sessions.row(id).and_then(|r| r.profile.clone());
        if let Some(profile) = profile {
            self.resume_claude_session(id, profile.config_dir.as_deref(), window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::SystemTime;

    use super::*;

    fn session() -> LiveSession {
        LiveSession {
            id: "s".into(),
            label: "Fix the build".into(),
            pane: None,
            cwd: PathBuf::new(),
            main_root: PathBuf::new(),
            rel: PathBuf::new(),
            branch: None,
            state: None,
            unread: false,
            todos: None,
            files: 0,
            modified: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_monorepo_package_lists_its_own_sessions_and_its_worktrees_only() {
        let at = |main: &str, rel: &str, branch: Option<&str>| {
            let mut s = session();
            s.main_root = main.into();
            s.rel = rel.into();
            s.branch = branch.map(String::from);
            s
        };
        let place = |main: &str, rel: &str| Place {
            main_root: main.into(),
            branch: None,
            rel: rel.into(),
        };
        let package = place("/repo", "pkg-a");
        assert!(in_project(&at("/repo", "pkg-a", None), &package));
        assert!(in_project(&at("/repo", "pkg-a/src", None), &package));
        assert!(in_project(&at("/repo", "pkg-a", Some("fix")), &package));
        assert!(!in_project(&at("/repo", "pkg-b", None), &package));
        assert!(
            !in_project(&at("/repo", "pkg-b", Some("nested")), &package),
            "a sibling package in a nested worktree on a branch"
        );
        assert!(!in_project(&at("/repo", "pkg-ab", None), &package));
        assert!(!in_project(&at("/repo", "", None), &package));
        assert!(!in_project(&at("/other", "pkg-a", None), &package));

        let top = place("/repo", "");
        assert!(in_project(&at("/repo", "", None), &top));
        assert!(in_project(&at("/repo", "pkg-b", Some("nested")), &top));
        assert!(
            in_project(&at("/repo", "", None), &top),
            "a detached nested worktree"
        );
        assert!(!in_project(&at("/other", "", None), &top));

        let worktree = place("/repo", "");
        assert!(
            in_project(&at("/repo", "", None), &worktree),
            "a worktree project lists the main checkout's sessions"
        );
    }

    #[test]
    fn a_session_with_no_row_still_has_an_id_to_copy() {
        let row = bare_row(&session());
        assert_eq!((row.id.as_str(), row.profile.is_none()), ("s", true));
    }

    #[test]
    fn a_bare_session_has_nothing_beside_its_label() {
        let text = row_text(&session());
        assert_eq!(
            text,
            RowText {
                todos: None,
                files: None,
                branch: None
            }
        );
    }

    #[test]
    fn todos_files_and_branch_read_as_short_text() {
        let mut s = session();
        s.todos = Some((3, 7));
        s.files = 1;
        s.branch = Some("claude/fix".into());
        let text = row_text(&s);
        assert_eq!(text.todos.as_deref(), Some("3/7"));
        assert_eq!(text.files.as_deref(), Some("1 file"));
        assert_eq!(text.branch.as_deref(), Some("claude/fix"));
        s.files = 12;
        assert_eq!(row_text(&s).files.as_deref(), Some("12 files"));
    }

    #[test]
    fn a_session_without_a_terminal_is_idle() {
        for unread in [false, true] {
            assert_eq!(dot_state(None, unread), DotState::Idle);
            assert_eq!(
                dot_state(Some(ClaudeState::NeedsInput), unread),
                DotState::NeedsInput
            );
            assert_eq!(
                dot_state(Some(ClaudeState::Running), unread),
                DotState::Running
            );
        }
    }

    #[test]
    fn a_finished_session_is_unread_until_looked_at() {
        assert_eq!(
            dot_state(Some(ClaudeState::Finished), true),
            DotState::Unread
        );
        assert_eq!(
            dot_state(Some(ClaudeState::Finished), false),
            DotState::Finished
        );
    }

    #[test]
    fn ages_read_in_the_largest_whole_unit() {
        for (secs, text) in [
            (0, "now"),
            (59, "now"),
            (60, "1m"),
            (3_599, "59m"),
            (3_600, "1h"),
            (86_399, "23h"),
            (86_400, "1d"),
            (7 * 86_400 - 1, "6d"),
            (7 * 86_400, "1w"),
            (365 * 86_400 - 1, "52w"),
            (365 * 86_400, "1y"),
            (-5, "now"),
        ] {
            assert_eq!(short_age(secs), text, "{secs}s");
        }
    }

    #[test]
    fn sections_split_at_the_cutoffs() {
        let cuts = Cutoffs {
            today: 1_000_000,
            yesterday: 1_000_000 - 86_400,
            week: 1_000_000 - 7 * 86_400,
        };
        let at = |modified| section(false, modified, cuts);
        assert_eq!(section(true, 0, cuts), Section::Active);
        assert_eq!(at(cuts.today), Section::Today);
        assert_eq!(at(cuts.today - 1), Section::Yesterday);
        assert_eq!(at(cuts.yesterday), Section::Yesterday);
        assert_eq!(at(cuts.yesterday - 1), Section::Week);
        assert_eq!(at(cuts.today - 6 * 86_400), Section::Week);
        assert_eq!(at(cuts.week), Section::Week);
        assert_eq!(at(cuts.today - 8 * 86_400), Section::Older);
    }

    #[test]
    fn only_the_oldest_section_starts_folded() {
        let mut flipped = HashSet::new();
        assert!(Section::Older.folded(&flipped));
        assert!(!Section::Today.folded(&flipped));
        flipped.extend([Section::Older, Section::Today]);
        assert!(!Section::Older.folded(&flipped));
        assert!(Section::Today.folded(&flipped));
    }

    #[test]
    fn the_spinner_turns_an_eighth_every_tick() {
        let at = |ms| spin_step(Duration::from_millis(ms));
        let eighth = TAU / 8.;
        assert_eq!(at(0), 0.);
        assert_eq!(at(124), 0.);
        assert_eq!(at(125), eighth);
        assert_eq!(at(1000), 0.);
        assert_eq!(at(1125), eighth);
    }

    #[test]
    fn a_tick_goes_on_only_if_the_list_drew_since_the_last() {
        assert!(keep_spinning(true));
        assert!(!keep_spinning(false));
    }

    #[test]
    fn ticks_wait_for_the_next_boundary_from_the_start() {
        let wait = |ms| until_next_tick(Duration::from_millis(ms));
        assert_eq!(wait(0), Duration::from_millis(125));
        assert_eq!(wait(1), Duration::from_millis(124));
        assert_eq!(wait(124), Duration::from_millis(1));
        assert_eq!(wait(125), Duration::from_millis(125));
        assert_eq!(wait(130), Duration::from_millis(120));
    }

    #[test]
    fn the_spinner_runs_only_while_something_visible_runs_and_motion_is_on() {
        assert!(should_spin(true, true, false));
        assert!(!should_spin(false, true, false));
        assert!(!should_spin(true, false, false));
        assert!(!should_spin(true, true, true));
    }

    #[test]
    fn children_sweep_to_the_sum_of_their_row_heights() {
        assert_eq!(expand_height(2, 3, 1.), Some(2. * 22. + 3. * 24.));
        assert_eq!(expand_height(0, 1, 1.5), Some(36.));
        assert_eq!(expand_height(4, 8, 1.), Some(4. * 22. + 8. * 24.));
        assert_eq!(expand_height(4, 9, 1.), None);
    }

    #[test]
    fn midnights_step_back_a_day_at_a_time() {
        let now = SystemTime::now();
        let today = local_midnight(now, 0);
        let back = unix_secs(now) - today;
        assert!((0..=DAY + 3_600).contains(&back), "{back}");
        for days in 1..=7 {
            let step = local_midnight(now, days - 1) - local_midnight(now, days);
            assert!((DAY - 3_600..=DAY + 3_600).contains(&step), "{step}");
        }
    }
}
