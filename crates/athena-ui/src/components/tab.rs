use gpui::{
    AnyElement, App, ClickEvent, ElementId, FontWeight, IntoElement, ParentElement, RenderOnce,
    SharedString, Window, div, prelude::*,
};

use super::{Badge, ClickHandler};
use crate::{ActiveTheme, Size, Space};

#[derive(IntoElement)]
pub struct Tab {
    id: ElementId,
    label: SharedString,
    active: bool,
    badge: Option<usize>,
    close: Option<AnyElement>,
    on_click: Option<ClickHandler>,
}

impl Tab {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            active: false,
            badge: None,
            close: None,
            on_click: None,
        }
    }

    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    /// A count after the label; zero shows nothing.
    pub fn badge(mut self, count: usize) -> Self {
        self.badge = Some(count);
        self
    }

    /// An element after the label, usually a close button.
    pub fn close(mut self, close: impl IntoElement) -> Self {
        self.close = Some(close.into_any_element());
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(f));
        self
    }
}

impl RenderOnce for Tab {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = cx.theme();
        let c = &t.color;
        let active = self.active;
        let tab = div()
            .id(self.id)
            .relative()
            .flex_none()
            .flex()
            .items_center()
            .cursor_pointer()
            .font_weight(FontWeight::MEDIUM)
            .text_color(if active { c.content } else { c.content_muted })
            .when(!active, |el| {
                el.hover(|s| s.text_color(c.content_secondary))
            })
            .child(self.label)
            .children(
                self.badge
                    .filter(|&n| n > 0)
                    .map(|n| div().ml(t.space(Space::Sm)).child(Badge::new(n))),
            )
            .children(self.close.map(|x| div().ml(t.space(Space::Sm)).child(x)))
            .h_full()
            .px(t.space(Space::Lg))
            .when(active, |el| {
                el.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .h(t.shape.hairline)
                        .bg(c.accent),
                )
            });
        match self.on_click {
            Some(f) => tab.on_click(f),
            None => tab,
        }
    }
}

/// A row of [`Tab`]s at the tab-bar height, with a hairline under it.
#[derive(IntoElement)]
pub struct TabBar {
    tabs: Vec<AnyElement>,
}

impl TabBar {
    pub fn new() -> Self {
        Self { tabs: Vec::new() }
    }
}

impl Default for TabBar {
    fn default() -> Self {
        Self::new()
    }
}

impl ParentElement for TabBar {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.tabs.extend(elements);
    }
}

impl RenderOnce for TabBar {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = cx.theme();
        div()
            .h(t.size(Size::TabBar))
            .flex_none()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(t.color.border)
            .children(self.tabs)
    }
}
