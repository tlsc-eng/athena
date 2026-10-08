use std::cmp::Reverse;
use std::ops::Range;

use gpui::Context;

use crate::buffer::{Buffer, Edit, Indent};
use crate::completion::ServerEdit;
use crate::display::TAB_WIDTH;
use crate::syntax::Lang;
use crate::view::{EditorEvent, EditorView};

impl EditorView {
    /// Whether Cmd+S formats first; `None` leaves it to the language (on for Go, as gofmt is the norm).
    pub fn set_format_on_save(&mut self, setting: Option<bool>) {
        self.format_setting = setting;
    }

    fn formats_on_save(&self) -> bool {
        self.format_setting
            .unwrap_or_else(|| self.lang() == Some(Lang::Go))
    }

    /// Cmd+S: asks the language server to format when that is on, else saves at once.
    pub(crate) fn save_formatted(&mut self, cx: &mut Context<Self>) {
        if self.formatting.is_some() {
            return;
        }
        let Some((version, indent)) = self.buf().map(|b| (b.version(), b.indent)) else {
            return;
        };
        if !self.formats_on_save() {
            self.save(cx);
            return;
        }
        self.format_requests += 1;
        let request = self.format_requests;
        self.formatting = Some((request, version));
        let (tab_size, insert_spaces) = match indent {
            Indent::Tab => (TAB_WIDTH as u32, false),
            Indent::Spaces(n) => (n as u32, true),
        };
        cx.emit(EditorEvent::Format {
            request,
            tab_size,
            insert_spaces,
        });
    }

    /// The answer to [`EditorEvent::Format`]: applies the edits, as one undo step that keeps the
    /// cursor, if the text has not changed since it was asked, then saves either way.
    pub fn format_and_save(
        &mut self,
        request: u64,
        edits: Vec<ServerEdit>,
        cx: &mut Context<Self>,
    ) {
        if self.formatting.map(|(asked, _)| asked) != Some(request) {
            return;
        }
        let Some((_, version)) = self.formatting.take() else {
            return;
        };
        let formatted = self
            .buf()
            .filter(|b| b.version() == version && !edits.is_empty())
            .map(|b| apply_server_edits(&b, &edits));
        if let Some(text) = formatted {
            self.replace_text(&text, cx);
        }
        self.save(cx);
    }
}

impl EditorView {
    /// Applies a language server's edits, such as a rename's, as one undo step; the cursor stays
    /// on the text it was on. The caller has checked they do not overlap.
    pub fn apply_server_edits(&mut self, edits: &[ServerEdit], cx: &mut Context<Self>) {
        if edits.is_empty() {
            return;
        }
        self.with_buffer(cx, |b, c| {
            let before = *c;
            let version = b.version();
            let list = server_edit_ranges(b, edits);
            b.apply_edits(c, &list, None);
            let mut kept = before;
            if let Some(applied) = b.edits_since(version) {
                let applied: Vec<Edit> = applied.copied().collect();
                kept.follow(&applied, b.len_chars());
            }
            *c = kept;
        });
    }
}

/// Char ranges for `edits`, last first, so each still holds when applied; at one place a
/// replacement goes before the inserts there, which then land in their given order.
fn server_edit_ranges(b: &Buffer, edits: &[ServerEdit]) -> Vec<(Range<usize>, String)> {
    let at = |(line, col): (u32, u32)| {
        if line as usize >= b.len_lines() {
            b.len_chars()
        } else {
            b.char_at_utf16(line, col)
        }
    };
    let mut ranges: Vec<(usize, Range<usize>, &str)> = edits
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let start = at(e.start);
            (i, start..at(e.end).max(start), e.text.as_str())
        })
        .collect();
    ranges.sort_by_key(|(i, r, _)| Reverse((r.start, r.end, *i)));
    ranges
        .into_iter()
        .map(|(_, r, text)| (r, text.to_string()))
        .collect()
}

