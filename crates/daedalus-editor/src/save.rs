use std::rc::Rc;

use gpui::{Context, WeakEntity};

use crate::buffer::{Buffer, Cursors, Edit, LineEnding};
use crate::editorconfig::{self, EditorConfig};
use crate::view::{EditorEvent, EditorView};

/// What a save does to the text before writing it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tidy {
    pub trim_trailing_whitespace: bool,
    pub insert_final_newline: bool,
    /// Line breaks to convert every line to.
    pub end_of_line: Option<LineEnding>,
}

/// Settings for trimming and the final newline; `None` leaves them off, as VS Code does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SaveSettings {
    pub trim_trailing_whitespace: Option<bool>,
    pub insert_final_newline: Option<bool>,
}

/// `.editorconfig` first, then settings; both off when neither says.
pub fn resolve_tidy(settings: SaveSettings, config: &EditorConfig) -> Tidy {
    Tidy {
        trim_trailing_whitespace: config
            .trim_trailing_whitespace
            .or(settings.trim_trailing_whitespace)
            .unwrap_or(false),
        insert_final_newline: config
            .insert_final_newline
            .or(settings.insert_final_newline)
            .unwrap_or(false),
        end_of_line: config.end_of_line,
    }
}

/// Lines of another view's carets, moved past the edits made since it last caught up.
fn caret_lines_now(b: &Buffer, seen: u64, cursors: &Cursors) -> Vec<usize> {
    let Some(edits) = b.edits_since(seen) else {
        return Vec::new();
    };
    let edits: Vec<Edit> = edits.copied().collect();
    let mut cursors = cursors.clone();
    cursors.follow(&edits, b.len_chars());
    cursors.all().iter().map(|c| b.line_of(c.head())).collect()
}

impl EditorView {
    /// Turns trimming and the final newline on save on or off.
    pub fn set_save_settings(&mut self, settings: SaveSettings) {
        self.save_settings = settings;
    }

