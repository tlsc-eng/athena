use gpui::{AnyView, App, Context, IntoElement, Render, SharedString, Window, div, prelude::*};

use crate::ActiveTheme;

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
            .px(t.ui(8.))
            .py(t.ui(4.))
            .rounded(t.shape.radius_control)
            .bg(t.color.tooltip_bg)
            .text_color(t.color.tooltip_fg)
            .font_family(t.typography.ui.clone())
            .text_size(t.typography.caption)
            .child(self.text.clone())
    }
}
