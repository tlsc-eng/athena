//! Review comments on a file's lines: a gutter dot on each commented line, a "+" on the hovered
//! one, and a popover that lists, adds, edits and deletes comments. The shell stores them.

use std::ops::RangeInclusive;

use athena_ui::motion::{self, Opening};
use athena_ui::{ActiveTheme, Button, ButtonKind, InputEvent, Space, TextInput};
use gpui::{
    Animation, AnyElement, Context, Entity, EventEmitter, Focusable, FontWeight, MouseButton,
    Pixels, Point, Render, Subscription, Window, anchored, div, point, prelude::*, px,
};

use crate::view::EditorView;

/// Width of the gutter column between the line numbers and the fold chevrons that holds the dots.
pub(crate) const REVIEW_COLUMN: f32 = 14.;
const POPOVER_WIDTH: f32 = 380.;

/// A comment as the views show it, already placed on the lines it belongs to now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewMark {
    pub id: u64,
    /// One-based and inclusive.
    pub lines: RangeInclusive<u32>,
    pub body: String,
    /// Its lines changed so much that this is only where it used to be.
    pub stale: bool,
    /// Already delivered to the session.
    pub sent: bool,
}

/// How a gutter dot looks; a line with several comments shows the loudest of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum MarkState {
    /// Filled muted, as something done with.
    Sent,
    /// A hollow ring: the comment lost its lines.
    Stale,
    Active,
}

impl ReviewMark {
    fn state(&self) -> MarkState {
        match (self.sent, self.stale) {
            (true, _) => MarkState::Sent,
            (false, true) => MarkState::Stale,
            (false, false) => MarkState::Active,
        }
    }
}

/// What the user did in a view's comment popover; whoever stores the comments answers it with
/// the view's `set_review_comments`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewEvent {
    Add {
        /// One-based and inclusive.
        lines: RangeInclusive<u32>,
        body: String,
    },
    Edit {
        id: u64,
        body: String,
    },
    Delete {
        id: u64,
    },
    /// The text the comments are placed in changed under the view (a diff finished computing).
    Moved,
}

enum PopoverEvent {
    Review(ReviewEvent),
    Close,
}

/// The lines a new comment covers: the selection when it spans several lines and holds the
/// clicked one, else the clicked line alone.
pub(crate) fn range_for(
    clicked: u32,
    selection: Option<RangeInclusive<u32>>,
) -> RangeInclusive<u32> {
    match selection {
        Some(s) if s.start() < s.end() && s.contains(&clicked) => s,
        _ => clicked..=clicked,
    }
}

/// One-based lines a selection from zero-based line `first` to `last` covers; one that ends at
/// the very start of a line does not take that line along.
pub(crate) fn selection_lines(first: usize, last: usize, end_col: usize) -> RangeInclusive<u32> {
    let last = if last > first && end_col == 0 {
        last - 1
    } else {
        last
    };
    first as u32 + 1..=last as u32 + 1
}

/// What saving the box asks for: an edit of the comment being edited, else a new comment on
/// `range`; nothing for blank text.
fn save_event(
    editing: Option<u64>,
    range: &RangeInclusive<u32>,
    text: &str,
) -> Option<ReviewEvent> {
    let body = text.trim().to_string();
    if body.is_empty() {
        return None;
    }
    Some(match editing {
        Some(id) => ReviewEvent::Edit { id, body },
        None => ReviewEvent::Add {
            lines: range.clone(),
            body,
        },
    })
}

/// Whether a row whose top is `top` below the viewport's top, `row_h` tall, is on screen.
pub(crate) fn row_visible(top: f32, row_h: f32, viewport_h: f32) -> bool {
    top + row_h > 0. && top < viewport_h
}

/// The last visual row of a line: the lowest of its drawn rows, never above its first.
pub(crate) fn last_row(drawn: impl Iterator<Item = usize>, first: usize) -> usize {
    drawn.fold(first, usize::max)
}

