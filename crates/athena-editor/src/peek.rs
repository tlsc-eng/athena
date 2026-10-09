//! The quick diff peek: a click on a gutter change bar shows what the lines were before, with
//! Stage, Revert, Next and Previous, as VS Code's dirty diff peek does.

use athena_ui::motion::{self, Opening};
use athena_ui::{ActiveTheme, Tooltip};
use gpui::{
    Animation, AnyElement, Context, FontWeight, HighlightStyle, MouseButton, Pixels, Point,
    SharedString, StyledText, anchored, div, point, prelude::*, px,
};

use crate::GutterMark;
use crate::blame::GitGutterEvent;
use crate::diff::{self, Change};
use crate::view::EditorView;

/// Most old lines the peek shows before it scrolls.
const MAX_LINES: usize = 12;
/// Width of the strip at the fold column's left edge where the change bars are drawn.
const BAR_HIT: f32 = 7.;

gpui::actions!(editor, [ShowNextChange, ShowPreviousChange]);

pub(crate) fn init(cx: &mut gpui::App) {
    cx.bind_keys([
        gpui::KeyBinding::new("alt-f3", ShowNextChange, Some("Editor")),
        gpui::KeyBinding::new("shift-alt-f3", ShowPreviousChange, Some("Editor")),
    ]);
}

/// The open peek: the file's index version and its changes against the buffer.
pub(crate) struct Peek {
    base: String,
    changes: Vec<Change>,
    current: usize,
    /// The buffer version `changes` were worked out from.
    version: u64,
    opened: Opening,
}

/// The change a click on zero-based `line` means: the one whose new lines hold it, else a
/// deletion marked at that line's top edge or its bottom edge.
pub(crate) fn change_at(changes: &[Change], line: usize) -> Option<usize> {
    changes
        .iter()
        .position(|c| c.new.contains(&line))
        .or_else(|| {
            changes
                .iter()
                .position(|c| c.new.is_empty() && c.new.start == line)
        })
        .or_else(|| {
            changes
                .iter()
                .position(|c| c.new.is_empty() && c.new.start == line + 1)
        })
}

/// The first line of the change mark after (or, going back, before) `line`, wrapping around.
pub(crate) fn next_mark(marks: &[GutterMark], line: usize, forward: bool) -> Option<usize> {
    let mut starts: Vec<usize> = marks
        .iter()
        .map(|m| match *m {
            GutterMark::Added { start, .. } | GutterMark::Modified { start, .. } => start,
            GutterMark::Removed { before } => before,
        })
        .collect();
    starts.sort_unstable();
    starts.dedup();
    match forward {
        true => starts.iter().find(|&&s| s > line).or(starts.first()),
        false => starts.iter().rev().find(|&&s| s < line).or(starts.last()),
    }
    .copied()
}

/// Whether zero-based `line` carries a change bar or a deletion mark at an edge of it.
fn marked(marks: &[GutterMark], line: usize) -> bool {
    marks.iter().any(|m| match *m {
        GutterMark::Added { start, len } | GutterMark::Modified { start, len } => {
            (start..start + len).contains(&line)
        }
        GutterMark::Removed { before } => before == line || before == line + 1,
    })
}

impl EditorView {
    /// A click on a change bar asks for the peek; true if it was on one.
    pub(crate) fn click_change_bar(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(layout) = self.layout.as_ref() else {
            return false;
        };
        let left = layout.fold_column.0;
        if position.x < left || position.x >= left + px(BAR_HIT) {
            return false;
        }
        let Some(line) = self
            .char_at_position(position)
            .and_then(|at| Some(self.buf()?.line_of(at)))
        else {
            return false;
        };
        if !marked(&self.gutter_marks, line) {
            return false;
        }
        cx.emit(GitGutterEvent::PeekChange { line });
        true
    }

    pub(crate) fn step_peek(&mut self, forward: bool, cx: &mut Context<Self>) {
        if let Some(peek) = self.peek.as_mut()
            && !peek.changes.is_empty()
        {
            let n = peek.changes.len();
            peek.current = match forward {
                true => (peek.current + 1) % n,
                false => (peek.current + n - 1) % n,
            };
            let line = peek.changes[peek.current].new.start;
            return self.reveal_peek(line, cx);
        }
        let Some(line) = next_mark(&self.gutter_marks, self.cursor_line(), forward) else {
            return;
        };
        cx.emit(GitGutterEvent::PeekChange { line });
    }

    fn reveal_peek(&mut self, line: usize, cx: &mut Context<Self>) {
        self.with_buffer(cx, |b, c| {
            let line = line.min(b.len_lines().saturating_sub(1));
            c.collapse();
            b.move_to(c.primary_mut(), b.char_at(line, 0), false);
        });
        self.center_cursor = true;
        cx.notify();
    }

