use std::collections::HashMap;
use std::path::Path;

use globset::GlobBuilder;

use crate::buffer::{Indent, LineEnding};

/// What the `.editorconfig` files above a file set for it; `None` leaves the editor's own choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EditorConfig {
    pub indent_style: Option<IndentStyle>,
    pub indent_size: Option<usize>,
    pub end_of_line: Option<LineEnding>,
    pub trim_trailing_whitespace: Option<bool>,
    pub insert_final_newline: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndentStyle {
    Tab,
    Space,
}

impl EditorConfig {
    /// The indentation to use instead of the `detected` one.
    pub fn indent(&self, detected: Indent) -> Indent {
        let detected_size = match detected {
            Indent::Spaces(n) => n,
            Indent::Tab => 4,
        };
        match (self.indent_style, self.indent_size) {
            (Some(IndentStyle::Tab), _) => Indent::Tab,
            (Some(IndentStyle::Space), size) => Indent::Spaces(size.unwrap_or(detected_size)),
            (None, Some(size)) if matches!(detected, Indent::Spaces(_)) => Indent::Spaces(size),
            (None, _) => detected,
        }
    }

    fn from_properties(props: &HashMap<String, String>) -> Self {
        let get = |key: &str| props.get(key).map(String::as_str);
        let flag = |key: &str| match get(key) {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => None,
        };
        let tab_width = get("tab_width").and_then(|v| v.parse().ok());
        Self {
            indent_style: match get("indent_style") {
                Some("tab") => Some(IndentStyle::Tab),
                Some("space") => Some(IndentStyle::Space),
                _ => None,
            },
            indent_size: match get("indent_size") {
                Some("tab") => tab_width,
                Some(n) => n.parse().ok().filter(|n| *n > 0),
                None => None,
            },
            end_of_line: match get("end_of_line") {
                Some("lf") => Some(LineEnding::Lf),
                Some("crlf") => Some(LineEnding::CrLf),
                _ => None,
            },
            trim_trailing_whitespace: flag("trim_trailing_whitespace"),
            insert_final_newline: flag("insert_final_newline"),
        }
    }
}

struct Section {
    glob: String,
    props: Vec<(String, String)>,
}

struct File {
    root: bool,
    sections: Vec<Section>,
}

/// Reads one `.editorconfig`; keys and values are lowercased, as the spec matches them.
fn parse(text: &str) -> File {
    let mut file = File {
        root: false,
        sections: Vec::new(),
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(glob) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            file.sections.push(Section {
                glob: glob.to_string(),
                props: Vec::new(),
            });
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim().to_lowercase(), value.trim().to_lowercase());
        match file.sections.last_mut() {
            Some(section) => section.props.push((key, value)),
            None if key == "root" => file.root = value == "true",
            None => {}
        }
    }
    file
}

/// Whether a section header matches `rel`, the file's path from the `.editorconfig`'s folder: a
/// glob without a slash matches the file name in any folder below.
fn section_matches(glob: &str, rel: &Path) -> bool {
    let pattern = match glob.strip_prefix('/') {
        Some(anchored) => anchored.to_string(),
        None if glob.contains('/') => glob.to_string(),
        None => format!("**/{glob}"),
    };
    GlobBuilder::new(&pattern)
        .literal_separator(true)
        .build()
        .is_ok_and(|g| g.compile_matcher().is_match(rel))
}

/// The settings for `path` from every `.editorconfig` above it, up to one marked `root = true`;
/// nearer files and later sections win, and `unset` clears a property.
pub fn resolve(path: &Path) -> EditorConfig {
    let mut files = Vec::new();
    let mut dir = path.parent();
    while let Some(at) = dir {
        if let Ok(text) = std::fs::read_to_string(at.join(".editorconfig")) {
            let file = parse(&text);
            let root = file.root;
            files.push((at, file));
            if root {
                break;
            }
        }
        dir = at.parent();
    }
    let mut props = HashMap::new();
    for (at, file) in files.iter().rev() {
        let Ok(rel) = path.strip_prefix(at) else {
            continue;
        };
        for section in file
            .sections
            .iter()
            .filter(|s| section_matches(&s.glob, rel))
        {
            props.extend(section.props.iter().cloned());
        }
    }
    EditorConfig::from_properties(&props)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("athena-editorconfig-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proj/src/web")).unwrap();
        dir
    }

    #[test]
    fn the_nearest_file_wins_and_root_stops_the_walk() {
        let dir = tree("walk");
        std::fs::write(
            dir.join(".editorconfig"),
            "[*]\ninsert_final_newline = true\nend_of_line = crlf\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("proj/.editorconfig"),
            "root = true\n[*]\nindent_style = space\nindent_size = 4\ntrim_trailing_whitespace = true\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("proj/src/.editorconfig"),
            "# comment\n[*.go]\nindent_style = tab\n[*.{ts,tsx}]\nindent_size = 2\ntrim_trailing_whitespace = unset\n",
        )
        .unwrap();
        let go = resolve(&dir.join("proj/src/main.go"));
        assert_eq!(go.indent_style, Some(IndentStyle::Tab));
        assert_eq!(go.trim_trailing_whitespace, Some(true));
        assert_eq!(
            go.insert_final_newline, None,
            "root = true hides the file above"
        );
        assert_eq!(go.end_of_line, None);
        let ts = resolve(&dir.join("proj/src/web/app.ts"));
        assert_eq!(ts.indent(Indent::Tab), Indent::Spaces(2));
        assert_eq!(ts.trim_trailing_whitespace, None, "unset clears it");
        let other = resolve(&dir.join("other.txt"));
        assert_eq!(other.end_of_line, Some(LineEnding::CrLf));
        assert_eq!(other.insert_final_newline, Some(true));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn later_sections_override_and_slashes_anchor_to_the_folder() {
        let dir = tree("sections");
        std::fs::write(
            dir.join("proj/.editorconfig"),
            "root=true\n[*]\nINDENT_SIZE = 8\n[src/*.ts]\nindent_size=3\n[Makefile]\nindent_style=TAB\n[*]\nend_of_line = LF\n",
        )
        .unwrap();
        let anchored = resolve(&dir.join("proj/src/a.ts"));
        assert_eq!(anchored.indent_size, Some(3));
        assert_eq!(anchored.end_of_line, Some(LineEnding::Lf));
        let deeper = resolve(&dir.join("proj/src/web/a.ts"));
        assert_eq!(
            deeper.indent_size,
            Some(8),
            "src/*.ts does not reach src/web"
        );
        let make = resolve(&dir.join("proj/src/web/Makefile"));
        assert_eq!(make.indent_style, Some(IndentStyle::Tab));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn indentation_overrides_what_was_detected() {
        let size_only = EditorConfig {
            indent_size: Some(2),
            ..EditorConfig::default()
        };
        assert_eq!(size_only.indent(Indent::Spaces(4)), Indent::Spaces(2));
        assert_eq!(size_only.indent(Indent::Tab), Indent::Tab);
        let spaces = EditorConfig {
            indent_style: Some(IndentStyle::Space),
            ..EditorConfig::default()
        };
        assert_eq!(spaces.indent(Indent::Tab), Indent::Spaces(4));
        let props = HashMap::from([
            ("indent_size".to_string(), "tab".to_string()),
            ("tab_width".to_string(), "6".to_string()),
            ("indent_style".to_string(), "space".to_string()),
        ]);
        assert_eq!(
            EditorConfig::from_properties(&props).indent(Indent::Tab),
            Indent::Spaces(6)
        );
    }
}
