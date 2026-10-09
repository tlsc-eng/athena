use std::path::Path;

use athena_ui::{ActiveTheme, Button, ButtonKind, Tooltip, empty_state};
use athena_workspace::{Item, ItemId, ItemKind, Layout, PaneId, Panel};
use gpui::{
    AnyElement, Context, FontWeight, MouseButton, MouseDownEvent, Window, div, prelude::*, px,
};

use super::Shell;
use super::dnd::{TabDrag, TabGhost};
use super::drawer::DrawerTab;

/// Width of the list of panel terminals, shown once there are two, as VS Code does.
const LIST_WIDTH: f32 = 180.;

/// What a panel terminal's drag names as its pane; no layout pane has this id.
pub(super) const PANEL_PANE: PaneId = PaneId(0);

#[derive(Debug, PartialEq, Eq)]
enum Toggle {
    Hide,
    Show,
    Open,
}

/// What Ctrl+` does: hides a panel terminal that has focus, else shows one, opening it if none.
fn toggle(shown: bool, focused: bool, empty: bool) -> Toggle {
    match (shown && focused, empty) {
        (true, _) => Toggle::Hide,
        (false, true) => Toggle::Open,
        (false, false) => Toggle::Show,
    }
}

/// Whether keys go to the panel's terminal: focus is in the drawer and it shows the terminal.
fn panel_has_keys(drawer: Option<DrawerTab>, drawer_focused: bool) -> bool {
    drawer == Some(DrawerTab::Terminal) && drawer_focused
}

