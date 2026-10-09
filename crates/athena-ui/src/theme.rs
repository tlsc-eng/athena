use std::ops::RangeInclusive;
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

/// Editor and terminal font size before any zoom.
pub const CODE_SIZE: f32 = 13.;

/// Zoom steps, 1px each, keeping code between 6px and 40px.
pub const CODE_ZOOM: RangeInclusive<i32> = -7..=27;

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

/// Terminal palette: the 16 ANSI colours plus defaults, minted at one tonal level.
#[derive(Clone)]
pub struct TerminalColors {
    pub foreground: Hsla,
    /// Bold default-colour text.
    pub bright_foreground: Hsla,
    /// The lightest faint (SGR 2) text may be pulled down to; kept readable on the background.
    pub dim_foreground: Hsla,
    pub background: Hsla,
    pub cursor: Hsla,
    pub cursor_text: Hsla,
    pub selection: Hsla,
    /// Normal 0..8 then bright 8..16, in ANSI order.
    pub ansi: [Hsla; 16],
}

/// Code colours, with hues shared with the terminal palette so code and shell output agree.
#[derive(Clone)]
pub struct SyntaxColors {
    pub text: Hsla,
    pub keyword: Hsla,
    pub function: Hsla,
    pub property: Hsla,
    pub type_: Hsla,
    pub attribute: Hsla,
    pub string: Hsla,
    pub string_special: Hsla,
    pub constant: Hsla,
    pub variable_builtin: Hsla,
    pub operator: Hsla,
    pub punctuation: Hsla,
    pub punctuation_special: Hsla,
    pub comment: Hsla,
    pub heading: Hsla,
    pub error: Hsla,
    pub bracket_match: Hsla,
    pub line_number: Hsla,
    pub line_number_active: Hsla,
    pub current_line: Hsla,
    pub indent_guide: Hsla,
    /// The guide of the block the cursor is in.
    pub indent_guide_active: Hsla,
    /// Dots and arrows marking spaces and tabs inside a selection.
    pub whitespace: Hsla,
}

/// Whether the colours are minted for a light or a dark background.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    Light,
    Dark,
}

#[derive(Clone)]
pub struct Theme {
    pub appearance: Appearance,
    pub color: Colors,
    pub terminal: TerminalColors,
    pub syntax: SyntaxColors,
    pub typography: Typography,
    pub shape: Shape,
    pub motion: Motion,
}

impl Global for Theme {}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

fn ca(hex: u32) -> Hsla {
    rgba(hex).into()
}

/// Dark scheme from the tlsc.io token scale (hephaestus tokens.css), converted from OKLCH.
fn dark_scheme() -> (Colors, TerminalColors, SyntaxColors) {
    (
        Colors {
            surface: c(0x1a1614),
            surface_sunken: c(0x0b0807),
            surface_accent: c(0x331510),
            surface_hover: ca(0x312d2a80),
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
            overlay: ca(0x0b0807b3),
            tooltip_bg: c(0xf9f7f3),
            tooltip_fg: c(0x1a1614),
            focus_ring: c(0xeb5e43),
        },
        TerminalColors {
            foreground: c(0xefeee7),
            bright_foreground: c(0xffffff),
            dim_foreground: c(0x9a958f),
            background: c(0x0b0807),
            cursor: c(0xeb5e43),
            cursor_text: c(0x0b0807),
            selection: c(0x331510),
            // Hues follow Claude Code's dark theme (success, error, warning, permission).
            ansi: [
                c(0x15110f),
                c(0xe9566a),
                c(0x4eba65),
                c(0xe0a93a),
                c(0x6f9ee6),
                c(0xc287bc),
                c(0x4fb6ba),
                c(0xd5d4ce),
                c(0x7a746e),
                c(0xff6b80),
                c(0x7ad08c),
                c(0xffc107),
                c(0xb1b9f9),
                c(0xe0b3da),
                c(0x86cccf),
                c(0xffffff),
            ],
        },
        SyntaxColors {
            text: c(0xefeee7),
            keyword: c(0xc287bc),
            function: c(0x6f9ee6),
            property: c(0xb1b9f9),
            type_: c(0x6fb07d),
            attribute: c(0x94cf9f),
            string: c(0xdaa24f),
            string_special: c(0xf0c374),
            constant: c(0x86cccf),
            variable_builtin: c(0xdcabd6),
            operator: c(0xd5d4ce),
            punctuation: c(0xa8a49e),
            punctuation_special: c(0x4fb6ba),
            comment: c(0x847f7a),
            heading: c(0x6f9ee6),
            error: c(0xe9566a),
            bracket_match: ca(0x5f5a5599),
            line_number: c(0x5f5a55),
            line_number_active: c(0xa8a49e),
            current_line: c(0x1a1614),
            indent_guide: c(0x312d2a),
            indent_guide_active: c(0x5f5a55),
            whitespace: c(0x5f5a55),
        },
    )
}

