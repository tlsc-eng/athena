mod assets;
mod components;
mod fonts;
mod icons;
mod input;
mod logo;
pub mod motion;
mod theme;

pub use assets::Assets;
pub use components::{Button, ButtonKind, Tooltip, empty_state};
pub use icons::{FileIcon, ICON_SIZE, file_icon, icon_for};
pub use input::{InputEvent, TextInput};
pub use logo::{Glyph, Lockup};
pub use theme::{Colors, Motion, Shape, SyntaxColors, TerminalColors, Theme, Typography};

use gpui::App;

/// Registers fonts and the theme global; call once before opening windows.
pub fn init(cx: &mut App) {
    fonts::register(cx);
    input::init(cx);
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
