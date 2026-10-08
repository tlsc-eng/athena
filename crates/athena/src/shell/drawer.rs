use athena_ui::{ActiveTheme, ButtonKind, motion};
use gpui::{Animation, AnyElement, Context, FontWeight, MouseButton, div, prelude::*, px};

use super::Shell;

const HEIGHT: f32 = 240.;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum DrawerTab {
    Containers,
    Playwright,
    Notifications,
    References,
}

impl DrawerTab {
    fn label(self) -> &'static str {
        match self {
            Self::Containers => "Containers",
            Self::Playwright => "Playwright",
            Self::Notifications => "Notifications",
            Self::References => "References",
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

    fn drawer_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.drawer {
            self.last_drawer_tab = tab;
        }
        if self.drawer == Some(DrawerTab::Notifications) {
            self.mark_all_read(cx);
        }
        self.containers_visible(self.drawer == Some(DrawerTab::Containers), cx);
        cx.notify();
    }

    pub(super) fn render_drawer(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tab = self.drawer?;
        let t = cx.theme().clone();
        let tabs = [
            DrawerTab::Containers,
            DrawerTab::Playwright,
            DrawerTab::Notifications,
            DrawerTab::References,
        ]
        .map(|candidate| {
            let active = candidate == tab;
            div()
                .id(candidate.label())
                .relative()
                .h_full()
                .px(px(12.))
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
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.drawer = Some(candidate);
                    this.drawer_changed(cx);
                }))
                .child(candidate.label())
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
            DrawerTab::References => self.render_references_count(cx),
        };
        let content = match tab {
            DrawerTab::Notifications => self.render_notifications(cx),
            DrawerTab::Containers => self.render_containers(cx),
            DrawerTab::Playwright => self.render_playwright(cx),
            DrawerTab::References => self.render_references(cx),
        };
        let drawer = div()
            .h(px(HEIGHT))
            .flex_none()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(t.color.border)
            .bg(t.color.surface_sunken)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(32.))
                    .flex_none()
                    .pr(px(8.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(t.color.surface)
                    .border_b_1()
                    .border_color(t.color.border)
                    .text_size(t.typography.caption)
                    .child(div().h_full().flex().children(tabs))
                    .children(action),
            )
            .child(div().flex_1().min_h_0().child(content));
        Some(motion::animate_if(
            t.motion.reduced,
            drawer,
            "drawer-open",
            Animation::new(t.motion.base).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d),
        ))
    }
}