/// Light scheme from the same token scale's light semantic layer.
fn light_scheme() -> (Colors, TerminalColors, SyntaxColors) {
    (
        Colors {
            surface: c(0xfefdfc),
            surface_sunken: c(0xf9f7f3),
            // A step stronger than the tokens' brand-50, so selections read on the sunken surface.
            surface_accent: c(0xffdbcf),
            surface_hover: ca(0xd5d4ce59),
            surface_active: c(0xefeee7),
            border: c(0xd5d4ce),
            border_strong: c(0x847f7a),
            content: c(0x1a1614),
            content_secondary: c(0x312d2a),
            content_muted: c(0x5f5a55),
            content_disabled: c(0xa8a49e),
            content_on_accent: c(0xf9f7f3),
            accent: c(0xc6361e),
            accent_hover: c(0xa1301d),
            accent_pressed: c(0x8e2a19),
            danger: c(0xaa1542),
            danger_surface: c(0xffe7ea),
            // Hovered danger buttons keep dark text, so the hover fill stays light.
            danger_strong: c(0xf9c6cd),
            success: c(0x456f4e),
            warning: c(0x9a6500),
            overlay: ca(0x0b080766),
            tooltip_bg: c(0x1a1614),
            tooltip_fg: c(0xf9f7f3),
            focus_ring: c(0xc6361e),
        },
        TerminalColors {
            foreground: c(0x1a1614),
            bright_foreground: c(0x0b0807),
            dim_foreground: c(0x75706b),
            background: c(0xf9f7f3),
            cursor: c(0xc6361e),
            cursor_text: c(0xfefdfc),
            selection: c(0xffdbcf),
            // Claude Code's light hues; white and bright white are greys, as VS Code's light
            // terminal has them, so text a program prints in white stays readable.
            ansi: [
                c(0x1a1614),
                c(0xab2b3f),
                c(0x2c7a39),
                c(0x85600f),
                c(0x3c4fd6),
                c(0x9a3b8f),
                c(0x0b6e74),
                c(0x75706b),
                c(0x5f5a55),
                c(0xc4374f),
                c(0x3b7d2a),
                c(0x8f6500),
                c(0x4a5ce8),
                c(0xa8459c),
                c(0x0f7d84),
                c(0x8f8a85),
            ],
        },
        SyntaxColors {
            text: c(0x1a1614),
            keyword: c(0x8f3f8a),
            function: c(0x3a55c8),
            property: c(0x5a4fcf),
            type_: c(0x2f7a3e),
            attribute: c(0x3a7745),
            string: c(0x8a5a00),
            string_special: c(0x9a4f00),
            constant: c(0x0b6e74),
            variable_builtin: c(0xa0457f),
            operator: c(0x5f5a55),
            punctuation: c(0x67625d),
            punctuation_special: c(0x0b6e74),
            comment: c(0x706a64),
            heading: c(0x3a55c8),
            error: c(0xab2b3f),
            bracket_match: ca(0xc9c6bf99),
            line_number: c(0x9a958f),
            line_number_active: c(0x312d2a),
            current_line: c(0xf1eee8),
            indent_guide: c(0xe6e3dc),
            indent_guide_active: c(0xc9c6bf),
            whitespace: c(0xc9c6bf),
        },
    )
}

fn scheme(appearance: Appearance) -> (Colors, TerminalColors, SyntaxColors) {
    match appearance {
        Appearance::Dark => dark_scheme(),
        Appearance::Light => light_scheme(),
    }
}

impl Theme {
    pub fn dark(reduced_motion: bool) -> Self {
        Self::new(Appearance::Dark, reduced_motion)
    }

    pub fn light(reduced_motion: bool) -> Self {
        Self::new(Appearance::Light, reduced_motion)
    }

