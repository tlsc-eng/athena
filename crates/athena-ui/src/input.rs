use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, Element, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, InspectorElementId,
    IntoElement, KeyBinding, LayoutId, PaintQuad, Pixels, Render, ShapedLine, SharedString, Style,
    TextRun, UTF16Selection, Window, actions, div, fill, point, prelude::*, px, relative, size,
};

use crate::ActiveTheme;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Home,
        End,
        Paste,
        Submit,
        SubmitBeside,
        Cancel,
        Up,
        Down,
        SelectAll
    ]
);

pub(crate) fn init(cx: &mut App) {
    let ctx = Some("TextInput");
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("cmd-left", Home, ctx),
        KeyBinding::new("cmd-right", End, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("cmd-v", Paste, ctx),
        KeyBinding::new("enter", Submit, ctx),
        KeyBinding::new("cmd-enter", SubmitBeside, ctx),
        KeyBinding::new("escape", Cancel, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("cmd-a", SelectAll, ctx),
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Changed,
    Submit,
    /// Cmd+Enter: submit, opening the result to the side.
    SubmitBeside,
    Cancel,
    Up,
    Down,
}

/// Single-line text field drawn in the theme, with IME composition.
pub struct TextInput {
    text: String,
    cursor: usize,
    /// The whole text is selected, so typing or pasting replaces it.
    all_selected: bool,
    marked: Option<Range<usize>>,
    placeholder: SharedString,
    focus: FocusHandle,
    /// Each drawn line with the byte its text starts at.
    layout: Vec<(usize, ShapedLine)>,
    bounds: Option<Bounds<Pixels>>,
    /// Set for a box that takes several lines and grows to this many rows before it scrolls.
    max_rows: Option<usize>,
    /// The first line shown when the text has more lines than the box has rows.
    scroll_row: usize,
}

impl EventEmitter<InputEvent> for TextInput {}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl TextInput {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            all_selected: false,
            marked: None,
            placeholder: placeholder.into(),
            focus: cx.focus_handle(),
            layout: Vec::new(),
            bounds: None,
            max_rows: None,
            scroll_row: 0,
        }
    }

    /// Makes the box take several lines: Enter breaks the line, Cmd+Enter submits, and the box
    /// grows to `max_rows` rows before it scrolls.
    pub fn multiline(mut self, max_rows: usize) -> Self {
        self.max_rows = Some(max_rows.max(1));
        self
    }

    /// Byte ranges of the text's lines, without their line breaks.
    fn lines(&self) -> Vec<Range<usize>> {
        line_ranges(&self.text, self.max_rows.is_some())
    }

    /// The line holding byte `at`, and `at`'s char column in it.
    fn line_at(&self, at: usize) -> (usize, usize) {
        let lines = self.lines();
        let i = lines.iter().rposition(|l| l.start <= at).unwrap_or(0);
        (i, self.text[lines[i].start..at].chars().count())
    }

    /// Moves to the line above or below, keeping the char column where the line is long enough.
    fn move_line(&mut self, down: bool, cx: &mut Context<Self>) {
        self.deselect(cx);
        let lines = self.lines();
        let (line, col) = self.line_at(self.cursor);
        let target = match down {
            true if line + 1 < lines.len() => line + 1,
            false if line > 0 => line - 1,
            _ => return,
        };
        let r = lines[target].clone();
        self.cursor = self.text[r.clone()]
            .char_indices()
            .nth(col)
            .map_or(r.end, |(i, _)| r.start + i);
        cx.notify();
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.all_selected = false;
        self.marked = None;
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    /// Selects the whole text, as a rename field starts, so the first keystroke replaces it.
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.all_selected = !self.text.is_empty();
        self.cursor = self.text.len();
        cx.notify();
    }

    /// The range an edit at the cursor replaces: everything while it is all selected.
    fn at_cursor(&self) -> Range<usize> {
        if self.all_selected {
            0..self.text.len()
        } else {
            self.cursor..self.cursor
        }
    }

    fn deselect(&mut self, cx: &mut Context<Self>) -> bool {
        let was = std::mem::take(&mut self.all_selected);
        if was {
            cx.notify();
        }
        was
    }

    fn edit(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        self.all_selected = false;
        self.text.replace_range(range.clone(), text);
        self.cursor = range.start + text.len();
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    fn prev_boundary(&self, at: usize) -> usize {
        self.text[..at]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self, at: usize) -> usize {
        self.text[at..]
            .chars()
            .next()
            .map_or(at, |c| at + c.len_utf8())
    }

    fn to_utf16(&self, byte: usize) -> usize {
        self.text[..byte].encode_utf16().count()
    }

    fn offset_from_utf16(&self, units: usize) -> usize {
        let mut seen = 0;
        for (i, c) in self.text.char_indices() {
            if seen >= units {
                return i;
            }
            seen += c.len_utf16();
        }
        self.text.len()
    }

    fn range_from_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(r.start)..self.offset_from_utf16(r.end)
    }
}