/// The buffer's text with the server's edits applied; same-place inserts keep their order.
pub(crate) fn apply_server_edits(b: &Buffer, edits: &[ServerEdit]) -> String {
    // An end past the last line means the end of the text.
    let at = |(line, col): (u32, u32)| {
        if line as usize >= b.len_lines() {
            b.len_chars()
        } else {
            b.char_at_utf16(line, col)
        }
    };
    let mut ranges: Vec<(usize, Range<usize>, &str)> = edits
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let start = at(e.start);
            (i, start..at(e.end).max(start), e.text.as_str())
        })
        .collect();
    ranges.sort_by_key(|(i, r, _)| (Reverse(r.start), Reverse(*i)));
    // Overlapping edits break the protocol; one reaching into an applied edit is dropped.
    let mut floor = usize::MAX;
    ranges.retain(|(_, r, _)| {
        let fits = r.end <= floor;
        if fits {
            floor = r.start;
        }
        fits
    });
    let mut rope = b.rope().clone();
    for (_, range, text) in ranges {
        rope.remove(range.clone());
        rope.insert(range.start, text);
    }
    rope.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(start: (u32, u32), end: (u32, u32), text: &str) -> ServerEdit {
        ServerEdit {
            start,
            end,
            text: text.into(),
        }
    }

    #[test]
    fn server_edits_apply_from_the_end_and_keep_insert_order() {
        let b = Buffer::new("package main\nfunc  f( ){}\n", None);
        let out = apply_server_edits(
            &b,
            &[
                edit((1, 4), (1, 6), " "),
                edit((1, 8), (1, 9), ""),
                edit((1, 10), (1, 10), " "),
                edit((1, 11), (1, 11), "\n"),
            ],
        );
        assert_eq!(out, "package main\nfunc f() {\n}\n");
        let b = Buffer::new("xy", None);
        let same_place = [edit((0, 1), (0, 1), "A"), edit((0, 1), (0, 1), "B")];
        assert_eq!(apply_server_edits(&b, &same_place), "xABy");
    }

    #[test]
    fn server_edits_are_one_undo_step_and_keep_the_cursor_on_its_text() {
        let mut b = Buffer::new("let a = 1;\nuse(a, a);\n", None);
        let mut c = crate::Cursor::at(b.line_start(1) + 4);
        let before = c;
        let edits = [
            edit((0, 4), (0, 5), "count"),
            edit((1, 4), (1, 5), "count"),
            edit((1, 7), (1, 8), "count"),
            edit((1, 0), (1, 0), "// "),
            edit((1, 0), (1, 3), "run"),
        ];
        let version = b.version();
        let list = server_edit_ranges(&b, &edits);
        b.apply_edits(&mut c, &list, None);
        let applied: Vec<Edit> = b.edits_since(version).unwrap().copied().collect();
        let mut kept = before;
        kept.follow(&applied, b.len_chars());
        assert_eq!(b.full_text(), "let count = 1;\n// run(count, count);\n");
        assert_eq!(
            kept.head(),
            b.line_start(1) + "// run(".chars().count(),
            "the cursor stays at the start of the renamed word"
        );
        assert!(b.undo(&mut c));
        assert_eq!(b.full_text(), "let a = 1;\nuse(a, a);\n");
        assert!(!b.undo(&mut c), "all of it was one step");
    }

    #[test]
    fn overlapping_server_edits_are_dropped_instead_of_panicking() {
        let b = Buffer::new("abcdefghij", None);
        let out = apply_server_edits(
            &b,
            &[
                edit((0, 6), (0, 10), ""),
                edit((0, 2), (0, 8), "X"),
                edit((0, 0), (0, 1), "Y"),
            ],
        );
        assert_eq!(out, "Ybcdef");
    }

    #[test]
    fn an_end_past_the_last_line_reaches_the_end_of_the_text() {
        let b = Buffer::new("a\nb", None);
        assert_eq!(
            apply_server_edits(&b, &[edit((0, 0), (5, 0), "x\n")]),
            "x\n"
        );
        let b = Buffer::new("x😀y\n", None);
        assert_eq!(
            apply_server_edits(&b, &[edit((0, 3), (0, 4), "z")]),
            "x😀z\n"
        );
    }
}
