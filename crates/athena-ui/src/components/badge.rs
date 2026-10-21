use gpui::{App, Empty, Hsla, IntoElement, RenderOnce, Window, div, prelude::*};

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

/// What a Claude session is doing, as the sidebar's dot shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DotState {
    Running,
    NeedsInput,
    Finished,
    Idle,
}

impl DotState {
    fn color(self, c: &Colors) -> Hsla {
        match self {
            DotState::Running => c.success,
            DotState::NeedsInput => c.warning,
            DotState::Finished | DotState::Idle => c.content_muted,
        }
    }

    /// An idle session has nothing to report, so its dot is an outline.
    fn hollow(self) -> bool {
        self == DotState::Idle
    }
}

/// A small square marking a session's state: filled when it has something to say, hollow when idle.
#[derive(IntoElement)]
pub struct StatusDot {
    state: DotState,
}

impl StatusDot {
    pub fn new(state: DotState) -> Self {
        Self { state }
    }
}

impl RenderOnce for StatusDot {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = cx.theme();
        let color = self.state.color(&t.color);
        let dot = div().size(t.space(Space::Sm)).flex_none();
        if self.state.hollow() {
            dot.border_1().border_color(color)
        } else {
            dot.bg(color)
        }
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
    fn dot_states_use_the_sidebar_colours() {
        for t in [Theme::dark(false), Theme::light(false)] {
            let c = &t.color;
            assert_eq!(DotState::Running.color(c), c.success);
            assert_eq!(DotState::NeedsInput.color(c), c.warning);
            assert_eq!(DotState::Finished.color(c), c.content_muted);
            assert_eq!(DotState::Idle.color(c), c.content_muted);
        }
        assert!(DotState::Idle.hollow());
        assert!(!DotState::Finished.hollow());
        assert!(!DotState::Running.hollow());
        assert!(!DotState::NeedsInput.hollow());
    }
}