/// A view's review comments and the popover open on one of its lines.
#[derive(Default)]
pub(crate) struct ReviewState {
    /// Comments can be read and written here.
    pub(crate) enabled: bool,
    /// The gutter column is kept, empty if need be, so text does not move when `enabled` flips.
    pub(crate) reserved: bool,
    marks: Vec<ReviewMark>,
    /// The one-based line the pointer is over.
    pub(crate) hover: Option<u32>,
    open: Option<Open>,
}

struct Open {
    line: u32,
    popover: Entity<CommentPopover>,
    _subscription: Subscription,
}

impl ReviewState {
    /// The dot for `line` if a comment starts there.
    pub(crate) fn dot(&self, line: u32) -> Option<MarkState> {
        let starts_here = self.marks.iter().filter(|m| *m.lines.start() == line);
        starts_here.map(ReviewMark::state).max()
    }

    pub(crate) fn column_shown(&self) -> bool {
        self.enabled || self.reserved
    }

    fn here(&self, line: u32) -> Vec<ReviewMark> {
        let mut here: Vec<ReviewMark> = self
            .marks
            .iter()
            .filter(|m| *m.lines.start() == line)
            .cloned()
            .collect();
        here.sort_by_key(|m| m.id);
        here
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            *self = Self {
                reserved: self.reserved,
                ..Self::default()
            };
        }
    }

    /// Tells the open popover its comment was stored, so it lets go of what was typed.
    pub(crate) fn saved(&self, cx: &mut gpui::App) {
        if let Some(open) = &self.open {
            open.popover.update(cx, |p, cx| p.saved(cx));
        }
    }

    pub(crate) fn set_marks(&mut self, marks: Vec<ReviewMark>, cx: &mut gpui::App) {
        self.marks = marks;
        if let Some(open) = &self.open {
            let here = self.here(open.line);
            open.popover.update(cx, |p, cx| p.set_comments(here, cx));
        }
    }

    /// The line the popover is open on.
    pub(crate) fn open_line(&self) -> Option<u32> {
        self.open.as_ref().map(|o| o.line)
    }

    pub(crate) fn popover(&self) -> Option<AnyElement> {
        self.open
            .as_ref()
            .map(|o| o.popover.clone().into_any_element())
    }

    /// Closes the popover; false if none was open.
    pub(crate) fn close(&mut self) -> bool {
        self.open.take().is_some()
    }

    /// Opens the popover on `line` for a new comment on `range`; `close` is how the host shuts
    /// it when asked to.
    pub(crate) fn open<H: EventEmitter<ReviewEvent> + 'static>(
        &mut self,
        line: u32,
        range: RangeInclusive<u32>,
        close: fn(&mut H, &mut Window, &mut Context<H>),
        window: &mut Window,
        cx: &mut Context<H>,
    ) {
        let here = self.here(line);
        let popover = cx.new(|cx| CommentPopover::new(range, here, window, cx));
        let subscription = cx.subscribe_in(
            &popover,
            window,
            move |this, _, event: &PopoverEvent, window, cx| match event {
                PopoverEvent::Review(e) => cx.emit(e.clone()),
                PopoverEvent::Close => close(this, window, cx),
            },
        );
        self.open = Some(Open {
            line,
            popover,
            _subscription: subscription,
        });
    }
}

/// The comments on one line, and the box to write or edit one in.
struct CommentPopover {
    range: RangeInclusive<u32>,
    here: Vec<ReviewMark>,
    editing: Option<u64>,
    input: Entity<TextInput>,
    opened: Opening,
    _input_events: Subscription,
}

impl EventEmitter<PopoverEvent> for CommentPopover {}

