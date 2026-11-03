use gpui::{App, FontWeight, SharedString, div, prelude::*, px};

use super::Button;
use crate::ActiveTheme;

/// Centred empty state: what the area is for, and the one action that fills it.
pub fn empty_state(
    title: impl Into<SharedString>,
    body: impl Into<SharedString>,
    action: Option<Button>,
    cx: &App,
) -> gpui::Div {
    let t = cx.theme();
    div()
        .max_w(t.ui(360.))
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