    /// Tidies the text as the save about to happen asks, as one undo step; an auto save leaves
    /// blanks on the lines of every view's carets, where the user may still be typing.
    pub(crate) fn tidy_for_save(&mut self, auto: bool, cx: &mut Context<Self>) {
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        let tidy = {
            let b = shared.buffer.borrow();
            let config = b.path.as_deref().map(editorconfig::resolve);
            resolve_tidy(self.save_settings, &config.unwrap_or_default())
        };
        if tidy == Tidy::default() {
            return;
        }
        self.follow_edits();
        let mut keep = Vec::new();
        if auto {
            let me = cx.entity_id();
            let others: Vec<_> = shared
                .views
                .borrow()
                .iter()
                .filter_map(WeakEntity::upgrade)
                .filter(|v| v.entity_id() != me)
                .collect();
            let b = shared.buffer.borrow();
            keep.extend(self.cursor.all().iter().map(|c| b.line_of(c.head())));
            for view in others {
                let v = view.read(cx);
                if v.buffer.as_ref().is_some_and(|s| Rc::ptr_eq(s, &shared)) {
                    keep.extend(caret_lines_now(&b, v.seen, &v.cursor));
                }
            }
        }
        let mut cursor = std::mem::take(&mut self.cursor);
        let (before, version) = {
            let mut b = shared.buffer.borrow_mut();
            let before = b.version();
            b.tidy(&mut cursor, tidy, &keep);
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
        self.with_buffer(cx, |b, c| b.tidy(c, tidy, &[]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{Cursor, Selection};

    const ALL: Tidy = Tidy {
        trim_trailing_whitespace: true,
        insert_final_newline: true,
        end_of_line: None,
    };

    #[test]
    fn nothing_is_tidied_unless_settings_or_editorconfig_ask() {
        let none = SaveSettings::default();
        assert_eq!(
            resolve_tidy(none, &EditorConfig::default()),
            Tidy::default()
        );
        let trim = SaveSettings {
            trim_trailing_whitespace: Some(true),
            insert_final_newline: None,
        };
        let tidy = resolve_tidy(trim, &EditorConfig::default());
        assert!(tidy.trim_trailing_whitespace && !tidy.insert_final_newline);
    }

    #[test]
    fn editorconfig_beats_settings() {
        let off = SaveSettings {
            trim_trailing_whitespace: Some(false),
            insert_final_newline: Some(true),
        };
        let config = EditorConfig {
            trim_trailing_whitespace: Some(true),
            insert_final_newline: Some(false),
            end_of_line: Some(LineEnding::CrLf),
            ..EditorConfig::default()
        };
        assert_eq!(
            resolve_tidy(off, &config),
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
        b.tidy(&mut cs, ALL, &[]);
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
    fn an_auto_save_keeps_blanks_on_lines_with_a_caret_in_any_view() {
        let mut b = Buffer::new("a  \nb  \nc  \n", None);
        let mut cs = carets(&[6]);
        b.tidy(&mut cs, ALL, &[1]);
        assert_eq!(b.full_text(), "a\nb  \nc\n");
        assert_eq!(heads(&cs), [4]);

        // Another view's caret on c, seen before a line was typed above it here.
        let mut b = Buffer::new("a  \nb  \nc  \n", None);
        let (seen, other) = (b.version(), carets(&[9]));
        let mut cs = carets(&[0]);
        b.apply_edits(cs.primary_mut(), &[(0..0, "new\n".to_string())], None);
        let keep = caret_lines_now(&b, seen, &other);
        assert_eq!(keep, [3]);
        b.tidy(&mut cs, ALL, &keep);
        assert_eq!(b.full_text(), "new\na\nb\nc  \n");
    }

    #[test]
    fn trimming_leaves_blanks_inside_multi_line_strings() {
        let cases = [
            ("x.rs", "let s = r\"a  \nb\";  \n", "let s = r\"a  \nb\";\n"),
            ("x.go", "var s = `a  \nb`  \n", "var s = `a  \nb`\n"),
            ("x.ts", "const s = `a  \nb`;  \n", "const s = `a  \nb`;\n"),
            (
                "x.py",
                "def f():  \n    \"\"\"a  \n    b\"\"\"\n",
                "def f():\n    \"\"\"a  \n    b\"\"\"\n",
            ),
            (
                "x.yaml",
                "k: |\n  a  \n  b\nj: 1  \n",
                "k: |\n  a  \n  b\nj: 1\n",
            ),
        ];
        for (path, text, want) in cases {
            let mut b = Buffer::new(text, Some(path.into()));
            b.tidy(&mut carets(&[0]), ALL, &[]);
            assert_eq!(b.full_text(), want, "{path}");
        }
    }

    #[test]
    fn trimming_crlf_text_keeps_its_breaks() {
        let mut b = Buffer::new("a \r\nb\t\r\nc", None);
        let mut cs = carets(&[6, 9]);
        b.tidy(&mut cs, ALL, &[]);
        assert_eq!(b.full_text(), "a\r\nb\r\nc\r\n");
        assert_eq!(heads(&cs), [4, 7]);
        let mut b = Buffer::new("x\r\n", None);
        let mut cs = carets(&[0]);
        b.tidy(&mut cs, ALL, &[]);
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
        b.tidy(&mut cs, to_crlf, &[]);
        assert_eq!(b.full_text(), "ab\r\ncd\r\nef\r\n");
        assert_eq!(heads(&cs), [4, 10]);
        assert_eq!(b.line_ending(), LineEnding::CrLf);
        let to_lf = Tidy {
            end_of_line: Some(LineEnding::Lf),
            ..Tidy::default()
        };
        b.tidy(&mut cs, to_lf, &[]);
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
        b.tidy(&mut cs, ALL, &[]);
        assert_eq!(b.full_text(), "ab\ncd\n");
        assert_eq!(cs.selection().range(), 1..2);
    }
}
