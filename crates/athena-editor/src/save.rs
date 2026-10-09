use gpui::Context;

use crate::buffer::LineEnding;
use crate::editorconfig::{self, EditorConfig};
use crate::syntax::Lang;
use crate::view::{EditorEvent, EditorView};

/// What a save does to the text before writing it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tidy {
    pub trim_trailing_whitespace: bool,
    pub insert_final_newline: bool,
    /// Line breaks to convert every line to.
    pub end_of_line: Option<LineEnding>,
}

/// Settings that override a language's [`save_defaults`]; `None` keeps the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SaveSettings {
    pub trim_trailing_whitespace: Option<bool>,
    pub insert_final_newline: Option<bool>,
}

/// Trimming and a final newline for the languages Athena's users write, as their formatters
/// leave files; Markdown keeps trailing blanks, which VS Code also leaves there as line breaks.
pub fn save_defaults(lang: Option<Lang>) -> Tidy {
    let (trim, newline) = match lang {
        Some(
            Lang::Go
            | Lang::TypeScript
            | Lang::Tsx
            | Lang::JavaScript
            | Lang::Rust
            | Lang::Python
            | Lang::Yaml
            | Lang::Json,
        ) => (true, true),
        Some(Lang::Markdown) => (false, true),
        _ => (false, false),
    };
    Tidy {
        trim_trailing_whitespace: trim,
        insert_final_newline: newline,
        end_of_line: None,
    }
}

/// `.editorconfig` first, then settings, then the language's defaults.
pub fn resolve_tidy(lang: Option<Lang>, settings: SaveSettings, config: &EditorConfig) -> Tidy {
    let defaults = save_defaults(lang);
    Tidy {
        trim_trailing_whitespace: config
            .trim_trailing_whitespace
            .or(settings.trim_trailing_whitespace)
            .unwrap_or(defaults.trim_trailing_whitespace),
        insert_final_newline: config
            .insert_final_newline
            .or(settings.insert_final_newline)
            .unwrap_or(defaults.insert_final_newline),
        end_of_line: config.end_of_line,
    }
}

impl EditorView {
    /// Overrides the language defaults for trimming and the final newline on save.
    pub fn set_save_settings(&mut self, settings: SaveSettings) {
        self.save_settings = settings;
    }

    /// Tidies the text as the save about to happen asks, as one undo step; an auto save leaves
    /// blanks on the carets' lines, where the user may still be typing.
    pub(crate) fn tidy_for_save(&mut self, auto: bool, cx: &mut Context<Self>) {
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        let tidy = {
            let b = shared.buffer.borrow();
            let config = b.path.as_deref().map(editorconfig::resolve);
            resolve_tidy(b.lang(), self.save_settings, &config.unwrap_or_default())
        };
        if tidy == Tidy::default() {
            return;
        }
        self.follow_edits();
        let mut cursor = std::mem::take(&mut self.cursor);
        let (before, version) = {
            let mut b = shared.buffer.borrow_mut();
            let before = b.version();
            b.tidy(&mut cursor, tidy, auto);
            (before, b.version())
        };
        self.follow_edits();
        self.cursor = cursor;
        if version == before {
            return;
        }
        // Unlike a typed edit this keeps the scroll and any open popup where they are.
        self.note_cursor_line(true, cx);
        self.refresh_find(false, cx);
        cx.emit(EditorEvent::Edited { version });
        shared.changed(cx);
    }