impl Render for TextInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("text-input")
            .key_context("TextInput")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Backspace, _, cx| {
                if this.all_selected {
                    this.edit(0..this.text.len(), "", cx);
                } else if this.cursor > 0 {
                    let start = this.prev_boundary(this.cursor);
                    this.edit(start..this.cursor, "", cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Delete, _, cx| {
                if this.all_selected {
                    this.edit(0..this.text.len(), "", cx);
                } else if this.cursor < this.text.len() {
                    let end = this.next_boundary(this.cursor);
                    this.edit(this.cursor..end, "", cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Left, _, cx| {
                // As in a macOS field, Left on a selection goes to its start.
                this.cursor = match this.deselect(cx) {
                    true => 0,
                    false => this.prev_boundary(this.cursor),
                };
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Right, _, cx| {
                if !this.deselect(cx) {
                    this.cursor = this.next_boundary(this.cursor);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Home, _, cx| {
                this.deselect(cx);
                let lines = this.lines();
                this.cursor = lines[this.line_at(this.cursor).0].start;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &End, _, cx| {
                this.deselect(cx);
                let lines = this.lines();
                this.cursor = lines[this.line_at(this.cursor).0].end;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.select_all(cx)))
            .on_action(cx.listener(|this, _: &Paste, _, cx| {
                if let Some(text) = cx
                    .read_from_clipboard()
                    .and_then(|c: ClipboardItem| c.text())
                {
                    let pasted = match this.max_rows {
                        Some(_) => text.replace("\r\n", "\n"),
                        None => text.lines().next().unwrap_or_default().to_string(),
                    };
                    this.edit(this.at_cursor(), &pasted, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Submit, _, cx| match this.max_rows {
                Some(_) => this.edit(this.at_cursor(), "\n", cx),
                None => cx.emit(InputEvent::Submit),
            }))
            .on_action(cx.listener(|_, _: &SubmitBeside, _, cx| cx.emit(InputEvent::SubmitBeside)))
            .on_action(cx.listener(|_, _: &Cancel, _, cx| cx.emit(InputEvent::Cancel)))
            .on_action(cx.listener(|this, _: &Up, _, cx| match this.max_rows {
                Some(_) => this.move_line(false, cx),
                None => cx.emit(InputEvent::Up),
            }))
            .on_action(cx.listener(|this, _: &Down, _, cx| match this.max_rows {
                Some(_) => this.move_line(true, cx),
                None => cx.emit(InputEvent::Down),
            }))
            .w_full()
            .child(InputElement { input: cx.entity() })
    }
}

struct InputElement {
    input: Entity<TextInput>,
}

struct InputFrame {
    lines: Vec<(usize, ShapedLine)>,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
    scroll_row: usize,
}

/// Byte ranges of `text`'s lines without their breaks; a single-line box has one line.
fn line_ranges(text: &str, multiline: bool) -> Vec<Range<usize>> {
    if !multiline {
        return std::iter::once(0..text.len()).collect();
    }
    let mut out = Vec::new();
    let mut start = 0;
    for (i, _) in text.match_indices('\n') {
        out.push(start..i);
        start = i + 1;
    }
    out.push(start..text.len());
    out
}

/// The first row to show so that `line` is in view in a box of `rows` rows.
fn scrolled_to(scroll: usize, line: usize, rows: usize) -> usize {
    if line < scroll {
        line
    } else if line >= scroll + rows {
        line + 1 - rows
    } else {
        scroll
    }
}

impl IntoElement for InputElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for InputElement {
    type RequestLayoutState = ();
    type PrepaintState = InputFrame;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let input = self.input.read(cx);
        let rows = match input.max_rows {
            Some(max) => input.lines().len().clamp(1, max),
            None => 1,
        };
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (window.line_height() * rows as f32).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> InputFrame {
        let input = self.input.read(cx);
        let theme = cx.theme();
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let lh = window.line_height();
        let focused = input.focus.is_focused(window);
        let ranges = match input.text.is_empty() {
            true => std::iter::once(0..0).collect(),
            false => input.lines(),
        };
        let rows = input.max_rows.unwrap_or(1);
        let (cursor_line, _) = input.line_at(input.cursor);
        let scroll_row = scrolled_to(input.scroll_row, cursor_line, rows);
        let mut lines = Vec::new();
        let mut selection = Vec::new();
        let mut cursor = None;
        for (i, range) in ranges.iter().enumerate() {
            let (text, color) = if input.text.is_empty() {
                (input.placeholder.to_string(), theme.color.content_disabled)
            } else {
                (input.text[range.clone()].to_string(), theme.color.content)
            };
            let base = TextRun {
                len: text.len(),
                font: style.font(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let marked = input
                .marked
                .as_ref()
                .filter(|m| !input.text.is_empty() && m.start >= range.start && m.end <= range.end)
                .map(|m| m.start - range.start..m.end - range.start);
            let runs = match marked {
                Some(m) => {
                    let underline = gpui::UnderlineStyle {
                        thickness: px(1.),
                        color: Some(color),
                        wavy: false,
                    };
                    vec![
                        TextRun {
                            len: m.start,
                            ..base.clone()
                        },
                        TextRun {
                            len: m.end - m.start,
                            underline: Some(underline),
                            ..base.clone()
                        },
                        TextRun {
                            len: text.len() - m.end,
                            ..base
                        },
                    ]
                    .into_iter()
                    .filter(|r| r.len > 0)
                    .collect()
                }
                None => vec![base],
            };
            let line = window
                .text_system()
                .shape_line(text.into(), font_size, &runs, None);
            let top = bounds.top() + lh * (i as f32 - scroll_row as f32);
            if (scroll_row..scroll_row + rows).contains(&i) {
                if focused && input.all_selected {
                    selection.push(fill(
                        Bounds::new(point(bounds.left(), top), size(line.width, lh)),
                        theme.color.surface_accent,
                    ));
                }
                if focused && i == cursor_line {
                    let x = match input.text.is_empty() {
                        true => px(0.),
                        false => line.x_for_index(input.cursor - range.start),
                    };
                    cursor = Some(fill(
                        Bounds::new(point(bounds.left() + x, top), size(px(1.), lh)),
                        theme.color.accent,
                    ));
                }
            }
            lines.push((range.start, line));
        }
        InputFrame {
            lines,
            cursor,
            selection,
            scroll_row,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        frame: &mut InputFrame,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        for quad in frame.selection.drain(..) {
            window.paint_quad(quad);
        }
        let lh = window.line_height();
        // Only a multi-line box clips: its scrolled-out lines sit above and below it.
        let mask = (frame.lines.len() > 1).then_some(gpui::ContentMask { bounds });
        window.with_content_mask(mask, |window| {
            for (i, (_, line)) in frame.lines.iter().enumerate() {
                let top = bounds.top() + lh * (i as f32 - frame.scroll_row as f32);
                let _ = line.paint(point(bounds.left(), top), lh, window, cx);
            }
        });
        if let Some(cursor) = frame.cursor.take() {
            window.paint_quad(cursor);
        }
        let (lines, scroll_row) = (std::mem::take(&mut frame.lines), frame.scroll_row);
        self.input.update(cx, |input, _| {
            input.layout = lines;
            input.scroll_row = scroll_row;
            input.bounds = Some(bounds);
        });
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range);
        *actual = Some(self.to_utf16(range.start)..self.to_utf16(range.end));
        Some(self.text[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let at = self.to_utf16(self.cursor);
        let start = if self.all_selected { 0 } else { at };
        Some(UTF16Selection {
            range: start..at,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked
            .as_ref()
            .map(|m| self.to_utf16(m.start)..self.to_utf16(m.end))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|r| self.range_from_utf16(&r))
            .or(self.marked.take())
            .unwrap_or(self.at_cursor());
        self.marked = None;
        self.edit(range, text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|r| self.range_from_utf16(&r))
            .or(self.marked.clone())
            .unwrap_or(self.at_cursor());
        self.edit(range.clone(), text, cx);
        self.marked = (!text.is_empty()).then(|| range.start..range.start + text.len());
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let (start, end) = (
            self.offset_from_utf16(range.start),
            self.offset_from_utf16(range.end),
        );
        let row = self.layout.iter().rposition(|(at, _)| *at <= start)?;
        let (at, line) = &self.layout[row];
        let rows = self.layout.len().clamp(1, self.max_rows.unwrap_or(1));
        let lh = bounds.size.height / rows as f32;
        let top = bounds.top() + lh * (row as f32 - self.scroll_row as f32);
        let x = |b: usize| line.x_for_index(b.saturating_sub(*at).min(line.len()));
        Some(Bounds::from_corners(
            point(bounds.left() + x(start), top),
            point(bounds.left() + x(end), top + lh),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multiline_box_splits_at_breaks_and_scrolls_the_cursor_into_view() {
        assert_eq!(line_ranges("ab\n\ncd", true), vec![0..2, 3..3, 4..6]);
        assert_eq!(line_ranges("ab\ncd", false), vec![0..5]);
        assert_eq!(line_ranges("", true), vec![0..0]);
        assert_eq!(scrolled_to(0, 7, 6), 2);
        assert_eq!(scrolled_to(4, 1, 6), 1);
        assert_eq!(scrolled_to(2, 5, 6), 2);
    }
}
