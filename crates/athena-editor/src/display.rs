use std::ops::Range;

/// Columns a tab advances to the next multiple of.
pub const TAB_WIDTH: usize = 4;

/// A buffer line as drawn: tabs expanded to spaces, with offsets back to buffer chars.
pub struct DisplayLine {
    pub text: String,
    /// Byte offset in `text` for each buffer char, plus one past the end.
    pub char_to_byte: Vec<usize>,
}

impl DisplayLine {
    pub fn new(line: &str) -> Self {
        let mut text = String::with_capacity(line.len());
        let mut char_to_byte = Vec::with_capacity(line.len() + 1);
        let mut col = 0;
        for c in line.chars() {
            char_to_byte.push(text.len());
            if c == '\t' {
                let n = TAB_WIDTH - col % TAB_WIDTH;
                text.extend(std::iter::repeat_n(' ', n));
                col += n;
            } else {
                text.push(c);
                col += 1;
            }
        }
        char_to_byte.push(text.len());
        Self { text, char_to_byte }
    }

    /// Buffer char (within the line) for a display byte, rounding into the char that contains it.
    pub fn char_for_byte(&self, byte: usize) -> usize {
        match self.char_to_byte.binary_search(&byte) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }
}

/// The char of `line` whose start is nearest column `col` (tabs expanded), or its end past it.
pub fn char_at_column(line: &str, col: usize) -> usize {
    let mut at = 0;
    for (i, c) in line.chars().enumerate() {
        let width = if c == '\t' {
            TAB_WIDTH - at % TAB_WIDTH
        } else {
            char_width(c)
        };
        if col < at + width {
            return if col - at <= width / 2 { i } else { i + 1 };
        }
        at += width;
    }
    line.chars().count()
}

/// The columns `text` spans, tabs expanded from its start.
pub fn column_width(text: &str) -> usize {
    text.chars().fold(0, |col, c| {
        col + if c == '\t' {
            TAB_WIDTH - col % TAB_WIDTH
        } else {
            char_width(c)
        }
    })
}

/// Lines hidden under a folded header; the header is the line before `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fold {
    pub start: usize,
    pub end: usize,
}

impl Fold {
    pub fn header(&self) -> usize {
        self.start - 1
    }

    fn contains(&self, line: usize) -> bool {
        self.start <= line && line <= self.end
    }
}

/// Columns a char takes: two for East Asian wide chars and most emoji, one otherwise.
fn char_width(c: char) -> usize {
    let wide = matches!(c as u32,
        0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x1F300..=0x1F64F | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD);
    if wide { 2 } else { 1 }
}

/// The columns a line's leading blanks reach.
fn indent_width(line: &str) -> usize {
    let mut col = 0;
    for c in line.chars() {
        match c {
            ' ' => col += 1,
            '\t' => col += TAB_WIDTH - col % TAB_WIDTH,
            _ => break,
        }
    }
    col
}

/// How far continuation rows of a wrapped line are indented: as far as the line itself, as
/// VS Code's `wrappingIndent: same`, unless that leaves too little room.
pub fn wrap_indent(line: &str, cols: usize) -> usize {
    let indent = indent_width(line);
    if indent * 2 > cols { 0 } else { indent }
}

/// Where the continuation rows of `line` start, as char offsets, wrapping at `cols` columns
/// after a blank where one fits and mid-word where none does.
pub fn wrap_breaks(line: &str, cols: usize) -> Vec<usize> {
    let cols = cols.max(1);
    let indent = wrap_indent(line, cols);
    let mut breaks = Vec::new();
    // The column each char starts at, tabs expanded from the start of the line.
    let mut at = Vec::with_capacity(line.len() + 1);
    let mut col = 0;
    let chars: Vec<char> = line.chars().collect();
    for &c in &chars {
        at.push(col);
        col += if c == '\t' {
            TAB_WIDTH - col % TAB_WIDTH
        } else {
            char_width(c)
        };
    }
    at.push(col);
    if col <= cols {
        return breaks;
    }
    let (mut start, mut room) = (0, cols);
    let mut blank_end = None;
    let mut i = 0;
    while i < chars.len() {
        // Blanks may hang past the edge, so a row never starts with the blank it broke at.
        if at[i + 1] - at[start] > room && i > start && !chars[i].is_whitespace() {
            let at_break = blank_end.filter(|&b| b > start).unwrap_or(i);
            breaks.push(at_break);
            (start, room) = (at_break, cols.saturating_sub(indent).max(1));
            blank_end = None;
            i = at_break;
            continue;
        }
        if chars[i].is_whitespace() && chars.get(i + 1).is_some_and(|c| !c.is_whitespace()) {
            blank_end = Some(i + 1);
        }
        i += 1;
    }
    breaks
}

