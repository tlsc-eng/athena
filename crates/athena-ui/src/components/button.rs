use gpui::{
    App, ClickEvent, ElementId, FontWeight, IntoElement, RenderOnce, SharedString, Window, div,
    prelude::*,
};

use super::ClickHandler;
use crate::ActiveTheme;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    Primary,
    Secondary,
    Ghost,
    Danger,
}

#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: SharedString,
    kind: ButtonKind,
    on_click: Option<ClickHandler>,
}

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>, kind: ButtonKind) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind,
            on_click: None,
        }
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(f));
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = cx.theme();
        let c = &t.color;
        let base = div()
            .id(self.id)
            .h(t.ui(28.))
            .px(t.ui(12.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(t.shape.radius_control)
            .text_size(t.typography.caption)
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .child(self.label);
        let styled = match self.kind {
            ButtonKind::Primary => base
                .bg(c.accent)
                .text_color(c.content_on_accent)
                .hover(|s| s.bg(c.accent_hover))
                .active(|s| s.bg(c.accent_pressed)),
            ButtonKind::Secondary => base
                .border_1()
                .border_color(c.border_strong)
                .text_color(c.content)
                .hover(|s| s.bg(c.surface_hover))
                .active(|s| s.bg(c.surface_active)),
            ButtonKind::Ghost => base
                .text_color(c.content_muted)
                .hover(|s| s.bg(c.surface_hover).text_color(c.content))
                .active(|s| s.bg(c.surface_active)),
            ButtonKind::Danger => base
                .bg(c.danger_surface)
                .text_color(c.danger)
                .hover(|s| s.bg(c.danger_strong).text_color(c.content))
                .active(|s| s.bg(c.danger_strong)),
        };
        match self.on_click {
            Some(f) => styled.on_click(f),
            None => styled,
        }
    }
}
