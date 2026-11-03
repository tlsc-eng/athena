use std::ops::Range;

use gpui::Context;

use crate::buffer::{Buffer, Cursor, Selection};
use crate::lsp_ui::Anchored;
use crate::view::EditorView;

/// A completed snippet being filled in: Tab and Shift+Tab move between its stops, and a stop
/// used in several places gets a caret in each, so they are typed together.
pub(crate) struct Snippet {
    /// Every stop's ranges, by the stop's place in the order Tab visits them.
    ranges: Anchored<usize>,
    /// How many stops Tab visits; the last is `$0`, or the end of the snippet.
    stops: usize,
    current: usize,
}

impl Snippet {
    /// A session for a snippet `len` chars long inserted at `base`, whose `stops` count from its
    /// start; `None` when it has no numbered stop.
    pub(crate) fn new(
        b: &Buffer,
        base: usize,
        len: usize,
        stops: &[(u32, Range<usize>)],
    ) -> Option<Self> {
        let mut numbers: Vec<u32> = stops.iter().map(|(n, _)| *n).filter(|n| *n > 0).collect();
        numbers.sort_unstable();
        numbers.dedup();
        if numbers.is_empty() {
            return None;
        }
        let at = |r: &Range<usize>| base + r.start..base + r.end;
        let mut items: Vec<(Range<usize>, usize)> = Vec::new();
        for (place, number) in numbers.iter().enumerate() {
            items.extend(
                stops
                    .iter()
                    .filter(|(n, _)| n == number)
                    .map(|(_, r)| (at(r), place)),
            );
        }
        let last = numbers.len();
        match stops.iter().find(|(n, _)| *n == 0) {
            Some((_, r)) => items.push((at(r), last)),
            None => items.push((base + len..base + len, last)),
        }
        Some(Self {
            ranges: Anchored::growing(b, items),
            stops: last + 1,
            current: 0,
        })
    }

    /// The ranges of the stop at `place` in `b` as it is now, in text order.
    fn ranges_of(&self, b: &Buffer, place: usize) -> Vec<Range<usize>> {
        let mut out: Vec<Range<usize>> = self
            .ranges
            .now(b)
            .into_iter()
            .filter(|(_, p)| *p == place)
            .map(|(r, _)| r)
            .collect();
        out.sort_by_key(|r| r.start);
        out.dedup();
        out
    }

    /// Whether `at` is still within the snippet, from its first stop to its last.
    fn holds(&self, b: &Buffer, at: usize) -> bool {
        let now = self.ranges.now(b);
        let start = now.iter().map(|(r, _)| r.start).min();
        let end = now.iter().map(|(r, _)| r.end).max();
        start.zip(end).is_some_and(|(s, e)| (s..=e).contains(&at))
    }

    /// What Tab (`by` 1) or Shift+Tab (-1) selects from a caret at `head`, and whether that is
    /// the last stop; `None` once the caret has left the snippet.
    fn step(&mut self, b: &Buffer, head: usize, by: isize) -> Option<(Vec<Cursor>, bool)> {
        let target = (self.current as isize + by).clamp(0, self.stops as isize - 1) as usize;
        let ranges = self.ranges_of(b, target);
        if !self.holds(b, head) || ranges.is_empty() {
            return None;
        }
        self.current = target;
        let carets = ranges
            .into_iter()
            .map(|r| Cursor {
                selection: Selection {
                    anchor: r.start,
                    head: r.end,
                },
                ..Cursor::default()
            })
            .collect();
        Some((carets, target + 1 == self.stops))
    }
}

impl EditorView {
    /// Tab (`by` 1) or Shift+Tab (-1) in a snippet selects the next or previous stop, and the
    /// last one ends it; false when no snippet holds the cursor, so the key does its usual work.
    pub(crate) fn step_snippet(&mut self, by: isize, cx: &mut Context<Self>) -> bool {
        let (Some(snippet), Some(shared)) = (self.snippet.as_mut(), self.buffer.clone()) else {
            return false;
        };
        let step = snippet.step(&shared.buffer.borrow(), self.cursor.head(), by);
        let Some((carets, last)) = step else {
            self.snippet = None;
            return false;
        };
        if last {
            self.snippet = None;
        }
        self.with_buffer(cx, |_, c| c.set(carets, 0));
        true
    }

