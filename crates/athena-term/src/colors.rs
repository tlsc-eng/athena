use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use athena_ui::TerminalColors;
use gpui::{Hsla, Rgba};

/// Resolves a cell colour: program overrides (OSC 4/10/11) win, then the Athena palette.
pub fn resolve(color: Color, overrides: &Colors, palette: &TerminalColors) -> Hsla {
    match color {
        Color::Spec(rgb) => rgb_to_hsla(rgb),
        Color::Indexed(i) => indexed(i as usize, overrides, palette),
        Color::Named(name) => named(name, overrides, palette),
    }
}

pub fn named(name: NamedColor, overrides: &Colors, palette: &TerminalColors) -> Hsla {
    if let Some(rgb) = overrides[name] {
        return rgb_to_hsla(rgb);
    }
    let i = name as usize;
    match name {
        NamedColor::Foreground => palette.foreground,
        NamedColor::BrightForeground => palette.bright_foreground,
        NamedColor::Background => palette.background,
        NamedColor::Cursor => palette.cursor,
        NamedColor::DimForeground => palette.dim_foreground,
        _ if i < 16 => palette.ansi[i],
        _ => {
            let normal = i - NamedColor::DimBlack as usize;
            dim(
                palette
                    .ansi
                    .get(normal)
                    .copied()
                    .unwrap_or(palette.foreground),
                palette,
            )
        }
    }
}

/// Faint (SGR 2) text: 35 % of the way to the background, but no darker than the theme's dim
/// foreground unless the colour already was.
pub fn dim(fg: Hsla, palette: &TerminalColors) -> Hsla {
    let (a, b) = (Rgba::from(fg), Rgba::from(palette.background));
    let mix = |x: f32, y: f32| x + (y - x) * 0.35;
    let mut out: Hsla = Rgba {
        r: mix(a.r, b.r),
        g: mix(a.g, b.g),
        b: mix(a.b, b.b),
        a: a.a,
    }
    .into();
    out.l = out.l.max(palette.dim_foreground.l.min(fg.l));
    out
}

pub fn indexed(i: usize, overrides: &Colors, palette: &TerminalColors) -> Hsla {
    if let Some(rgb) = overrides[i] {
        return rgb_to_hsla(rgb);
    }
    if i < 16 {
        return palette.ansi[i];
    }
    rgb_to_hsla(xterm_256(i as u8))
}

/// Standard xterm 6x6x6 cube and grey ramp for indices 16..=255.
pub fn xterm_256(i: u8) -> Rgb {
    if i >= 232 {
        let v = 8 + (i - 232) * 10;
        return Rgb { r: v, g: v, b: v };
    }
    let i = i - 16;
    let step = |c: u8| if c == 0 { 0 } else { 55 + c * 40 };
    Rgb {
        r: step(i / 36),
        g: step((i / 6) % 6),
        b: step(i % 6),
    }
}

pub fn rgb_to_hsla(rgb: Rgb) -> Hsla {
    Rgba {
        r: rgb.r as f32 / 255.,
        g: rgb.g as f32 / 255.,
        b: rgb.b as f32 / 255.,
        a: 1.,
    }
    .into()
}

pub fn hsla_to_rgb(c: Hsla) -> Rgb {
    let rgba = Rgba::from(c);
    let to = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    Rgb {
        r: to(rgba.r),
        g: to(rgba.g),
        b: to(rgba.b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dim_text_stays_readable_and_never_brightens() {
        let palette = athena_ui::Theme::dark(false).terminal;
        let dimmed = dim(palette.foreground, &palette);
        assert!(dimmed.l < palette.foreground.l);
        assert!(dimmed.l >= palette.dim_foreground.l);
        let black = palette.ansi[0];
        assert!(dim(black, &palette).l <= black.l);
    }

    #[test]
    fn cube_and_greys() {
        assert_eq!(xterm_256(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(xterm_256(196), Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(
            xterm_256(231),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(xterm_256(232), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(
            xterm_256(255),
            Rgb {
                r: 238,
                g: 238,
                b: 238
            }
        );
    }
}
