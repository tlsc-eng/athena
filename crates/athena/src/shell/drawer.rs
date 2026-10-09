use athena_ui::motion::{self, Closing, Opening};
use athena_ui::{ActiveTheme, ButtonKind};
use athena_workspace::{Axis, UiState};
use gpui::{
    Animation, AnyElement, Context, FontWeight, MouseButton, MouseDownEvent, Window, div,
    prelude::*, px,
};

use super::Shell;
use super::panes::{DIVIDER_HIT, Drag, clamp_drawer_height, resize_handle};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum DrawerTab {
    Terminal,
    Containers,
    Playwright,
    Tests,
    Notifications,
    References,
    Changes,
    Search,
    Problems,
    Timeline,
}

impl DrawerTab {
    fn label(self) -> &'static str {
        match self {
            Self::Terminal => "Terminal",
            Self::Containers => "Containers",
            Self::Playwright => "Playwright",
            Self::Tests => "Tests",
            Self::Notifications => "Notifications",
            Self::References => "References",
            Self::Changes => "Changes",
            Self::Search => "Search",
            Self::Problems => "Problems",
            Self::Timeline => "Timeline",
        }
    }
}

impl Shell {
    /// Opens the drawer on `tab`, or closes it if that tab is already showing.
    pub(super) fn toggle_drawer_tab(&mut self, tab: DrawerTab, cx: &mut Context<Self>) {
        self.drawer = if self.drawer == Some(tab) {
            None
        } else {
            Some(tab)
        };
        self.drawer_changed(cx);
    }

    pub(super) fn show_drawer_tab(&mut self, tab: DrawerTab, cx: &mut Context<Self>) {
        self.drawer = Some(tab);
        self.drawer_changed(cx);
    }

    /// Cmd+J: reopens the last tab, or closes the drawer.
    pub(super) fn toggle_drawer(&mut self, cx: &mut Context<Self>) {
        self.drawer = match self.drawer {
            Some(_) => None,
            None => Some(self.last_drawer_tab),
        };
        self.drawer_changed(cx);
    }