/// Soft wrapping: the rows each buffer line takes at a width.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Wrap {
    cols: usize,
    /// Rows per buffer line; 0 marks a line whose rows must be counted again.
    rows: Vec<usize>,
    /// Rows before each line, one entry past the last line.
    before: Vec<usize>,
    stale: bool,
}

/// Maps buffer lines to the visual rows left after folding and wrapping.
#[derive(Default)]
pub struct DisplayMap {
    /// Sorted by `start` and never overlapping.
    folds: Vec<Fold>,
    wrap: Option<Wrap>,
}

impl DisplayMap {
    pub fn is_empty(&self) -> bool {
        self.folds.is_empty()
    }

    pub fn folds(&self) -> impl Iterator<Item = Fold> + '_ {
        self.folds.iter().copied()
    }

    /// Rows before `line` with nothing folded.
    fn before(&self, line: usize) -> usize {
        match &self.wrap {
            Some(w) => w
                .before
                .get(line)
                .or(w.before.last())
                .copied()
                .unwrap_or(line),
            None => line,
        }
    }

    /// The first row a line is drawn on; a hidden line maps to its fold's header.
    pub fn row_of(&self, line: usize) -> usize {
        let mut hidden = 0;
        for f in &self.folds {
            if f.start > line {
                break;
            }
            if f.contains(line) {
                return self.before(f.header()) - hidden;
            }
            hidden += self.before(f.end + 1) - self.before(f.start);
        }
        self.before(line) - hidden
    }

    /// The line drawn on `row`.
    pub fn line_of(&self, row: usize) -> usize {
        let mut row = row;
        for f in &self.folds {
            if self.before(f.start) > row {
                break;
            }
            row += self.before(f.end + 1) - self.before(f.start);
        }
        match &self.wrap {
            Some(w) => w.before.partition_point(|&b| b <= row).saturating_sub(1),
            None => row,
        }
    }

    pub fn row_count(&self, lines: usize) -> usize {
        let hidden: usize = self
            .folds
            .iter()
            .map(|f| self.before(f.end + 1) - self.before(f.start))
            .sum();
        self.before(lines) - hidden
    }

    /// Counts every line's rows again, as after edits that were not followed one by one.
    pub fn reset_wrap(&mut self) {
        if let Some(w) = self.wrap.as_mut() {
            w.rows.clear();
            w.stale = true;
        }
    }

    /// The column count lines wrap at, if they do.
    pub fn wrap_cols(&self) -> Option<usize> {
        self.wrap.as_ref().map(|w| w.cols)
    }

    /// Wraps lines at `cols` columns, or stops wrapping with `None`; rows are counted by [`Self::sync_wrap`].
    pub fn set_wrap(&mut self, cols: Option<usize>) {
        match (cols, &mut self.wrap) {
            (None, _) => self.wrap = None,
            (Some(cols), Some(w)) if w.cols == cols => {}
            (Some(cols), _) => {
                self.wrap = Some(Wrap {
                    cols,
                    rows: Vec::new(),
                    before: Vec::new(),
                    stale: true,
                })
            }
        }
    }

    /// Counts the rows of lines that changed since the last count, for a text of `lines` lines.
    pub fn sync_wrap(&mut self, lines: usize, text: impl Fn(usize) -> String) {
        let Some(w) = self.wrap.as_mut() else {
            return;
        };
        if !w.stale && w.rows.len() == lines {
            return;
        }
        w.rows.resize(lines, 0);
        for (line, rows) in w.rows.iter_mut().enumerate() {
            if *rows == 0 {
                *rows = wrap_breaks(&text(line), w.cols).len() + 1;
            }
        }
        w.before.clear();
        w.before.reserve(lines + 1);
        let mut total = 0;
        for rows in &w.rows {
            w.before.push(total);
            total += rows;
        }
        w.before.push(total);
        w.stale = false;
    }

    pub fn fold_containing(&self, line: usize) -> Option<Fold> {
        self.folds.iter().copied().find(|f| f.contains(line))
    }

    /// The fold whose header is `line`, if it is folded.
    pub fn folded_at(&self, line: usize) -> Option<Fold> {
        self.folds.iter().copied().find(|f| f.header() == line)
    }

    /// Folds `fold`, absorbing any folds inside or overlapping it.
    pub fn fold(&mut self, fold: Fold) {
        if fold.start == 0 || fold.end < fold.start {
            return;
        }
        self.folds
            .retain(|f| f.end < fold.start || f.start > fold.end);
        let at = self.folds.partition_point(|f| f.start < fold.start);
        self.folds.insert(at, fold);
    }

    /// Unfolds the fold headed by `line`; returns whether there was one.
    pub fn unfold_at(&mut self, line: usize) -> bool {
        let before = self.folds.len();
        self.folds.retain(|f| f.header() != line);
        self.folds.len() != before
    }

    pub fn clear(&mut self) {
        self.folds.clear();
    }

    /// Unfolds whatever hides `line`; returns whether anything changed.
    pub fn reveal(&mut self, line: usize) -> bool {
        let before = self.folds.len();
        self.folds.retain(|f| !f.contains(line));
        self.folds.len() != before
    }

    /// Follows an edit that replaced `old_lines` line breaks from `first` with `new_lines`.
    /// Folds the edit reaches into are dropped, as is one whose header gained or lost lines.
    pub fn apply_edit(&mut self, first: usize, old_lines: usize, new_lines: usize) {
        if let Some(w) = self.wrap.as_mut() {
            let end = (first + old_lines + 1).min(w.rows.len());
            if first < end {
                w.rows
                    .splice(first..end, std::iter::repeat_n(0, new_lines + 1));
            }
            w.stale = true;
        }
        let last = first + old_lines;
        let delta = new_lines as isize - old_lines as isize;
        self.folds.retain_mut(|f| {
            if f.end < first {
                return true;
            }
            if f.start <= last || (delta != 0 && f.header() == first) {
                return false;
            }
            f.start = (f.start as isize + delta) as usize;
            f.end = (f.end as isize + delta) as usize;
            true
        });
    }
}

