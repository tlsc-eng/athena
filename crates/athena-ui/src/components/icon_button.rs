use gpui::{
    AnyElement, App, ClickEvent, ElementId, FontWeight, IntoElement, RenderOnce, SharedString,
    Window, div, prelude::*,
};

use super::{ClickHandler, Tooltip};
use crate::{ActiveTheme, Size, Space};

/// A compact title-bar or status-bar button: an icon and/or label, with an optional count.
#[derive(IntoElement)]
pub struct IconButton {
    id: ElementId,
    icon: Option<AnyElement>,
    label: Option<SharedString>,
    badge: Option<usize>,
    active: bool,
    fill: bool,
    tooltip: Option<SharedString>,
    on_click: Option<ClickHandler>,
}

impl IconButton {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            icon: None,
            label: None,
            badge: None,
            active: false,
            fill: false,
            tooltip: None,
            on_click: None,
        }
    }

    pub fn icon(mut self, icon: impl IntoElement) -> Self {
        self.icon = Some(icon.into_any_element());
        self
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// A count after the label; zero shows nothing.
    pub fn badge(mut self, count: usize) -> Self {
        self.badge = Some(count);
        self
    }

    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    /// Fills the height of its container with square corners, for a status bar's flush cells.
    pub fn fill(mut self) -> Self {
        self.fill = true;
        self
    }

    pub fn tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(f));
        self
    }
}

impl RenderOnce for IconButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = cx.theme();
        let c = &t.color;
        let button = div()
            .id(self.id)
            .map(|el| {
                if self.fill {
                    el.h_full()
                } else {
                    el.h(t.size(Size::Control)).rounded(t.shape.radius_control)
                }
            })
            .px(t.space(Space::Md))
            .flex()
            .items_center()
            .gap(t.space(Space::Sm))
            .cursor_pointer()
            .text_color(if self.active {
                c.content
            } else {
                c.content_muted
            })
            .hover(|s| s.bg(c.surface_hover).text_color(c.content))
            .children(self.icon)
            .children(self.label)
            .children(self.badge.filter(|&n| n > 0).map(|n| {
                div()
                    .text_color(c.accent)
                    .font_weight(FontWeight::MEDIUM)
                    .child(n.to_string())
            }))
            .when_some(self.tooltip, |el, text| {
                el.tooltip(move |_, cx| Tooltip::view(text.clone(), cx))
            });
        match self.on_click {
            Some(f) => button.on_click(f),
            None => button,
        }
    }
}
