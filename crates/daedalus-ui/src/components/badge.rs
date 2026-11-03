use gpui::{
    App, Empty, Hsla, IntoElement, RenderOnce, Transformation, Window, div, prelude::*, radians,
    svg,
};

use crate::{ActiveTheme, Colors, Space};

const MAX_COUNT: usize = 99;

fn badge_text(count: usize) -> Option<String> {
    match count {
        0 => None,
        1..=MAX_COUNT => Some(count.to_string()),
        _ => Some(format!("{MAX_COUNT}+")),
    }
}

/// A count pill; renders nothing for zero and "99+" above 99.
#[derive(IntoElement)]
pub struct Badge {
    count: usize,
}

impl Badge {
    pub fn new(count: usize) -> Self {
        Self { count }
    }
}

impl RenderOnce for Badge {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = cx.theme();
        match badge_text(self.count) {
            Some(text) => div()
                .flex_none()
                .px(t.space(Space::Sm))
                .rounded(t.shape.radius_control)
                .bg(t.color.surface_active)
                .text_color(t.color.content_muted)
                .text_size(t.typography.caption)
                .child(text)
                .into_any_element(),
            None => Empty.into_any_element(),
        }
    }
}

/// What a Claude session is doing, as the sidebar's glyph shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DotState {
    Running,
    NeedsInput,
    /// Finished and not looked at since.
    Unread,
    Finished,
    Idle,
}

impl DotState {
    fn look(self, c: &Colors) -> (&'static str, Hsla) {
        match self {
            DotState::NeedsInput => ("ui/session-ask.svg", c.warning),
            DotState::Unread => ("ui/session-done.svg", c.success),
            DotState::Running => ("ui/session-running.svg", c.content_muted),
            DotState::Finished => ("ui/session-done-outline.svg", c.content_muted),
            DotState::Idle => ("ui/session-idle.svg", c.content_disabled),
        }
    }
}

/// A small glyph whose shape and colour mark a session's state.
#[derive(IntoElement)]
pub struct StatusDot {
    state: DotState,
    rotation: f32,
}

impl StatusDot {
    pub fn new(state: DotState) -> Self {
        Self {
            state,
            rotation: 0.,
        }
    }

    /// Turns the glyph by `radians`, for the running spinner to drive.
    pub fn rotation(mut self, radians: f32) -> Self {
        self.rotation = radians;
        self
    }
}

impl RenderOnce for StatusDot {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = cx.theme();
        let (asset, color) = self.state.look(&t.color);
        svg()
            .path(asset)
            .size(t.space(Space::Lg))
            .flex_none()
            .text_color(color)
            .with_transformation(Transformation::rotate(radians(self.rotation)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Theme;

    #[test]
    fn badge_hides_zero_and_caps_at_99() {
        assert_eq!(badge_text(0), None);
        assert_eq!(badge_text(5).as_deref(), Some("5"));
        assert_eq!(badge_text(99).as_deref(), Some("99"));
        assert_eq!(badge_text(100).as_deref(), Some("99+"));
        assert_eq!(badge_text(usize::MAX).as_deref(), Some("99+"));
    }

    #[test]
    fn dot_states_pick_a_glyph_and_colour_in_both_themes() {
        for t in [Theme::dark(false), Theme::light(false)] {
            let c = &t.color;
            assert_eq!(
                DotState::NeedsInput.look(c),
                ("ui/session-ask.svg", c.warning)
            );
            assert_eq!(DotState::Unread.look(c), ("ui/session-done.svg", c.success));
            assert_eq!(
                DotState::Running.look(c),
                ("ui/session-running.svg", c.content_muted)
            );
            assert_eq!(
                DotState::Finished.look(c),
                ("ui/session-done-outline.svg", c.content_muted)
            );
            assert_eq!(
                DotState::Idle.look(c),
                ("ui/session-idle.svg", c.content_disabled)
            );
        }
    }

    #[test]
    fn every_state_has_its_own_bundled_glyph() {
        let c = &Theme::dark(false).color;
        let assets: Vec<_> = [
            DotState::NeedsInput,
            DotState::Unread,
            DotState::Running,
            DotState::Finished,
            DotState::Idle,
        ]
        .map(|s| s.look(c).0)
        .into();
        for (i, a) in assets.iter().enumerate() {
            assert!(crate::assets::bundled(a).is_some(), "{a} is not bundled");
            assert!(!assets[..i].contains(a), "{a} is used twice");
        }
    }
}