impl Shell {
    fn panel_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        panel_has_keys(self.drawer, self.drawer_focus.contains_focused(window, cx))
    }

    /// ⌘W while the panel has focus closes its terminal, not the editor area's tab; false otherwise.
    pub(super) fn close_focused_panel_terminal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.panel_focused(window, cx) {
            return false;
        }
        if let Some(id) = self.panel().and_then(Panel::active_item).map(|i| i.id) {
            self.close_panel_terminal(id, window, cx);
        }
        true
    }

    /// Attaches the terminals among `items` that have a session, so a restored tab shows its
    /// session's title, program or folder before it is first opened.
    pub(super) fn attach_terminals<'a>(
        &mut self,
        root: &Path,
        items: impl IntoIterator<Item = &'a Item>,
        cx: &mut Context<Self>,
    ) {
        for item in items {
            if matches!(item.kind, ItemKind::Terminal { session: Some(_) }) {
                self.item_view(root, item, cx);
            }
        }
    }

    /// Reopens the drawer on the terminal panel when it was showing at the last quit.
    pub(super) fn restore_terminal_panel(&mut self) {
        if self.workspace.ui.terminal_panel_open && self.panel().is_some_and(|p| !p.is_empty()) {
            self.drawer = Some(DrawerTab::Terminal);
            self.drawer_shown = self.drawer;
            self.last_drawer_tab = DrawerTab::Terminal;
        }
    }

    /// Remembers whether the terminal panel is showing, for the next launch.
    pub(super) fn note_terminal_panel(&mut self, cx: &mut Context<Self>) {
        let open = self.drawer == Some(DrawerTab::Terminal);
        if self.workspace.ui.terminal_panel_open != open {
            self.workspace.ui.terminal_panel_open = open;
            self.schedule_save(cx);
        }
    }

    fn panel(&self) -> Option<&Panel> {
        Some(&self.workspace.active_project()?.panel)
    }

    fn panel_mut(&mut self) -> Option<&mut Panel> {
        let i = self.workspace.active?;
        Some(&mut self.workspace.projects.get_mut(i)?.panel)
    }

    /// Ctrl+`: shows the panel's terminal and focuses it, or hides the panel when that has focus.
    pub(super) fn toggle_terminal_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(empty) = self.panel().map(Panel::is_empty) else {
            return;
        };
        let shown = self.drawer == Some(DrawerTab::Terminal);
        match toggle(shown, self.drawer_focus.contains_focused(window, cx), empty) {
            Toggle::Hide => {
                self.drawer = None;
                self.drawer_changed(cx);
                self.focus_active_item(window, cx);
            }
            Toggle::Open => self.new_panel_terminal(window, cx),
            Toggle::Show => {
                self.show_drawer_tab(DrawerTab::Terminal, cx);
                self.focus_panel_terminal(window, cx);
            }
        }
    }

    /// Shows the panel's terminal, opening one if it has none, and focuses it.
    pub(super) fn show_terminal_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.panel().is_some_and(Panel::is_empty) {
            true => self.new_panel_terminal(window, cx),
            false => {
                self.show_drawer_tab(DrawerTab::Terminal, cx);
                self.focus_panel_terminal(window, cx);
            }
        }
    }

    pub(super) fn new_panel_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.panel_mut() else {
            return;
        };
        panel.adopt(Item {
            id: ItemId(0),
            kind: ItemKind::Terminal { session: None },
            view: None,
        });
        self.schedule_save(cx);
        self.show_drawer_tab(DrawerTab::Terminal, cx);
        self.focus_panel_terminal(window, cx);
    }

    pub(super) fn focus_panel_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        let Some(item) = self.panel().and_then(|p| p.active_item()).cloned() else {
            return;
        };
        if let Some(view) = self.item_view(&root, &item, cx) {
            window.focus(&view.focus_handle(cx));
        }
        cx.notify();
    }

    pub(super) fn activate_panel_terminal(
        &mut self,
        id: ItemId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.panel_mut().is_some_and(|p| p.activate(id)) {
            self.schedule_save(cx);
            self.show_drawer_tab(DrawerTab::Terminal, cx);
            self.focus_panel_terminal(window, cx);
        }
    }

    /// Hangs up a panel terminal; closing the last one hides the panel, as in VS Code.
    pub(super) fn close_panel_terminal(
        &mut self,
        id: ItemId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root() else {
            return;
        };
        let Some(panel) = self.panel_mut() else {
            return;
        };
        let Some(item) = panel.take(id) else {
            return;
        };
        let emptied = panel.is_empty();
        self.item_menus.remove(&(root.clone(), id));
        match self.items.remove(&(root, id)) {
            Some(view) => view.close(cx),
            None => athena_term::kill_sessions(super::panes::unviewed_sessions([&item], |_| false)),
        }
        self.schedule_save(cx);
        self.after_panel_change(emptied, window, cx);
    }

    fn after_panel_change(&mut self, emptied: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.drawer != Some(DrawerTab::Terminal) {
            return cx.notify();
        }
        if emptied {
            self.drawer = None;
            self.drawer_changed(cx);
            self.focus_active_item(window, cx);
        } else {
            self.focus_panel_terminal(window, cx);
        }
        cx.notify();
    }

    /// Moves a panel terminal into the editor area, joining `pane` or the focused pane.
    pub(super) fn panel_terminal_to_editor(
        &mut self,
        id: ItemId,
        pane: Option<PaneId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<ItemId> {
        let root = self.active_root()?;
        let i = self.workspace.active?;
        let project = &mut self.workspace.projects[i];
        if let (Some(layout), Some(pane)) = (&project.layout, pane) {
            layout.pane(pane)?;
        }
        let item = project.panel.take(id)?;
        let emptied = project.panel.is_empty();
        let new_id = match project.layout.as_mut() {
            Some(layout) => {
                let pane = pane.unwrap_or(layout.focused);
                layout.focused = pane;
                layout.add_item(pane, item.kind)?
            }
            None => {
                let layout = Layout::new(item.kind);
                let new_id = layout.items().next()?.id;
                project.layout = Some(layout);
                new_id
            }
        };
        self.rekey_item(&root, id, new_id);
        self.zoomed = None;
        if pane.is_none() {
            self.tab_born = Some((new_id, athena_ui::motion::Opening::now()));
            self.after_layout_change(window, cx);
        }
        if emptied && self.drawer == Some(DrawerTab::Terminal) {
            self.drawer = None;
            self.drawer_changed(cx);
        }
        Some(new_id)
    }

    /// Moves a terminal tab out of the editor area into the panel.
    pub(super) fn terminal_to_panel(
        &mut self,
        id: ItemId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root() else {
            return;
        };
        let is_terminal = self
            .workspace
            .active_project()
            .and_then(|p| p.layout.as_ref())
            .and_then(|l| l.items().find(|i| i.id == id))
            .is_some_and(|i| matches!(i.kind, ItemKind::Terminal { .. }));
        if !is_terminal {
            return;
        }
        let Some(item) = self.take_from_layout(&root, id) else {
            return;
        };
        let Some(panel) = self.panel_mut() else {
            return;
        };
        let new_id = panel.adopt(item);
        self.rekey_item(&root, id, new_id);
        self.zoomed = None;
        self.after_layout_change(window, cx);
        self.show_drawer_tab(DrawerTab::Terminal, cx);
        self.focus_panel_terminal(window, cx);
    }

    /// The focused tab, if a terminal, goes to the panel; else the panel's goes to the editor area.
    pub(super) fn move_terminal(
        &mut self,
        to_panel: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The focused terminal is already in the panel; the editor area's must not be taken.
        if to_panel && self.panel_focused(window, cx) {
            return;
        }
        let project = self.workspace.active_project();
        if to_panel {
            let item = project
                .and_then(|p| p.layout.as_ref())
                .and_then(|l| l.focused_pane())
                .and_then(|p| p.active_item())
                .map(|i| i.id);
            if let Some(id) = item {
                self.terminal_to_panel(id, window, cx);
            }
        } else if let Some(id) = project.and_then(|p| p.panel.active_item()).map(|i| i.id) {
            self.panel_terminal_to_editor(id, None, window, cx);
        }
    }

    /// Gives a live view the key a tab took on in its new place.
    fn rekey_item(&mut self, root: &Path, from: ItemId, to: ItemId) {
        let (old, new) = ((root.to_path_buf(), from), (root.to_path_buf(), to));
        if let Some(view) = self.items.remove(&old) {
            self.items.insert(new.clone(), view);
        }
        if self.item_menus.remove(&old) {
            self.item_menus.insert(new);
        }
    }

    pub(super) fn render_terminal_panel_actions(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let active = self.panel()?.active_item()?.id;
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(4.))
                .child(
                    Button::new("panel-terminal-new", "New Terminal", ButtonKind::Ghost)
                        .on_click(cx.listener(|this, _, w, cx| this.new_panel_terminal(w, cx))),
                )
                .child(
                    Button::new(
                        "panel-terminal-move",
                        "Move to Editor Area",
                        ButtonKind::Ghost,
                    )
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.panel_terminal_to_editor(active, None, w, cx);
                    })),
                )
                .child(
                    Button::new("panel-terminal-kill", "Kill", ButtonKind::Ghost).on_click(
                        cx.listener(move |this, _, w, cx| this.close_panel_terminal(active, w, cx)),
                    ),
                )
                .into_any_element(),
        )
    }

    pub(super) fn render_terminal_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let Some(root) = self.active_root() else {
            return div().into_any_element();
        };
        let panel = self.panel().cloned().unwrap_or_default();
        self.attach_terminals(&root, &panel.terminals, cx);
        let tint = t.color.accent.opacity(0.12);
        let body = match panel.active_item() {
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(empty_state(
                    "No terminals in the panel",
                    "Press ⌃` to open one here, or drag a terminal tab in.",
                    Some(
                        Button::new(
                            "panel-terminal-empty",
                            "New Terminal",
                            ButtonKind::Secondary,
                        )
                        .on_click(cx.listener(|this, _, w, cx| this.new_panel_terminal(w, cx))),
                    ),
                    cx,
                )),
            Some(item) => div()
                .size_full()
                .children(self.item_view(&root, item, cx).map(|v| v.element())),
        };
        let list = (panel.terminals.len() > 1).then(|| {
            let rows: Vec<AnyElement> = panel
                .terminals
                .iter()
                .enumerate()
                .map(|(index, item)| self.render_panel_row(&root, item, index == panel.active, cx))
                .collect();
            div()
                .id("panel-terminal-list")
                .w(t.ui(LIST_WIDTH))
                .flex_none()
                .h_full()
                .overflow_y_scroll()
                .border_l_1()
                .border_color(t.color.border)
                .bg(t.color.surface)
                .py(px(4.))
                .children(rows)
        });
        div()
            .id("panel-terminal")
            .size_full()
            .flex()
            .drag_over::<TabDrag>(move |s, drag, _, _| match drag.terminal {
                true => s.bg(tint),
                false => s,
            })
            .on_drop(cx.listener(|this, drag: &TabDrag, window, cx| {
                if drag.terminal && drag.pane != PANEL_PANE {
                    this.terminal_to_panel(drag.item, window, cx);
                }
            }))
            .child(div().flex_1().min_w_0().h_full().child(body))
            .children(list)
            .into_any_element()
    }

    fn render_panel_row(
        &self,
        root: &Path,
        item: &Item,
        active: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let id = item.id;
        let label = self.item_label(root, item, cx);
        let close_group = format!("panel-row-{}", id.0);
        let drag = TabDrag {
            pane: PANEL_PANE,
            item: id,
            label: label.clone().into(),
            terminal: true,
        };
        div()
            .id(("panel-row", id.0))
            .group(close_group.clone())
            .h(t.ui(24.))
            .mx(px(4.))
            .pl(px(8.))
            .pr(px(4.))
            .flex()
            .items_center()
            .gap(px(6.))
            .rounded(t.shape.radius_control)
            .text_size(t.typography.caption)
            .cursor_pointer()
            .text_color(if active {
                t.color.content
            } else {
                t.color.content_muted
            })
            .when(active, |el| {
                el.bg(t.color.surface_active)
                    .font_weight(FontWeight::MEDIUM)
            })
            .when(!active, |el| {
                el.hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
            })
            .on_click(cx.listener(move |this, _, w, cx| this.activate_panel_terminal(id, w, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, w, cx| {
                    cx.stop_propagation();
                    this.open_panel_terminal_menu(id, event.position, w, cx)
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _, w, cx| this.close_panel_terminal(id, w, cx)),
            )
            .on_drag(drag, |drag: &TabDrag, _, _, cx| {
                let label = drag.label.clone();
                cx.new(|_| TabGhost { label })
            })
            .children(self.item_badge(root, item, &t, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(label),
            )
            .child(
                div()
                    .id(("panel-row-close", id.0))
                    .size(px(16.))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .rounded(t.shape.radius_control)
                    .invisible()
                    .group_hover(close_group, |s| s.visible())
                    .text_color(t.color.content_muted)
                    .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                    .tooltip(|_, cx| Tooltip::view("Kill Terminal", cx))
                    .on_click(cx.listener(move |this, _, w, cx| {
                        cx.stop_propagation();
                        this.close_panel_terminal(id, w, cx);
                    }))
                    .child("×"),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_focused_drawer_showing_the_terminal_takes_the_keys() {
        assert!(panel_has_keys(Some(DrawerTab::Terminal), true));
        assert!(!panel_has_keys(Some(DrawerTab::Terminal), false));
        assert!(!panel_has_keys(Some(DrawerTab::Problems), true));
        assert!(!panel_has_keys(None, false));
    }

    #[test]
    fn ctrl_backtick_hides_a_focused_panel_terminal_and_otherwise_brings_one_up() {
        assert_eq!(toggle(true, true, false), Toggle::Hide);
        assert_eq!(toggle(true, false, false), Toggle::Show);
        assert_eq!(toggle(false, false, false), Toggle::Show);
        assert_eq!(toggle(false, false, true), Toggle::Open);
        assert_eq!(toggle(true, false, true), Toggle::Open);
    }
}
