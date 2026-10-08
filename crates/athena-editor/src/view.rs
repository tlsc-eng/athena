use std::ops::Range;
use std::path::{Path, PathBuf};

use athena_ui::{ActiveTheme, InputEvent, TextInput, empty_state};
use gpui::{
    App, Bounds, ClipboardItem, Context, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point,
    Render, ScrollWheelEvent, ShapedLine, Size, Subscription, UTF16Selection, Window, actions, div,
    prelude::*, px,
};

use crate::buffer::Buffer;
use crate::display::DisplayLine;
use crate::element::EditorElement;

actions!(
    editor,
    [
        GoToDefinition,
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        MoveWordLeft,
        MoveWordRight,
        SelectWordLeft,
        SelectWordRight,
        MoveLineStart,
        MoveLineEnd,
        SelectLineStart,
        SelectLineEnd,
        MoveDocStart,
        MoveDocEnd,
        SelectDocStart,
        SelectDocEnd,
        PageUp,
        PageDown,
        Backspace,
        Delete,
        DeleteWordBack,
        DeleteToLineStart,
        Newline,
        Tab,
        SelectAll,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        Save,
        Find,
        FindNext,
        FindPrev,
        ToggleComment,
        Escape,
    ]
);

pub fn init(cx: &mut App) {
    let ctx = Some("Editor");
    cx.bind_keys([
        KeyBinding::new("left", MoveLeft, ctx),
        KeyBinding::new("right", MoveRight, ctx),
        KeyBinding::new("up", MoveUp, ctx),
        KeyBinding::new("down", MoveDown, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("shift-up", SelectUp, ctx),
        KeyBinding::new("shift-down", SelectDown, ctx),
        KeyBinding::new("alt-left", MoveWordLeft, ctx),
        KeyBinding::new("alt-right", MoveWordRight, ctx),
        KeyBinding::new("alt-shift-left", SelectWordLeft, ctx),
        KeyBinding::new("alt-shift-right", SelectWordRight, ctx),
        KeyBinding::new("cmd-left", MoveLineStart, ctx),
        KeyBinding::new("home", MoveLineStart, ctx),
        KeyBinding::new("cmd-right", MoveLineEnd, ctx),
        KeyBinding::new("end", MoveLineEnd, ctx),
        KeyBinding::new("cmd-shift-left", SelectLineStart, ctx),
        KeyBinding::new("cmd-shift-right", SelectLineEnd, ctx),
        KeyBinding::new("cmd-up", MoveDocStart, ctx),
        KeyBinding::new("cmd-down", MoveDocEnd, ctx),
        KeyBinding::new("cmd-shift-up", SelectDocStart, ctx),
        KeyBinding::new("cmd-shift-down", SelectDocEnd, ctx),
        KeyBinding::new("pageup", PageUp, ctx),
        KeyBinding::new("pagedown", PageDown, ctx),
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("shift-backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("alt-backspace", DeleteWordBack, ctx),
        KeyBinding::new("cmd-backspace", DeleteToLineStart, ctx),
        KeyBinding::new("enter", Newline, ctx),
        KeyBinding::new("tab", Tab, ctx),
        KeyBinding::new("cmd-a", SelectAll, ctx),
        KeyBinding::new("cmd-c", Copy, ctx),
        KeyBinding::new("cmd-x", Cut, ctx),
        KeyBinding::new("cmd-v", Paste, ctx),
        KeyBinding::new("cmd-z", Undo, ctx),
        KeyBinding::new("cmd-shift-z", Redo, ctx),
        KeyBinding::new("cmd-s", Save, ctx),
        KeyBinding::new("cmd-f", Find, ctx),
        KeyBinding::new("cmd-g", FindNext, ctx),
        KeyBinding::new("cmd-shift-g", FindPrev, ctx),
        KeyBinding::new("cmd-/", ToggleComment, ctx),
        KeyBinding::new("escape", Escape, ctx),
        KeyBinding::new("f12", GoToDefinition, ctx),
    ]);
}

pub enum EditorEvent {
    /// Dirty state or save result changed; tab strips should redraw.
    Changed,
    /// The text changed; `version` only ever grows.
    Edited {
        version: u64,
    },
    Saved,
    /// Zero-based line and UTF-16 column of the symbol to look up.
    GoToDefinition {
        line: u32,
        character: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MarkerSeverity {
    Error,
    Warning,
    Info,
}

/// A diagnostic to draw, positioned as language servers count (zero-based line, UTF-16 column).
#[derive(Clone, Debug)]
pub struct Marker {
    pub start: (u32, u32),
    pub end: (u32, u32),
    pub severity: MarkerSeverity,
    pub message: String,
}

/// What the element laid out last frame, kept for hit-testing and IME placement.
pub(crate) struct EditorLayout {
    pub origin: Point<Pixels>,
    pub text_left: Pixels,
    pub line_height: Pixels,
    pub lines: Vec<(usize, DisplayLine, ShapedLine)>,
}

struct FindBar {
    input: Entity<TextInput>,
    matches: Vec<Range<usize>>,
    current: usize,
    _subscription: Subscription,
}

pub struct EditorView {
    pub(crate) buffer: Option<Buffer>,
    error: Option<String>,
    path: PathBuf,
    pub(crate) focus: FocusHandle,
    pub(crate) scroll: Point<f32>,
    pub(crate) viewport: Size<Pixels>,
    pub(crate) layout: Option<EditorLayout>,
    pub(crate) autoscroll: bool,
    pub(crate) marked: Option<String>,
    find: Option<FindBar>,
    save_error: Option<String>,
    selecting: bool,
    was_dirty: bool,
    pub(crate) markers: Vec<Marker>,
}

impl EventEmitter<EditorEvent> for EditorView {}

impl Focusable for EditorView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EditorView {
    pub fn open(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let (buffer, error) = match Buffer::open(&path) {
            Ok(b) => (Some(b), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        Self {
            buffer,
            error,
            path,
            focus: cx.focus_handle(),
            scroll: Point::default(),
            viewport: Size::default(),
            layout: None,
            autoscroll: true,
            marked: None,
            find: None,
            save_error: None,
            selecting: false,
            was_dirty: false,
            markers: Vec::new(),
        }
    }

    pub fn lang(&self) -> Option<crate::Lang> {
        self.buffer.as_ref()?.lang()
    }

    pub fn text(&self) -> Option<String> {
        Some(self.buffer.as_ref()?.full_text())
    }

    pub fn version(&self) -> Option<u64> {
        Some(self.buffer.as_ref()?.version())
    }

    pub fn set_markers(&mut self, markers: Vec<Marker>, cx: &mut Context<Self>) {
        self.markers = markers;
        cx.notify();
    }

    /// Moves the cursor to a zero-based line and UTF-16 column.
    pub fn go_to_position(&mut self, line: u32, character: u32, cx: &mut Context<Self>) {
        self.with_buffer(cx, |b| {
            let at = b.char_at_utf16(line, character);
            b.move_to(at, false);
        });
    }

    fn markers_at_cursor(&self) -> Vec<&Marker> {
        let Some(b) = self.buffer.as_ref() else {
            return Vec::new();
        };
        let (line, _) = b.utf16_position(b.selection.head);
        let mut found: Vec<&Marker> = self
            .markers
            .iter()
            .filter(|m| m.start.0 <= line && line <= m.end.0)
            .collect();
        found.sort_by_key(|m| m.severity);
        found
    }

    fn definition_at(&self, char: usize, cx: &mut Context<Self>) {
        if let Some(b) = self.buffer.as_ref() {
            let (line, character) = b.utf16_position(char);
            cx.emit(EditorEvent::GoToDefinition { line, character });
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 1-based cursor line and column, and the selected text if any.
    pub fn cursor(&self) -> Option<(u32, u32, Option<String>)> {
        let b = self.buffer.as_ref()?;
        let head = b.selection.head;
        let line = b.line_of(head);
        let selection = (!b.selection.is_empty()).then(|| b.selected_text());
        Some((line as u32 + 1, b.column_of(head) as u32 + 1, selection))
    }

    /// Moves the cursor to the start of a 1-based line and scrolls it into view.
    pub fn go_to_line(&mut self, line: u32, cx: &mut Context<Self>) {
        self.with_buffer(cx, |b| {
            let line = (line.max(1) as usize - 1).min(b.len_lines().saturating_sub(1));
            let at = b.line_start(line);
            b.move_to(at, false);
        });
    }

    pub fn is_dirty(&self) -> bool {
        self.buffer.as_ref().is_some_and(Buffer::is_dirty)
    }

    pub fn save(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(buffer) = self.buffer.as_mut() else {
            return false;
        };
        let result = buffer.save();
        self.save_error = result.as_ref().err().map(|e| format!("{e:#}"));
        if result.is_ok() {
            cx.emit(EditorEvent::Saved);
        }
        self.changed(cx);
        result.is_ok()
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        let dirty = self.is_dirty();
        if dirty != self.was_dirty {
            self.was_dirty = dirty;
        }
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn with_buffer(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Buffer)) {
        let Some(buffer) = self.buffer.as_mut() else {
            return;
        };
        let before = buffer.version();
        f(buffer);
        let edited = buffer.version() != before;
        self.autoscroll = true;
        if edited {
            self.refresh_find(false, cx);
            let version = self.buffer.as_ref().map_or(0, Buffer::version);
            cx.emit(EditorEvent::Edited { version });
            self.changed(cx);
        } else {
            cx.notify();
        }
    }

    fn page_lines(&self) -> isize {
        let lh = self.layout.as_ref().map_or(px(20.), |l| l.line_height);
        ((self.viewport.height / lh) as isize - 2).max(1)
    }

    /// Buffer char under a window position, from last frame's layout.
    fn char_at_position(&self, position: Point<Pixels>) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        let buffer = self.buffer.as_ref()?;
        let y = position.y - layout.origin.y + px(self.scroll.y);
        let line = ((y / layout.line_height).floor().max(0.) as usize)
            .min(buffer.len_lines().saturating_sub(1));
        let x = position.x - layout.text_left + px(self.scroll.x);
        let col = match layout.lines.iter().find(|(l, _, _)| *l == line) {
            Some((_, display, shaped)) => display.char_for_byte(shaped.closest_index_for_x(x)),
            None => 0,
        };
        Some(buffer.char_at(line, col))
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let Some(at) = self.char_at_position(event.position) else {
            return;
        };
        let Some(buffer) = self.buffer.as_mut() else {
            return;
        };
        match event.click_count {
            2 => buffer.select_word_at(at),
            n if n >= 3 => buffer.select_line_at(at),
            _ => buffer.move_to(at, event.modifiers.shift),
        }
        if event.modifiers.platform && event.click_count == 1 {
            self.definition_at(at, cx);
            cx.notify();
            return;
        }
        self.selecting = true;
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            self.selecting = false;
            return;
        }
        if let Some(at) = self.char_at_position(event.position)
            && let Some(buffer) = self.buffer.as_mut()
        {
            buffer.move_to(at, true);
            self.autoscroll = true;
            cx.notify();
        }
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let lh = self.layout.as_ref().map_or(px(20.), |l| l.line_height);
        let delta = event.delta.pixel_delta(lh);
        let lines = self.buffer.as_ref().map_or(1, Buffer::len_lines) as f32;
        let max_y = ((lines - 1.) * f32::from(lh)).max(0.);
        self.scroll.y = (self.scroll.y - f32::from(delta.y)).clamp(0., max_y);
        self.scroll.x = (self.scroll.x - f32::from(delta.x)).max(0.);
        self.autoscroll = false;
        cx.notify();
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(buffer) = &self.buffer {
            let text = if buffer.selection.is_empty() {
                let line = buffer.line_of(buffer.selection.head);
                format!("{}\n", buffer.line(line))
            } else {
                buffer.selected_text()
            };
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let seed = self
            .buffer
            .as_ref()
            .map(Buffer::selected_text)
            .filter(|s| !s.is_empty() && !s.contains('\n'));
        let input = match &self.find {
            Some(find) => find.input.clone(),
            None => {
                let input = cx.new(|cx| TextInput::new("Find", cx));
                let subscription =
                    cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                        match event {
                            InputEvent::Changed => this.refresh_find(true, cx),
                            InputEvent::Submit | InputEvent::Down => this.step_find(1),
                            InputEvent::Up => this.step_find(-1),
                            InputEvent::Cancel => {
                                this.find = None;
                                window.focus(&this.focus);
                            }
                        }
                        cx.notify();
                    });
                self.find = Some(FindBar {
                    input: input.clone(),
                    matches: Vec::new(),
                    current: 0,
                    _subscription: subscription,
                });
                input
            }
        };
        if let Some(seed) = seed {
            input.update(cx, |i, cx| i.set_text(seed, cx));
        }
        window.focus(&input.focus_handle(cx));
        cx.notify();
    }

    /// Re-runs the search; `jump` moves the selection to the first match at or after the cursor.
    fn refresh_find(&mut self, jump: bool, cx: &App) {
        let Some(query) = self
            .find
            .as_ref()
            .map(|f| f.input.read(cx).text().to_string())
        else {
            return;
        };
        let (Some(find), Some(buffer)) = (self.find.as_mut(), self.buffer.as_mut()) else {
            return;
        };
        find.matches = buffer.find_all(&query);
        let head = buffer.selection.range().start;
        find.current = find
            .matches
            .iter()
            .position(|m| m.start >= head)
            .unwrap_or(0);
        if jump && let Some(m) = find.matches.get(find.current) {
            buffer.selection = crate::Selection {
                anchor: m.start,
                head: m.end,
            };
            self.autoscroll = true;
        }
    }

    fn step_find(&mut self, step: isize) {
        let (Some(find), Some(buffer)) = (self.find.as_mut(), self.buffer.as_mut()) else {
            return;
        };
        if find.matches.is_empty() {
            return;
        }
        let len = find.matches.len() as isize;
        find.current = (find.current as isize + step).rem_euclid(len) as usize;
        let m = &find.matches[find.current];
        buffer.selection = crate::Selection {
            anchor: m.start,
            head: m.end,
        };
        self.autoscroll = true;
    }

    pub(crate) fn find_matches(&self) -> &[Range<usize>] {
        self.find.as_ref().map_or(&[], |f| f.matches.as_slice())
    }

    fn render_find(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let find = self.find.as_ref()?;
        let t = cx.theme();
        let count = match find.matches.len() {
            0 => "No results".to_string(),
            n => format!("{} of {n}", find.current + 1),
        };
        Some(
            div()
                .flex_none()
                .h(px(36.))
                .px(px(12.))
                .flex()
                .items_center()
                .gap(px(12.))
                .bg(t.color.surface)
                .border_b_1()
                .border_color(t.color.border)
                .text_size(t.typography.caption)
                .child(
                    div()
                        .w(px(280.))
                        .h(px(24.))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .bg(t.color.surface_sunken)
                        .border_1()
                        .border_color(t.color.accent)
                        .rounded(t.shape.radius_control)
                        .child(find.input.clone()),
                )
                .child(div().text_color(t.color.content_muted).child(count)),
        )
    }
}

impl EditorView {
    /// The diagnostics on the cursor's line, worst first.
    fn render_marker_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let markers = self.markers_at_cursor();
        let worst = markers.first()?;
        let t = cx.theme();
        let color = marker_color(worst.severity, t);
        let first_line = worst.message.lines().next().unwrap_or_default().to_string();
        let more = (markers.len() > 1).then(|| format!("+{} more", markers.len() - 1));
        Some(
            div()
                .flex_none()
                .h(px(28.))
                .px(px(12.))
                .flex()
                .items_center()
                .gap(px(8.))
                .border_t_1()
                .border_color(t.color.border)
                .text_size(t.typography.caption)
                .child(div().size(px(6.)).flex_none().bg(color))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(t.color.content_secondary)
                        .child(first_line),
                )
                .children(more.map(|m| div().text_color(t.color.content_muted).child(m))),
        )
    }
}

pub(crate) fn marker_color(severity: MarkerSeverity, t: &athena_ui::Theme) -> gpui::Hsla {
    match severity {
        MarkerSeverity::Error => t.color.danger,
        MarkerSeverity::Warning => t.color.warning,
        MarkerSeverity::Info => t.color.content_disabled,
    }
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let focused = self.focus.is_focused(window);
        let root = div()
            .id("editor")
            .size_full()
            .flex()
            .flex_col()
            .bg(t.color.surface_sunken);
        if let Some(error) = &self.error {
            return root.items_center().justify_center().child(empty_state(
                "Can't open this file",
                error.clone(),
                None,
                cx,
            ));
        }
        root.children(self.render_find(cx))
            .child(
                div()
                    .id("editor-body")
                    .flex_1()
                    .min_h_0()
                    .track_focus(&self.focus)
                    .key_context("Editor")
                    .cursor_text()
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                    .on_action(cx.listener(|this, _: &GoToDefinition, _, cx| {
                        if let Some(head) = this.buffer.as_ref().map(|b| b.selection.head) {
                            this.definition_at(head, cx);
                        }
                    }))
                    .on_mouse_move(cx.listener(Self::mouse_move))
                    .on_scroll_wheel(cx.listener(Self::scroll_wheel))
                    .on_action(cx.listener(|this, _: &MoveLeft, _, cx| {
                        this.with_buffer(cx, |b| b.move_left(false))
                    }))
                    .on_action(cx.listener(|this, _: &MoveRight, _, cx| {
                        this.with_buffer(cx, |b| b.move_right(false))
                    }))
                    .on_action(cx.listener(|this, _: &MoveUp, _, cx| {
                        this.with_buffer(cx, |b| b.move_vertical(-1, false))
                    }))
                    .on_action(cx.listener(|this, _: &MoveDown, _, cx| {
                        this.with_buffer(cx, |b| b.move_vertical(1, false))
                    }))
                    .on_action(cx.listener(|this, _: &SelectLeft, _, cx| {
                        this.with_buffer(cx, |b| b.move_left(true))
                    }))
                    .on_action(cx.listener(|this, _: &SelectRight, _, cx| {
                        this.with_buffer(cx, |b| b.move_right(true))
                    }))
                    .on_action(cx.listener(|this, _: &SelectUp, _, cx| {
                        this.with_buffer(cx, |b| b.move_vertical(-1, true))
                    }))
                    .on_action(cx.listener(|this, _: &SelectDown, _, cx| {
                        this.with_buffer(cx, |b| b.move_vertical(1, true))
                    }))
                    .on_action(cx.listener(|this, _: &MoveWordLeft, _, cx| {
                        this.with_buffer(cx, |b| b.move_word(false, false))
                    }))
                    .on_action(cx.listener(|this, _: &MoveWordRight, _, cx| {
                        this.with_buffer(cx, |b| b.move_word(true, false))
                    }))
                    .on_action(cx.listener(|this, _: &SelectWordLeft, _, cx| {
                        this.with_buffer(cx, |b| b.move_word(false, true))
                    }))
                    .on_action(cx.listener(|this, _: &SelectWordRight, _, cx| {
                        this.with_buffer(cx, |b| b.move_word(true, true))
                    }))
                    .on_action(cx.listener(|this, _: &MoveLineStart, _, cx| {
                        this.with_buffer(cx, |b| b.move_line_start(false))
                    }))
                    .on_action(cx.listener(|this, _: &MoveLineEnd, _, cx| {
                        this.with_buffer(cx, |b| b.move_line_end(false))
                    }))
                    .on_action(cx.listener(|this, _: &SelectLineStart, _, cx| {
                        this.with_buffer(cx, |b| b.move_line_start(true))
                    }))
                    .on_action(cx.listener(|this, _: &SelectLineEnd, _, cx| {
                        this.with_buffer(cx, |b| b.move_line_end(true))
                    }))
                    .on_action(cx.listener(|this, _: &MoveDocStart, _, cx| {
                        this.with_buffer(cx, |b| b.move_to(0, false))
                    }))
                    .on_action(cx.listener(|this, _: &MoveDocEnd, _, cx| {
                        this.with_buffer(cx, |b| b.move_to(usize::MAX, false))
                    }))
                    .on_action(cx.listener(|this, _: &SelectDocStart, _, cx| {
                        this.with_buffer(cx, |b| b.move_to(0, true))
                    }))
                    .on_action(cx.listener(|this, _: &SelectDocEnd, _, cx| {
                        this.with_buffer(cx, |b| b.move_to(usize::MAX, true))
                    }))
                    .on_action(cx.listener(|this, _: &PageUp, _, cx| {
                        let n = this.page_lines();
                        this.with_buffer(cx, |b| b.move_vertical(-n, false))
                    }))
                    .on_action(cx.listener(|this, _: &PageDown, _, cx| {
                        let n = this.page_lines();
                        this.with_buffer(cx, |b| b.move_vertical(n, false))
                    }))
                    .on_action(cx.listener(|this, _: &Backspace, _, cx| {
                        this.with_buffer(cx, Buffer::backspace)
                    }))
                    .on_action(cx.listener(|this, _: &Delete, _, cx| {
                        this.with_buffer(cx, Buffer::delete_forward)
                    }))
                    .on_action(cx.listener(|this, _: &DeleteWordBack, _, cx| {
                        this.with_buffer(cx, Buffer::delete_word_back)
                    }))
                    .on_action(cx.listener(|this, _: &DeleteToLineStart, _, cx| {
                        this.with_buffer(cx, Buffer::delete_to_line_start)
                    }))
                    .on_action(
                        cx.listener(|this, _: &Newline, _, cx| {
                            this.with_buffer(cx, Buffer::newline)
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &Tab, _, cx| this.with_buffer(cx, Buffer::tab)),
                    )
                    .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                        this.with_buffer(cx, Buffer::select_all)
                    }))
                    .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy(cx)))
                    .on_action(cx.listener(|this, _: &Cut, _, cx| {
                        this.copy(cx);
                        this.with_buffer(cx, |b| {
                            if b.selection.is_empty() {
                                b.select_line_at(b.selection.head);
                            }
                            b.backspace();
                        })
                    }))
                    .on_action(cx.listener(|this, _: &Paste, _, cx| {
                        if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                            this.with_buffer(cx, |b| b.insert(&text));
                        }
                    }))
                    .on_action(cx.listener(|this, _: &Undo, _, cx| {
                        this.with_buffer(cx, |b| {
                            b.undo();
                        })
                    }))
                    .on_action(cx.listener(|this, _: &Redo, _, cx| {
                        this.with_buffer(cx, |b| {
                            b.redo();
                        })
                    }))
                    .on_action(cx.listener(|this, _: &Save, _, cx| {
                        this.save(cx);
                    }))
                    .on_action(cx.listener(|this, _: &Find, window, cx| this.open_find(window, cx)))
                    .on_action(cx.listener(|this, _: &FindNext, _, cx| {
                        this.step_find(1);
                        cx.notify();
                    }))
                    .on_action(cx.listener(|this, _: &FindPrev, _, cx| {
                        this.step_find(-1);
                        cx.notify();
                    }))
                    .on_action(cx.listener(|this, _: &ToggleComment, _, cx| {
                        this.with_buffer(cx, Buffer::toggle_comment)
                    }))
                    .on_action(cx.listener(|this, _: &Escape, _, cx| {
                        if this.find.take().is_some() {
                            cx.notify();
                        } else {
                            let head = this.buffer.as_ref().map(|b| b.selection.head);
                            if let Some(head) = head {
                                this.with_buffer(cx, |b| b.move_to(head, false));
                            }
                        }
                    }))
                    .child(EditorElement::new(cx.entity(), focused)),
            )
            .children(self.render_marker_bar(cx))
            .children(self.save_error.clone().map(|err| {
                div()
                    .flex_none()
                    .h(px(28.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .border_t_1()
                    .border_color(t.color.border)
                    .text_size(t.typography.caption)
                    .text_color(t.color.danger)
                    .child(format!("Not saved: {err}"))
            }))
    }
}

impl EntityInputHandler for EditorView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let marked = self.marked.clone().unwrap_or_default();
        let units: Vec<u16> = marked.encode_utf16().collect();
        let r = range.start.min(units.len())..range.end.min(units.len());
        *actual = Some(r.clone());
        String::from_utf16(&units[r]).ok()
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.marked.as_ref().map_or(0, |m| m.encode_utf16().count());
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|m| 0..m.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = None;
        if !text.is_empty() {
            let text = text.to_string();
            self.with_buffer(cx, |b| b.insert(&text));
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = (!text.is_empty()).then(|| text.to_string());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout.as_ref()?;
        let buffer = self.buffer.as_ref()?;
        let head = buffer.selection.head;
        let line = buffer.line_of(head);
        let (_, display, shaped) = layout.lines.iter().find(|(l, _, _)| *l == line)?;
        let col = buffer.column_of(head);
        let x =
            layout.text_left + shaped.x_for_index(display.char_to_byte[col]) - px(self.scroll.x);
        let y = layout.origin.y + layout.line_height * line as f32 - px(self.scroll.y);
        Some(Bounds::new(
            gpui::point(x, y),
            gpui::size(px(2.), layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}
