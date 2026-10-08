use std::cell::{Ref, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use athena_ui::motion::{self, Closing, Opening};
use athena_ui::{
    ActiveTheme, Button, ButtonKind, ContextMenu, InputEvent, MenuItem, TextInput, empty_state,
};
use gpui::{
    Animation, App, Bounds, ClipboardItem, Context, DismissEvent, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, IntoElement, KeyBinding, MouseButton, MouseDownEvent,
    MouseMoveEvent, Pixels, Point, Render, ScrollWheelEvent, ShapedLine, Size, Subscription, Task,
    UTF16Selection, Window, actions, div, prelude::*, px,
};

use crate::buffer::{Buffer, Cursor, Edit, SaveError, UNDO_GROUP};
use crate::completion::Completing;
use crate::display::{DisplayLine, DisplayMap, Fold};
use crate::element::EditorElement;
use crate::hover::Hovering;
use crate::line_jump::LineJump;
use crate::shared::{self, SharedBuffer};

actions!(
    editor,
    [
        GoToDefinition,
        FindReferences,
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
        FoldAtCursor,
        UnfoldAtCursor,
        FoldAll,
        UnfoldAll,
        GoToLine,
        ShowCompletions,
        ShowHover,
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
        KeyBinding::new("cmd-alt-g", GoToDefinition, ctx),
        KeyBinding::new("shift-f12", FindReferences, ctx),
        KeyBinding::new("cmd-alt-r", FindReferences, ctx),
        KeyBinding::new("cmd-k cmd-[", FoldAtCursor, ctx),
        KeyBinding::new("cmd-k cmd-]", UnfoldAtCursor, ctx),
        KeyBinding::new("cmd-k cmd-0", FoldAll, ctx),
        KeyBinding::new("cmd-k cmd-j", UnfoldAll, ctx),
        KeyBinding::new("ctrl-g", GoToLine, ctx),
        KeyBinding::new("cmd-l", GoToLine, ctx),
        KeyBinding::new("ctrl-space", ShowCompletions, ctx),
        KeyBinding::new("cmd-k cmd-i", ShowHover, ctx),
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
    /// Zero-based line and UTF-16 column of the symbol whose uses to list.
    FindReferences {
        line: u32,
        character: u32,
    },
    /// The cursor moved to another zero-based line, or the text of its line changed.
    CursorMoved {
        line: u32,
    },
    /// Documentation is wanted for the symbol at a zero-based line and UTF-16 column; answer
    /// with [`EditorView::show_hover`].
    Hover {
        request: u64,
        line: u32,
        character: u32,
    },
    /// Suggestions are wanted at a zero-based line and UTF-16 column; answer with
    /// [`EditorView::show_completions`]. `trigger` is the character typed that asked, if any.
    Complete {
        request: u64,
        line: u32,
        character: u32,
        trigger: Option<String>,
    },
}

/// A change bar in the gutter, in zero-based lines of the saved file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GutterMark {
    Added {
        start: usize,
        len: usize,
    },
    Modified {
        start: usize,
        len: usize,
    },
    /// Lines were removed just above `before`.
    Removed {
        before: usize,
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
    /// Left and right edge of the gutter column holding fold chevrons.
    pub fold_column: (Pixels, Pixels),
}

struct FindBar {
    input: Entity<TextInput>,
    matches: Vec<Range<usize>>,
    current: usize,
    _subscription: Subscription,
}

pub struct EditorView {
    /// Shared with every other tab on the same file; the cursor and folds stay per view.
    pub(crate) buffer: Option<Rc<SharedBuffer>>,
    pub(crate) cursor: Cursor,
    /// The buffer version this view's cursor and folds have followed up to.
    seen: u64,
    _buffer_watch: Option<Subscription>,
    error: Option<String>,
    path: PathBuf,
    pub(crate) focus: FocusHandle,
    pub(crate) scroll: Point<f32>,
    pub(crate) viewport: Size<Pixels>,
    pub(crate) layout: Option<EditorLayout>,
    pub(crate) autoscroll: bool,
    /// The next autoscroll puts the cursor's line mid-screen rather than just in view.
    pub(crate) center_cursor: bool,
    pub(crate) line_jump: Option<LineJump>,
    pub(crate) hovering: Hovering,
    pub(crate) completing: Completing,
    /// Set around an edit that typing made, which narrows the suggestion list instead of closing it.
    typing: bool,
    pub(crate) marked: Option<String>,
    find: Option<FindBar>,
    find_opening: Option<Opening>,
    /// A dismissed find bar, still drawn while it fades out.
    find_closing: Option<(FindBar, Closing)>,
    find_generation: u64,
    save_error: Option<String>,
    selecting: bool,
    was_dirty: bool,
    pub(crate) markers: Vec<Marker>,
    pub(crate) display: DisplayMap,
    /// Foldable regions by header line, valid for one buffer version.
    fold_cache: RefCell<(u64, HashMap<usize, Option<Fold>>)>,
    pub(crate) gutter_hover: bool,
    autosave: Option<Duration>,
    autosave_task: Option<Task<()>>,
    /// The file changed on disk while this buffer had unsaved edits; the bar asks what to keep.
    conflict: bool,
    pub(crate) gutter_marks: Vec<GutterMark>,
    /// A caption drawn after the cursor's line while the cursor stays on that zero-based line.
    pub(crate) blame: Option<(usize, String)>,
    cursor_line: usize,
    context_menu: Option<(Entity<ContextMenu>, Subscription)>,
}

impl EventEmitter<EditorEvent> for EditorView {}

impl Focusable for EditorView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EditorView {
    /// Shows `path`, sharing the buffer of any other tab already showing it.
    pub fn open(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let (buffer, error) = match shared::open(&path, cx) {
            Ok(b) => (Some(b), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        let seen = buffer.as_ref().map_or(0, |b| b.buffer.borrow().version());
        let watch = buffer.as_ref().map(|b| Self::watch(b, cx));
        Self {
            buffer,
            cursor: Cursor::default(),
            seen,
            _buffer_watch: watch,
            error,
            path,
            focus: cx.focus_handle(),
            scroll: Point::default(),
            viewport: Size::default(),
            layout: None,
            autoscroll: true,
            center_cursor: false,
            line_jump: None,
            hovering: Hovering::default(),
            completing: Completing::default(),
            typing: false,
            marked: None,
            find: None,
            find_opening: None,
            find_closing: None,
            find_generation: 0,
            save_error: None,
            selecting: false,
            was_dirty: false,
            markers: Vec::new(),
            display: DisplayMap::default(),
            fold_cache: RefCell::default(),
            gutter_hover: false,
            autosave: None,
            autosave_task: None,
            conflict: false,
            gutter_marks: Vec::new(),
            blame: None,
            cursor_line: 0,
            context_menu: None,
        }
    }

    fn watch(shared: &SharedBuffer, cx: &mut Context<Self>) -> Subscription {
        cx.observe(&shared.signal, |this, _, cx| this.buffer_changed(cx))
    }

    pub(crate) fn buf(&self) -> Option<Ref<'_, Buffer>> {
        self.buffer.as_ref().map(|b| b.buffer.borrow())
    }

    /// Another view edited or saved the shared buffer.
    fn buffer_changed(&mut self, cx: &mut Context<Self>) {
        if self.follow_edits() {
            self.refresh_find(false, cx);
            self.note_cursor_line(true, cx);
        }
        if self.conflict && self.buf().is_some_and(|b| !b.changed_on_disk()) {
            self.conflict = false;
        }
        if self.buf().is_some_and(|b| !b.is_dirty()) {
            self.save_error = None;
        }
        self.changed(cx);
    }

    /// Moves the cursor and folds past edits made since this view last looked; false if none.
    pub(crate) fn follow_edits(&mut self) -> bool {
        let Some(shared) = self.buffer.clone() else {
            return false;
        };
        let b = shared.buffer.borrow();
        if b.version() == self.seen {
            return false;
        }
        match b.edits_since(self.seen) {
            Some(edits) => {
                let edits: Vec<Edit> = edits.copied().collect();
                for e in &edits {
                    self.display
                        .apply_edit(e.line, e.lines_removed, e.lines_inserted);
                }
                self.cursor.follow(&edits, b.len_chars());
            }
            None => {
                self.display.clear();
                self.cursor.follow([], b.len_chars());
            }
        }
        self.seen = b.version();
        true
    }

    pub fn lang(&self) -> Option<crate::Lang> {
        self.buf()?.lang()
    }

    pub fn text(&self) -> Option<String> {
        Some(self.buf()?.full_text())
    }

    pub fn version(&self) -> Option<u64> {
        Some(self.buf()?.version())
    }

    pub fn set_markers(&mut self, markers: Vec<Marker>, cx: &mut Context<Self>) {
        self.markers = markers;
        cx.notify();
    }

    pub fn set_gutter_marks(&mut self, marks: Vec<GutterMark>, cx: &mut Context<Self>) {
        if self.gutter_marks != marks {
            self.gutter_marks = marks;
            cx.notify();
        }
    }

    pub fn set_blame(&mut self, blame: Option<(usize, String)>, cx: &mut Context<Self>) {
        if self.blame != blame {
            self.blame = blame;
            cx.notify();
        }
    }

    /// Zero-based line the cursor is on.
    pub fn cursor_line(&self) -> usize {
        self.cursor_line
    }

    fn note_cursor_line(&mut self, edited: bool, cx: &mut Context<Self>) {
        let Some(line) = self.buf().map(|b| b.line_of(self.cursor.head())) else {
            return;
        };
        if line != self.cursor_line || edited {
            self.cursor_line = line;
            cx.emit(EditorEvent::CursorMoved { line: line as u32 });
        }
    }

    /// Replaces the whole text as one undoable edit spanning only the part that differs.
    pub fn replace_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.with_buffer(cx, |b, c| replace_differing(b, c, text));
    }

    /// Moves the cursor to a zero-based line and UTF-16 column.
    pub fn go_to_position(&mut self, line: u32, character: u32, cx: &mut Context<Self>) {
        self.with_buffer(cx, |b, c| {
            let at = b.char_at_utf16(line, character);
            b.move_to(c, at, false);
        });
    }

    fn markers_at_cursor(&self) -> Vec<&Marker> {
        let Some((line, _)) = self.buf().map(|b| b.utf16_position(self.cursor.head())) else {
            return Vec::new();
        };
        let mut found: Vec<&Marker> = self
            .markers
            .iter()
            .filter(|m| m.start.0 <= line && line <= m.end.0)
            .collect();
        found.sort_by_key(|m| m.severity);
        found
    }

    fn definition_at(&self, char: usize, cx: &mut Context<Self>) {
        if let Some((line, character)) = self.buf().map(|b| b.utf16_position(char)) {
            cx.emit(EditorEvent::GoToDefinition { line, character });
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 1-based cursor line and column, and the selected text if any.
    pub fn cursor(&self) -> Option<(u32, u32, Option<String>)> {
        let b = self.buf()?;
        let head = self.cursor.head().min(b.len_chars());
        let line = b.line_of(head);
        let selection = (!self.cursor.selection.is_empty()).then(|| b.selected_text(&self.cursor));
        Some((line as u32 + 1, b.column_of(head) as u32 + 1, selection))
    }

    /// Zero-based cursor line and UTF-16 column, as [`Self::go_to_position`] takes them.
    pub fn cursor_utf16(&self) -> Option<(u32, u32)> {
        let b = self.buf()?;
        Some(b.utf16_position(self.cursor.head()))
    }

    /// Follows the file to `path` after it was renamed on disk, keeping any unsaved edits.
    /// The caller re-registers the file with its language server.
    pub fn set_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(shared) = self.buffer.clone() {
            let moved = shared.buffer.borrow().path.as_deref() != Some(path.as_path());
            if moved {
                shared.buffer.borrow_mut().set_path(path.clone());
                shared::register(&shared, &path, cx);
                shared.changed(cx);
            }
        }
        // The new name may be another language, which folds differently.
        self.display.clear();
        *self.fold_cache.borrow_mut() = Default::default();
        self.path = path;
        self.changed(cx);
    }

    /// Moves the cursor to the start of a 1-based line and scrolls it into view.
    pub fn go_to_line(&mut self, line: u32, cx: &mut Context<Self>) {
        self.with_buffer(cx, |b, c| {
            let line = (line.max(1) as usize - 1).min(b.len_lines().saturating_sub(1));
            let at = b.line_start(line);
            b.move_to(c, at, false);
        });
    }

    pub fn is_dirty(&self) -> bool {
        self.buf().is_some_and(|b| b.is_dirty())
    }

    pub fn save(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(shared) = self.buffer.clone() else {
            return false;
        };
        let checked = shared.buffer.borrow_mut().save_checked();
        let result = match checked {
            Err(SaveError::Conflict) => {
                self.conflict = true;
                cx.notify();
                return false;
            }
            Err(SaveError::Io(e)) => Err(e),
            Ok(()) => Ok(()),
        };
        self.save_error = result.as_ref().err().map(|e| format!("{e:#}"));
        if result.is_ok() {
            cx.emit(EditorEvent::Saved);
            shared.changed(cx);
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

    pub(crate) fn with_buffer(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Buffer, &mut Cursor),
    ) {
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        self.follow_edits();
        let mut cursor = self.cursor;
        let (before, version) = {
            let mut b = shared.buffer.borrow_mut();
            let before = b.version();
            f(&mut b, &mut cursor);
            (before, b.version())
        };
        let edited = version != before;
        // Folds follow this view's own edits too; the cursor is the one the edit left.
        self.follow_edits();
        self.cursor = cursor;
        self.reveal_selection();
        self.autoscroll = true;
        self.note_cursor_line(edited, cx);
        self.hide_hover(cx);
        if !std::mem::take(&mut self.typing) {
            self.dismiss_completion(cx);
        }
        if edited {
            self.refresh_find(false, cx);
            cx.emit(EditorEvent::Edited { version });
            shared.changed(cx);
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
        let buffer = self.buf()?;
        let y = position.y - layout.origin.y + px(self.scroll.y);
        let rows = self.display.row_count(buffer.len_lines());
        let row = ((y / layout.line_height).floor().max(0.) as usize).min(rows.saturating_sub(1));
        let line = self.display.line_of(row);
        let x = position.x - layout.text_left + px(self.scroll.x);
        let col = match layout.lines.iter().find(|(l, _, _)| *l == line) {
            Some((_, display, shaped)) => display.char_for_byte(shaped.closest_index_for_x(x)),
            None => 0,
        };
        Some(buffer.char_at(line, col))
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        self.hide_hover(cx);
        self.dismiss_completion(cx);
        if self.click_fold_column(event.position, cx) {
            return;
        }
        let Some(at) = self.char_at_position(event.position) else {
            return;
        };
        self.follow_edits();
        let Some(buffer) = self.buffer.clone() else {
            return;
        };
        let b = buffer.buffer.borrow();
        match event.click_count {
            2 => b.select_word_at(&mut self.cursor, at),
            n if n >= 3 => b.select_line_at(&mut self.cursor, at),
            _ => b.move_to(&mut self.cursor, at, event.modifiers.shift),
        }
        drop(b);
        self.note_cursor_line(false, cx);
        if event.modifiers.platform && event.click_count == 1 {
            self.definition_at(at, cx);
            cx.notify();
            return;
        }
        self.selecting = true;
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let hover = self
            .layout
            .as_ref()
            .is_some_and(|l| event.position.x < l.fold_column.1);
        if hover != self.gutter_hover {
            self.gutter_hover = hover;
            cx.notify();
        }
        if event.pressed_button.is_none() {
            self.hover_pointer(event.position, cx);
        }
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            self.selecting = false;
            return;
        }
        if let Some(at) = self.char_at_position(event.position)
            && let Some(buffer) = self.buffer.clone()
        {
            buffer.buffer.borrow().move_to(&mut self.cursor, at, true);
            self.autoscroll = true;
            self.note_cursor_line(false, cx);
            cx.notify();
        }
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let lh = self.layout.as_ref().map_or(px(20.), |l| l.line_height);
        let delta = event.delta.pixel_delta(lh);
        let lines = self
            .buf()
            .map_or(1, |b| self.display.row_count(b.len_lines())) as f32;
        let max_y = ((lines - 1.) * f32::from(lh)).max(0.);
        self.scroll.y = (self.scroll.y - f32::from(delta.y)).clamp(0., max_y);
        self.scroll.x = (self.scroll.x - f32::from(delta.x)).max(0.);
        self.autoscroll = false;
        self.hide_hover(cx);
        self.dismiss_completion(cx);
        cx.notify();
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        let text = self.buf().map(|buffer| {
            if self.cursor.selection.is_empty() {
                let line = buffer.line_of(self.cursor.head());
                format!("{}\n", buffer.line(line))
            } else {
                buffer.selected_text(&self.cursor)
            }
        });
        if let Some(text) = text {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let seed = self
            .buf()
            .map(|b| b.selected_text(&self.cursor))
            .filter(|s| !s.is_empty() && !s.contains('\n'));
        let input = match &self.find {
            Some(find) => find.input.clone(),
            None => {
                let input = cx.new(|cx| TextInput::new("Find", cx));
                let subscription =
                    cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                        match event {
                            InputEvent::Changed => this.refresh_find(true, cx),
                            InputEvent::Submit | InputEvent::SubmitBeside | InputEvent::Down => {
                                this.step_find(1)
                            }
                            InputEvent::Up => this.step_find(-1),
                            InputEvent::Cancel => {
                                this.close_find(cx);
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
                self.find_closing = None;
                self.find_opening = Some(Opening::now());
                input
            }
        };
        if let Some(seed) = seed {
            input.update(cx, |i, cx| i.set_text(seed, cx));
        }
        window.focus(&input.focus_handle(cx));
        cx.notify();
    }

    /// Starts the find bar's fade-out (its matches stop highlighting at once); false if it was closed.
    fn close_find(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(find) = self.find.take() else {
            return false;
        };
        self.find_generation += 1;
        let generation = self.find_generation;
        self.find_closing = Some((find, Closing::new(generation)));
        let t = cx.theme();
        let delay = motion::exit_delay(t.motion.reduced, t.motion.fast);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .find_closing
                    .as_ref()
                    .is_some_and(|(_, c)| c.generation == generation)
                {
                    this.find_closing = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
        true
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
        let (Some(find), Some(buffer)) = (self.find.as_mut(), self.buffer.as_ref()) else {
            return;
        };
        find.matches = buffer.buffer.borrow().find_all(&query);
        let head = self.cursor.selection.range().start;
        find.current = find
            .matches
            .iter()
            .position(|m| m.start >= head)
            .unwrap_or(0);
        if jump && let Some(m) = find.matches.get(find.current) {
            self.cursor.selection = crate::Selection {
                anchor: m.start,
                head: m.end,
            };
            self.autoscroll = true;
            self.reveal_selection();
        }
    }

    fn step_find(&mut self, step: isize) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        if find.matches.is_empty() {
            return;
        }
        let len = find.matches.len() as isize;
        find.current = (find.current as isize + step).rem_euclid(len) as usize;
        let m = &find.matches[find.current];
        self.cursor.selection = crate::Selection {
            anchor: m.start,
            head: m.end,
        };
        self.autoscroll = true;
        self.reveal_selection();
    }

    pub(crate) fn find_matches(&self) -> &[Range<usize>] {
        self.find.as_ref().map_or(&[], |f| f.matches.as_slice())
    }

    fn render_find(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (find, closing) = match (&self.find, &self.find_closing) {
            (Some(find), _) => (find, None),
            (None, Some((find, closing))) => (find, Some(*closing)),
            (None, None) => return None,
        };
        let t = cx.theme();
        let count = match find.matches.len() {
            0 => "No results".to_string(),
            n => format!("{} of {n}", find.current + 1),
        };
        let row = div()
            .size_full()
            .flex()
            .items_center()
            .gap(px(12.))
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
            .child(div().text_color(t.color.content_muted).child(count));
        // The bar's height snaps so the text below re-lays out once; only the row inside moves.
        let row = match closing {
            Some(closing) => motion::animate_exit(
                t.motion.reduced,
                row,
                ("find-close", closing.generation),
                t.motion.fast,
                |el, d| el.opacity(1. - d).top(px(-8. * d)),
            ),
            None => motion::animate_enter(
                t.motion.reduced,
                self.find_opening.is_some_and(|o| o.running(t.motion.fast)),
                row,
                "find-open",
                Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
                |el, d| el.opacity(d).top(px(-8. * (1. - d))),
            ),
        };
        Some(
            div()
                .flex_none()
                .h(px(36.))
                .px(px(12.))
                .overflow_hidden()
                .bg(t.color.surface)
                .border_b_1()
                .border_color(t.color.border)
                .text_size(t.typography.caption)
                .child(row),
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

/// Replaces `b`'s text with `text` through one edit covering only the changed span, keeping the selection.
fn replace_differing(b: &mut Buffer, c: &mut Cursor, text: &str) {
    let old: Vec<char> = b.rope().chars().collect();
    let new: Vec<char> = text.chars().collect();
    if old == new {
        return;
    }
    let prefix = old.iter().zip(&new).take_while(|(a, c)| a == c).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, c)| a == c)
        .count();
    let (old_end, new_end) = (old.len() - suffix, new.len() - suffix);
    let map = |at: usize| match at {
        at if at <= prefix => at,
        at if at >= old_end => at - old_end + new_end,
        // Inside the changed span, keep the offset; replacements rarely change lengths much.
        at => at.min(new_end),
    };
    let selection = c.selection;
    b.replace_range(
        c,
        prefix..old_end,
        &new[prefix..new_end].iter().collect::<String>(),
    );
    c.selection = crate::Selection {
        anchor: map(selection.anchor),
        head: map(selection.head),
    };
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
            .relative()
            .bg(t.color.surface_sunken);
        if let Some(error) = &self.error {
            return root.items_center().justify_center().child(empty_state(
                "Can't open this file",
                error.clone(),
                None,
                cx,
            ));
        }
        root.children(self.render_conflict(cx))
            .children(self.render_find(cx))
            .child(
                div()
                    .id("editor-body")
                    .flex_1()
                    .min_h_0()
                    .track_focus(&self.focus)
                    .key_context("Editor")
                    .cursor_text()
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::open_context_menu))
                    .on_action(cx.listener(|this, _: &GoToDefinition, _, cx| {
                        this.definition_at(this.cursor.head(), cx);
                    }))
                    .on_action(cx.listener(|this, _: &FindReferences, _, cx| {
                        let head = this.cursor.head();
                        if let Some((line, character)) = this.buf().map(|b| b.utf16_position(head))
                        {
                            cx.emit(EditorEvent::FindReferences { line, character });
                        }
                    }))
                    .on_mouse_move(cx.listener(Self::mouse_move))
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if !hovered && this.gutter_hover {
                            this.gutter_hover = false;
                            cx.notify();
                        }
                        if !hovered {
                            this.hover_left(cx);
                        }
                    }))
                    .on_scroll_wheel(cx.listener(Self::scroll_wheel))
                    .on_action(cx.listener(|this, _: &MoveLeft, _, cx| {
                        this.with_buffer(cx, |b, c| b.move_left(c, false))
                    }))
                    .on_action(cx.listener(|this, _: &MoveRight, _, cx| {
                        this.with_buffer(cx, |b, c| b.move_right(c, false))
                    }))
                    .on_action(cx.listener(|this, _: &MoveUp, _, cx| {
                        if this.completion_open() {
                            return this.step_completion(-1, cx);
                        }
                        this.move_rows(-1, false, cx)
                    }))
                    .on_action(cx.listener(|this, _: &MoveDown, _, cx| {
                        if this.completion_open() {
                            return this.step_completion(1, cx);
                        }
                        this.move_rows(1, false, cx)
                    }))
                    .on_action(cx.listener(|this, _: &SelectLeft, _, cx| {
                        this.with_buffer(cx, |b, c| b.move_left(c, true))
                    }))
                    .on_action(cx.listener(|this, _: &SelectRight, _, cx| {
                        this.with_buffer(cx, |b, c| b.move_right(c, true))
                    }))
                    .on_action(
                        cx.listener(|this, _: &SelectUp, _, cx| this.move_rows(-1, true, cx)),
                    )
                    .on_action(
                        cx.listener(|this, _: &SelectDown, _, cx| this.move_rows(1, true, cx)),
                    )
                    .on_action(cx.listener(|this, _: &MoveWordLeft, _, cx| {
                        this.with_buffer(cx, |b, c| b.move_word(c, false, false))
                    }))
                    .on_action(cx.listener(|this, _: &MoveWordRight, _, cx| {
                        this.with_buffer(cx, |b, c| b.move_word(c, true, false))
                    }))
                    .on_action(cx.listener(|this, _: &SelectWordLeft, _, cx| {
                        this.with_buffer(cx, |b, c| b.move_word(c, false, true))
                    }))
                    .on_action(cx.listener(|this, _: &SelectWordRight, _, cx| {
                        this.with_buffer(cx, |b, c| b.move_word(c, true, true))
                    }))
                    .on_action(cx.listener(|this, _: &MoveLineStart, _, cx| {
                        this.with_buffer(cx, |b, c| b.move_line_start(c, false))
                    }))
                    .on_action(cx.listener(|this, _: &MoveLineEnd, _, cx| {
                        this.with_buffer(cx, |b, c| b.move_line_end(c, false))
                    }))
                    .on_action(cx.listener(|this, _: &SelectLineStart, _, cx| {
                        this.with_buffer(cx, |b, c| b.move_line_start(c, true))
                    }))
                    .on_action(cx.listener(|this, _: &SelectLineEnd, _, cx| {
                        this.with_buffer(cx, |b, c| b.move_line_end(c, true))
                    }))
                    .on_action(cx.listener(|this, _: &MoveDocStart, _, cx| {
                        this.with_buffer(cx, |b, c| b.move_to(c, 0, false))
                    }))
                    .on_action(cx.listener(|this, _: &MoveDocEnd, _, cx| {
                        this.with_buffer(cx, |b, c| b.move_to(c, usize::MAX, false))
                    }))
                    .on_action(cx.listener(|this, _: &SelectDocStart, _, cx| {
                        this.with_buffer(cx, |b, c| b.move_to(c, 0, true))
                    }))
                    .on_action(cx.listener(|this, _: &SelectDocEnd, _, cx| {
                        this.with_buffer(cx, |b, c| b.move_to(c, usize::MAX, true))
                    }))
                    .on_action(cx.listener(|this, _: &PageUp, _, cx| {
                        let n = this.page_lines();
                        this.move_rows(-n, false, cx)
                    }))
                    .on_action(cx.listener(|this, _: &PageDown, _, cx| {
                        let n = this.page_lines();
                        this.move_rows(n, false, cx)
                    }))
                    .on_action(cx.listener(|this, _: &Backspace, _, cx| {
                        let open = this.completion_open();
                        this.typing = open;
                        this.with_buffer(cx, |b, c| b.backspace(c));
                        if open {
                            this.refilter_completion(cx);
                        }
                    }))
                    .on_action(cx.listener(|this, _: &Delete, _, cx| {
                        this.with_buffer(cx, |b, c| b.delete_forward(c))
                    }))
                    .on_action(cx.listener(|this, _: &DeleteWordBack, _, cx| {
                        this.with_buffer(cx, |b, c| b.delete_word_back(c))
                    }))
                    .on_action(cx.listener(|this, _: &DeleteToLineStart, _, cx| {
                        this.with_buffer(cx, |b, c| b.delete_to_line_start(c))
                    }))
                    .on_action(cx.listener(|this, _: &Newline, _, cx| {
                        if this.completion_open() {
                            return this.accept_completion(None, cx);
                        }
                        this.with_buffer(cx, |b, c| b.newline(c))
                    }))
                    .on_action(cx.listener(|this, _: &Tab, _, cx| {
                        if this.completion_open() {
                            return this.accept_completion(None, cx);
                        }
                        this.with_buffer(cx, |b, c| b.tab(c))
                    }))
                    .on_action(
                        cx.listener(|this, _: &ShowCompletions, _, cx| this.complete_now(cx)),
                    )
                    .on_action(cx.listener(|this, _: &ShowHover, _, cx| this.hover_at_cursor(cx)))
                    .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                        this.with_buffer(cx, |b, c| b.select_all(c))
                    }))
                    .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy(cx)))
                    .on_action(cx.listener(|this, _: &Cut, _, cx| {
                        this.copy(cx);
                        this.with_buffer(cx, |b, c| {
                            if c.selection.is_empty() {
                                b.select_line_at(c, c.head());
                            }
                            b.backspace(c);
                        })
                    }))
                    .on_action(cx.listener(|this, _: &Paste, _, cx| {
                        if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                            this.with_buffer(cx, |b, c| b.insert(c, &text));
                        }
                    }))
                    .on_action(cx.listener(|this, _: &Undo, _, cx| {
                        this.with_buffer(cx, |b, c| {
                            b.undo(c);
                        })
                    }))
                    .on_action(cx.listener(|this, _: &Redo, _, cx| {
                        this.with_buffer(cx, |b, c| {
                            b.redo(c);
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
                        this.with_buffer(cx, |b, c| b.toggle_comment(c))
                    }))
                    .on_action(cx.listener(|this, _: &Escape, _, cx| {
                        if this.dismiss_completion(cx) || this.hide_hover(cx) {
                            return;
                        }
                        if !this.close_find(cx) {
                            let head = this.cursor.head();
                            this.with_buffer(cx, |b, c| b.move_to(c, head, false));
                        }
                    }))
                    .on_action(cx.listener(|this, _: &FoldAtCursor, _, cx| this.fold_at_cursor(cx)))
                    .on_action(
                        cx.listener(|this, _: &UnfoldAtCursor, _, cx| this.unfold_at_cursor(cx)),
                    )
                    .on_action(cx.listener(|this, _: &FoldAll, _, cx| this.fold_all(cx)))
                    .on_action(cx.listener(|this, _: &UnfoldAll, _, cx| {
                        this.display.clear();
                        cx.notify();
                    }))
                    .on_action(
                        cx.listener(|this, _: &GoToLine, window, cx| {
                            this.open_line_jump(window, cx)
                        }),
                    )
                    .child(EditorElement::new(cx.entity(), focused)),
            )
            .children(self.render_line_jump(cx))
            .children(self.render_hover(cx))
            .children(focused.then(|| self.render_completion(cx)).flatten())
            .children(self.render_marker_bar(cx))
            .children(self.context_menu.as_ref().map(|(menu, _)| menu.clone()))
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

impl EditorView {
    /// The region `line` can fold, cached until the text changes.
    pub(crate) fn fold_at(&self, line: usize) -> Option<Fold> {
        let buffer = self.buf()?;
        let mut cache = self.fold_cache.borrow_mut();
        if cache.0 != buffer.version() {
            *cache = (buffer.version(), HashMap::new());
        }
        *cache.1.entry(line).or_insert_with(|| buffer.fold_at(line))
    }

    /// Unfolds whatever hides the selection's ends, as a cursor never sits inside a fold.
    fn reveal_selection(&mut self) {
        if self.display.is_empty() {
            return;
        }
        let selection = self.cursor.selection;
        let Some((anchor, head)) = self
            .buf()
            .map(|b| (b.line_of(selection.anchor), b.line_of(selection.head)))
        else {
            return;
        };
        self.display.reveal(anchor);
        self.display.reveal(head);
    }

    /// Vertical moves count visual rows, so folded blocks are stepped over.
    fn move_rows(&mut self, rows: isize, extend: bool, cx: &mut Context<Self>) {
        let Some((line, lines)) = self
            .buf()
            .map(|b| (b.line_of(self.cursor.head()), b.len_lines()))
        else {
            return;
        };
        let row = self.display.row_of(line) as isize + rows;
        let count = self.display.row_count(lines) as isize;
        let target = (0..count)
            .contains(&row)
            .then(|| self.display.line_of(row as usize));
        let len = lines as isize;
        self.with_buffer(cx, |b, c| match target {
            Some(line) => b.move_to_line(c, line, extend),
            None => b.move_vertical(c, if row < 0 { -len } else { len }, extend),
        });
    }

    fn click_fold_column(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(layout) = self.layout.as_ref() else {
            return false;
        };
        if position.x < layout.fold_column.0 || position.x >= layout.fold_column.1 {
            return false;
        }
        let Some(line) = self
            .char_at_position(position)
            .and_then(|at| Some(self.buf()?.line_of(at)))
        else {
            return false;
        };
        if !self.display.unfold_at(line) {
            match self.fold_at(line) {
                Some(fold) => self.display.fold(fold),
                None => return false,
            }
        }
        self.cursor_out_of_folds();
        cx.notify();
        true
    }

    /// Moves a cursor that a new fold swallowed up to that fold's header.
    fn cursor_out_of_folds(&mut self) {
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        let b = shared.buffer.borrow();
        let line = b.line_of(self.cursor.head());
        if let Some(fold) = self.display.fold_containing(line) {
            let col = b.column_of(self.cursor.head());
            b.move_to(&mut self.cursor, b.char_at(fold.header(), col), false);
        }
    }

    fn fold_at_cursor(&mut self, cx: &mut Context<Self>) {
        let Some(line) = self.buf().map(|b| b.line_of(self.cursor.head())) else {
            return;
        };
        // The innermost open region around the cursor, as VS Code picks it.
        let found = (line.saturating_sub(2000)..=line).rev().find_map(|l| {
            let fold = self.fold_at(l)?;
            (self.display.folded_at(l).is_none() && (l == line || fold.end >= line)).then_some(fold)
        });
        if let Some(fold) = found {
            self.display.fold(fold);
            self.cursor_out_of_folds();
            self.autoscroll = true;
            cx.notify();
        }
    }

    fn unfold_at_cursor(&mut self, cx: &mut Context<Self>) {
        if let Some(line) = self.buf().map(|b| b.line_of(self.cursor.head()))
            && self.display.unfold_at(line)
        {
            cx.notify();
        }
    }

    fn fold_all(&mut self, cx: &mut Context<Self>) {
        let Some(lines) = self.buf().map(|b| b.len_lines()) else {
            return;
        };
        self.display.clear();
        let mut line = 0;
        while line < lines {
            match self.fold_at(line) {
                Some(fold) => {
                    self.display.fold(fold);
                    line = fold.end + 1;
                }
                None => line += 1,
            }
        }
        self.cursor_out_of_folds();
        self.autoscroll = true;
        cx.notify();
    }
}

impl EditorView {
    /// Saves this long after the last edit (never sooner than an undo step closes); `None` is off.
    pub fn set_autosave(&mut self, delay: Option<Duration>, cx: &mut Context<Self>) {
        self.autosave = delay.map(|d| d.max(UNDO_GROUP));
        if self.autosave.is_some() {
            self.schedule_autosave(cx);
        } else {
            self.autosave_task = None;
        }
    }

    /// Restarts the auto-save countdown; call on every edit.
    pub fn schedule_autosave(&mut self, cx: &mut Context<Self>) {
        let Some(delay) = self.autosave else { return };
        self.autosave_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| this.autosave_now(cx)).ok();
        }));
    }

    fn autosave_now(&mut self, cx: &mut Context<Self>) {
        self.autosave_task = None;
        // A failed save or a conflict waits for the user rather than retrying on every keystroke.
        if self.is_dirty() && self.error.is_none() && self.save_error.is_none() && !self.conflict {
            self.save(cx);
        }
    }

    /// Picks up a change made on disk by another program: a clean buffer reloads, a dirty one asks.
    pub fn check_disk(&mut self, cx: &mut Context<Self>) {
        let Some((changed, dirty)) = self.buf().map(|b| (b.changed_on_disk(), b.is_dirty())) else {
            return;
        };
        if !changed {
            return;
        }
        if dirty {
            self.conflict = true;
            cx.notify();
        } else {
            self.reload(cx);
        }
    }

    /// Replaces the buffer with the file on disk, as one step that undo can take back.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let mut result = Ok(());
        self.with_buffer(cx, |b, c| result = b.reload_from_disk(c));
        self.conflict &= result.is_err();
        self.save_error = result.err().map(|e| format!("{e:#}"));
        if let Some(shared) = &self.buffer {
            shared.changed(cx);
        }
        self.changed(cx);
    }

    /// Keeps this buffer's text, replacing what another program wrote.
    fn overwrite(&mut self, cx: &mut Context<Self>) {
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        let result = shared.buffer.borrow_mut().save();
        self.conflict = false;
        self.save_error = result.as_ref().err().map(|e| format!("{e:#}"));
        if result.is_ok() {
            cx.emit(EditorEvent::Saved);
            shared.changed(cx);
        }
        self.changed(cx);
    }

    /// Writes the buffer to `path` and keeps editing it there.
    /// Writes the buffer to `path` and keeps editing it there; other tabs stay on the old file.
    pub fn save_as(&mut self, path: PathBuf, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let shared = self
            .buffer
            .clone()
            .ok_or_else(|| anyhow::anyhow!("nothing to save"))?;
        // Two strong counts are this view's and the clone above; more means another tab shares it.
        if Rc::strong_count(&shared) > 2 {
            let mut fork = {
                let b = shared.buffer.borrow();
                let mut fork = Buffer::new(&b.full_text(), Some(path.clone()));
                fork.indent = b.indent;
                fork
            };
            fork.save()?;
            let fork = shared::adopt(fork, &path, cx);
            self.seen = fork.buffer.borrow().version();
            self._buffer_watch = Some(Self::watch(&fork, cx));
            self.cursor.follow([], fork.buffer.borrow().len_chars());
            self.buffer = Some(fork);
        } else {
            shared.buffer.borrow_mut().save_as(path.clone())?;
            shared::register(&shared, &path, cx);
        }
        // A new language folds differently, and the fold cache is keyed only on the text version.
        self.display.clear();
        *self.fold_cache.borrow_mut() = Default::default();
        self.path = path;
        self.conflict = false;
        self.save_error = None;
        cx.emit(EditorEvent::Saved);
        self.changed(cx);
        Ok(())
    }

    fn render_conflict(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.conflict {
            return None;
        }
        let t = cx.theme();
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Some(
            div()
                .flex_none()
                .h(px(36.))
                .px(px(12.))
                .flex()
                .items_center()
                .gap(px(8.))
                .bg(t.color.surface)
                .border_b_1()
                .border_color(t.color.border)
                .text_size(t.typography.caption)
                .child(div().size(px(6.)).flex_none().bg(t.color.warning))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(t.color.content_secondary)
                        .child(format!("{name} changed on disk while you were editing it.")),
                )
                .child(
                    Button::new("conflict-reload", "Reload", ButtonKind::Ghost)
                        .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                )
                .child(
                    Button::new("conflict-overwrite", "Overwrite", ButtonKind::Secondary)
                        .on_click(cx.listener(|this, _, _, cx| this.overwrite(cx))),
                ),
        )
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
            self.typing = true;
            self.with_buffer(cx, |b, c| b.insert(c, &text));
            self.completion_after_typing(&text, cx);
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
        let buffer = self.buf()?;
        let head = self.cursor.head();
        let line = buffer.line_of(head);
        let (_, display, shaped) = layout.lines.iter().find(|(l, _, _)| *l == line)?;
        // Another tab's edit can leave last frame's line shorter than the text is now.
        let byte = display
            .char_to_byte
            .get(buffer.column_of(head))
            .or(display.char_to_byte.last())?;
        let x = layout.text_left + shaped.x_for_index(*byte) - px(self.scroll.x);
        let row = self.display.row_of(line);
        let y = layout.origin.y + layout.line_height * row as f32 - px(self.scroll.y);
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

