mod assets;
mod components;
mod fonts;
mod icons;
mod input;
mod logo;
mod menu;
pub mod motion;
mod theme;

pub use assets::Assets;
pub use components::{
    Badge, Button, ButtonKind, DotState, IconButton, ListRow, SegmentedControl, StatusDot, Tab,
    TabBar, Tooltip, empty_state,
};
pub use icons::{FileIcon, ICON_SIZE, file_icon, icon_for};
pub use input::{InputEvent, TextInput};
pub use logo::{Glyph, Lockup};
pub use menu::{ContextMenu, MenuItem};
pub use theme::{
    Appearance, CODE_SIZE, CODE_ZOOM, Colors, Motion, Shape, Size, Space, SyntaxColors,
    TerminalColors, Theme, Typography, UI_ZOOM,
};

use gpui::{App, Styled, WindowAppearance};

/// Registers fonts and the theme global; call once before opening windows.
pub fn init(cx: &mut App) {
    fonts::register(cx);
    input::init(cx);
    menu::init(cx);
    let appearance = system_appearance(cx);
    cx.set_global(Theme::new(appearance, motion::system_reduce_motion()));
}

/// The appearance macOS currently asks apps to use.
pub fn system_appearance(cx: &App) -> Appearance {
    appearance_for(cx.window_appearance())
}

pub fn appearance_for(appearance: WindowAppearance) -> Appearance {
    match appearance {
        WindowAppearance::Light | WindowAppearance::VibrantLight => Appearance::Light,
        WindowAppearance::Dark | WindowAppearance::VibrantDark => Appearance::Dark,
    }
}

/// Recolours every window, unless `appearance` is already the one shown.
pub fn set_appearance(appearance: Appearance, cx: &mut App) {
    if cx.theme().appearance != appearance {
        cx.global_mut::<Theme>().set_appearance(appearance);
        cx.refresh_windows();
    }
}

pub trait ActiveTheme {
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}

/// Text that ends in "…" when a line overflows its box.
/// Put it on an element whose only child is the text, not on a flex container, where the bare text cannot shrink.
pub trait Ellipsis: Styled + Sized {
    fn ellipsis(self) -> Self {
        self.whitespace_normal().line_clamp(1).text_ellipsis()
    }
}

impl<E: Styled> Ellipsis for E {}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{WhiteSpace, div};

    #[test]
    fn ellipsis_clamps_one_normal_whitespace_line() {
        let mut el = div().whitespace_nowrap().ellipsis();
        let style = el.text_style().clone().unwrap();
        assert_eq!(style.white_space, Some(WhiteSpace::Normal));
        assert_eq!(style.line_clamp, Some(1));
        assert!(style.text_overflow.is_some());
    }
}