impl CommentPopover {
    fn new(
        range: RangeInclusive<u32>,
        here: Vec<ReviewMark>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| TextInput::new("Comment for Claude…", cx).multiline(6));
        let events =
            cx.subscribe_in(
                &input,
                window,
                |this, _, event: &InputEvent, _, cx| match event {
                    InputEvent::SubmitBeside => this.save(cx),
                    InputEvent::Cancel => cx.emit(PopoverEvent::Close),
                    _ => {}
                },
            );
        window.focus(&input.focus_handle(cx));
        Self {
            range,
            here,
            editing: None,
            input,
            opened: Opening::now(),
            _input_events: events,
        }
    }

    fn set_comments(&mut self, here: Vec<ReviewMark>, cx: &mut Context<Self>) {
        if self
            .editing
            .is_some_and(|id| here.iter().all(|m| m.id != id))
        {
            self.editing = None;
        }
        self.here = here;
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text();
        if let Some(event) = save_event(self.editing, &self.range, text) {
            cx.emit(PopoverEvent::Review(event));
        }
    }

    /// The comment was stored; only now does the box let go of the text, so a failed save loses
    /// nothing.
    fn saved(&mut self, cx: &mut Context<Self>) {
        self.editing = None;
        self.input.update(cx, |i, cx| i.set_text("", cx));
        cx.notify();
    }

    fn edit(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mark) = self.here.iter().find(|m| m.id == id) else {
            return;
        };
        let body = mark.body.clone();
        self.editing = Some(id);
        self.input.update(cx, |i, cx| i.set_text(body, cx));
        window.focus(&self.input.focus_handle(cx));
        cx.notify();
    }

    fn delete(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.editing == Some(id) {
            self.editing = None;
            self.input.update(cx, |i, cx| i.set_text("", cx));
        }
        cx.emit(PopoverEvent::Review(ReviewEvent::Delete { id }));
    }

    fn title(&self) -> String {
        match (self.editing, self.range.start(), self.range.end()) {
            (Some(_), ..) => "Edit comment".to_string(),
            (None, a, b) if a == b => format!("Comment on line {a}"),
            (None, a, b) => format!("Comment on lines {a}–{b}"),
        }
    }
}

impl Render for CommentPopover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let space = |s: Space| t.space(s);
        let comments = self.here.iter().map(|m| {
            let id = m.id;
            div()
                .flex()
                .flex_col()
                .gap(space(Space::Xs))
                .px(space(Space::Lg))
                .py(space(Space::Md))
                .border_b_1()
                .border_color(t.color.border)
                .child(div().text_color(t.color.content).child(m.body.clone()))
                .when(m.sent, |el| {
                    el.child(div().text_color(t.color.content_muted).child("Sent"))
                })
                .when(m.stale && !m.sent, |el| {
                    el.child(
                        div()
                            .text_color(t.color.warning)
                            .child("The lines this was written on have changed."),
                    )
                })
                .child(
                    div()
                        .flex()
                        .gap(space(Space::Xs))
                        .child(
                            Button::new(("comment-edit", id as usize), "Edit", ButtonKind::Ghost)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.edit(id, window, cx)
                                })),
                        )
                        .child(
                            Button::new(
                                ("comment-delete", id as usize),
                                "Delete",
                                ButtonKind::Ghost,
                            )
                            .on_click(cx.listener(move |this, _, _, cx| this.delete(id, cx))),
                        ),
                )
        });
        let panel = div()
            .id("review-comments")
            .occlude()
            .w(px(POPOVER_WIDTH))
            .flex()
            .flex_col()
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.accent)
            .rounded(t.shape.radius_panel)
            .shadow(vec![t.popover_shadow()])
            .font_family(t.typography.ui.clone())
            .text_size(t.typography.caption)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .when(!self.here.is_empty(), |el| {
                el.child(
                    div()
                        .id("review-comment-list")
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .children(comments),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space(Space::Sm))
                    .p(space(Space::Md))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(t.color.content_secondary)
                            .child(self.title()),
                    )
                    .child(
                        div()
                            .px(space(Space::Md))
                            .py(space(Space::Sm))
                            .bg(t.color.surface_sunken)
                            .border_1()
                            .border_color(t.color.border)
                            .rounded(t.shape.radius_control)
                            .child(self.input.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(space(Space::Sm))
                            .child(
                                Button::new("comment-cancel", "Cancel", ButtonKind::Ghost)
                                    .on_click(
                                        cx.listener(|_, _, _, cx| cx.emit(PopoverEvent::Close)),
                                    ),
                            )
                            .child(
                                Button::new("comment-save", "Save (⌘↵)", ButtonKind::Primary)
                                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                            ),
                    ),
            );
        motion::animate_enter(
            t.motion.reduced,
            self.opened.running(t.motion.fast),
            panel,
            "review-comments-open",
            Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d),
        )
    }
}