/// The visual column a line's code starts at; `None` for a blank line.
fn indent_column(s: &str) -> Option<usize> {
    if s.trim().is_empty() {
        return None;
    }
    let mut col = 0;
    for c in s.chars() {
        match c {
            ' ' => col += 1,
            '\t' => col += TAB_WIDTH - col % TAB_WIDTH,
            _ => break,
        }
    }
    Some(col)
}

/// The block indented under `line`: following lines indented deeper, ignoring blank ones.
pub fn indent_fold_at(line: usize, lines: usize, text: impl Fn(usize) -> String) -> Option<Fold> {
    let header = indent_column(&text(line))?;
    let mut end = line;
    for l in line + 1..lines {
        match indent_column(&text(l)) {
            None => continue,
            Some(i) if i > header => end = l,
            Some(_) => break,
        }
    }
    (end > line).then_some(Fold {
        start: line + 1,
        end,
    })
}

/// Indentation guides for a run of lines: how many each line shows, and the guide of the block
/// the cursor is in, drawn brighter as VS Code does.
pub struct Guides {
    from: usize,
    levels: Vec<usize>,
    /// Guide index and the first and last line it is highlighted on.
    active: Option<(usize, usize, usize)>,
}

impl Guides {
    /// Guides for `lines` (which should include `cursor` for it to be highlighted), with
    /// `size`-column indent steps; `offside` languages end a block at its last indented line.
    pub fn new(
        lines: Range<usize>,
        total: usize,
        size: usize,
        offside: bool,
        cursor: usize,
        text: impl Fn(usize) -> String,
    ) -> Self {
        // Blank lines at the edges look this far out for the code around them.
        const REACH: usize = 100;
        let size = size.max(1);
        let level = |col: usize| col.div_ceil(size);
        let cols: Vec<Option<usize>> = lines.clone().map(|l| indent_column(&text(l))).collect();
        let outside = |range: Range<usize>, rev: bool| {
            let mut it: Box<dyn Iterator<Item = usize>> = if rev {
                Box::new(range.rev())
            } else {
                Box::new(range)
            };
            it.find_map(|l| indent_column(&text(l)))
        };
        let mut below = vec![None; cols.len()];
        let mut next = outside(lines.end..(lines.end + REACH).min(total), false);
        for (i, col) in cols.iter().enumerate().rev() {
            below[i] = next;
            if col.is_some() {
                next = *col;
            }
        }
        let mut above = outside(lines.start.saturating_sub(REACH)..lines.start, true);
        let mut levels = Vec::with_capacity(cols.len());
        for (i, col) in cols.iter().enumerate() {
            levels.push(match (col, above.map(level), below[i].map(level)) {
                (Some(col), ..) => level(*col),
                (None, Some(a), Some(b)) if a == b => a,
                (None, Some(a), Some(b)) if a < b => a + 1,
                (None, Some(_), Some(b)) if offside => b,
                (None, Some(_), Some(b)) => b + 1,
                _ => 0,
            });
            if col.is_some() {
                above = *col;
            }
        }
        let active = active_guide(&levels, cursor.wrapping_sub(lines.start))
            .map(|(guide, a, b)| (guide, lines.start + a, lines.start + b));
        Self {
            from: lines.start,
            levels,
            active,
        }
    }

