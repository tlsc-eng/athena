use gpui::Context;

use crate::Lang;
use crate::buffer::{Buffer, Cursor};
use crate::display::{DisplayMap, char_at_column_from, column_width, wrap_breaks, wrap_indent};
use crate::view::EditorView;

/// The rows a wrapped line is cut into, as char ranges of the line.
fn segments(text: &str, cols: usize) -> Vec<(usize, usize)> {
    let breaks = wrap_breaks(text, cols);
    let ends = breaks.iter().copied().chain([text.chars().count()]);
    std::iter::once(0)
        .chain(breaks.iter().copied())
        .zip(ends)
        .collect()
}

impl EditorView {
    /// Whether long lines wrap: this tab's choice, else its language's setting, else the
    /// workspace's, with Markdown wrapping by default as in VS Code.
    pub fn word_wrap(&self) -> bool {
        self.wrap
            .or(self.wrap_language)
            .unwrap_or_else(|| self.wrap_default || self.lang() == Some(Lang::Markdown))
    }

    /// Wraps this tab's lines or not; `None` follows the workspace default.
    pub fn set_word_wrap(&mut self, wrap: Option<bool>, cx: &mut Context<Self>) {
        self.wrap = wrap;
        self.wrap_changed(cx);
    }

    /// Whether tabs that have not chosen wrap long lines.
    pub fn set_word_wrap_default(&mut self, wrap: bool, cx: &mut Context<Self>) {
        self.wrap_default = wrap;
        self.wrap_changed(cx);
    }

    /// The word wrap a `"[lang]"` settings block chooses, which beats the built-in Markdown default.
    pub fn set_word_wrap_language(&mut self, wrap: Option<bool>, cx: &mut Context<Self>) {
        self.wrap_language = wrap;
        self.wrap_changed(cx);
    }

    pub(crate) fn toggle_word_wrap(&mut self, cx: &mut Context<Self>) {
        self.set_word_wrap(Some(!self.word_wrap()), cx);
    }

    fn wrap_changed(&mut self, cx: &mut Context<Self>) {
        if !self.word_wrap() {
            self.display.set_wrap(None);
        }
        // Column goals count chars unwrapped and screen columns wrapped.
        for c in self.cursor_goals() {
            *c = None;
        }
        self.scroll.x = 0.;
        self.autoscroll = true;
        cx.notify();
    }

    fn cursor_goals(&mut self) -> impl Iterator<Item = &mut Option<usize>> {
        self.cursor.all_mut().iter_mut().map(|c| &mut c.goal_column)
    }

    /// Counts wrapped rows for lines changed since they were last counted.
    pub(crate) fn sync_wrap(&mut self) {
        if self.display.wrap_cols().is_none() {
            return;
        }
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        let b = shared.buffer.borrow();
        self.display.sync_wrap(b.len_lines(), |l| b.line(l));
    }

    pub(crate) fn caret_row(&self, b: &Buffer, at: usize) -> usize {
        caret_row(&self.display, b, at)
    }

    pub(crate) fn row_target(&self, b: &Buffer, c: &Cursor, rows: isize) -> Option<(usize, usize)> {
        row_target(&self.display, b, c, rows)
    }

    pub(crate) fn row_edge(&self, b: &Buffer, c: &Cursor, end: bool) -> Option<usize> {
        row_edge(&self.display, b, c, end)
    }
}

/// The visual row a caret at `at` is drawn on.
/// The visual row a caret at `at` is drawn on.
fn caret_row(display: &DisplayMap, b: &Buffer, at: usize) -> usize {
    let line = b.line_of(at);
    let sub = display.wrap_cols().map_or(0, |cols| {
        let col = at - b.line_start(line);
        wrap_breaks(&b.line(line), cols).partition_point(|&x| x <= col)
    });
    display.row_of(line) + sub
}

/// Where a caret lands `rows` wrapped rows away, aiming for the screen column it aims for,
/// with that column; `None` past the first or last row.
fn row_target(display: &DisplayMap, b: &Buffer, c: &Cursor, rows: isize) -> Option<(usize, usize)> {
    let cols = display.wrap_cols()?;
    let line = b.line_of(c.head());
    let text = b.line(line);
    let col = c.head() - b.line_start(line);
    let segs = segments(&text, cols);
    let sub = segs.iter().rposition(|&(s, _)| s <= col).unwrap_or(0);
    let lead = if sub > 0 { wrap_indent(&text, cols) } else { 0 };
    // Tabs expand from the line's start, so a row's columns count from where it starts.
    let upto = |n: usize| column_width(&text.chars().take(n).collect::<String>());
    let goal = c
        .goal_column
        .unwrap_or(lead + upto(col) - upto(segs[sub].0));
    let row = (display.row_of(line) + sub) as isize + rows;
    if row < 0 || row as usize >= display.row_count(b.len_lines()) {
        return None;
    }
    let target = display.line_of(row as usize);
    let tsub = row as usize - display.row_of(target);
    let ttext = b.line(target);
    let tsegs = segments(&ttext, cols);
    let (s, e) = tsegs[tsub.min(tsegs.len() - 1)];
    let lead = if tsub > 0 {
        wrap_indent(&ttext, cols)
    } else {
        0
    };
    let piece: String = ttext.chars().skip(s).take(e - s).collect();
    let base = column_width(&ttext.chars().take(s).collect::<String>());
    let mut at = s + char_at_column_from(&piece, base, goal.saturating_sub(lead));
    // A caret at a wrap point is drawn on the next row, so this row ends a char sooner.
    if tsub + 1 < tsegs.len() {
        at = at.min(e.saturating_sub(1)).max(s);
    }
    Some((b.line_start(target) + at, goal))
}

