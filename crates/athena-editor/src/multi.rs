use std::ops::Range;

use gpui::{Context, Pixels, Point, px};

use crate::buffer::{Cursor, Cursors, Selection};
use crate::display::char_at_column;
use crate::view::EditorView;

fn selecting(r: Range<usize>) -> Cursor {
    Cursor {
        selection: Selection {
            anchor: r.start,
            head: r.end,
        },
        ..Cursor::default()
    }
}

impl EditorView {
    /// Cmd+D: selects the word at each caret, then adds the next occurrence of the primary
    /// selection; `skip` (Cmd+K Cmd+D) moves the primary selection to it instead.
    pub(crate) fn add_next_occurrence(&mut self, skip: bool, cx: &mut Context<Self>) {
        self.follow_edits();
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        let b = shared.buffer.borrow();
        if self.cursor.all().iter().all(|c| c.selection.is_empty()) {
            let Some(word) = b.word_around(self.cursor.head()).filter(|_| !skip) else {
                return;
            };
            let all = self
                .cursor
                .all()
                .iter()
                .map(|c| match b.word_around(c.head()) {
                    Some(r) => selecting(r),
                    None => *c,
                });
            self.occurrence = Some((b.text(word), true));
            self.cursor.set(all.collect(), self.cursor.primary_index());
        } else {
            let query = b.selected_text(self.cursor.primary());
            // As in VS Code, a search started from a bare caret matches whole words, and one
            // started with the find bar closed matches case.
            let word = self
                .occurrence
                .as_ref()
                .is_some_and(|(q, word)| *q == query && *word);
            let matches = b.find(&query, !self.find_open(), word);
            let from = self.cursor.selection().range().end;
            let taken =
                |r: &Range<usize>| self.cursor.all().iter().any(|c| c.selection.range() == *r);
            let Some(next) = matches
                .iter()
                .filter(|r| r.start >= from)
                .chain(&matches)
                .find(|r| !taken(r))
                .cloned()
            else {
                return;
            };
            if skip && !self.cursor.is_multi() {
                self.cursor = Cursors::new(selecting(next));
            } else {
                if skip {
                    self.cursor.remove(self.cursor.primary_index());
                }
                self.cursor.add(selecting(next));
            }
            self.occurrence = Some((query, word));
        }
        drop(b);
        self.carets_changed(cx);
    }

    /// Cmd+Shift+L: a caret on every occurrence of the primary selection, or of the word at it.
    pub(crate) fn select_all_occurrences(&mut self, cx: &mut Context<Self>) {
        self.follow_edits();
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        let b = shared.buffer.borrow();
        let primary = self.cursor.selection();
        let (query, word) = if primary.is_empty() {
            let Some(r) = b.word_around(primary.head) else {
                return;
            };
            (b.text(r), true)
        } else {
            (b.selected_text(self.cursor.primary()), false)
        };
        let matches = b.find(&query, !self.find_open(), word);
        let at = primary.range().start;
        let Some(main) = matches
            .iter()
            .position(|r| r.end >= at)
            .or(matches.len().checked_sub(1))
        else {
            return;
        };
        self.cursor
            .set(matches.into_iter().map(selecting).collect(), main);
        self.occurrence = Some((query, word));
        drop(b);
        self.carets_changed(cx);
    }

    /// Cmd+Alt+Up/Down: a caret on the row above or below each caret, in the column it aims for.
    pub(crate) fn add_caret_vertically(&mut self, dir: isize, cx: &mut Context<Self>) {
        self.follow_edits();
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        let b = shared.buffer.borrow();
        let rows = self.display.row_count(b.len_lines()) as isize;
        let mut all = self.cursor.all().to_vec();
        let mut primary = None;
        let order: Vec<Cursor> = if dir < 0 {
            self.cursor.all().to_vec()
        } else {
            self.cursor.all().iter().rev().copied().collect()
        };
        for c in order {
            let mut added = c;
            added.selection = Selection::cursor(c.head());
            if self.display.wrap_cols().is_some() {
                let Some((at, goal)) = self.row_target(&b, &c, dir) else {
                    continue;
                };
                b.move_to(&mut added, at, false);
                added.goal_column = Some(goal);
            } else {
                let row = self.display.row_of(b.line_of(c.head())) as isize + dir;
                if !(0..rows).contains(&row) {
                    continue;
                }
                b.move_to_line(&mut added, self.display.line_of(row as usize), false);
            }
            all.push(added);
            // The caret furthest in that direction leads, so the view follows the column's end.
            primary.get_or_insert(all.len() - 1);
        }
        let Some(primary) = primary else {
            return;
        };
        self.cursor.set(all, primary);
        drop(b);
        self.carets_changed(cx);
    }

    /// Lays carets over the box between where a Shift+Alt drag started and `position`; lines
    /// shorter than the box get a caret at their end, as VS Code clamps them.
    pub(crate) fn select_columns(&mut self, position: Point<Pixels>) {
        let Some((from_line, from_col)) = self.column_select else {
            return;
        };
        let (Some(at), Some(to_col)) = (self.char_at_position(position), self.column_at(position))
        else {
            return;
        };
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        let b = shared.buffer.borrow();
        let to_line = b.line_of(at);
        let mut all = Vec::new();
        let mut primary = 0;
        for line in from_line.min(to_line)..=from_line.max(to_line) {
            if self.display.fold_containing(line).is_some() {
                continue;
            }
            let text = b.line(line);
            let start = b.line_start(line);
            if line == to_line {
                primary = all.len();
            }
            all.push(Cursor {
                selection: Selection {
                    anchor: start + char_at_column(&text, from_col),
                    head: start + char_at_column(&text, to_col),
                },
                ..Cursor::default()
            });
        }
        self.cursor.set(all, primary);
    }

    /// The column with tabs expanded under a window position, from last frame's layout.
    pub(crate) fn column_at(&self, position: Point<Pixels>) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        let x = position.x - layout.text_left + px(self.scroll.x);
        Some((x / layout.cell).round().max(0.) as usize)
    }

    fn carets_changed(&mut self, cx: &mut Context<Self>) {
        self.reveal_selection();
        self.autoscroll = true;
        self.note_cursor_line(false, cx);
        cx.notify();
    }
}
