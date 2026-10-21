use athena_ui::{ActiveTheme, SegmentedControl, Size, Space};
use athena_workspace::SideMode;
use gpui::{AnyElement, Context, Focusable, FontWeight, Window, div, prelude::*};

use super::Shell;

const MODES: [(SideMode, &str); 4] = [
    (SideMode::Sessions, "Sessions"),
    (SideMode::Files, "Files"),
    (SideMode::Outline, "Outline"),
    (SideMode::Git, "Git"),
];

/// Which Git sections are folded; kept off `UiState` because it is not worth restoring.
#[derive(Default)]
pub(super) struct SidePanelState {
    changes_collapsed: bool,
    timeline_collapsed: bool,
}

/// Whether ShowChanges hides the panel: only when it is already showing Git.
fn hides_git_panel(panel_visible: bool, mode: SideMode) -> bool {
    panel_visible && mode == SideMode::Git
}

impl Shell {
    /// Switches the side panel to `mode`, showing the panel first if it was hidden.
    pub(super) fn select_side_mode(&mut self, mode: SideMode, cx: &mut Context<Self>) {
        if !self.workspace.ui.tree_visible {
            self.toggle_tree(cx);
        }
        if self.workspace.ui.side_mode != mode {
            self.workspace.ui.side_mode = mode;
            self.schedule_save(cx);
        }
        cx.notify();
    }

    /// Shows Git mode with Changes open, unlike ShowChanges, which hides the panel when it is already there.
    pub(super) fn show_changes(&mut self, cx: &mut Context<Self>) {
        self.side_panel.changes_collapsed = false;
        self.select_side_mode(SideMode::Git, cx);
    }

    /// Shows Git mode with Timeline open.
    pub(super) fn show_timeline(&mut self, cx: &mut Context<Self>) {
        self.side_panel.timeline_collapsed = false;
        self.select_side_mode(SideMode::Git, cx);
    }

    pub(super) fn toggle_changes(&mut self, cx: &mut Context<Self>) {
        if hides_git_panel(self.workspace.ui.tree_visible, self.workspace.ui.side_mode) {
            self.toggle_tree(cx);
        } else {
            self.show_changes(cx);
        }
    }

    /// `select_side_mode`, then focuses Outline's filter, which is what asking for the outline means.
    pub(super) fn show_side_mode(
        &mut self,
        mode: SideMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_side_mode(mode, cx);
        if mode == SideMode::Outline {
            let filter = self.outline_filter(cx);
            window.focus(&filter.focus_handle(cx));
        }
    }

    pub(super) fn render_side_switch(&self, cx: &mut Context<Self>) -> AnyElement {
        let shell = cx.entity();
        let selected = MODES
            .iter()
            .position(|(mode, _)| *mode == self.workspace.ui.side_mode)
            .unwrap_or(0);
        SegmentedControl::new("side-mode", MODES.map(|(_, label)| label), selected)
            .on_select(move |ix, window, cx| {
                shell.update(cx, |this, cx| this.show_side_mode(MODES[ix].0, window, cx))
            })
            .into_any_element()
    }

    /// The panel's content for the current mode; `files` is the Explorer tree, built by the caller.
    pub(super) fn render_side_body(
        &mut self,
        files: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match self.workspace.ui.side_mode {
            SideMode::Sessions => self.render_sessions_panel(cx),
            SideMode::Files => files,
            SideMode::Outline => self.render_outline(cx),
            SideMode::Git => self.render_git_panel(cx),
        }
    }

    /// The one call site Sessions mode renders through.
    fn render_sessions_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.render_sessions_list(cx)
    }

    fn render_git_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let changes = (!self.side_panel.changes_collapsed).then(|| self.render_changes(cx));
        let timeline = (!self.side_panel.timeline_collapsed).then(|| self.render_timeline(cx));
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.git_section(
                "side-changes",
                "Changes",
                changes,
                |s| &mut s.changes_collapsed,
                false,
                cx,
            ))
            .child(self.git_section(
                "side-timeline",
                "Timeline",
                timeline,
                |s| &mut s.timeline_collapsed,
                true,
                cx,
            ))
            .into_any_element()
    }

    /// A header that folds its `body`; a folded section keeps only the header's height.
    fn git_section(
        &self,
        id: &'static str,
        title: &'static str,
        body: Option<AnyElement>,
        collapsed: fn(&mut SidePanelState) -> &mut bool,
        divider: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let open = body.is_some();
        div()
            .min_h_0()
            .flex()
            .flex_col()
            .when(open, |el| el.flex_1())
            .when(!open, |el| el.flex_none())
            .when(divider, |el| el.border_t_1().border_color(t.color.border))
            .child(
                div()
                    .id(id)
                    .h(t.size(Size::Row))
                    .flex_none()
                    .px(t.space(Space::Md))
                    .flex()
                    .items_center()
                    .gap(t.space(Space::Sm))
                    .cursor_pointer()
                    .text_size(t.typography.caption)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.color.content_secondary)
                    .hover(|s| s.bg(t.color.surface_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let flag = collapsed(&mut this.side_panel);
                        *flag = !*flag;
                        cx.notify();
                    }))
                    .child(if open { "▾" } else { "▸" })
                    .child(title),
            )
            .children(body.map(|body| div().flex_1().min_h_0().overflow_hidden().child(body)))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_changes_hides_the_panel_only_when_it_is_visible_on_git() {
        assert!(hides_git_panel(true, SideMode::Git));
        assert!(!hides_git_panel(false, SideMode::Git));
        for mode in [SideMode::Sessions, SideMode::Files, SideMode::Outline] {
            assert!(!hides_git_panel(true, mode));
            assert!(!hides_git_panel(false, mode));
        }
    }
}