    /// Opens the peek on the change at zero-based `line`, diffing the buffer against `base`
    /// (the file's index version; `None` when it is not in the index).
    pub fn show_change_peek(&mut self, base: Option<String>, line: usize, cx: &mut Context<Self>) {
        let (Some(text), Some(version)) = (self.text(), self.version()) else {
            return;
        };
        let base = base.unwrap_or_default();
        let changes = diff::diff_lines(&diff::lines(&base), &diff::lines(&text));
        let Some(current) = change_at(&changes, line) else {
            self.peek = None;
            return cx.notify();
        };
        let start = changes[current].new.start;
        self.peek = Some(Peek {
            base,
            changes,
            current,
            version,
            opened: Opening::now(),
        });
        self.reveal_peek(start, cx);
    }

    /// Closes the peek; false if none was open.
    pub(crate) fn close_peek(&mut self, cx: &mut Context<Self>) -> bool {
        let open = self.peek.take().is_some();
        if open {
            cx.notify();
        }
        open
    }

    /// Diffs the peek again after the buffer or its base changed; closes it once nothing differs.
    fn refresh_peek(&mut self, cx: &mut Context<Self>) {
        let (Some(text), Some(version)) = (self.text(), self.version()) else {
            return;
        };
        let Some(peek) = self.peek.as_mut() else {
            return;
        };
        peek.changes = diff::diff_lines(&diff::lines(&peek.base), &diff::lines(&text));
        peek.version = version;
        if peek.changes.is_empty() {
            self.peek = None;
        } else {
            peek.current = peek.current.min(peek.changes.len() - 1);
        }
        cx.notify();
    }

    fn revert_peeked(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.text() else {
            return;
        };
        let Some(peek) = self.peek.as_ref() else {
            return;
        };
        let Some(change) = peek.changes.get(peek.current) else {
            return;
        };
        let reverted = diff::revert_change(&diff::lines(&peek.base), &diff::lines(&text), change);
        self.replace_text(&reverted, cx);
        self.refresh_peek(cx);
    }

    fn stage_peeked(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.text() else {
            return;
        };
        let Some(peek) = self.peek.as_mut() else {
            return;
        };
        let Some(change) = peek.changes.get(peek.current) else {
            return;
        };
        let staged = diff::apply_change(&diff::lines(&peek.base), &diff::lines(&text), change);
        let expected = std::mem::replace(&mut peek.base, staged.clone());
        cx.emit(GitGutterEvent::StageChange {
            contents: staged,
            expected,
        });
        self.refresh_peek(cx);
    }

