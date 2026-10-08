use std::time::Duration;

use gpui::{BoxShadow, Global, Hsla, Pixels, SharedString, point, px, rgb, rgba};

/// Semantic colour tokens; components use only these, never raw palette values.
#[derive(Clone)]
pub struct Colors {
    pub surface: Hsla,
    pub surface_sunken: Hsla,
    pub surface_accent: Hsla,
    pub surface_hover: Hsla,
    pub surface_active: Hsla,
    pub border: Hsla,
    pub border_strong: Hsla,
    pub content: Hsla,
    pub content_secondary: Hsla,
    pub content_muted: Hsla,
    pub content_disabled: Hsla,
    pub content_on_accent: Hsla,
    pub accent: Hsla,
    pub accent_hover: Hsla,
    pub accent_pressed: Hsla,
    pub danger: Hsla,
    pub danger_surface: Hsla,
    pub danger_strong: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub overlay: Hsla,
    pub tooltip_bg: Hsla,
    pub tooltip_fg: Hsla,
    pub focus_ring: Hsla,
}

#[derive(Clone)]
pub struct Typography {
    pub ui: SharedString,
    pub mono: SharedString,
    pub caption: Pixels,
    pub body: Pixels,
    pub heading: Pixels,
    pub display: Pixels,
    pub code: Pixels,
}

#[derive(Clone)]
pub struct Shape {
    pub radius_control: Pixels,
    pub radius_panel: Pixels,
    pub hairline: Pixels,
}

#[derive(Clone)]
pub struct Motion {
    pub reduced: bool,
    pub fast: Duration,
    pub base: Duration,
    pub slow: Duration,
}

#[derive(Clone)]
pub struct Theme {
    pub color: Colors,
    pub typography: Typography,
    pub shape: Shape,
    pub motion: Motion,
}

impl Global for Theme {}

impl Theme {
    /// Dark scheme from the tlsc.io token scale (hephaestus tokens.css), converted from OKLCH.
    pub fn dark(reduced_motion: bool) -> Self {
        let c = |hex: u32| -> Hsla { rgb(hex).into() };
        Self {
            color: Colors {
                surface: c(0x1a1614),
                surface_sunken: c(0x0b0807),
                surface_accent: c(0x331510),
                surface_hover: rgba(0x312d2a80).into(),
                surface_active: c(0x312d2a),
                border: c(0x312d2a),
                border_strong: c(0x5f5a55),
                content: c(0xf9f7f3),
                content_secondary: c(0xd5d4ce),
                content_muted: c(0xa8a49e),
                content_disabled: c(0x847f7a),
                content_on_accent: c(0x0b0807),
                accent: c(0xeb5e43),
                accent_hover: c(0xf37e61),
                accent_pressed: c(0xd9563a),
                danger: c(0xee6476),
                danger_surface: c(0x3f181d),
                danger_strong: c(0xaa1542),
                success: c(0x6fb07d),
                warning: c(0xdaa24f),
                overlay: rgba(0x0b0807b3).into(),
                tooltip_bg: c(0xf9f7f3),
                tooltip_fg: c(0x1a1614),
                focus_ring: c(0xeb5e43),
            },
            typography: Typography {
                ui: "Geist".into(),
                mono: "Geist Mono".into(),
                caption: px(12.),
                body: px(14.),
                heading: px(18.),
                display: px(24.),
                code: px(13.),
            },
            shape: Shape {
                radius_control: px(2.),
                radius_panel: px(4.),
                hairline: px(1.),
            },
            motion: Motion {
                reduced: reduced_motion,
                fast: Duration::from_millis(120),
                base: Duration::from_millis(200),
                slow: Duration::from_millis(320),
            },
        }
    }

    pub fn popover_shadow(&self) -> BoxShadow {
        BoxShadow {
            color: rgba(0x00000099).into(),
            offset: point(px(0.), px(8.)),
            blur_radius: px(24.),
            spread_radius: px(-8.),
        }
    }
}