    pub fn new(appearance: Appearance, reduced_motion: bool) -> Self {
        let (color, terminal, syntax) = scheme(appearance);
        Self {
            appearance,
            color,
            terminal,
            syntax,
            typography: Typography {
                ui: "Geist".into(),
                mono: "Geist Mono".into(),
                caption: px(12.),
                body: px(14.),
                heading: px(18.),
                display: px(24.),
                code: px(CODE_SIZE),
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

    /// Swaps every colour for `appearance`'s, keeping font zoom and motion settings.
    pub fn set_appearance(&mut self, appearance: Appearance) {
        let (color, terminal, syntax) = scheme(appearance);
        self.appearance = appearance;
        self.color = color;
        self.terminal = terminal;
        self.syntax = syntax;
    }

    pub fn is_dark(&self) -> bool {
        self.appearance == Appearance::Dark
    }

    /// Sizes editor and terminal text `zoom` steps from the default, clamped to [`CODE_ZOOM`].
    pub fn set_code_zoom(&mut self, zoom: i32) {
        let zoom = zoom.clamp(*CODE_ZOOM.start(), *CODE_ZOOM.end());
        self.typography.code = px(CODE_SIZE + zoom as f32);
    }

    pub fn popover_shadow(&self) -> BoxShadow {
        BoxShadow {
            color: match self.appearance {
                Appearance::Dark => ca(0x00000099),
                Appearance::Light => ca(0x1a16142e),
            },
            offset: point(px(0.), px(8.)),
            blur_radius: px(24.),
            spread_radius: px(-8.),
        }
    }
}

/// WCAG contrast ratio between two opaque colours.
#[cfg(test)]
pub(crate) fn contrast(a: Hsla, b: Hsla) -> f32 {
    let luminance = |c: Hsla| {
        let c = gpui::Rgba::from(c);
        let f = |v: f32| {
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b)
    };
    let (x, y) = (luminance(a), luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn both() -> [Theme; 2] {
        [Theme::dark(false), Theme::light(false)]
    }

    fn assert_reads(name: &str, fg: Hsla, bg: Hsla, min: f32, t: &Theme) {
        let ratio = contrast(fg, bg);
        assert!(
            ratio >= min,
            "{:?} {name}: {ratio:.2} < {min}",
            t.appearance
        );
    }

    fn colors(c: &Colors) -> Vec<(&'static str, Hsla)> {
        // Destructured without `..`, so a new token cannot be left out of the checks below.
        let Colors {
            surface,
            surface_sunken,
            surface_accent,
            surface_hover,
            surface_active,
            border,
            border_strong,
            content,
            content_secondary,
            content_muted,
            content_disabled,
            content_on_accent,
            accent,
            accent_hover,
            accent_pressed,
            danger,
            danger_surface,
            danger_strong,
            success,
            warning,
            overlay,
            tooltip_bg,
            tooltip_fg,
            focus_ring,
        } = c.clone();
        vec![
            ("surface", surface),
            ("surface_sunken", surface_sunken),
            ("surface_accent", surface_accent),
            ("surface_hover", surface_hover),
            ("surface_active", surface_active),
            ("border", border),
            ("border_strong", border_strong),
            ("content", content),
            ("content_secondary", content_secondary),
            ("content_muted", content_muted),
            ("content_disabled", content_disabled),
            ("content_on_accent", content_on_accent),
            ("accent", accent),
            ("accent_hover", accent_hover),
            ("accent_pressed", accent_pressed),
            ("danger", danger),
            ("danger_surface", danger_surface),
            ("danger_strong", danger_strong),
            ("success", success),
            ("warning", warning),
            ("overlay", overlay),
            ("tooltip_bg", tooltip_bg),
            ("tooltip_fg", tooltip_fg),
            ("focus_ring", focus_ring),
        ]
    }

    fn syntax_text(s: &SyntaxColors) -> Vec<(&'static str, Hsla)> {
        let SyntaxColors {
            text,
            keyword,
            function,
            property,
            type_,
            attribute,
            string,
            string_special,
            constant,
            variable_builtin,
            operator,
            punctuation,
            punctuation_special,
            comment,
            heading,
            error,
            bracket_match: _,
            line_number: _,
            line_number_active,
            current_line: _,
            indent_guide: _,
            indent_guide_active: _,
            whitespace: _,
        } = s.clone();
        vec![
            ("text", text),
            ("keyword", keyword),
            ("function", function),
            ("property", property),
            ("type", type_),
            ("attribute", attribute),
            ("string", string),
            ("string_special", string_special),
            ("constant", constant),
            ("variable_builtin", variable_builtin),
            ("operator", operator),
            ("punctuation", punctuation),
            ("punctuation_special", punctuation_special),
            ("comment", comment),
            ("heading", heading),
            ("error", error),
            ("line_number_active", line_number_active),
        ]
    }

    #[test]
    fn every_colour_token_differs_between_light_and_dark() {
        let [dark, light] = both();
        for ((name, d), (_, l)) in colors(&dark.color).into_iter().zip(colors(&light.color)) {
            assert_ne!(d, l, "{name} is the same in both themes");
        }
        for ((name, d), (_, l)) in syntax_text(&dark.syntax)
            .into_iter()
            .zip(syntax_text(&light.syntax))
        {
            assert_ne!(d, l, "syntax {name} is the same in both themes");
        }
        for i in 0..16 {
            assert_ne!(dark.terminal.ansi[i], light.terminal.ansi[i], "ansi {i}");
        }
    }

    #[test]
    fn interface_text_reads_on_its_surfaces_in_both_themes() {
        for t in both() {
            let c = &t.color;
            for bg in [c.surface, c.surface_sunken] {
                assert_reads("content", c.content, bg, 7., &t);
                assert_reads("content_secondary", c.content_secondary, bg, 4.5, &t);
                assert_reads("content_muted", c.content_muted, bg, 4.5, &t);
                assert_reads("accent", c.accent, bg, 4.5, &t);
                assert_reads("danger", c.danger, bg, 4.5, &t);
                assert_reads("success", c.success, bg, 3., &t);
                assert_reads("warning", c.warning, bg, 3., &t);
            }
            assert_reads(
                "content on surface_active",
                c.content,
                c.surface_active,
                7.,
                &t,
            );
            assert_reads(
                "content on surface_accent",
                c.content,
                c.surface_accent,
                7.,
                &t,
            );
            assert_reads("on accent", c.content_on_accent, c.accent, 4.5, &t);
            assert_reads("danger on its surface", c.danger, c.danger_surface, 4.5, &t);
            assert_reads(
                "content on danger_strong",
                c.content,
                c.danger_strong,
                4.5,
                &t,
            );
            assert_reads("tooltip", c.tooltip_fg, c.tooltip_bg, 7., &t);
        }
    }

    #[test]
    fn code_colours_read_on_the_editor_background_in_both_themes() {
        for t in both() {
            for bg in [t.color.surface_sunken, t.syntax.current_line] {
                for (name, fg) in syntax_text(&t.syntax) {
                    assert_reads(name, fg, bg, 4.5, &t);
                }
            }
        }
    }

    #[test]
    fn terminal_colours_read_on_the_terminal_background_in_both_themes() {
        for t in both() {
            let term = &t.terminal;
            let bg = term.background;
            assert_reads("foreground", term.foreground, bg, 7., &t);
            assert_reads("bright foreground", term.bright_foreground, bg, 7., &t);
            assert_reads("dim foreground", term.dim_foreground, bg, 4.5, &t);
            assert_reads("cursor text", term.cursor_text, term.cursor, 4.5, &t);
            assert_reads("text on selection", term.foreground, term.selection, 7., &t);
            for (i, &fg) in term.ansi.iter().enumerate() {
                // Dark black is a background colour; the greys only need to stay legible.
                if t.is_dark() && i == 0 {
                    continue;
                }
                let min = if [7, 8, 15].contains(&i) { 3. } else { 4.5 };
                assert_reads(&format!("ansi {i}"), fg, bg, min, &t);
            }
        }
    }

    #[test]
    fn switching_appearance_keeps_zoom_and_motion() {
        let mut theme = Theme::dark(true);
        theme.set_code_zoom(3);
        theme.set_appearance(Appearance::Light);
        assert!(!theme.is_dark());
        assert_eq!(theme.typography.code, px(16.));
        assert!(theme.motion.reduced);
        assert_eq!(theme.color.surface, Theme::light(false).color.surface);
    }

    #[test]
    fn code_zoom_steps_a_pixel_and_stays_in_range() {
        let mut theme = Theme::dark(false);
        theme.set_code_zoom(2);
        assert_eq!(theme.typography.code, px(15.));
        theme.set_code_zoom(-100);
        assert_eq!(theme.typography.code, px(6.));
        theme.set_code_zoom(100);
        assert_eq!(theme.typography.code, px(40.));
        theme.set_code_zoom(0);
        assert_eq!(theme.typography.code, px(13.));
    }
}
