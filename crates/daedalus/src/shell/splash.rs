use std::time::{Duration, Instant};

use daedalus_ui::{ActiveTheme, motion};
use gpui::{
    Animation, AnimationExt, AnyElement, Bounds, BoxShadow, Context, Hsla, IntoElement, Pixels,
    Size, Window, canvas, div, fill, linear_color_stop, linear_gradient, point, prelude::*, px,
};

use super::Shell;

const TOTAL_MS: f32 = 1450.;
/// Outlasts the animation by a frame or two so the overlay never vanishes mid-fade.
const CLEAR_AFTER_MS: f32 = TOTAL_MS + 50.;
const PREVIEWS_AT_MS: f32 = 1300.;

const GLYPH_HEIGHT: f32 = 256.;
const VB_W: f32 = 3016.5;
const VB_H: f32 = 3258.5;
const COL: f32 = 377.25;
const SLOT_CX: f32 = (1508.25 + 1885.) / 2.;
const DOLLY_START: f32 = 820.;
const DOLLY_END: f32 = 1420.;
const SLOT_FADE_END: f32 = 1000.;
const GLOW_ALPHA: f32 = 0.35;
const GLOW_SIGMA: f32 = 0.05;
const TRAIL_ALPHA: f32 = 0.7;
/// The dolly closes on a window this wide at 256 px; wider windows need a proportionally deeper dive.
const DOLLY_REF_WIDTH: f32 = 1440.;
const DOLLY_SCALE: f32 = 32.;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Role {
    Backdrop,
    Mark,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Fill {
    Solid {
        role: Role,
        alpha: f32,
    },
    /// CSS-style angle: 0 is toward the top, 90 toward the right.
    Gradient {
        role: Role,
        angle: f32,
        from: f32,
        to: f32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Glow {
    role: Role,
    alpha: f32,
    sigma: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Quad {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    fill: Fill,
    glow: Option<Glow>,
}

struct Entry {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    start: f32,
    dur: f32,
    dx: f32,
    dy: f32,
}

const RISE: f32 = 0.45 * VB_H;
const ENTRIES: [Entry; 7] = [
    Entry {
        x: 377.,
        y: 0.,
        w: COL,
        h: VB_H,
        start: 0.,
        dur: 380.,
        dx: 0.,
        dy: RISE,
    },
    Entry {
        x: 1131.,
        y: 0.,
        w: COL,
        h: VB_H,
        start: 60.,
        dur: 380.,
        dx: 0.,
        dy: RISE,
    },
    Entry {
        x: 2639.25,
        y: 0.,
        w: COL,
        h: VB_H,
        start: 120.,
        dur: 380.,
        dx: 0.,
        dy: RISE,
    },
    Entry {
        x: 1885.,
        y: 2493.5,
        w: COL,
        h: 765.,
        start: 180.,
        dur: 380.,
        dx: 0.,
        dy: -RISE,
    },
    Entry {
        x: 1885.,
        y: 1246.75,
        w: COL,
        h: 765.,
        start: 220.,
        dur: 380.,
        dx: 0.,
        dy: -RISE,
    },
    Entry {
        x: 1885.,
        y: 0.,
        w: COL,
        h: 765.,
        start: 260.,
        dur: 380.,
        dx: 0.,
        dy: -RISE,
    },
    Entry {
        x: 0.,
        y: 0.,
        w: 377.,
        h: 382.5,
        start: 340.,
        dur: 300.,
        dx: -1.2 * COL,
        dy: 0.,
    },
];

fn ms(v: f32) -> Duration {
    Duration::from_secs_f32(v / 1000.)
}

fn seg(t: f32, t0: f32, t1: f32, ease: impl Fn(f32) -> f32) -> f32 {
    ease(((t - t0) / (t1 - t0)).clamp(0., 1.))
}

fn trail(x: f32, y: f32, w: f32, h: f32, angle: f32, alpha: f32) -> Quad {
    Quad {
        x,
        y,
        w,
        h,
        fill: Fill::Gradient {
            role: Role::Mark,
            angle,
            from: alpha,
            to: 0.,
        },
        glow: None,
    }
}

/// The splash at `t_ms` for a viewport of this size: the monogram rises into place, then the camera rushes into the gap between B and C.
fn splash_frame(t_ms: f32, viewport: Size<f32>) -> Vec<Quad> {
    let t = t_ms.clamp(0., TOTAL_MS);
    let (vw, vh) = (viewport.width, viewport.height);
    let standard = motion::ease_standard();
    let enter = motion::ease_enter();
    let push = motion::cubic_bezier(0.55, 0., 0.15, 1.);

    let mark_h = GLYPH_HEIGHT;
    let u = mark_h / VB_H;
    let (ox, oy) = ((vw - VB_W * u) / 2., (vh - VB_H * u) / 2.);
    let sx = |v: f32| ox + v * u;
    let sy_ = |v: f32| oy + v * u;

    let k = seg(t, DOLLY_START, DOLLY_END, &push);
    let s = (DOLLY_SCALE * (vw / DOLLY_REF_WIDTH).max(1.)).powf(k);
    let sy = s * (1. + 1.5 * k);
    let fy = VB_H / 2.;
    let width_gain = s.powf(0.45);
    let map_x = |cx: f32, w: f32| {
        let w2 = w * width_gain;
        (SLOT_CX + (cx - SLOT_CX) * s - w2 / 2., w2)
    };
    let map_y = |v: f32| fy + (v - fy) * sy;

    let (bx, bw) = map_x(1131. + COL / 2., COL);
    let (c1x, _) = map_x(1885. + COL / 2., COL);
    let (sl, sr) = (sx(bx + bw), sx(c1x));
    let (st, sb) = (sy_(map_y(0.)), sy_(map_y(VB_H)));
    let backdrop = |x, y, w, h, alpha| Quad {
        x,
        y,
        w,
        h,
        fill: Fill::Solid {
            role: Role::Backdrop,
            alpha,
        },
        glow: None,
    };
    let mut quads = vec![
        backdrop(0., 0., vw, st, 1.),
        backdrop(0., sb, vw, vh - sb, 1.),
        backdrop(0., st, sl, sb - st, 1.),
        backdrop(sr, st, vw - sr, sb - st, 1.),
        backdrop(
            sl,
            st,
            sr - sl,
            sb - st,
            1. - seg(t, DOLLY_START, SLOT_FADE_END, &standard),
        ),
    ];

    let glow_alpha = GLOW_ALPHA
        * seg(t, 600., 780., &standard)
        * (1. - seg(t, DOLLY_START, SLOT_FADE_END, &standard));
    for e in &ENTRIES {
        if t < e.start {
            continue;
        }
        let r = seg(t, e.start, e.start + e.dur, &enter);
        let alpha = ((t - e.start) / (e.dur * 0.35)).clamp(0., 1.);
        let (off_x, off_y) = (e.dx * (1. - r), e.dy * (1. - r));
        let (px_, w) = map_x(e.x + off_x + e.w / 2., e.w);
        let (y0, y1) = (map_y(e.y + off_y), map_y(e.y + off_y + e.h));
        let (x, w) = (sx(px_), w * u);
        let (y, h) = (sy_(y0), sy_(y1) - sy_(y0));

        let lead = 0.6 * (off_x.abs() + off_y.abs()) * u;
        if lead > 0.5 {
            let a = TRAIL_ALPHA * alpha;
            if e.dy > 0. {
                quads.push(trail(x, y + h, w, lead, 180., a));
            }
            if e.dy < 0. {
                quads.push(trail(x, y - lead, w, lead, 0., a));
            }
            if e.dx != 0. {
                quads.push(trail(x - lead, y, lead, h, 270., a));
            }
        }
        let tip = h * 0.35 * k;
        if tip > 0.5 {
            quads.push(trail(x, y - tip, w, tip, 0., 1.));
            quads.push(trail(x, y + h, w, tip, 180., 1.));
        }
        quads.push(Quad {
            x,
            y,
            w,
            h,
            fill: Fill::Solid {
                role: Role::Mark,
                alpha,
            },
            glow: (glow_alpha > 0.001).then_some(Glow {
                role: Role::Mark,
                alpha: glow_alpha,
                sigma: GLOW_SIGMA * mark_h,
            }),
        });
    }
    quads
}

fn paint_frame(t_ms: f32, bounds: Bounds<Pixels>, backdrop: Hsla, mark: Hsla, window: &mut Window) {
    let role = |r| match r {
        Role::Backdrop => backdrop,
        Role::Mark => mark,
    };
    let viewport = Size {
        width: bounds.size.width.into(),
        height: bounds.size.height.into(),
    };
    for q in splash_frame(t_ms, viewport) {
        if q.w <= 0. || q.h <= 0. {
            continue;
        }
        let quad = Bounds::new(
            bounds.origin + point(px(q.x), px(q.y)),
            gpui::size(px(q.w), px(q.h)),
        );
        if let Some(g) = q.glow {
            window.paint_shadows(
                quad,
                Default::default(),
                &[BoxShadow {
                    color: role(g.role).opacity(g.alpha),
                    offset: point(px(0.), px(0.)),
                    blur_radius: px(g.sigma),
                    spread_radius: px(0.),
                }],
            );
        }
        match q.fill {
            Fill::Solid { role: r, alpha } => window.paint_quad(fill(quad, role(r).opacity(alpha))),
            Fill::Gradient {
                role: r,
                angle,
                from,
                to,
            } => window.paint_quad(fill(
                quad,
                linear_gradient(
                    angle,
                    linear_color_stop(role(r).opacity(from), 0.),
                    linear_color_stop(role(r).opacity(to), 1.),
                ),
            )),
        }
    }
}

fn overlay(start: Instant, backdrop: Hsla, mark: Hsla) -> AnyElement {
    div()
        .absolute()
        .inset_0()
        .occlude()
        .with_animation("splash", Animation::new(ms(TOTAL_MS)), move |el, _| {
            el.child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, _| {
                        let t = (start.elapsed().as_secs_f32() * 1000.).min(TOTAL_MS);
                        paint_frame(t, bounds, backdrop, mark, window)
                    },
                )
                .size_full(),
            )
        })
        .into_any_element()
}

impl Shell {
    pub(super) fn begin_splash(&mut self, cx: &mut Context<Self>) {
        if cx.theme().motion.reduced {
            return;
        }
        self.splash = Some(Instant::now());
        self.splash_revealed = false;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(ms(PREVIEWS_AT_MS)).await;
            let _ = this.update(cx, |this, cx| {
                this.splash_revealed = true;
                cx.notify();
            });
            cx.background_executor()
                .timer(ms(CLEAR_AFTER_MS - PREVIEWS_AT_MS))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.splash = None;
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_splash(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let start = self.splash?;
        let color = &cx.theme().color;
        Some(overlay(start, color.surface_sunken, color.content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWPORTS: [(f32, f32); 3] = [(1440., 900.), (1920., 1200.), (2560., 1600.)];

    const NARROW_VIEWPORTS: [(f32, f32); 3] = [(640., 400.), (800., 600.), (1440., 2560.)];

    fn vp(w: f32, h: f32) -> Size<f32> {
        Size {
            width: w,
            height: h,
        }
    }

    fn alpha(q: &Quad) -> f32 {
        match q.fill {
            Fill::Solid { alpha, .. } => alpha,
            Fill::Gradient { from, to, .. } => from.max(to),
        }
    }

    fn is_backdrop(q: &Quad) -> bool {
        matches!(
            q.fill,
            Fill::Solid {
                role: Role::Backdrop,
                ..
            }
        )
    }

    fn is_mark(q: &Quad) -> bool {
        matches!(
            q.fill,
            Fill::Solid {
                role: Role::Mark,
                ..
            }
        )
    }

    fn covers(q: &Quad, x: f32, y: f32) -> bool {
        q.w > 0. && q.h > 0. && x >= q.x && x <= q.x + q.w && y >= q.y && y <= q.y + q.h
    }

    fn backdrop_touches_viewport(t: f32, (w, h): (f32, f32)) -> bool {
        splash_frame(t, vp(w, h)).iter().any(|q| {
            is_backdrop(q)
                && alpha(q) > 0.
                && q.w > 0.
                && q.h > 0.
                && q.x < w
                && q.x + q.w > 0.
                && q.y < h
                && q.y + q.h > 0.
        })
    }

    fn first_clear(viewport: (f32, f32)) -> f32 {
        (0..=TOTAL_MS as u32)
            .map(|t| t as f32)
            .find(|&t| !backdrop_touches_viewport(t, viewport))
            .expect("the backdrop never leaves the viewport")
    }

    #[test]
    fn backdrop_covers_the_whole_viewport_at_the_start() {
        let (w, h) = (1440., 900.);
        let quads = splash_frame(0., vp(w, h));
        for i in 0..=48 {
            for j in 0..=30 {
                let (x, y) = (w * i as f32 / 48., h * j as f32 / 30.);
                assert!(
                    quads
                        .iter()
                        .any(|q| is_backdrop(q) && alpha(q) >= 1. && covers(q, x, y)),
                    "({x}, {y}) is uncovered"
                );
            }
        }
    }

    #[test]
    fn the_hold_shows_the_monogram_centred_at_mark_height() {
        let (w, h) = (1440., 900.);
        let u = GLYPH_HEIGHT / VB_H;
        let (ox, oy) = ((w - VB_W * u) / 2., (h - GLYPH_HEIGHT) / 2.);
        let svg = [
            (377., 0., COL, VB_H),
            (1131., 0., COL, VB_H),
            (2639.25, 0., COL, VB_H),
            (1885., 2493.5, COL, 765.),
            (1885., 1246.75, COL, 765.),
            (1885., 0., COL, 765.),
            (0., 0., 377., 382.5),
        ];
        let marks: Vec<_> = splash_frame(700., vp(w, h))
            .into_iter()
            .filter(is_mark)
            .collect();
        assert_eq!(marks.len(), svg.len());
        for (x, y, rw, rh) in svg {
            let (x, y, rw, rh) = (ox + x * u, oy + y * u, rw * u, rh * u);
            assert!(
                marks.iter().any(|q| (q.x - x).abs() < 1e-3
                    && (q.y - y).abs() < 1e-3
                    && (q.w - rw).abs() < 1e-3
                    && (q.h - rh).abs() < 1e-3
                    && alpha(q) == 1.),
                "no mark quad at ({x}, {y}, {rw}, {rh})"
            );
        }
    }

    #[test]
    fn previews_return_only_once_the_backdrop_has_left_every_viewport() {
        for viewport in VIEWPORTS.into_iter().chain(NARROW_VIEWPORTS) {
            let first = first_clear(viewport);
            assert!(PREVIEWS_AT_MS >= first, "{viewport:?} clears at {first}");
            if viewport.0 >= 1440. {
                assert!(
                    PREVIEWS_AT_MS - first <= 60.,
                    "{viewport:?} clears at {first}"
                );
            }
            for t in (PREVIEWS_AT_MS as u32)..=(TOTAL_MS as u32) {
                assert!(
                    !backdrop_touches_viewport(t as f32, viewport),
                    "{viewport:?} at {t}"
                );
            }
        }
    }

    #[test]
    fn nothing_is_left_of_the_backdrop_at_the_end() {
        for viewport in VIEWPORTS {
            assert!(!backdrop_touches_viewport(TOTAL_MS, viewport));
        }
    }

    #[test]
    fn every_value_is_finite_through_the_whole_run() {
        for viewport in VIEWPORTS {
            for t in 0..=TOTAL_MS as u32 {
                for q in splash_frame(t as f32, vp(viewport.0, viewport.1)) {
                    let glow = q.glow.map_or(0., |g| g.alpha + g.sigma);
                    assert!(
                        [q.x, q.y, q.w, q.h, alpha(&q), glow]
                            .iter()
                            .all(|v| v.is_finite())
                    );
                }
            }
        }
    }

    #[test]
    fn time_outside_the_run_is_clamped() {
        let v = vp(1440., 900.);
        assert_eq!(splash_frame(-50., v), splash_frame(0., v));
        assert_eq!(splash_frame(TOTAL_MS + 500., v), splash_frame(TOTAL_MS, v));
    }

    #[test]
    fn visible_marks_move_smoothly_before_the_dolly() {
        // Below half alpha the part is nearly invisible, and ease_enter's Newton solve is unreliable there.
        const MAX_STEP: f32 = 3.;
        let v = vp(1440., 900.);
        let centres = |t: f32| -> Vec<(f32, f32, f32)> {
            splash_frame(t, v)
                .iter()
                .filter(|q| is_mark(q))
                .map(|q| (q.x + q.w / 2., q.y + q.h / 2., alpha(q)))
                .collect()
        };
        let mut last = centres(0.);
        for t in 1..DOLLY_START as u32 {
            let now = centres(t as f32);
            // Parts enter in table order, so the shorter frame is a prefix of the longer one.
            for (a, b) in last
                .iter()
                .zip(&now)
                .filter(|(a, b)| a.2 >= 0.5 && b.2 >= 0.5)
            {
                assert!(
                    (a.0 - b.0).abs() < MAX_STEP && (a.1 - b.1).abs() < MAX_STEP,
                    "jump at {t} ms: {a:?} -> {b:?}"
                );
            }
            last = now;
        }
    }
}
