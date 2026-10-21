use std::collections::HashMap;

use athena_proto::PaneId;
use athena_term::ClaudeState;
use athena_ui::{
    ActiveTheme, Button, ButtonKind, DotState, ListRow, Space, StatusDot, empty_state,
};
use gpui::{AnyElement, Context, MouseDownEvent, Window, div, prelude::*, px};

use super::Shell;
use super::live::LiveSession;
use crate::transcripts::Price;

/// What a row says besides its label, as shown in its detail line.
#[derive(Debug, PartialEq)]
struct RowText {
    /// `3/7`: finished todos out of all of them.
    todos: Option<String>,
    files: Option<String>,
    cost: Option<String>,
    branch: Option<String>,
}

fn row_text(session: &LiveSession, prices: &HashMap<String, Price>) -> RowText {
    let cost = session.cost_label(prices);
    RowText {
        todos: session.todos.map(|(done, all)| format!("{done}/{all}")),
        files: match session.files {
            0 => None,
            1 => Some("1 file".into()),
            n => Some(format!("{n} files")),
        },
        cost: (!cost.is_empty()).then_some(cost),
        branch: session.branch.clone(),
    }
}

fn dot_state(state: Option<ClaudeState>) -> DotState {
    match state {
        Some(ClaudeState::Running) => DotState::Running,
        Some(ClaudeState::NeedsInput) => DotState::NeedsInput,
        Some(ClaudeState::Finished) => DotState::Finished,
        None => DotState::Idle,
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
    /// Sessions mode: the sessions of the active project, and of its worktrees, as one list.
    pub(in crate::shell) fn render_sessions_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
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
        let main_root = self.review.sessions.place(&root).0;
        let sessions: Vec<LiveSession> = self
            .live_sessions(cx)
            .into_iter()
            .filter(|s| s.main_root == main_root)
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
        let prices = self.settings.file.claude_prices.clone();
        let mut out: Vec<AnyElement> = Vec::new();
        if let Some(bar) = self.render_review_bar(true, &t, cx) {
            out.push(bar);
        }
        for (i, session) in sessions.iter().enumerate() {
            out.push(self.render_live_row(i, session, &prices, cx));
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
                    true,
                    &t,
                    cx,
                ));
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

    fn render_live_row(
        &self,
        i: usize,
        session: &LiveSession,
        prices: &HashMap<String, Price>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let text = row_text(session, prices);
        let expandable = session.files > 0;
        let open = expandable && self.review.sessions.expanded.contains(&session.id);
        let (toggle_id, open_id, menu_id) =
            (session.id.clone(), session.id.clone(), session.id.clone());
        let pane = session.pane;
        let chevron = div()
            .id(("session-chevron", i))
            .w(px(10.))
            .flex_none()
            .text_color(t.color.content_muted)
            .when(expandable, |el| {
                el.cursor_pointer()
                    .hover(|s| s.text_color(t.color.content))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        let expanded = &mut this.review.sessions.expanded;
                        if !expanded.remove(&toggle_id) {
                            expanded.insert(toggle_id.clone());
                        }
                        cx.notify();
                    }))
                    .child(if open { "▾" } else { "▸" })
            });
        let detail =
            (text.branch.is_some() || text.files.is_some() || text.cost.is_some()).then(|| {
                div()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(t.space(Space::Sm))
                    .children(text.branch.map(|branch| {
                        div()
                            .min_w_0()
                            .max_w(t.ui(110.))
                            .truncate()
                            .px(t.space(Space::Sm))
                            .rounded(t.shape.radius_control)
                            .bg(t.color.surface_active)
                            .text_color(t.color.content_secondary)
                            .child(branch)
                    }))
                    .children(text.files.map(|files| div().flex_none().child(files)))
                    .children(text.cost.map(|cost| div().flex_none().child(cost)))
            });
        ListRow::new(("claude-live-session", i), session.label.clone())
            .leading(
                div()
                    .flex()
                    .items_center()
                    .gap(t.space(Space::Sm))
                    .child(chevron)
                    .child(StatusDot::new(dot_state(session.state))),
            )
            .emphasized(session.unread)
            .tooltip(session.label.clone())
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_live_session(&open_id, pane, window, cx)
            }))
            .on_context_menu(cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                if let Some(row) = this.review.sessions.row(&menu_id).cloned() {
                    this.open_session_menu(row, ev.position, window, cx)
                }
            }))
            .when_some(text.todos, |row, todos| row.meta(todos))
            .when_some(detail, |row, detail| row.detail(detail))
            .into_any_element()
    }

    /// Focuses the terminal running the session, else resumes it in a new one.
    fn open_live_session(
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
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::SystemTime;

    use super::*;
    use crate::transcripts::Tokens;

    fn session() -> LiveSession {
        LiveSession {
            id: "s".into(),
            label: "Fix the build".into(),
            pane: None,
            cwd: PathBuf::new(),
            main_root: PathBuf::new(),
            branch: None,
            state: None,
            unread: false,
            todos: None,
            files: 0,
            usage: BTreeMap::new(),
            modified: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_bare_session_has_nothing_beside_its_label() {
        let text = row_text(&session(), &HashMap::new());
        assert_eq!(
            text,
            RowText {
                todos: None,
                files: None,
                cost: None,
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
        let text = row_text(&s, &HashMap::new());
        assert_eq!(text.todos.as_deref(), Some("3/7"));
        assert_eq!(text.files.as_deref(), Some("1 file"));
        assert_eq!(text.branch.as_deref(), Some("claude/fix"));
        s.files = 12;
        assert_eq!(
            row_text(&s, &HashMap::new()).files.as_deref(),
            Some("12 files")
        );
    }

    #[test]
    fn cost_appears_once_tokens_are() {
        let mut s = session();
        s.usage.insert(
            "model-without-a-price".into(),
            Tokens {
                input: 10,
                ..Tokens::default()
            },
        );
        assert_eq!(
            row_text(&s, &HashMap::new()).cost.as_deref(),
            Some("cost n/a")
        );
    }

    #[test]
    fn a_session_without_a_terminal_is_idle() {
        assert_eq!(dot_state(None), DotState::Idle);
        assert_eq!(
            dot_state(Some(ClaudeState::NeedsInput)),
            DotState::NeedsInput
        );
        assert_eq!(dot_state(Some(ClaudeState::Running)), DotState::Running);
        assert_eq!(dot_state(Some(ClaudeState::Finished)), DotState::Finished);
    }
}
