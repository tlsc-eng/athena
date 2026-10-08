use gpui::{App, IntoElement, Pixels, RenderOnce, Window, div, prelude::*, svg};

use crate::ActiveTheme;

const GLYPH_ASPECT: f32 = 3016.5 / 3258.5;

/// The tlsc logogram shared by the product family, drawn in the current text colour.
#[derive(IntoElement)]
pub struct Glyph {
    height: Pixels,
}

impl Glyph {
    pub fn new(height: Pixels) -> Self {
        Self { height }
    }
}

impl RenderOnce for Glyph {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        svg()
            .path("brand/tlsc.svg")
            .h(self.height)
            .w(self.height * GLYPH_ASPECT)
            .text_color(cx.theme().color.content)
    }
}

/// Glyph plus lowercase wordmark, proportioned like the tlsc.io family lock-ups.
#[derive(IntoElement)]
pub struct Lockup {
    size: Pixels,
}

impl Lockup {
    pub fn new(size: Pixels) -> Self {
        Self { size }
    }
}

impl RenderOnce for Lockup {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .flex()
            .items_center()
            .gap(self.size * 0.25)
            .text_color(theme.color.content)
            .child(Glyph::new(self.size))
            .child(
                div()
                    .font_family(theme.typography.ui.clone())
                    .text_size(self.size * 0.69)
                    .line_height(self.size * 0.69)
                    .child("athena"),
            )
    }
}
