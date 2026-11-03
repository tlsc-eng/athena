use std::time::{Duration, Instant};

use daedalus_ui::{ActiveTheme, motion};
use gpui::{
    Animation, AnimationExt, AnyElement, Context, Hsla, IntoElement, Transformation, div,
    prelude::*, px, size, svg,
};

use super::Shell;

const DURATION: Duration = Duration::from_millis(800);
/// Outlasts the animation by a frame or two so the overlay never vanishes mid-fade.
const CLEAR_AFTER: Duration = Duration::from_millis(850);

const SCALE_FROM: f32 = 0.375;
const SCALE_TO: f32 = 5.;
const FADE_START: f32 = 0.45;
const GLYPH_HEIGHT: f32 = 256.;
const GLYPH_ASPECT: f32 = 3016.5 / 3258.5;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Frame {
    scale: f32,
    opacity: f32,
}

/// The splash at linear progress `d`; only the zoom is eased, so the fade timing stays predictable.
fn splash_frame(d: f32) -> Frame {
    let d = d.clamp(0., 1.);
    let eased = motion::ease_exit()(d);
    Frame {
        scale: SCALE_FROM + (SCALE_TO - SCALE_FROM) * eased,
        opacity: 1. - ((d - FADE_START) / (1. - FADE_START)).clamp(0., 1.),
    }
}

fn overlay(background: Hsla, glyph: Hsla) -> AnyElement {
    let mark = svg()
        .path("brand/tlsc.svg")
        .h(px(GLYPH_HEIGHT))
        .w(px(GLYPH_HEIGHT * GLYPH_ASPECT))
        .text_color(glyph)
        .with_animation("splash-glyph", Animation::new(DURATION), |el, d| {
            let s = splash_frame(d).scale;
            el.with_transformation(Transformation::scale(size(s, s)))
        });
    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .bg(background)
        .child(mark)
        .with_animation("splash", Animation::new(DURATION), |el, d| {
            el.opacity(splash_frame(d).opacity)
        })
        .into_any_element()
}

impl Shell {
    pub(super) fn begin_splash(&mut self, cx: &mut Context<Self>) {
        if cx.theme().motion.reduced {
            return;
        }
        self.splash = Some(Instant::now());
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CLEAR_AFTER).await;
            let _ = this.update(cx, |this, cx| {
                this.splash = None;
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_splash(&self, cx: &Context<Self>) -> Option<AnyElement> {
        self.splash?;
        let color = &cx.theme().color;
        Some(overlay(color.surface_sunken, color.content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_small_and_fully_opaque() {
        let f = splash_frame(0.);
        assert!((f.scale - 0.375).abs() < 1e-6);
        assert_eq!(f.opacity, 1.);
    }

    #[test]
    fn ends_large_and_fully_transparent() {
        let f = splash_frame(1.);
        assert!((f.scale - 5.).abs() < 1e-3);
        assert_eq!(f.opacity, 0.);
    }

    #[test]
    fn scale_never_shrinks_as_progress_advances() {
        let mut last = splash_frame(0.).scale;
        for i in 1..=100 {
            let scale = splash_frame(i as f32 / 100.).scale;
            assert!(scale >= last, "scale fell at step {i}");
            last = scale;
        }
    }

    #[test]
    fn opacity_holds_until_the_fade_starts_then_reaches_zero() {
        assert_eq!(splash_frame(0.2).opacity, 1.);
        assert_eq!(splash_frame(0.45).opacity, 1.);
        assert!((splash_frame(0.725).opacity - 0.5).abs() < 1e-5);
        let mut last = 1.;
        for i in 45..=100 {
            let o = splash_frame(i as f32 / 100.).opacity;
            assert!(o <= last);
            last = o;
        }
        assert_eq!(last, 0.);
    }

    #[test]
    fn progress_outside_zero_to_one_is_clamped() {
        assert_eq!(splash_frame(-3.), splash_frame(0.));
        assert_eq!(splash_frame(7.), splash_frame(1.));
    }
}
