use std::fs;
use std::path::{Path, PathBuf};

use gpui::{Hsla, Rgba};
use serde_json::{Map, Value, json};

use crate::components::DotState;
use crate::motion::{EASE_ENTER, EASE_EXIT, EASE_STANDARD, SPIN_STEPS, SPIN_TICK_MS};
use crate::{Colors, Size, Space, Theme};

fn hex(color: Hsla) -> String {
    let rgba = Rgba::from(color);
    let byte = |v: f32| (v * 255.).round() as u8;
    let (r, g, b, a) = (byte(rgba.r), byte(rgba.g), byte(rgba.b), byte(rgba.a));
    if a < 255 {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}")
    }
}

/// Widening an f32 straight to f64 would print 0.2 as 0.20000000298023224, so it goes through its shortest decimal.
fn num(v: f32) -> Value {
    if v.fract() == 0. {
        json!(v as u64)
    } else {
        json!(v.to_string().parse::<f64>().unwrap())
    }
}

fn colors(c: &Colors) -> Value {
    macro_rules! fields {
        ($($name:ident),*) => {{
            let mut map = Map::new();
            $(map.insert(stringify!($name).into(), json!(hex(c.$name)));)*
            Value::Object(map)
        }};
    }
    fields!(
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
        focus_ring
    )
}

fn appearance(theme: &Theme) -> Value {
    let t = &theme.terminal;
    json!({
        "color": colors(&theme.color),
        "terminal": {
            "foreground": hex(t.foreground),
            "bright_foreground": hex(t.bright_foreground),
            "dim_foreground": hex(t.dim_foreground),
            "background": hex(t.background),
            "cursor": hex(t.cursor),
            "cursor_text": hex(t.cursor_text),
            "selection": hex(t.selection),
            "ansi": t.ansi.iter().map(|c| hex(*c)).collect::<Vec<_>>(),
        },
    })
}

/// Each state's token key, the icon it shows and the colour token it is drawn in.
const GLYPHS: [(&str, DotState, &str, &str); 5] = [
    (
        "needs_input",
        DotState::NeedsInput,
        "session-ask",
        "warning",
    ),
    ("unread", DotState::Unread, "session-done", "success"),
    (
        "running",
        DotState::Running,
        "session-running",
        "content_muted",
    ),
    (
        "finished",
        DotState::Finished,
        "session-done-outline",
        "content_muted",
    ),
    ("idle", DotState::Idle, "session-idle", "content_disabled"),
];

fn session_glyphs(theme: &Theme) -> Value {
    let named = colors(&theme.color);
    let mut map = Map::new();
    for (key, state, icon, color) in GLYPHS {
        let (path, drawn) = state.look(&theme.color);
        assert_eq!(path, format!("ui/{icon}.svg"), "{key} icon drifted");
        assert_eq!(hex(drawn), named[color], "{key} colour drifted");
        map.insert(key.into(), json!({ "icon": icon, "color": color }));
    }
    Value::Object(map)
}

fn tokens() -> Value {
    let dark = Theme::dark(false);
    let light = Theme::light(false);
    let ty = &dark.typography;
    let px = |p: gpui::Pixels| num(f32::from(p));
    let space = [
        ("xxs", Space::Xxs),
        ("xs", Space::Xs),
        ("sm", Space::Sm),
        ("md", Space::Md),
        ("lg", Space::Lg),
        ("xl", Space::Xl),
        ("xxl", Space::Xxl),
    ]
    .map(|(name, s)| (name, num(s.value())));
    let size = [
        ("row", Size::Row),
        ("control", Size::Control),
        ("tab_bar", Size::TabBar),
        ("title_bar", Size::TitleBar),
    ]
    .map(|(name, s)| (name, num(s.value())));
    let curve = |c: [f32; 4]| Value::Array(c.map(num).to_vec());
    let m = &dark.motion;
    json!({
        "version": 1,
        "appearances": { "dark": appearance(&dark), "light": appearance(&light) },
        "typography": {
            "ui": ty.ui.as_ref(),
            "mono": ty.mono.as_ref(),
            "caption": px(ty.caption),
            "body": px(ty.body),
            "heading": px(ty.heading),
            "display": px(ty.display),
            "code": px(ty.code),
        },
        "space": Map::from_iter(space.map(|(k, v)| (k.to_string(), v))),
        "size": Map::from_iter(size.map(|(k, v)| (k.to_string(), v))),
        "radius": {
            "control": px(dark.shape.radius_control),
            "panel": px(dark.shape.radius_panel),
            "hairline": px(dark.shape.hairline),
        },
        "motion": {
            "fast_ms": m.fast.as_millis() as u64,
            "base_ms": m.base.as_millis() as u64,
            "slow_ms": m.slow.as_millis() as u64,
            "easing": {
                "standard": curve(EASE_STANDARD),
                "enter": curve(EASE_ENTER),
                "exit": curve(EASE_EXIT),
            },
            "spinner": { "tick_ms": SPIN_TICK_MS as u64, "steps": SPIN_STEPS as u64 },
        },
        "session_glyphs": session_glyphs(&dark),
    })
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every file under `design/` with the bytes it must hold.
fn expected() -> Vec<(PathBuf, Vec<u8>)> {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    let design = root().join("design");
    let mut json = serde_json::to_string_pretty(&tokens()).unwrap();
    json.push('\n');
    let mut files = vec![(design.join("tokens.json"), json.into_bytes())];
    for (_, _, icon, _) in GLYPHS {
        let name = format!("{icon}.svg");
        let bytes = fs::read(assets.join("ui").join(&name)).unwrap();
        files.push((design.join("icons").join(name), bytes));
    }
    let brand = fs::read(assets.join("brand/tlsc.svg")).unwrap();
    files.push((design.join("brand/tlsc.svg"), brand));
    files
}

#[test]
fn design_files_match_the_theme_and_assets() {
    let update = std::env::var_os("UPDATE_TOKENS").is_some_and(|v| v == "1");
    for (path, bytes) in expected() {
        if update {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, &bytes).unwrap();
        } else {
            let on_disk = fs::read(&path).unwrap_or_default();
            assert!(
                on_disk == bytes,
                "{} is stale; run with UPDATE_TOKENS=1 and commit the result",
                path.display()
            );
        }
    }
}

#[test]
fn dark_accent_exports_as_six_digit_hex() {
    assert_eq!(
        tokens()["appearances"]["dark"]["color"]["accent"],
        "#eb5e43"
    );
}

#[test]
fn translucent_colours_export_with_an_alpha_byte() {
    assert_eq!(
        tokens()["appearances"]["light"]["color"]["surface_hover"],
        "#d5d4ce59"
    );
}