    pub(super) fn drawer_changed(&mut self, cx: &mut Context<Self>) {
        match (
            std::mem::replace(&mut self.drawer_shown, self.drawer),
            self.drawer,
        ) {
            (None, Some(_)) => {
                self.drawer_closing = None;
                self.drawer_opening = Some(Opening::now());
            }
            (Some(tab), None) => {
                let generation = self.next_generation();
                self.drawer_closing = Some((tab, Closing::new(generation)));
                let t = cx.theme();
                let delay = motion::exit_delay(t.motion.reduced, t.motion.fast);
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(delay).await;
                    let _ = this.update(cx, |this, cx| {
                        if this
                            .drawer_closing
                            .is_some_and(|(_, c)| c.generation == generation)
                        {
                            this.drawer_closing = None;
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
            _ => {}
        }
        if let Some(tab) = self.drawer {
            self.last_drawer_tab = tab;
        }
        if self.drawer == Some(DrawerTab::Notifications) {
            self.mark_all_read(cx);
        }
        self.containers_visible(self.drawer == Some(DrawerTab::Containers), cx);
        cx.notify();
    }

    fn drawer_height(&self, window: &Window) -> f32 {
        let window_h = f32::from(window.viewport_size().height);
        clamp_drawer_height(self.workspace.ui.drawer_height, window_h)
    }

    pub(super) fn render_drawer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (tab, closing) = match (self.drawer, self.drawer_closing) {
            (Some(tab), _) => (tab, None),
            (None, Some((tab, closing))) => (tab, Some(closing)),
            (None, None) => return None,
        };
        let t = cx.theme().clone();
        let tabs = [
            DrawerTab::Problems,
            DrawerTab::Terminal,
            DrawerTab::Containers,
            DrawerTab::Playwright,
            DrawerTab::Tests,
            DrawerTab::Notifications,
            DrawerTab::References,
            DrawerTab::Changes,
            DrawerTab::Timeline,
            DrawerTab::Search,
        ]
        .map(|candidate| {
            let active = candidate == tab;
            div()
                .id(candidate.label())
                .relative()
                .h_full()
                .px(t.ui(12.))
                .flex()
                .items_center()
                .cursor_pointer()
                .font_weight(FontWeight::MEDIUM)
                .text_color(if active {
                    t.color.content
                } else {
                    t.color.content_muted
                })
                .when(!active, |el| {
                    el.hover(|s| s.text_color(t.color.content_secondary))
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    if candidate == DrawerTab::Terminal {
                        return this.show_terminal_panel(window, cx);
                    }
                    this.drawer = Some(candidate);
                    this.drawer_changed(cx);
                }))
                .child(candidate.label())
                .when(candidate == DrawerTab::Problems, |el| {
                    el.children(self.render_problems_badge(cx))
                })
                .when(active, |el| {
                    el.child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom_0()
                            .h(px(1.))
                            .bg(t.color.accent),
                    )
                })
        });
        let action: Option<AnyElement> = match tab {
            DrawerTab::Notifications => Some(
                athena_ui::Button::new("notices-clear", "Clear", ButtonKind::Ghost)
                    .on_click(cx.listener(|this, _, _, cx| this.clear_notifications(cx)))
                    .into_any_element(),
            ),
            DrawerTab::Containers => self.render_containers_action(cx),
            DrawerTab::Playwright => self.render_playwright_action(cx),
            DrawerTab::Tests => self.render_tests_action(cx),
            DrawerTab::References => self.render_references_count(cx),
            DrawerTab::Changes => self.render_changes_count(cx),
            DrawerTab::Search => self.render_search_status(cx),
            DrawerTab::Problems => None,
            DrawerTab::Terminal => self.render_terminal_panel_actions(cx),
            DrawerTab::Timeline => self.render_timeline_title(cx),
        };
        let content = match tab {
            DrawerTab::Notifications => self.render_notifications(cx),
            DrawerTab::Containers => self.render_containers(cx),
            DrawerTab::Playwright => self.render_playwright(cx),
            DrawerTab::Tests => self.render_tests(cx),
            DrawerTab::References => self.render_references(cx),
            DrawerTab::Changes => self.render_changes(cx),
            DrawerTab::Search => self.render_search(cx),
            DrawerTab::Problems => self.render_problems(cx),
            DrawerTab::Terminal => self.render_terminal_panel(cx),
            DrawerTab::Timeline => self.render_timeline(cx),
        };
        // Only the search field and the panel terminal take focus; once hidden it has nowhere to go.
        if self.drawer_focus.contains_focused(window, cx)
            && (closing.is_some() || !matches!(tab, DrawerTab::Search | DrawerTab::Terminal))
        {
            self.focus_active_item(window, cx);
        }
        let drawer = div()
            .track_focus(&self.drawer_focus)
            .size_full()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(t.color.border)
            .bg(t.color.surface_sunken)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .when(closing.is_some(), |el| {
                el.capture_any_mouse_down(|_, _, cx| cx.stop_propagation())
            })
            .child(
                div()
                    .h(t.ui(32.))
                    .flex_none()
                    .pr(px(8.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(t.color.surface)
                    .border_b_1()
                    .border_color(t.color.border)
                    .text_size(t.typography.caption)
                    // Tabs give way to the tab's own buttons when a zoomed interface runs out of room.
                    .child(
                        div()
                            .h_full()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .flex()
                            .children(tabs),
                    )
                    .children(action.map(|a| div().flex_none().child(a))),
            )
            .child(div().flex_1().min_h_0().child(content));
        // The box keeps its full height throughout, so the panes above resize once, not per frame.
        let drawer = match closing {
            Some(closing) => motion::animate_exit(
                t.motion.reduced,
                drawer,
                ("drawer-close", closing.generation),
                t.motion.fast,
                |el, d| el.opacity(1. - d).top(px(16. * d)),
            ),
            None => motion::animate_enter(
                t.motion.reduced,
                self.drawer_opening
                    .is_some_and(|o| o.running(t.motion.base)),
                drawer,
                "drawer-open",
                Animation::new(t.motion.base).with_easing(motion::ease_enter()),
                |el, d| el.opacity(d).top(px(16. * (1. - d))),
            ),
        };
        Some(
            div()
                .h(px(self.drawer_height(window)))
                .flex_none()
                .overflow_hidden()
                .child(drawer)
                .into_any_element(),
        )
    }

    /// The drag strip on the drawer's top edge; a double-click restores the default height.
    pub(super) fn render_drawer_handle(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.drawer?;
        let h = self.drawer_height(window);
        Some(
            resize_handle("drawer-edge", Axis::Vertical, cx.theme().color.accent)
                .absolute()
                .left_0()
                .right_0()
                .bottom(px(h - 1. - DIVIDER_HIT))
                .h(px(1. + 2. * DIVIDER_HIT))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        if event.click_count == 2 {
                            this.drag = None;
                            this.workspace.ui.drawer_height = UiState::default().drawer_height;
                            this.schedule_save(cx);
                            cx.notify();
                            return;
                        }
                        this.drag = Some(Drag::Drawer {
                            start_y: event.position.y,
                            start_h: h,
                        });
                    }),
                )
                .into_any_element(),
        )
    }
}
