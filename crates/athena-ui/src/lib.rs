mod assets;
mod components;
mod fonts;
mod logo;
pub mod motion;
mod theme;

pub use assets::Assets;
pub use components::{Button, ButtonKind, Tooltip, empty_state};
pub use logo::{Glyph, Lockup};
pub use theme::{Colors, Motion, Shape, Theme, Typography};

use gpui::App;

/// Registers fonts and the theme global; call once before opening windows.
pub fn init(cx: &mut App) {
    fonts::register(cx);
    cx.set_global(Theme::dark(motion::system_reduce_motion()));
}

pub trait ActiveTheme {
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}