/// The popover under a line, to the right of the gutter, in window coordinates.
pub(crate) fn anchor(popover: AnyElement, at: Point<Pixels>) -> AnyElement {
    anchored().position(at).child(popover).into_any_element()
}

impl EventEmitter<ReviewEvent> for EditorView {}

impl EditorView {
    /// Lets the user comment on lines (the gutter shows where); only for files that belong to a
    /// Claude Code session under review.
    pub fn set_review(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.review.enabled != enabled {
            self.review.set_enabled(enabled);
            cx.notify();
        }
    }

    /// The comments on this file, placed on the lines they belong to now.
    pub fn set_review_comments(&mut self, marks: Vec<ReviewMark>, cx: &mut Context<Self>) {
        if self.review.enabled {
            self.review.set_marks(marks, cx);
            cx.notify();
        }
    }

    /// The one-based line drawn under `y`, if any.
    fn review_line_at(&self, position: Point<Pixels>) -> Option<u32> {
        let layout = self.layout.as_ref()?;
        if position.x < layout.origin.x || self.sticky_at(position).is_some() {
            return None;
        }
        let y = position.y - layout.origin.y + px(self.scroll.y);
        let row = (y / layout.line_height).floor().max(0.) as usize;
        layout.row(row).map(|r| r.line as u32 + 1)
    }

    pub(crate) fn hover_review(&mut self, position: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        let line = position
            .filter(|_| self.review.enabled)
            .and_then(|p| self.review_line_at(p));
        if line != self.review.hover {
            self.review.hover = line;
            cx.notify();
        }
    }

    /// The lines of a selection spanning several, if that is what the primary caret holds.
    fn selected_lines(&self) -> Option<RangeInclusive<u32>> {
        let selection = self.cursor.selection();
        if selection.is_empty() {
            return None;
        }
        let b = self.buf()?;
        let range = selection.range();
        let last = b.line_of(range.end);
        Some(selection_lines(
            b.line_of(range.start),
            last,
            range.end - b.line_start(last),
        ))
    }

    /// A click on a line's dot or "+" opens or closes the popover; true if it was on one.
    pub(crate) fn click_review_column(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(layout) = self.layout.as_ref() else {
            return false;
        };
        let (left, right) = layout.review_column;
        if !self.review.enabled || position.x < left || position.x >= right {
            return false;
        }
        let Some(line) = self.review_line_at(position) else {
            return false;
        };
        let was_open = self.review.open_line() == Some(line);
        self.review.close();
        if !was_open {
            let range = range_for(line, self.selected_lines());
            self.review
                .open(line, range, Self::close_review_popover, window, cx);
        }
        cx.notify();
        true
    }

