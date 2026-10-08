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
        Down
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
    marked: Option<Range<usize>>,
    placeholder: SharedString,
    focus: FocusHandle,
    layout: Option<ShapedLine>,
    bounds: Option<Bounds<Pixels>>,
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
            marked: None,
            placeholder: placeholder.into(),
            focus: cx.focus_handle(),
            layout: None,
            bounds: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.marked = None;
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    fn edit(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
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
                if this.cursor > 0 {
                    let start = this.prev_boundary(this.cursor);
                    this.edit(start..this.cursor, "", cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Delete, _, cx| {
                if this.cursor < this.text.len() {
                    let end = this.next_boundary(this.cursor);
                    this.edit(this.cursor..end, "", cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Left, _, cx| {
                this.cursor = this.prev_boundary(this.cursor);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Right, _, cx| {
                this.cursor = this.next_boundary(this.cursor);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Home, _, cx| {
                this.cursor = 0;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &End, _, cx| {
                this.cursor = this.text.len();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Paste, _, cx| {
                if let Some(text) = cx
                    .read_from_clipboard()
                    .and_then(|c: ClipboardItem| c.text())
                {
                    let line = text.lines().next().unwrap_or_default().to_string();
                    this.edit(this.cursor..this.cursor, &line, cx);
                }
            }))
            .on_action(cx.listener(|_, _: &Submit, _, cx| cx.emit(InputEvent::Submit)))
            .on_action(cx.listener(|_, _: &SubmitBeside, _, cx| cx.emit(InputEvent::SubmitBeside)))
            .on_action(cx.listener(|_, _: &Cancel, _, cx| cx.emit(InputEvent::Cancel)))
            .on_action(cx.listener(|_, _: &Up, _, cx| cx.emit(InputEvent::Up)))
            .on_action(cx.listener(|_, _: &Down, _, cx| cx.emit(InputEvent::Down)))
            .w_full()
            .child(InputElement { input: cx.entity() })
    }
}

struct InputElement {
    input: Entity<TextInput>,
}

struct InputFrame {
    line: ShapedLine,
    cursor: Option<PaintQuad>,
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
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
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
        let (text, color) = if input.text.is_empty() {
            (input.placeholder.to_string(), theme.color.content_disabled)
        } else {
            (input.text.clone(), theme.color.content)
        };
        let base = TextRun {
            len: text.len(),
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = match &input.marked {
            Some(m) if !input.text.is_empty() => {
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
            _ => vec![base],
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(text.into(), font_size, &runs, None);
        let cursor_x = if input.text.is_empty() {
            px(0.)
        } else {
            line.x_for_index(input.cursor)
        };
        let cursor = input.focus.is_focused(window).then(|| {
            fill(
                Bounds::new(
                    point(bounds.left() + cursor_x, bounds.top()),
                    size(px(1.), bounds.size.height),
                ),
                theme.color.accent,
            )
        });
        InputFrame { line, cursor }
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
        let _ = frame
            .line
            .paint(bounds.origin, window.line_height(), window, cx);
        if let Some(cursor) = frame.cursor.take() {
            window.paint_quad(cursor);
        }
        let line = frame.line.clone();
        self.input.update(cx, |input, _| {
            input.layout = Some(line);
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
        Some(UTF16Selection {
            range: at..at,
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
            .unwrap_or(self.cursor..self.cursor);
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
            .unwrap_or(self.cursor..self.cursor);
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
        let line = self.layout.as_ref()?;
        let start = line.x_for_index(self.offset_from_utf16(range.start));
        let end = line.x_for_index(self.offset_from_utf16(range.end));
        Some(Bounds::from_corners(
            point(bounds.left() + start, bounds.top()),
            point(bounds.left() + end, bounds.bottom()),
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