    /// Converts every line break to `to` and keeps using it; one undo step.
    pub fn convert_line_endings(&mut self, to: LineEnding, cx: &mut Context<Self>) {
        let tidy = Tidy {
            end_of_line: Some(to),
            ..Tidy::default()
        };
        self.with_buffer(cx, |b, c| b.tidy(c, tidy, false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{Buffer, Cursor, Cursors, Selection};

    const ALL: Tidy = Tidy {
        trim_trailing_whitespace: true,
        insert_final_newline: true,
        end_of_line: None,
    };

    #[test]
    fn markdown_keeps_trailing_blanks_and_other_text_is_left_alone() {
        assert_eq!(save_defaults(Some(Lang::Go)), ALL);
        let md = save_defaults(Some(Lang::Markdown));
        assert!(!md.trim_trailing_whitespace && md.insert_final_newline);
        assert_eq!(save_defaults(None), Tidy::default());
        assert_eq!(save_defaults(Some(Lang::Toml)), Tidy::default());
    }

    #[test]
    fn editorconfig_beats_settings_which_beat_the_language() {
        let off = SaveSettings {
            trim_trailing_whitespace: Some(false),
            insert_final_newline: None,
        };
        let tidy = resolve_tidy(Some(Lang::Go), off, &EditorConfig::default());
        assert!(!tidy.trim_trailing_whitespace && tidy.insert_final_newline);
        let config = EditorConfig {
            trim_trailing_whitespace: Some(true),
            insert_final_newline: Some(false),
            end_of_line: Some(LineEnding::CrLf),
            ..EditorConfig::default()
        };
        let tidy = resolve_tidy(Some(Lang::Go), off, &config);
        assert_eq!(
            tidy,
            Tidy {
                trim_trailing_whitespace: true,
                insert_final_newline: false,
                end_of_line: Some(LineEnding::CrLf),
            }
        );
    }

    fn carets(at: &[usize]) -> Cursors {
        let mut cs = Cursors::new(Cursor::at(at[0]));
        for &a in &at[1..] {
            cs.add(Cursor::at(a));
        }
        cs
    }

    fn heads(cs: &Cursors) -> Vec<usize> {
        cs.all().iter().map(|c| c.head()).collect()
    }

    #[test]
    fn saving_trims_every_line_adds_a_newline_and_is_one_undo_step() {
        let mut b = Buffer::new("a  \n\tb\t\nc ", None);
        // Carets in a's blanks, at the end of b's line and at the very end.
        let mut cs = carets(&[2, 7, 10]);
        b.tidy(&mut cs, ALL, false);
        assert_eq!(b.full_text(), "a\n\tb\nc\n");
        assert_eq!(
            heads(&cs),
            [1, 4, 6],
            "the end caret stays before the new break"
        );
        b.undo_all(&mut cs);
        assert_eq!(b.full_text(), "a  \n\tb\t\nc ");
    }

    #[test]
    fn an_auto_save_keeps_blanks_on_lines_with_a_caret() {
        let mut b = Buffer::new("a  \nb  \nc  \n", None);
        let mut cs = carets(&[6]);
        b.tidy(&mut cs, ALL, true);
        assert_eq!(b.full_text(), "a\nb  \nc\n");
        assert_eq!(heads(&cs), [4]);
    }

    #[test]
    fn trimming_crlf_text_keeps_its_breaks() {
        let mut b = Buffer::new("a \r\nb\t\r\nc", None);
        let mut cs = carets(&[6, 9]);
        b.tidy(&mut cs, ALL, false);
        assert_eq!(b.full_text(), "a\r\nb\r\nc\r\n");
        assert_eq!(heads(&cs), [4, 7]);
        let mut b = Buffer::new("x\r\n", None);
        let mut cs = carets(&[0]);
        b.tidy(&mut cs, ALL, false);
        assert_eq!(b.full_text(), "x\r\n", "a final break is not doubled");
    }

    #[test]
    fn converting_line_breaks_keeps_carets_on_their_lines() {
        let mut b = Buffer::new("ab\ncd\r\nef\n", None);
        let mut cs = carets(&[3, 9]);
        let to_crlf = Tidy {
            end_of_line: Some(LineEnding::CrLf),
            ..Tidy::default()
        };
        b.tidy(&mut cs, to_crlf, false);
        assert_eq!(b.full_text(), "ab\r\ncd\r\nef\r\n");
        assert_eq!(heads(&cs), [4, 10]);
        assert_eq!(b.line_ending(), LineEnding::CrLf);
        let to_lf = Tidy {
            end_of_line: Some(LineEnding::Lf),
            ..Tidy::default()
        };
        b.tidy(&mut cs, to_lf, false);
        assert_eq!(b.full_text(), "ab\ncd\nef\n");
        assert_eq!(heads(&cs), [3, 8]);
        b.undo_all(&mut cs);
        assert_eq!(b.full_text(), "ab\r\ncd\r\nef\r\n", "one undo step");
    }

    #[test]
    fn a_selection_over_trimmed_blanks_shrinks_with_them() {
        let mut b = Buffer::new("ab   \ncd", None);
        let mut cs = Cursors::new(Cursor {
            selection: Selection { anchor: 1, head: 4 },
            ..Cursor::default()
        });
        b.tidy(&mut cs, ALL, false);
        assert_eq!(b.full_text(), "ab\ncd\n");
        assert_eq!(cs.selection().range(), 1..2);
    }
}
