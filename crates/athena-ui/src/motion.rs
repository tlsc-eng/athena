use std::time::{Duration, Instant};

use gpui::{Animation, AnimationExt, AnyElement, ElementId, IntoElement};

/// CSS-style cubic-bezier easing, solved for t by Newton iteration.
pub fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32) -> impl Fn(f32) -> f32 + Clone {
    move |x| {
        let bez = |a: f32, b: f32, t: f32| {
            3. * a * (1. - t).powi(2) * t + 3. * b * (1. - t) * t * t + t.powi(3)
        };
        let mut t = x;
        for _ in 0..8 {
            let err = bez(x1, x2, t) - x;
            let slope =
                3. * (1. - t).powi(2) * x1 + 6. * (1. - t) * t * (x2 - x1) + 3. * t * t * (1. - x2);
            if slope.abs() < 1e-6 {
                break;
            }
            t -= err / slope;
        }
        bez(y1, y2, t.clamp(0., 1.))
    }
}

pub fn ease_standard() -> impl Fn(f32) -> f32 + Clone {
    cubic_bezier(0.2, 0., 0., 1.)
}

pub fn ease_enter() -> impl Fn(f32) -> f32 + Clone {
    cubic_bezier(0., 0., 0., 1.)
}

pub fn ease_exit() -> impl Fn(f32) -> f32 + Clone {
    cubic_bezier(0.3, 0., 1., 1.)
}

/// Runs `animator` as an animation, or renders its final frame directly when motion is reduced.
pub fn animate_if<E: IntoElement + AnimationExt + 'static>(
    reduced: bool,
    element: E,
    id: impl Into<ElementId>,
    animation: Animation,
    animator: impl Fn(E, f32) -> E + 'static,
) -> AnyElement {
    if reduced {
        animator(element, 1.).into_any_element()
    } else {
        element
            .with_animation(id, animation, animator)
            .into_any_element()
    }
}

/// Marks an element that is animating out; `generation` keys the oneshot so a reopen replays it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Closing {
    pub since: Instant,
    pub generation: u64,
}

impl Closing {
    pub fn new(generation: u64) -> Self {
        Self {
            since: Instant::now(),
            generation,
        }
    }
}

/// Exit counterpart of [`animate_if`]: eases out, and renders the final (gone) frame when motion is reduced.
pub fn animate_exit<E: IntoElement + AnimationExt + 'static>(
    reduced: bool,
    element: E,
    id: impl Into<ElementId>,
    duration: Duration,
    animator: impl Fn(E, f32) -> E + 'static,
) -> AnyElement {
    animate_if(
        reduced,
        element,
        id,
        Animation::new(duration).with_easing(ease_exit()),
        animator,
    )
}

/// How long to keep a closing element mounted before removing it.
pub fn exit_delay(reduced: bool, duration: Duration) -> Duration {
    if reduced { Duration::ZERO } else { duration }
}

/// Reads macOS "Reduce motion" (System Settings, Accessibility, Display).
pub fn system_reduce_motion() -> bool {
    #[cfg(target_os = "macos")]
    {
        objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_fixed() {
        for ease in [cubic_bezier(0.2, 0., 0., 1.), cubic_bezier(0.3, 0., 1., 1.)] {
            assert!(ease(0.).abs() < 1e-4);
            assert!((ease(1.) - 1.).abs() < 1e-4);
        }
    }

    #[test]
    fn standard_is_front_loaded() {
        let ease = ease_standard();
        assert!(ease(0.5) > 0.8);
    }

    #[test]
    fn exit_delay_is_zero_when_motion_is_reduced() {
        let d = Duration::from_millis(120);
        assert_eq!(exit_delay(true, d), Duration::ZERO);
        assert_eq!(exit_delay(false, d), d);
    }

    #[test]
    fn exit_is_back_loaded() {
        let ease = ease_exit();
        assert!(ease(0.5) < 0.5);
    }
}
