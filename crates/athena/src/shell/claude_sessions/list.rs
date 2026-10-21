use std::collections::HashSet;
use std::f32::consts::FRAC_PI_2;
use std::time::{SystemTime, UNIX_EPOCH};

use athena_proto::PaneId;
use athena_term::ClaudeState;
use athena_ui::{
    ActiveTheme, Button, ButtonKind, DotState, Ellipsis, IconButton, ListRow, Size, Space,
    StatusDot, Theme, empty_state,
};
use gpui::{
    AnyElement, Context, FontWeight, MouseDownEvent, Svg, Transformation, Window, div, prelude::*,
    radians, svg,
};

use super::live::{LiveSession, Place};
use super::{SessionRow, Shell};

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

/// The disclosure arrow, pointing right and turned down while `open`.
fn chevron(open: bool, t: &Theme) -> Svg {
    svg()
        .path("ui/chevron.svg")
        .size(t.ui(10.))
        .flex_none()
        .text_color(t.color.content_muted)
        .when(open, |s| {
            s.with_transformation(Transformation::rotate(radians(FRAC_PI_2)))
        })
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
        let headed =
            sections.len() > 1 || sections.first().is_some_and(|(s, _)| *s != Section::Active);
        let current = self.current_session_id(cx);
        let muted = !window.is_window_active();
        for (n, (at, rows)) in sections.iter().enumerate() {
            let folded = headed && at.folded(&self.review.sessions.folded);
            if headed {
                out.push(self.render_section_header(n, *at, rows.len(), folded, &t, cx));
            }
            if folded {
                continue;
            }
            for &(i, session) in rows {
                let selected = current.as_deref() == Some(session.id.as_str());
                out.push(self.render_live_row(
                    i,
                    session,
                    (selected, muted),
                    now_secs - unix_secs(session.modified),
                    cx,
                ));
                if !self.review.sessions.expanded.contains(&session.id) {
                    continue;
                }
                let Some(row) = self.review.sessions.row(&session.id).cloned() else {
                    continue;
                };
                out.extend(self.render_live_plan(i, &row, &t));
                for (j, file) in row.files.iter().enumerate() {
                    out.push(self.render_file_row(
                        i * 10_000 + j,
                        &row.id,
                        file,
                        &row.root,
                        &t,
                        cx,
                    ));
                }
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
            .on_click(cx.listener(move |this, _, _, cx| {
                let flipped = &mut this.review.sessions.folded;
                if !flipped.remove(&section) {
                    flipped.insert(section);
                }
                cx.notify();
            }))
            .child(chevron(!folded, t))
            .child(label)
            .into_any_element()
    }

    fn render_live_row(
        &self,
        i: usize,
        session: &LiveSession,
        (selected, muted): (bool, bool),
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
                        let expanded = &mut this.review.sessions.expanded;
                        if !expanded.remove(&toggle_id) {
                            expanded.insert(toggle_id.clone());
                        }
                        cx.notify();
                    }))
                    .child(
                        chevron(open, &t).group_hover(group.clone(), |s| s.text_color(c.content)),
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
        ListRow::new(("claude-live-session", i), session.label.clone())
            .group(group)
            .leading(
                div()
                    .flex()
                    .items_center()
                    .gap(t.space(Space::Sm))
                    .child(chevron_slot)
                    .child(StatusDot::new(dot)),
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
