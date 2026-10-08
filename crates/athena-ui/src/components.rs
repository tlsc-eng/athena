use gpui::{
    AnyView, App, ClickEvent, Context, ElementId, FontWeight, IntoElement, Render, RenderOnce,
    SharedString, Window, div, prelude::*, px,
};

use crate::ActiveTheme;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    Primary,
    Secondary,
    Ghost,
    Danger,
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

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
            .h(px(28.))
            .px(px(12.))
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

/// Inverse caption tooltip, shown after gpui's hover delay without animation.
pub struct Tooltip {
    text: SharedString,
}

impl Tooltip {
    pub fn view(text: impl Into<SharedString>, cx: &mut App) -> AnyView {
        let text = text.into();
        cx.new(|_| Self { text }).into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div()
            .px(px(8.))
            .py(px(4.))
            .rounded(t.shape.radius_control)
            .bg(t.color.tooltip_bg)
            .text_color(t.color.tooltip_fg)
            .font_family(t.typography.ui.clone())
            .text_size(t.typography.caption)
            .child(self.text.clone())
    }
}

/// Centred empty state: what the area is for, and the one action that fills it.
pub fn empty_state(
    title: impl Into<SharedString>,
    body: impl Into<SharedString>,
    action: Option<Button>,
    cx: &App,
) -> gpui::Div {
    let t = cx.theme();
    div()
        .max_w(px(360.))
        .flex()
        .flex_col()
        .items_center()
        .gap(px(8.))
        .child(
            div()
                .text_size(t.typography.body)
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.color.content)
                .child(title.into()),
        )
        .child(
            div()
                .text_size(t.typography.caption)
                .text_color(t.color.content_muted)
                .text_center()
                .child(body.into()),
        )
        .children(action.map(|a| div().pt(px(8.)).child(a)))
}