    pub(crate) fn render_peek(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.peek.as_ref()?.version != self.version()? {
            self.refresh_peek(cx);
        }
        let peek = self.peek.as_ref()?;
        let layout = self.layout.as_ref()?;
        let change = peek.changes.get(peek.current)?;
        let lh = layout.line_height;
        let b = self.buf()?;
        // Under the change's last line, or on the edge where lines were deleted.
        let anchor_line = match change.new.is_empty() {
            true => change.new.start,
            false => change.new.end,
        };
        let row = self.display.row_of(anchor_line.min(b.len_lines()));
        drop(b);
        let y = layout.origin.y + lh * row as f32 - px(self.scroll.y);
        if y < layout.origin.y || y > layout.origin.y + self.viewport.height - lh {
            return None;
        }
        let t = cx.theme().clone();
        let base_lines = diff::lines(&peek.base);
        let old: Vec<SharedString> = base_lines[change.old.clone()]
            .iter()
            .map(|l| SharedString::from(diff::display(l).replace('\t', "    ")))
            .collect();
        let removed = old.len();
        let added = change.new.len();
        let summary = match (removed, added) {
            (0, n) => format!("{n} added line{}", if n == 1 { "" } else { "s" }),
            (n, 0) => format!("{n} deleted line{}", if n == 1 { "" } else { "s" }),
            (o, n) => format!("{o} line{} changed to {n}", if o == 1 { "" } else { "s" }),
        };
        let title = format!(
            "Change {} of {} · {summary}",
            peek.current + 1,
            peek.changes.len()
        );
        let button = |id: &'static str, label: &'static str, tip: &'static str| {
            div()
                .id(id)
                .h(px(22.))
                .px(px(8.))
                .flex()
                .items_center()
                .rounded(t.shape.radius_control)
                .cursor_pointer()
                .text_color(t.color.content_muted)
                .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                .tooltip(move |_, cx| Tooltip::view(tip, cx))
                .child(label)
        };
        let header = div()
            .h(px(30.))
            .flex_none()
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(4.))
            .border_b_1()
            .border_color(t.color.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.color.content_secondary)
                    .child(title),
            )
            .child(
                button("peek-stage", "Stage", "Stage this change")
                    .on_click(cx.listener(|this, _, _, cx| this.stage_peeked(cx))),
            )
            .child(
                button("peek-revert", "Revert", "Revert this change (undo with ⌘Z)")
                    .on_click(cx.listener(|this, _, _, cx| this.revert_peeked(cx))),
            )
            .child(
                button("peek-prev", "↑", "Previous change (⇧⌥F3)")
                    .on_click(cx.listener(|this, _, _, cx| this.step_peek(false, cx))),
            )
            .child(
                button("peek-next", "↓", "Next change (⌥F3)")
                    .on_click(cx.listener(|this, _, _, cx| this.step_peek(true, cx))),
            )
            .child(
                button("peek-close", "✕", "Close (Esc)").on_click(cx.listener(|this, _, _, cx| {
                    this.close_peek(cx);
                })),
            );
        let body = match old.is_empty() {
            true => div()
                .px(px(12.))
                .py(px(6.))
                .text_color(t.color.content_muted)
                .child("These lines are new; the index has nothing here.")
                .into_any_element(),
            false => div()
                .id("peek-old")
                .max_h(lh * MAX_LINES as f32)
                .overflow_y_scroll()
                .font_family(t.typography.mono.clone())
                .text_size(t.typography.code)
                .children(old.into_iter().map(|line| {
                    let len = line.len();
                    div()
                        .h(lh)
                        .flex()
                        .items_center()
                        .bg(t.color.danger.opacity(0.12))
                        .child(
                            div()
                                .w(px(24.))
                                .flex_none()
                                .flex()
                                .justify_center()
                                .text_color(t.color.danger)
                                .child("−"),
                        )
                        .child(div().whitespace_nowrap().child(
                            StyledText::new(line).with_highlights([(
                                0..len,
                                HighlightStyle {
                                    color: Some(t.syntax.text),
                                    ..Default::default()
                                },
                            )]),
                        ))
                }))
                .into_any_element(),
        };
        let width = self.viewport.width - px(16.);
        let panel = div()
            .id("change-peek")
            .occlude()
            .w(width)
            .flex()
            .flex_col()
            .bg(t.color.surface)
            .border_t_2()
            .border_b_2()
            .border_color(t.color.accent)
            .shadow(vec![t.popover_shadow()])
            .font_family(t.typography.ui.clone())
            .text_size(t.typography.caption)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(header)
            .child(body);
        let panel = motion::animate_enter(
            t.motion.reduced,
            peek.opened.running(t.motion.fast),
            panel,
            "change-peek-open",
            Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d),
        );
        Some(
            anchored()
                .position(point(layout.origin.x + px(8.), y))
                .child(panel)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(old: std::ops::Range<usize>, new: std::ops::Range<usize>) -> Change {
        Change { old, new }
    }

    #[test]
    fn a_click_maps_to_the_change_holding_the_line_or_a_deletion_at_its_edge() {
        let changes = vec![change(1..2, 1..3), change(5..7, 6..6), change(9..9, 8..10)];
        assert_eq!(change_at(&changes, 1), Some(0));
        assert_eq!(change_at(&changes, 2), Some(0));
        assert_eq!(change_at(&changes, 6), Some(1), "deleted above line 6");
        assert_eq!(
            change_at(&changes, 5),
            Some(1),
            "the mark straddles the edge"
        );
        assert_eq!(change_at(&changes, 9), Some(2));
        assert_eq!(change_at(&changes, 4), None);
    }

    #[test]
    fn the_real_diff_lines_up_with_git_hunk_marks() {
        let base = "a\nb\nc\nd\ne\n";
        let text = "a\nB\nc\ne\nf\n";
        let changes = diff::diff_lines(&diff::lines(base), &diff::lines(text));
        assert_eq!(changes.len(), 3);
        assert_eq!(change_at(&changes, 1), Some(0));
        assert_eq!(base.lines().nth(changes[0].old.start), Some("b"));
        assert_eq!(change_at(&changes, 3), Some(1), "d was deleted above e");
        assert_eq!(change_at(&changes, 4), Some(2));
        let reverted = diff::revert_change(&diff::lines(base), &diff::lines(text), &changes[1]);
        assert_eq!(reverted, "a\nB\nc\nd\ne\nf\n");
    }

    #[test]
    fn next_and_previous_marks_wrap_around() {
        let marks = [
            GutterMark::Modified { start: 4, len: 2 },
            GutterMark::Removed { before: 9 },
            GutterMark::Added { start: 1, len: 1 },
        ];
        assert_eq!(next_mark(&marks, 1, true), Some(4));
        assert_eq!(next_mark(&marks, 9, true), Some(1));
        assert_eq!(next_mark(&marks, 4, false), Some(1));
        assert_eq!(next_mark(&marks, 0, false), Some(9));
        assert_eq!(next_mark(&[], 0, true), None);
        assert!(marked(&marks, 8) && marked(&marks, 5) && !marked(&marks, 6));
    }
}