impl EditorView {
    fn open_context_menu(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus);
        self.hide_hover(cx);
        self.dismiss_completion(cx);
        // As in VS Code, a right-click outside the selection moves the cursor there first.
        if let Some(at) = self.char_at_position(event.position)
            && let Some(buffer) = self.buffer.clone()
            && !self.cursor.selection.range().contains(&at)
        {
            buffer.buffer.borrow().move_to(&mut self.cursor, at, false);
            self.note_cursor_line(false, cx);
            cx.notify();
        }
        let item = |label: &'static str, hint: &'static str, action: Box<dyn gpui::Action>| {
            MenuItem::new(label, move |window, cx| {
                window.dispatch_action(action.boxed_clone(), cx)
            })
            .hint(hint)
        };
        let items = vec![
            item("Go to Definition", "F12", Box::new(GoToDefinition)),
            item("Find References", "⇧F12", Box::new(FindReferences)),
            MenuItem::separator(),
            item("Cut", "⌘X", Box::new(Cut)),
            item("Copy", "⌘C", Box::new(Copy)),
            item("Paste", "⌘V", Box::new(Paste)),
            MenuItem::separator(),
            item("Toggle Line Comment", "⌘/", Box::new(ToggleComment)),
        ];
        let menu = ContextMenu::build(event.position, items, window, cx);
        let subscription = cx.subscribe_in(&menu, window, |this, menu, _: &DismissEvent, _, cx| {
            if this.context_menu.as_ref().is_some_and(|(m, _)| m == menu) {
                this.context_menu = None;
                cx.notify();
            }
        });
        self.context_menu = Some((menu, subscription));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_text_is_one_undo_step_that_keeps_the_cursor() {
        let mut b = Buffer::new("let old = 1;\nkeep\nold();\n", None);
        let mut c = Cursor::at(b.line_start(1) + 2);
        replace_differing(&mut b, &mut c, "let new = 1;\nkeep\nnew();\n");
        assert_eq!(b.full_text(), "let new = 1;\nkeep\nnew();\n");
        assert_eq!(c.head(), b.line_start(1) + 2);
        b.undo(&mut c);
        assert_eq!(b.full_text(), "let old = 1;\nkeep\nold();\n");
        replace_differing(&mut b, &mut c, "let old = 1;\nkeep\nold();\n");
        assert!(!b.undo(&mut c), "an unchanged text adds no undo step");
    }
}
