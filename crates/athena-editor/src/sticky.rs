use gpui::{Context, Pixels, Point};

use crate::buffer::Buffer;
use crate::display::{Fold, indent_column};
use crate::view::EditorView;

/// Lines scanned upward for the scopes around the top line.
const REACH: usize = 2000;

impl EditorView {
    /// The scope headers sticky scroll pins over the text when `top` is the first line shown.
    pub(crate) fn sticky_lines(&self, top: usize, max: usize) -> Vec<usize> {
        match self.buf() {
            Some(b) => scope_headers(&b, top, max, |l| self.fold_at(l)),
            None => Vec::new(),
        }
    }

    /// A click on a pinned header scrolls to it and puts the cursor there, as VS Code does.
    pub(crate) fn click_sticky(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(line) = self.sticky_at(position) else {
            return false;
        };
        let Some(shared) = self.buffer.clone() else {
            return false;
        };
        let b = shared.buffer.borrow();
        let code = b
            .line(line)
            .chars()
            .take_while(|c| c.is_whitespace())
            .count();
        self.cursor.collapse();
        b.move_to(self.cursor.primary_mut(), b.line_start(line) + code, false);
        drop(b);
        let lh = self
            .layout
            .as_ref()
            .map_or(20., |l| f32::from(l.line_height));
        self.scroll.y = self.display.row_of(line) as f32 * lh;
        self.autoscroll = false;
        self.note_cursor_line(false, cx);
        cx.notify();
        true
    }

    /// The pinned header under a window position, if any.
    pub(crate) fn sticky_at(&self, position: Point<Pixels>) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        let y = position.y - layout.origin.y;
        if y < Pixels::ZERO {
            return None;
        }
        layout
            .sticky
            .get((y / layout.line_height).floor() as usize)
            .copied()
    }
}

/// The headers of the foldable scopes that contain `top` but start above it, outermost first
/// and at most `max`.
fn scope_headers(
    b: &Buffer,
    top: usize,
    max: usize,
    fold_at: impl Fn(usize) -> Option<Fold>,
) -> Vec<usize> {
    if max == 0 || top >= b.len_lines() {
        return Vec::new();
    }
    let indent_of = |l: usize| {
        let line: String = b
            .rope()
            .line(l)
            .chars()
            .take_while(|c| *c != '\n')
            .collect();
        indent_column(&line)
    };
    let mut below = indent_of(top).unwrap_or(usize::MAX);
    let mut found = Vec::new();
    for line in (top.saturating_sub(REACH)..top).rev() {
        // Only a line indented less than everything since can open a scope around `top`.
        let Some(col) = indent_of(line).filter(|&col| col < below) else {
            continue;
        };
        below = col;
        if fold_at(line).is_some_and(|f| f.end >= top) {
            found.push(line);
        }
        if col == 0 {
            break;
        }
    }
    found.reverse();
    found.truncate(max);
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn headers_of_the_scopes_around_the_top_line_stick() {
        let src = "mod a {\n    fn f() {\n        if x {\n            y();\n\n            z();\n        }\n    }\n}\nfn g() {}\n";
        let b = Buffer::new(src, Some(PathBuf::from("/x/a.rs")));
        let fold = |l| b.fold_at(l);
        assert_eq!(scope_headers(&b, 3, 5, fold), [0, 1, 2]);
        assert_eq!(
            scope_headers(&b, 4, 5, fold),
            [0, 1, 2],
            "blank lines stay inside"
        );
        assert_eq!(
            scope_headers(&b, 5, 2, fold),
            [0, 1],
            "outermost first, capped"
        );
        assert_eq!(
            scope_headers(&b, 6, 5, fold),
            [0, 1],
            "a closing line leaves its scope"
        );
        assert!(scope_headers(&b, 9, 5, fold).is_empty());
        assert!(scope_headers(&b, 0, 5, fold).is_empty());
    }
}