    /// Escape leaves the snippet, keeping the carets where they are; false if none was active.
    pub(crate) fn leave_snippet(&mut self) -> bool {
        self.snippet.take().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Cursors;

    fn selections(c: &Cursors) -> Vec<(usize, usize)> {
        c.all()
            .iter()
            .map(|c| (c.selection.range().start, c.selection.range().end))
            .collect()
    }

    /// Steps as `step_snippet` does, on a bare buffer and carets.
    fn step(s: &mut Option<Snippet>, b: &Buffer, c: &mut Cursors, by: isize) -> bool {
        let Some((carets, last)) = s.as_mut().and_then(|s| s.step(b, c.head(), by)) else {
            *s = None;
            return false;
        };
        if last {
            *s = None;
        }
        c.set(carets, 0);
        true
    }

    #[test]
    fn tab_visits_each_stop_then_the_end_and_mirrors_are_typed_together() {
        // `for ${2:i} := ${1:0}; $2 < n; $2++ {\n\t$0\n}` completed after "x := 1\n".
        let text = "for i := 0; i < n; i++ {\n\t\n}";
        let mut b = Buffer::new(&format!("x := 1\n{text}\n"), None);
        let base = 7;
        let stops = vec![(2, 4..5), (1, 9..10), (2, 12..13), (2, 19..20), (0, 26..26)];
        let mut s = Snippet::new(&b, base, text.chars().count(), &stops);
        let mut c = Cursors::new(Cursor::at(16));
        assert!(step(&mut s, &b, &mut c, 0));
        assert_eq!(selections(&c), [(16, 17)], "stop 1 first");
        assert!(step(&mut s, &b, &mut c, 1));
        assert_eq!(
            selections(&c),
            [(11, 12), (19, 20), (26, 27)],
            "stop 2's three places"
        );
        b.edit_each(&mut c, |b, c| b.insert(c, "idx"));
        assert_eq!(b.line(1), "for idx := 0; idx < n; idx++ {");
        assert!(step(&mut s, &b, &mut c, -1));
        assert_eq!(
            selections(&c),
            [(18, 19)],
            "Shift+Tab goes back to stop 1, moved by the edits"
        );
        assert!(step(&mut s, &b, &mut c, 1));
        assert!(step(&mut s, &b, &mut c, 1));
        assert_eq!(selections(&c), [(39, 39)], "$0 last");
        assert!(s.is_none(), "reaching $0 ends the snippet");
        assert!(!step(&mut s, &b, &mut c, 1));
    }

    #[test]
    fn a_snippet_without_zero_ends_after_its_text_and_typing_into_an_empty_stop_grows_it() {
        let mut b = Buffer::new("Println()\n", None);
        let mut s = Snippet::new(&b, 0, 9, &[(1, 8..8)]);
        let mut c = Cursors::new(Cursor::at(8));
        assert!(step(&mut s, &b, &mut c, 0));
        b.edit_each(&mut c, |b, c| b.insert(c, "a, b"));
        assert_eq!(s.as_ref().unwrap().ranges_of(&b, 0), vec![8..12]);
        assert!(step(&mut s, &b, &mut c, 1));
        assert_eq!(selections(&c), [(13, 13)], "after the closing parenthesis");
        assert!(s.is_none());
    }

    #[test]
    fn leaving_the_snippet_ends_it() {
        let b = Buffer::new("f(x)\nrest\n", None);
        let mut s = Snippet::new(&b, 0, 4, &[(1, 2..3), (0, 4..4)]);
        let mut c = Cursors::new(Cursor::at(7));
        assert!(!step(&mut s, &b, &mut c, 1));
        assert!(s.is_none());
        assert!(
            Snippet::new(&b, 0, 4, &[(0, 4..4)]).is_none(),
            "nothing to visit"
        );
    }
}