    fn close_review_popover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.review.close();
        window.focus(&self.focus);
        cx.notify();
    }

    /// A click elsewhere in the editor puts the popover away.
    pub(crate) fn dismiss_review(&mut self, cx: &mut Context<Self>) {
        if self.review.close() {
            cx.notify();
        }
    }

    pub(crate) fn render_review(&self) -> Option<AnyElement> {
        let line = self.review.open_line()?;
        let layout = self.layout.as_ref()?;
        let lh = layout.line_height;
        let first = self.display.row_of(line as usize - 1);
        let top = lh * first as f32 - px(self.scroll.y);
        if !row_visible(top.into(), lh.into(), self.viewport.height.into()) {
            return None;
        }
        let drawn = layout.rows.iter().filter(|r| r.line == line as usize - 1);
        let last = last_row(drawn.map(|r| r.row), first);
        let y = layout.origin.y + lh * (last + 1) as f32 - px(self.scroll.y);
        Some(anchor(self.review.popover()?, point(layout.text_left, y)))
    }

    /// Tells the open popover its comment was stored.
    pub fn review_saved(&mut self, cx: &mut Context<Self>) {
        self.review.saved(cx);
    }

    /// Reserves the gutter column for comments, so the text does not move when a file becomes
    /// reviewable.
    pub fn set_review_reserved(&mut self, reserved: bool, cx: &mut Context<Self>) {
        if self.review.reserved != reserved {
            self.review.reserved = reserved;
            cx.notify();
        }
    }

    /// Closes the popover because the view scrolled out from under it.
    pub(crate) fn dismiss_review_on_scroll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.close() {
            window.focus(&self.focus);
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selection_spanning_the_clicked_line_sets_the_range_otherwise_the_line_does() {
        assert_eq!(range_for(5, Some(3..=7)), 3..=7);
        assert_eq!(range_for(3, Some(3..=7)), 3..=7);
        assert_eq!(range_for(9, Some(3..=7)), 9..=9, "clicked outside it");
        assert_eq!(range_for(4, Some(4..=4)), 4..=4, "one line is not a range");
        assert_eq!(range_for(4, None), 4..=4);
    }

    #[test]
    fn a_selection_ending_at_a_line_start_leaves_that_line_out() {
        assert_eq!(selection_lines(2, 5, 3), 3..=6);
        assert_eq!(selection_lines(2, 5, 0), 3..=5, "whole lines selected");
        assert_eq!(selection_lines(2, 2, 0), 3..=3, "a caret's own line stays");
    }

    fn mark(id: u64, line: u32, stale: bool, sent: bool) -> ReviewMark {
        ReviewMark {
            id,
            lines: line..=line + 1,
            body: String::new(),
            stale,
            sent,
        }
    }

    #[test]
    fn a_dot_sits_on_a_comments_first_line_and_shows_the_loudest_state_there() {
        let state = ReviewState {
            marks: vec![
                mark(1, 4, true, false),
                mark(2, 4, false, false),
                mark(3, 9, true, false),
                mark(4, 12, false, true),
                mark(5, 12, true, false),
                mark(6, 15, true, true),
            ],
            ..Default::default()
        };
        assert_eq!(state.dot(4), Some(MarkState::Active));
        assert_eq!(state.dot(9), Some(MarkState::Stale));
        assert_eq!(state.dot(12), Some(MarkState::Stale), "unsent beats sent");
        assert_eq!(state.dot(15), Some(MarkState::Sent), "sent wins over stale");
        assert_eq!(state.dot(5), None, "its last line has none");
        assert_eq!(state.here(4).len(), 2);
    }

    #[test]
    fn the_popover_hides_only_when_its_rows_top_is_off_screen() {
        assert!(row_visible(0., 20., 400.));
        assert!(row_visible(380., 20., 400.), "the bottom row still shows");
        assert!(row_visible(-10., 20., 400.), "partly scrolled off the top");
        assert!(!row_visible(-20., 20., 400.));
        assert!(!row_visible(400., 20., 400.));
    }

    #[test]
    fn the_popover_sits_under_the_last_drawn_row_of_a_wrapped_line() {
        assert_eq!(last_row([4, 5, 6].into_iter(), 4), 6);
        assert_eq!(last_row(std::iter::empty(), 4), 4, "nothing drawn yet");
        assert_eq!(last_row([2].into_iter(), 4), 4, "never above the first row");
    }

    #[test]
    fn saving_blank_text_asks_for_nothing_and_an_edit_keeps_its_comment() {
        assert_eq!(save_event(None, &(3..=4), "  \n "), None);
        assert_eq!(
            save_event(None, &(3..=4), " why \n"),
            Some(ReviewEvent::Add {
                lines: 3..=4,
                body: "why".into()
            })
        );
        assert_eq!(
            save_event(Some(7), &(3..=4), "now"),
            Some(ReviewEvent::Edit {
                id: 7,
                body: "now".into()
            })
        );
    }
}
