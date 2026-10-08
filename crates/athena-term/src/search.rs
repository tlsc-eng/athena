use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Boundary, Column, Line, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::search::{Match, RegexSearch};

/// Matches kept per search; a broad pattern over full scrollback stops here, newest first.
pub const MAX_MATCHES: usize = 1000;

/// Find-in-terminal state: the compiled query and its matches over scrollback and screen.
#[derive(Default)]
pub struct Search {
    pub case_sensitive: bool,
    pub regex: bool,
    query: String,
    compiled: Option<RegexSearch>,
    /// The query is not a valid regular expression.
    pub invalid: bool,
    /// Sorted top to bottom, as grid points (negative lines are scrollback).
    pub matches: Vec<Match>,
    /// More matches exist than [`MAX_MATCHES`].
    pub truncated: bool,
    pub current: Option<usize>,
    /// Scrollback length the match lines are relative to.
    history: usize,
}

/// One row's slice of a match, in viewport cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub row: usize,
    pub start: usize,
    pub end: usize,
    pub current: bool,
}

impl Search {
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Recompiles for a new query or toggle; matches are found by the next [`Search::run`].
    pub fn set_query(&mut self, query: &str) {
        self.query = query.to_string();
        self.invalid = false;
        self.compiled = None;
        if query.is_empty() {
            return;
        }
        match RegexSearch::new(&pattern(query, self.case_sensitive, self.regex)) {
            Ok(regex) => self.compiled = Some(regex),
            Err(_) => self.invalid = true,
        }
    }

    /// Finds every match, making current the last one starting at or before `anchor` (the first
    /// one if none does), or the bottom of the viewport without an anchor.
    pub fn run<T>(&mut self, term: &Term<T>, anchor: Option<Point>) {
        self.history = term.grid().history_size();
        let Some(regex) = self.compiled.as_mut() else {
            self.matches.clear();
            self.truncated = false;
            self.current = None;
            return;
        };
        // Scanning up from the bottom keeps the newest matches when the cap is hit. `RegexIter`
        // resumes a leftward scan inside the last match, so each scan restarts left of it here.
        let end = Point::new(term.topmost_line(), Column(0));
        let mut point = Point::new(term.bottommost_line(), term.last_column());
        let mut matches: Vec<Match> = Vec::new();
        self.truncated = false;
        while let Some(found) = term.regex_search_left(regex, point, end) {
            if matches.len() == MAX_MATCHES {
                self.truncated = true;
                break;
            }
            let start = *found.start();
            matches.push(found);
            if start <= end {
                break;
            }
            point = start.sub(term, Boundary::None, 1);
        }
        matches.reverse();
        self.matches = matches;

        let anchor = anchor.unwrap_or_else(|| {
            let bottom = term.screen_lines() as i32 - 1 - term.grid().display_offset() as i32;
            Point::new(Line(bottom), term.last_column())
        });
        let at_or_before = self.matches.partition_point(|m| *m.start() <= anchor);
        self.current = match at_or_before {
            _ if self.matches.is_empty() => None,
            0 => Some(0),
            n => Some(n - 1),
        };
    }

    pub fn current_match(&self) -> Option<&Match> {
        self.matches.get(self.current?)
    }

    /// Moves to the next older (`up`) or newer match, wrapping around.
    pub fn step(&mut self, up: bool) {
        let len = self.matches.len();
        if len == 0 {
            return;
        }
        self.current = Some(match (self.current, up) {
            (None, _) => len - 1,
            (Some(i), true) => (i + len - 1) % len,
            (Some(i), false) => (i + 1) % len,
        });
    }

    /// Keeps match lines on their text after new output pushed lines into scrollback; false once
    /// scrollback holds `limit` lines, where the lines pushed can no longer be counted.
    pub fn follow_scroll<T>(&mut self, term: &Term<T>, limit: usize) -> bool {
        let history = term.grid().history_size();
        let pushed = history.saturating_sub(self.history) as i32;
        self.history = history;
        if pushed != 0 {
            for m in &mut self.matches {
                let (start, end) = (*m.start(), *m.end());
                *m = Point::new(start.line - pushed, start.column)
                    ..=Point::new(end.line - pushed, end.column);
            }
        }
        history < limit
    }

    /// Per-row pieces of the matches visible in a viewport scrolled back by `display_offset`.
    pub fn spans(&self, display_offset: usize, rows: usize, cols: usize) -> Vec<Span> {
        let top = Line(-(display_offset as i32));
        let bottom = Line(top.0 + rows as i32 - 1);
        let first = self.matches.partition_point(|m| m.end().line < top);
        let mut spans = Vec::new();
        for (i, m) in self.matches.iter().enumerate().skip(first) {
            if m.start().line > bottom {
                break;
            }
            let current = self.current == Some(i);
            for line in m.start().line.0.max(top.0)..=m.end().line.0.min(bottom.0) {
                let start = if line == m.start().line.0 {
                    m.start().column.0
                } else {
                    0
                };
                let end = if line == m.end().line.0 {
                    m.end().column.0 + 1
                } else {
                    cols
                };
                spans.push(Span {
                    row: (line - top.0) as usize,
                    start,
                    end: end.min(cols),
                    current,
                });
            }
        }
        spans
    }
}