    /// How many guides `line` shows; guide `k` sits at column `k * size`.
    pub fn level(&self, line: usize) -> usize {
        line.checked_sub(self.from)
            .and_then(|i| self.levels.get(i))
            .copied()
            .unwrap_or(0)
    }

    pub fn is_active(&self, guide: usize, line: usize) -> bool {
        self.active
            .is_some_and(|(g, a, b)| g == guide && (a..=b).contains(&line))
    }
}

/// The guide around the line at `at`: the one under a block header, else the innermost one the
/// line sits in; with the lines it runs over, as indexes into `levels`.
fn active_guide(levels: &[usize], at: usize) -> Option<(usize, usize, usize)> {
    let here = *levels.get(at)?;
    let next = levels.get(at + 1).copied().unwrap_or(0);
    let (guide, seed) = if next > here {
        (here, at + 1)
    } else {
        (here.checked_sub(1)?, at)
    };
    let mut start = seed;
    while start > 0 && levels[start - 1] > guide {
        start -= 1;
    }
    let mut end = seed;
    while end + 1 < levels.len() && levels[end + 1] > guide {
        end += 1;
    }
    Some((guide, start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold(start: usize, end: usize) -> Fold {
        Fold { start, end }
    }

    #[test]
    fn rows_round_trip_with_two_folds() {
        let mut map = DisplayMap::default();
        map.fold(fold(3, 5));
        map.fold(fold(8, 9));
        let visible = [0, 1, 2, 6, 7, 10, 11];
        assert_eq!(map.row_count(12), visible.len());
        for (row, line) in visible.iter().enumerate() {
            assert_eq!(map.line_of(row), *line);
            assert_eq!(map.row_of(*line), row);
        }
        assert_eq!(map.row_of(4), 2, "hidden lines sit on their header's row");
        assert!(map.fold_containing(9).is_some() && map.fold_containing(7).is_none());
        assert_eq!(map.folded_at(7), Some(fold(8, 9)));
    }

    #[test]
    fn folding_an_outer_block_absorbs_inner_folds() {
        let mut map = DisplayMap::default();
        map.fold(fold(4, 5));
        map.fold(fold(2, 8));
        assert_eq!(map.folds, vec![fold(2, 8)]);
        assert!(map.unfold_at(1));
        assert!(map.is_empty());
    }

    #[test]
    fn edits_shift_folds_below_and_drop_touched_ones() {
        let mut map = DisplayMap::default();
        map.fold(fold(3, 5));
        map.fold(fold(10, 12));
        map.apply_edit(0, 0, 2);
        assert_eq!(map.folds, vec![fold(5, 7), fold(12, 14)]);
        map.apply_edit(6, 1, 0);
        assert_eq!(map.folds, vec![fold(11, 13)]);
        map.apply_edit(10, 0, 0);
        assert_eq!(
            map.folds,
            vec![fold(11, 13)],
            "typing on the header keeps the fold"
        );
        map.apply_edit(10, 0, 1);
        assert!(map.is_empty(), "a new line under the header unfolds");
    }

    #[test]
    fn reveal_unfolds_only_what_hides_the_line() {
        let mut map = DisplayMap::default();
        map.fold(fold(3, 5));
        map.fold(fold(8, 9));
        assert!(!map.reveal(7));
        assert!(map.reveal(9));
        assert_eq!(map.folds, vec![fold(3, 5)]);
    }

    #[test]
    fn indent_folds_python_blocks() {
        let src =
            "class A:\n    def f(self):\n        x = 1\n\n        return x\n\n    y = 2\nz = 3\n";
        let lines: Vec<&str> = src.lines().collect();
        let text = |l: usize| lines[l].to_string();
        assert_eq!(indent_fold_at(0, lines.len(), text), Some(fold(1, 6)));
        assert_eq!(indent_fold_at(1, lines.len(), text), Some(fold(2, 4)));
        assert_eq!(indent_fold_at(2, lines.len(), text), None);
        assert_eq!(indent_fold_at(3, lines.len(), text), None);
    }

    fn guides(src: &str, size: usize, offside: bool, cursor: usize) -> Guides {
        let lines: Vec<&str> = src.split('\n').collect();
        Guides::new(0..lines.len(), lines.len(), size, offside, cursor, |l| {
            lines[l].to_string()
        })
    }

    #[test]
    fn guides_follow_indentation_through_blank_lines() {
        let src = "func f() {\n\tif x {\n\n\t\ty()\n\t}\n\n}";
        let g = guides(src, 4, false, 3);
        let levels: Vec<usize> = (0..7).map(|l| g.level(l)).collect();
        assert_eq!(levels, [0, 1, 2, 2, 1, 1, 0]);
        assert!(g.is_active(1, 2) && g.is_active(1, 3), "the cursor's block");
        assert!(!g.is_active(0, 3) && !g.is_active(1, 4));

        let header = guides(src, 4, false, 1);
        assert!(
            header.is_active(1, 3),
            "a header highlights the block under it"
        );
        assert!(!header.is_active(0, 1));
    }

    #[test]
    fn guides_round_partial_indents_up_and_offside_blocks_end_early() {
        let g = guides("a\n  b\n\nc", 4, true, 0);
        assert_eq!((g.level(1), g.level(2)), (1, 0));
        let g = guides("a\n  b\n\nc", 4, false, 0);
        assert_eq!(g.level(2), 1);
        assert_eq!(guides("", 4, false, 0).level(0), 0);
    }

    #[test]
    fn expands_tabs_to_stops() {
        let d = DisplayLine::new("\tx\ty");
        assert_eq!(d.text, "    x   y");
        assert_eq!(d.char_to_byte, vec![0, 4, 5, 8, 9]);
        assert_eq!(d.char_for_byte(2), 0);
        assert_eq!(d.char_for_byte(4), 1);
        assert_eq!(d.char_for_byte(9), 4);
    }

    #[test]
    fn wraps_after_blanks_with_continuation_indent_and_splits_long_words() {
        assert_eq!(wrap_breaks("aaaa bbbb cccc", 10), [10]);
        assert_eq!(wrap_breaks("aaaa bbbb cccc", 14), Vec::<usize>::new());
        assert_eq!(wrap_breaks("abcdefghij", 4), [4, 8]);
        // Continuation rows start four columns in, leaving eight for "baz qux".
        assert_eq!(wrap_breaks("    foo bar baz qux", 12), [12]);
        assert_eq!(wrap_breaks("    foo bar baz qux quux", 12), [12, 20]);
        assert_eq!(wrap_indent("    foo", 12), 4);
        assert_eq!(wrap_indent("            foo", 12), 0, "too deep to keep");
        assert_eq!(
            wrap_breaks("日本語日本語", 6),
            [3],
            "wide chars take two columns"
        );
        assert_eq!(column_width("\t日a"), 7);
    }

    #[test]
    fn columns_land_on_the_nearest_char_and_clamp_to_the_end() {
        assert_eq!(char_at_column("\tab", 1), 0);
        assert_eq!(char_at_column("\tab", 3), 1);
        assert_eq!(char_at_column("\tab", 5), 2);
        assert_eq!(char_at_column("\tab", 40), 3);
        assert_eq!(char_at_column("", 4), 0);
    }
}
