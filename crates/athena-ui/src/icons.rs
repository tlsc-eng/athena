use std::path::Path;

use gpui::{App, Div, Hsla, div, prelude::*, px, svg};

use crate::{ActiveTheme, Theme};

/// Edge of a file icon in the tree and on tabs.
pub const ICON_SIZE: f32 = 14.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tint {
    Blue,
    Cyan,
    Amber,
    Accent,
    Magenta,
    Red,
    Green,
    Muted,
    Plain,
}

/// A seti-ui glyph (VS Code's default icon theme) and the palette colour it is drawn in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FileIcon {
    pub asset: &'static str,
    pub color: Hsla,
}

/// Exact file names, checked before extensions; a trailing `*` matches any suffix.
const NAMES: &[(&str, &str, Tint)] = &[
    ("Dockerfile*", "docker", Tint::Blue),
    ("Containerfile*", "docker", Tint::Blue),
    ("docker-compose*", "docker", Tint::Blue),
    ("compose.y*ml", "docker", Tint::Blue),
    (".env*", "config", Tint::Amber),
    (".gitignore", "git_ignore", Tint::Accent),
    (".gitattributes", "git", Tint::Accent),
    (".gitmodules", "git", Tint::Accent),
    (".gitkeep", "git", Tint::Accent),
    (".dockerignore", "docker", Tint::Muted),
    (".editorconfig", "editorconfig", Tint::Muted),
    ("Makefile", "makefile", Tint::Amber),
    ("GNUmakefile", "makefile", Tint::Amber),
    ("LICENSE*", "license", Tint::Amber),
    ("LICENCE*", "license", Tint::Amber),
    ("COPYING*", "license", Tint::Amber),
    ("package.json", "npm", Tint::Red),
    ("package-lock.json", "npm", Tint::Muted),
    ("yarn.lock", "yarn", Tint::Blue),
    ("tsconfig*.json", "tsconfig", Tint::Blue),
    (".eslintrc*", "eslint", Tint::Magenta),
    ("eslint.config.*", "eslint", Tint::Magenta),
    ("vite.config.*", "vite", Tint::Amber),
    ("Cargo.toml", "rust", Tint::Accent),
    ("Cargo.lock", "lock", Tint::Green),
    ("go.mod", "go", Tint::Cyan),
    ("go.sum", "go", Tint::Muted),
];

const EXTENSIONS: &[(&str, &str, Tint)] = &[
    ("go", "go", Tint::Cyan),
    ("ts", "typescript", Tint::Blue),
    ("mts", "typescript", Tint::Blue),
    ("cts", "typescript", Tint::Blue),
    ("tsx", "react", Tint::Blue),
    ("jsx", "react", Tint::Cyan),
    ("js", "javascript", Tint::Amber),
    ("mjs", "javascript", Tint::Amber),
    ("cjs", "javascript", Tint::Amber),
    ("json", "json", Tint::Amber),
    ("jsonc", "json", Tint::Amber),
    ("rs", "rust", Tint::Accent),
    ("py", "python", Tint::Blue),
    ("yml", "yml", Tint::Magenta),
    ("yaml", "yml", Tint::Magenta),
    ("toml", "config", Tint::Muted),
    ("ini", "config", Tint::Muted),
    ("conf", "config", Tint::Muted),
    ("cfg", "config", Tint::Muted),
    ("plist", "settings", Tint::Muted),
    ("html", "html", Tint::Red),
    ("htm", "html", Tint::Red),
    ("css", "css", Tint::Blue),
    ("scss", "sass", Tint::Magenta),
    ("sass", "sass", Tint::Magenta),
    ("less", "less", Tint::Blue),
    ("md", "markdown", Tint::Blue),
    ("markdown", "markdown", Tint::Blue),
    ("mdx", "markdown", Tint::Amber),
    ("mmd", "markdown", Tint::Magenta),
    ("mermaid", "markdown", Tint::Magenta),
    ("sh", "shell", Tint::Green),
    ("bash", "shell", Tint::Green),
    ("zsh", "shell", Tint::Green),
    ("fish", "shell", Tint::Green),
    ("lock", "lock", Tint::Green),
    ("swift", "swift", Tint::Accent),
    ("c", "c", Tint::Blue),
    ("h", "c", Tint::Magenta),
    ("cc", "cpp", Tint::Blue),
    ("cpp", "cpp", Tint::Blue),
    ("hpp", "cpp", Tint::Magenta),
    ("cs", "c-sharp", Tint::Blue),
    ("java", "java", Tint::Red),
    ("kt", "kotlin", Tint::Accent),
    ("kts", "kotlin", Tint::Accent),
    ("rb", "ruby", Tint::Red),
    ("php", "php", Tint::Magenta),
    ("lua", "lua", Tint::Blue),
    ("sql", "db", Tint::Magenta),
    ("db", "db", Tint::Magenta),
    ("sqlite", "db", Tint::Magenta),
    ("xml", "xml", Tint::Accent),
    ("csv", "csv", Tint::Green),
    ("tsv", "csv", Tint::Green),
    ("pdf", "pdf", Tint::Red),
    ("zip", "zip", Tint::Amber),
    ("gz", "zip", Tint::Amber),
    ("tgz", "zip", Tint::Amber),
    ("tar", "zip", Tint::Amber),
    ("ttf", "font", Tint::Red),
    ("otf", "font", Tint::Red),
    ("woff", "font", Tint::Red),
    ("woff2", "font", Tint::Red),
    ("mp4", "video", Tint::Red),
    ("mov", "video", Tint::Red),
    ("webm", "video", Tint::Red),
    ("mp3", "audio", Tint::Magenta),
    ("wav", "audio", Tint::Magenta),
    ("png", "image", Tint::Magenta),
    ("jpg", "image", Tint::Magenta),
    ("jpeg", "image", Tint::Magenta),
    ("gif", "image", Tint::Magenta),
    ("webp", "image", Tint::Magenta),
    ("bmp", "image", Tint::Magenta),
    ("tif", "image", Tint::Magenta),
    ("tiff", "image", Tint::Magenta),
    ("ico", "image", Tint::Magenta),
    ("icns", "image", Tint::Magenta),
    ("svg", "svg", Tint::Amber),
    ("vue", "vue", Tint::Green),
    ("svelte", "svelte", Tint::Red),
    ("tf", "terraform", Tint::Magenta),
    ("tfvars", "terraform", Tint::Magenta),
    ("graphql", "graphql", Tint::Magenta),
    ("gql", "graphql", Tint::Magenta),
    ("wasm", "wasm", Tint::Magenta),
    ("zig", "zig", Tint::Amber),
    ("prisma", "prisma", Tint::Blue),
    ("tex", "tex", Tint::Plain),
    ("scala", "scala", Tint::Red),
    ("hs", "haskell", Tint::Magenta),
    ("ex", "elixir", Tint::Magenta),
    ("exs", "elixir", Tint::Magenta),
    ("dart", "dart", Tint::Cyan),
    ("clj", "clojure", Tint::Green),
    ("env", "config", Tint::Amber),
];