/// Home and End on a wrapped line go to the start or end of the caret's row first, as in
/// VS Code; `None` means the whole line's.
fn row_edge(display: &DisplayMap, b: &Buffer, c: &Cursor, end: bool) -> Option<usize> {
    let cols = display.wrap_cols()?;
    let line = b.line_of(c.head());
    let start = b.line_start(line);
    let col = c.head() - start;
    let segs = segments(&b.line(line), cols);
    let sub = segs.iter().rposition(|&(s, _)| s <= col).unwrap_or(0);
    let (s, e) = segs[sub];
    if end {
        (sub + 1 < segs.len() && col + 1 < e).then(|| start + e - 1)
    } else {
        (sub > 0 && col > s).then_some(start + s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::Fold;

    fn wrapped(text: &str, cols: usize) -> (Buffer, DisplayMap) {
        let b = Buffer::new(text, None);
        let mut d = DisplayMap::default();
        d.set_wrap(Some(cols));
        d.sync_wrap(b.len_lines(), |l| b.line(l));
        (b, d)
    }

    #[test]
    fn vertical_moves_step_through_wrapped_rows_keeping_the_column() {
        // Row one is "alpha beta ", row two "gamma delta", then the short line.
        let (b, d) = wrapped("alpha beta gamma delta\nxy", 11);
        let mut c = Cursor::at(2);
        assert_eq!(caret_row(&d, &b, 2), 0);
        let (at, goal) = row_target(&d, &b, &c, 1).unwrap();
        assert_eq!((at, goal), (13, 2), "same column on the continuation row");
        assert_eq!(caret_row(&d, &b, at), 1);
        b.move_to(&mut c, at, false);
        c.goal_column = Some(goal);
        let (at, _) = row_target(&d, &b, &c, 1).unwrap();
        assert_eq!(at, b.line_start(1) + 2, "the short line below");
        c = Cursor::at(at);
        c.goal_column = Some(goal);
        let (up, _) = row_target(&d, &b, &c, -2).unwrap();
        assert_eq!(up, 2);
        assert_eq!(row_target(&d, &b, &c, 1), None, "past the last row");
    }

    #[test]
    fn tabs_on_a_continuation_row_keep_their_stops() {
        // The second row is "c\tdd" from column 10, so its tab spans one column, not three.
        let (b, d) = wrapped("aaaa bbbb c\tdd\naaaaaaaaaaaaaaaaa", 10);
        let c = Cursor::at(13);
        assert_eq!(caret_row(&d, &b, 13), 1);
        let (at, goal) = row_target(&d, &b, &c, 1).unwrap();
        assert_eq!(goal, 3, "the screen column of the second d");
        assert_eq!(at, b.line_start(1) + 3);
        let back = Cursor {
            goal_column: Some(goal),
            ..Cursor::at(at)
        };
        assert_eq!(row_target(&d, &b, &back, -1).unwrap().0, 13);
    }

    #[test]
    fn home_and_end_stop_at_the_wrapped_row_first() {
        let (b, d) = wrapped("alpha beta gamma delta", 11);
        let c = Cursor::at(2);
        assert_eq!(
            row_edge(&d, &b, &c, true),
            Some(10),
            "before the blank the row ends with"
        );
        assert_eq!(
            row_edge(&d, &b, &Cursor::at(10), true),
            None,
            "then the line's end"
        );
        assert_eq!(row_edge(&d, &b, &Cursor::at(15), false), Some(11));
        assert_eq!(
            row_edge(&d, &b, &Cursor::at(11), false),
            None,
            "then the line's start"
        );
        assert_eq!(
            row_edge(&d, &b, &Cursor::at(15), true),
            None,
            "the last row ends the line"
        );
    }

    #[test]
    fn rows_count_folds_and_wraps_together() {
        let text = "a\nalpha beta gamma delta epsilon\nb {\nalpha beta gamma delta\n}\nc";
        let (b, mut d) = wrapped(text, 11);
        assert_eq!(d.row_count(b.len_lines()), 9);
        assert_eq!(
            (d.row_of(1), d.row_of(2), d.row_of(3), d.row_of(4)),
            (1, 4, 5, 7)
        );
        assert_eq!((d.line_of(3), d.line_of(6), d.line_of(7)), (1, 3, 4));
        d.fold(Fold { start: 3, end: 3 });
        assert_eq!(d.row_count(b.len_lines()), 7);
        assert_eq!((d.row_of(3), d.row_of(4), d.row_of(5)), (4, 5, 6));
        assert_eq!((d.line_of(4), d.line_of(5), d.line_of(6)), (2, 4, 5));
        assert_eq!(caret_row(&d, &b, b.line_start(5)), 6);
    }

    #[test]
    fn edits_recount_only_the_lines_they_touch() {
        let (mut b, mut d) = wrapped("short\nshort", 11);
        assert_eq!(d.row_count(2), 2);
        let mut c = Cursor::at(b.line_start(1) + 5);
        let version = b.version();
        b.insert(&mut c, " and now much longer");
        for e in b.edits_since(version).unwrap() {
            d.apply_edit(e.line, e.lines_removed, e.lines_inserted);
        }
        d.sync_wrap(b.len_lines(), |l| b.line(l));
        assert_eq!(d.row_count(2), 4);
        assert_eq!(d.line_of(3), 1);
    }
}
