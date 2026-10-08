use std::ops::Range;

use athena_ui::ActiveTheme;
use athena_ui::motion::{self, Opening};
use gpui::{
    Animation, AnyElement, Context, Corner, FontWeight, HighlightStyle, IntoElement, StyledText,
    anchored, deferred, div, point, prelude::*, px,
};

use crate::view::{EditorEvent, EditorView};

const MAX_WIDTH: f32 = 560.;

/// The signature of the call being typed, with the argument the cursor is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub label: String,
    /// Bytes of `label` naming the active parameter.
    pub active: Option<Range<usize>>,
    pub documentation: Option<String>,
}

#[derive(Default)]
pub(crate) struct Signing {
    shown: Option<(Signature, Opening)>,
    pending: Option<u64>,
    requests: u64,
    /// Characters that open or move the help, such as `(` and `,`.
    triggers: Vec<String>,
}

impl EditorView {
    pub fn set_signature_triggers(&mut self, triggers: Vec<String>) {
        self.signing.triggers = triggers;
    }

    pub(crate) fn signing_shown(&self) -> bool {
        self.signing.shown.is_some()
    }

    /// Typing a trigger opens the help; while it shows, every keystroke asks again so the active
    /// argument follows the cursor and the help closes once the call does.
    pub(crate) fn signature_after_typing(&mut self, typed: &str, cx: &mut Context<Self>) {
        let trigger = self
            .signing
            .triggers
            .iter()
            .any(|t| typed.ends_with(t.as_str()));
        if trigger || self.signing.shown.is_some() {
            self.request_signature(cx);
        }
    }

    pub(crate) fn request_signature(&mut self, cx: &mut Context<Self>) {
        let Some((line, character)) = self.cursor_utf16() else {
            return;
        };
        self.signing.requests += 1;
        let request = self.signing.requests;
        self.signing.pending = Some(request);
        cx.emit(EditorEvent::SignatureHelp {
            request,
            line,
            character,
        });
    }

    /// The answer to [`EditorEvent::SignatureHelp`]; `None` (outside any call) closes the help.
    pub fn show_signature(
        &mut self,
        request: u64,
        signature: Option<Signature>,
        cx: &mut Context<Self>,
    ) {
        if self.signing.pending != Some(request) {
            return;
        }
        self.signing.pending = None;
        let opened = self
            .signing
            .shown
            .as_ref()
            .map_or_else(Opening::now, |(_, o)| *o);
        self.signing.shown = signature.map(|s| (s, opened));
        cx.notify();
    }

    pub(crate) fn hide_signature(&mut self, cx: &mut Context<Self>) -> bool {
        self.signing.pending = None;
        let was_shown = self.signing.shown.take().is_some();
        if was_shown {
            cx.notify();
        }
        was_shown
    }

    pub(crate) fn render_signature(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (signature, opened) = self.signing.shown.as_ref()?;
        let origin = self.char_origin(self.cursor.head())?;
        let t = cx.theme();
        let highlights = signature
            .active
            .clone()
            .filter(|r| signature.label.get(r.clone()).is_some())
            .map(|r| {
                (
                    r,
                    HighlightStyle {
                        color: Some(t.color.accent),
                        font_weight: Some(FontWeight::BOLD),
                        ..Default::default()
                    },
                )
            });
        let panel = div()
            .id("signature-help")
            .occlude()
            .max_w(px(MAX_WIDTH))
            .px(px(10.))
            .py(px(6.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_panel)
            .shadow(vec![t.popover_shadow()])
            .text_size(t.typography.caption)
            .child(
                div()
                    .font_family(t.typography.mono.clone())
                    .text_color(t.color.content)
                    .child(
                        StyledText::new(signature.label.clone())
                            .with_highlights(highlights.into_iter().collect::<Vec<_>>()),
                    ),
            )
            .children(signature.documentation.clone().map(|doc| {
                div()
                    .font_family(t.typography.ui.clone())
                    .text_color(t.color.content_muted)
                    .child(doc)
            }));
        let panel = motion::animate_enter(
            t.motion.reduced,
            opened.running(t.motion.fast),
            panel,
            "signature-open",
            Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d),
        );
        Some(
            deferred(
                anchored()
                    .position(point(origin.x, origin.y - px(2.)))
                    .anchor(Corner::BottomLeft)
                    .snap_to_window_with_margin(px(8.))
                    .child(panel),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}