fn matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, tail)) => {
            name.len() >= head.len() + tail.len() && name.starts_with(head) && name.ends_with(tail)
        }
    }
}

fn lookup(path: &Path) -> (&'static str, Tint) {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if let Some((_, icon, tint)) = NAMES.iter().find(|(p, _, _)| matches(p, name)) {
        return (icon, *tint);
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    ext.and_then(|ext| EXTENSIONS.iter().find(|(e, _, _)| *e == ext))
        .map_or(("default", Tint::Plain), |(_, icon, tint)| (icon, *tint))
}

impl Tint {
    fn color(self, t: &Theme) -> Hsla {
        match self {
            Self::Blue => t.terminal.ansi[4],
            Self::Cyan => t.terminal.ansi[6],
            Self::Amber => t.color.warning,
            Self::Accent => t.color.accent,
            Self::Magenta => t.terminal.ansi[5],
            Self::Red => t.color.danger,
            Self::Green => t.color.success,
            Self::Muted => t.color.content_muted,
            Self::Plain => t.color.content_disabled,
        }
    }
}

/// The icon for a file or folder, matching special names before extensions.
pub fn icon_for(path: &Path, is_dir: bool, t: &Theme) -> FileIcon {
    if is_dir {
        return FileIcon {
            asset: "icons/folder.svg",
            color: t.color.content_muted,
        };
    }
    let (icon, tint) = lookup(path);
    FileIcon {
        asset: asset_path(icon),
        color: tint.color(t),
    }
}

/// A 14 px slot holding the tinted icon for `path`.
pub fn file_icon(path: &Path, is_dir: bool, cx: &App) -> Div {
    let icon = icon_for(path, is_dir, cx.theme());
    // Seti's file glyphs fill about half of their 32-unit box, so they are drawn larger than the slot.
    let glyph = if is_dir { ICON_SIZE } else { ICON_SIZE * 1.6 };
    div()
        .size(px(ICON_SIZE))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .child(
            svg()
                .path(icon.asset)
                .size(px(glyph))
                .flex_none()
                .text_color(icon.color),
        )
}

fn asset_path(icon: &str) -> &'static str {
    crate::assets::bundled(&format!("icons/{icon}.svg")).unwrap_or("icons/default.svg")
}

#[cfg(test)]
mod tests {
    use gpui::AssetSource;

    use super::*;
    use crate::Assets;

    #[test]
    fn every_mapped_icon_is_bundled() {
        let t = Theme::dark(false);
        let names = NAMES.iter().map(|(n, _, _)| n.replace('*', "x"));
        let exts = EXTENSIONS.iter().map(|(e, _, _)| format!("file.{e}"));
        for name in names.chain(exts).chain(["README".into(), "src".into()]) {
            let icon = icon_for(Path::new(&name), name == "src", &t);
            assert!(
                Assets.load(icon.asset).unwrap().is_some(),
                "{name} → {}",
                icon.asset
            );
        }
        for (_, icon, _) in NAMES.iter().chain(EXTENSIONS) {
            assert_ne!(
                asset_path(icon),
                "icons/default.svg",
                "{icon} is not bundled"
            );
        }
    }

    #[test]
    fn names_win_over_extensions() {
        let t = Theme::dark(false);
        let icon = |p: &str| icon_for(Path::new(p), false, &t).asset;
        assert_eq!(icon("/x/Dockerfile.dev"), "icons/docker.svg");
        assert_eq!(icon("/x/.env.local"), "icons/config.svg");
        assert_eq!(icon("/x/package.json"), "icons/npm.svg");
        assert_eq!(icon("/x/other.json"), "icons/json.svg");
        assert_eq!(icon("/x/Cargo.lock"), "icons/lock.svg");
        assert_eq!(icon("/x/main.GO"), "icons/go.svg");
        assert_eq!(icon("/x/notes"), "icons/default.svg");
        assert_eq!(
            icon_for(Path::new("/x/src"), true, &t).asset,
            "icons/folder.svg"
        );
    }
}
