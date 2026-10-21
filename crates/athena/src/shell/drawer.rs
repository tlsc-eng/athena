use athena_ui::motion::{self, Closing, Opening};
use athena_ui::{ActiveTheme, MenuItem, Size, Tab, TabBar};
use athena_workspace::{Axis, DrawerTabName, UiState};
use gpui::{
    Animation, AnyElement, ClickEvent, Context, MouseButton, MouseDownEvent, Window, div,
    prelude::*, px,
};

use super::Shell;
use super::menus::shell_item;
use super::panes::{DIVIDER_HIT, Drag, clamp_drawer_height, resize_handle};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum DrawerTab {
    Terminal,
    Containers,
    Playwright,
    Tests,
    References,
    Search,
    Problems,
    Debug,
}

/// The tabs the bar lists; Containers and Playwright sit behind its Tools menu.
const BAR_TABS: [DrawerTab; 6] = [
    DrawerTab::Terminal,
    DrawerTab::Problems,
    DrawerTab::Tests,
    DrawerTab::Debug,
    DrawerTab::Search,
    DrawerTab::References,
];

const TOOLS: [DrawerTab; 2] = [DrawerTab::Containers, DrawerTab::Playwright];

impl DrawerTab {
    fn label(self) -> &'static str {
        match self {
            Self::Terminal => "Terminal",
            Self::Containers => "Containers",
            Self::Playwright => "Playwright",
            Self::Tests => "Tests",
            Self::References => "References",
            Self::Search => "Search",
            Self::Problems => "Problems",
            Self::Debug => "Debug",
        }
    }

    fn name(self) -> DrawerTabName {
        match self {
            Self::Terminal => DrawerTabName::Terminal,
            Self::Containers => DrawerTabName::Containers,
            Self::Playwright => DrawerTabName::Playwright,
            Self::Tests => DrawerTabName::Tests,
            Self::References => DrawerTabName::References,
            Self::Search => DrawerTabName::Search,
            Self::Problems => DrawerTabName::Problems,
            Self::Debug => DrawerTabName::Debug,
        }
    }

    /// The tab Cmd+J opens first: the one saved at the last quit, or the terminal.
    pub(super) fn restored(saved: Option<DrawerTabName>) -> Self {
        match saved {
            Some(DrawerTabName::Containers) => Self::Containers,
            Some(DrawerTabName::Playwright) => Self::Playwright,
            Some(DrawerTabName::Tests) => Self::Tests,
            Some(DrawerTabName::References) => Self::References,
            Some(DrawerTabName::Search) => Self::Search,
            Some(DrawerTabName::Problems) => Self::Problems,
            Some(DrawerTabName::Debug) => Self::Debug,
            Some(DrawerTabName::Terminal) | None => Self::Terminal,
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
            if self.workspace.ui.drawer_tab != Some(tab.name()) {
                self.workspace.ui.drawer_tab = Some(tab.name());
                self.schedule_save(cx);
            }
        }
        self.containers_visible(self.drawer == Some(DrawerTab::Containers), cx);
        self.note_terminal_panel(cx);
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
        let problems = self.problems_total();
        let mut tabs: Vec<AnyElement> = BAR_TABS
            .map(|candidate| {
                let entry = Tab::new(candidate.label(), candidate.label())
                    .active(candidate == tab)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if candidate == DrawerTab::Terminal {
                            return this.show_terminal_panel(window, cx);
                        }
                        this.show_drawer_tab(candidate, cx);
                    }));
                match candidate {
                    DrawerTab::Problems => entry.badge(problems),
                    _ => entry,
                }
                .into_any_element()
            })
            .into();
        let tools_open = TOOLS.contains(&tab);
        let tools_label = match tools_open {
            true => format!("{} ▾", tab.label()),
            false => "Tools ▾".to_string(),
        };
        tabs.push(
            Tab::new("drawer-tools", tools_label)
                .active(tools_open)
                .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                    let items: Vec<MenuItem> = TOOLS
                        .iter()
                        .map(|&tool| {
                            shell_item(tool.label(), cx, move |this, _, cx| {
                                this.show_drawer_tab(tool, cx)
                            })
                        })
                        .collect();
                    this.open_context_menu(event.position(), items, window, cx);
                }))
                .into_any_element(),
        );
        let action: Option<AnyElement> = match tab {
            DrawerTab::Containers => self.render_containers_action(cx),
            DrawerTab::Playwright => self.render_playwright_action(cx),
            DrawerTab::Tests => self.render_tests_action(cx),
            DrawerTab::References => self.render_references_count(cx),
            DrawerTab::Search => self.render_search_status(cx),
            DrawerTab::Problems => None,
            DrawerTab::Terminal => self.render_terminal_panel_actions(cx),
            DrawerTab::Debug => self.render_debug_action(cx),
        };
        let content = match tab {
            DrawerTab::Containers => self.render_containers(cx),
            DrawerTab::Playwright => self.render_playwright(cx),
            DrawerTab::Tests => self.render_tests(cx),
            DrawerTab::References => self.render_references(cx),
            DrawerTab::Search => self.render_search(cx),
            DrawerTab::Problems => self.render_problems(cx),
            DrawerTab::Terminal => self.render_terminal_panel(cx),
            DrawerTab::Debug => self.render_debug(window, cx),
        };
        // Only the search field, panel terminal and Debug fields take focus; once hidden, none can.
        if self.drawer_focus.contains_focused(window, cx)
            && (closing.is_some()
                || !matches!(
                    tab,
                    DrawerTab::Search | DrawerTab::Terminal | DrawerTab::Debug
                ))
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
                    .h(t.size(Size::TabBar))
                    .flex_none()
                    .flex()
                    .items_center()
                    .bg(t.color.surface)
                    .text_size(t.typography.caption)
                    // Tabs give way to the tab's own buttons when a zoomed interface runs out of room.
                    .child(
                        div()
                            .h_full()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(TabBar::new().children(tabs)),
                    )
                    // Carries the hairline the tab bar draws under the tabs on to the window's edge.
                    .child(
                        div()
                            .h_full()
                            .flex_none()
                            .pr(px(8.))
                            .flex()
                            .items_center()
                            .border_b_1()
                            .border_color(t.color.border)
                            .children(action),
                    ),
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