/// Lines to scroll the display by so `line` is visible, centring it when it is not.
pub fn reveal_delta<T>(term: &Term<T>, line: Line) -> i32 {
    let rows = term.screen_lines() as i32;
    let offset = term.grid().display_offset() as i32;
    let row = line.0 + offset;
    if (0..rows).contains(&row) {
        return 0;
    }
    let target = (rows / 2 - line.0).clamp(0, term.grid().history_size() as i32);
    target - offset
}

/// The regex alacritty compiles: its own smart case is replaced by the explicit toggle.
fn pattern(query: &str, case_sensitive: bool, regex: bool) -> String {
    let flags = if case_sensitive { "(?-i)" } else { "(?i)" };
    if regex {
        return format!("{flags}{query}");
    }
    let mut out = String::from(flags);
    for c in query.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Scroll;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;

    use super::*;

    fn term(cols: usize, rows: usize, text: &str) -> Term<VoidListener> {
        let config = Config {
            scrolling_history: 10_000,
            ..Config::default()
        };
        let mut term = Term::new(config, &TermSize::new(cols, rows), VoidListener);
        feed(&mut term, text);
        term
    }

    fn feed(term: &mut Term<VoidListener>, text: &str) {
        let mut parser: Processor = Processor::new();
        parser.advance(term, text.as_bytes());
    }

    fn search(query: &str) -> Search {
        let mut s = Search::default();
        s.set_query(query);
        s
    }

    fn found(s: &Search) -> Vec<(i32, usize, usize)> {
        s.matches
            .iter()
            .map(|m| (m.start().line.0, m.start().column.0, m.end().column.0))
            .collect()
    }

    #[test]
    fn finds_matches_in_scrollback_and_screen_top_to_bottom() {
        let t = term(
            20,
            3,
            "error one\r\nok\r\nerror two\r\nfine\r\nan error\r\n",
        );
        let mut s = search("error");
        s.run(&t, None);
        assert_eq!(t.grid().history_size(), 3);
        assert_eq!(found(&s), [(-3, 0, 4), (-1, 0, 4), (1, 3, 7)]);
        assert_eq!(s.current, Some(2), "starts at the newest match on screen");
    }

    #[test]
    fn case_is_ignored_unless_asked_for() {
        let t = term(20, 3, "Error\r\nerror\r\nERROR");
        let mut s = search("Error");
        s.run(&t, None);
        assert_eq!(
            s.matches.len(),
            3,
            "an uppercase letter does not turn on case"
        );
        s.case_sensitive = true;
        s.set_query("Error");
        s.run(&t, None);
        assert_eq!(found(&s), [(0, 0, 4)]);
        s.set_query("error");
        s.run(&t, None);
        assert_eq!(found(&s), [(1, 0, 4)]);
    }

    #[test]
    fn plain_queries_match_regex_characters_literally() {
        let t = term(30, 3, "a.c abc (x) [y] a+b $HOME");
        for (query, col) in [
            ("a.c", 0),
            ("(x)", 8),
            ("[y]", 12),
            ("a+b", 16),
            ("$HOME", 20),
        ] {
            let mut s = search(query);
            s.run(&t, None);
            assert_eq!(found(&s), [(0, col, col + query.len() - 1)], "{query}");
        }
    }

    #[test]
    fn regex_mode_compiles_the_query_and_flags_bad_patterns() {
        let t = term(30, 3, "build 12 ok\r\nbuild 345 ok");
        let mut s = Search {
            regex: true,
            ..Search::default()
        };
        s.set_query(r"\d+");
        s.run(&t, None);
        assert_eq!(found(&s), [(0, 6, 7), (1, 6, 8)]);
        s.set_query("x*");
        s.run(&t, None);
        assert!(
            s.matches.len() <= MAX_MATCHES,
            "a pattern matching nothing still ends"
        );
        s.set_query("(unclosed");
        assert!(s.invalid);
        s.run(&t, None);
        assert!(s.matches.is_empty() && s.current.is_none());
    }

    #[test]
    fn an_empty_query_finds_nothing() {
        let t = term(10, 3, "anything");
        let mut s = search("");
        s.run(&t, None);
        assert!(s.matches.is_empty());
        assert!(!s.invalid);
    }

    #[test]
    fn a_match_wrapped_onto_the_next_row_is_split_into_spans() {
        let t = term(10, 3, "xxxxxxxneedle");
        let mut s = search("needle");
        s.run(&t, None);
        assert_eq!(s.matches.len(), 1);
        assert_eq!(
            s.spans(0, 3, 10),
            [
                Span {
                    row: 0,
                    start: 7,
                    end: 10,
                    current: true
                },
                Span {
                    row: 1,
                    start: 0,
                    end: 3,
                    current: true
                },
            ]
        );
    }

    #[test]
    fn spans_cover_only_the_visible_rows() {
        let mut t = term(10, 3, "");
        for i in 0..10 {
            feed(&mut t, &format!("hit {i}\r\n"));
        }
        let mut s = search("hit");
        s.run(&t, None);
        assert_eq!(s.matches.len(), 10);
        let rows = |s: &Search, offset| -> Vec<usize> {
            s.spans(offset, 3, 10).iter().map(|sp| sp.row).collect()
        };
        assert_eq!(rows(&s, 0), [0, 1], "the last row is the empty prompt line");
        assert_eq!(rows(&s, 8), [0, 1, 2]);
        assert_eq!(s.spans(8, 3, 10)[0].start, 0);
    }

    #[test]
    fn stepping_wraps_in_both_directions() {
        let t = term(10, 3, "a\r\na\r\na");
        let mut s = search("a");
        s.run(&t, None);
        assert_eq!(s.current, Some(2));
        s.step(false);
        assert_eq!(s.current, Some(0), "past the newest wraps to the oldest");
        s.step(true);
        assert_eq!(s.current, Some(2));
        s.step(true);
        assert_eq!(s.current, Some(1));
    }

    #[test]
    fn an_anchor_keeps_the_current_match_near_where_it_was() {
        let t = term(10, 4, "a\r\na\r\na\r\na");
        let mut s = search("a");
        s.run(&t, Some(Point::new(Line(1), Column(5))));
        assert_eq!(s.current, Some(1));
        s.run(&t, Some(Point::new(Line(-5), Column(0))));
        assert_eq!(
            s.current,
            Some(0),
            "nothing at or before the anchor picks the first"
        );
    }

    #[test]
    fn new_output_shifts_matches_with_their_text() {
        let mut t = term(10, 3, "hit\r\n");
        let mut s = search("hit");
        s.run(&t, None);
        assert_eq!(found(&s), [(0, 0, 2)]);
        feed(&mut t, "a\r\nb\r\nc\r\n");
        assert!(s.follow_scroll(&t, 10_000));
        let shifted = found(&s);
        assert!(shifted[0].0 < 0, "{shifted:?}");
        s.run(&t, None);
        assert_eq!(found(&s), shifted, "a fresh search agrees");
    }

    #[test]
    fn full_scrollback_reports_that_matches_cannot_be_followed() {
        let config = Config {
            scrolling_history: 4,
            ..Config::default()
        };
        let mut t = Term::new(config, &TermSize::new(10, 3), VoidListener);
        feed(&mut t, "hit\r\n");
        let mut s = search("hit");
        s.run(&t, None);
        feed(&mut t, "a\r\nb\r\n");
        assert!(s.follow_scroll(&t, 4), "room left: the shift is exact");
        feed(&mut t, "c\r\nd\r\ne\r\n");
        assert!(
            !s.follow_scroll(&t, 4),
            "scrollback filled up during this output"
        );
        let mut fresh = search("hit");
        fresh.run(&t, None);
        assert_eq!(found(&fresh), [(-4, 0, 2)]);
        feed(&mut t, "f\r\n");
        assert!(
            !s.follow_scroll(&t, 4),
            "a full scrollback hides how far lines moved"
        );
        s.run(&t, None);
        assert!(
            s.matches.is_empty(),
            "re-running finds the match scrolled out"
        );
    }

    #[test]
    fn broad_patterns_stop_at_the_cap_and_keep_the_newest() {
        let mut t = term(40, 5, "");
        for i in 0..(MAX_MATCHES + 50) {
            feed(&mut t, &format!("line {i}\r\n"));
        }
        let mut s = search("line");
        s.run(&t, None);
        assert!(s.truncated);
        assert_eq!(s.matches.len(), MAX_MATCHES);
        let newest = s.matches.last().unwrap().start().line;
        assert_eq!(newest, Line(3), "the last line written before the prompt");
    }

    #[test]
    fn reveal_centres_an_offscreen_line_and_leaves_a_visible_one() {
        let mut t = term(10, 4, "");
        for i in 0..20 {
            feed(&mut t, &format!("{i}\r\n"));
        }
        assert_eq!(reveal_delta(&t, Line(2)), 0);
        let delta = reveal_delta(&t, Line(-10));
        assert_eq!(delta, 12);
        t.scroll_display(Scroll::Delta(delta));
        assert_eq!(reveal_delta(&t, Line(-10)), 0);
        assert_eq!(
            reveal_delta(&t, Line(-17)),
            t.grid().history_size() as i32 - 12,
            "clamped at the top of scrollback"
        );
    }

    #[test]
    fn ten_thousand_lines_search_quickly() {
        let mut t = term(160, 50, "");
        let mut text = String::new();
        for i in 0..10_000 {
            text.push_str(&format!(
                "{i:05} compiling crate v1.2.3 (/Users/someone/code/project) warning: unused `x`{}\r\n",
                if i % 97 == 0 { " ERROR" } else { "" }
            ));
        }
        feed(&mut t, &text);
        let mut s = search("error");
        let started = Instant::now();
        s.run(&t, None);
        let took = started.elapsed();
        assert_eq!(s.matches.len(), 104);
        // Debug builds run the DFA about ten times slower than release (~10 ms here).
        let budget = if cfg!(debug_assertions) { 1500 } else { 150 };
        assert!(took.as_millis() < budget, "{took:?}");
    }
}
