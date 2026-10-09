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

use crate::buffer::{
    Buffer, Cursor, Cursors, DiskState, DiskText, Edit, SaveError, Selection, UNDO_GROUP,
    read_disk_text,
};
use crate::completion::Completing;
use crate::display::{DisplayLine, DisplayMap, Fold};
use crate::element::EditorElement;
use crate::find::{self, FindOptions};
use crate::hover::Hovering;
use crate::line_jump::LineJump;
use crate::shared::{self, SharedBuffer};
use crate::signature::Signing;

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
        IndentLines,
        OutdentLines,
        MoveLinesUp,
        MoveLinesDown,
        CopyLinesUp,
        CopyLinesDown,
        DeleteLines,
        InsertLineBelow,
        InsertLineAbove,
        FindReplace,
        AddNextOccurrence,
        SkipOccurrence,
        SelectAllOccurrences,
        AddCursorAbove,
        AddCursorBelow,
        ToggleWordWrap,
        ToggleMatchCase,
        ToggleWholeWord,
        ToggleRegex,
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
        KeyBinding::new("shift-tab", OutdentLines, ctx),
        KeyBinding::new("cmd-]", IndentLines, ctx),
        KeyBinding::new("cmd-[", OutdentLines, ctx),
        KeyBinding::new("alt-up", MoveLinesUp, ctx),
        KeyBinding::new("alt-down", MoveLinesDown, ctx),
        KeyBinding::new("alt-shift-up", CopyLinesUp, ctx),
        KeyBinding::new("alt-shift-down", CopyLinesDown, ctx),
        KeyBinding::new("cmd-shift-k", DeleteLines, ctx),
        KeyBinding::new("cmd-enter", InsertLineBelow, ctx),
        // VS Code's cmd-shift-enter (Insert Line Above) stays Athena's Zoom Pane.
        KeyBinding::new("cmd-alt-f", FindReplace, ctx),
        // These shadow the shell's Split Right and Focus Pane Up/Down while an editor has focus,
        // as VS Code binds them; pane focus still moves left and right from an editor.
        KeyBinding::new("cmd-d", AddNextOccurrence, ctx),
        KeyBinding::new("cmd-alt-up", AddCursorAbove, ctx),
        KeyBinding::new("cmd-alt-down", AddCursorBelow, ctx),
        KeyBinding::new("cmd-k cmd-d", SkipOccurrence, ctx),
        KeyBinding::new("cmd-shift-l", SelectAllOccurrences, ctx),
        KeyBinding::new("alt-z", ToggleWordWrap, ctx),
    ]);
    // The shell's Search tab names its field row ProjectSearch to share these.
    for bar in [Some("FindBar"), Some("ProjectSearch")] {
        cx.bind_keys([
            KeyBinding::new("alt-c", ToggleMatchCase, bar),
            KeyBinding::new("alt-w", ToggleWholeWord, bar),
            KeyBinding::new("alt-r", ToggleRegex, bar),
            KeyBinding::new("cmd-alt-c", ToggleMatchCase, bar),
            KeyBinding::new("cmd-alt-w", ToggleWholeWord, bar),
            KeyBinding::new("cmd-alt-r", ToggleRegex, bar),
        ]);
    }
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
    /// The signature of the call around a zero-based line and UTF-16 column is wanted; answer
    /// with [`EditorView::show_signature`].
    SignatureHelp {
        request: u64,
        line: u32,
        character: u32,
    },
    /// Cmd+S wants the file formatted before it is saved; answer with
    /// [`EditorView::format_and_save`], with no edits if formatting is not possible.
    Format {
        request: u64,
        tab_size: u32,
        insert_spaces: bool,
    },
    /// Suggestions are wanted at a zero-based line and UTF-16 column; answer with
    /// [`EditorView::show_completions`]. `trigger` is the character typed that asked, if any.
    Complete {
        request: u64,
        line: u32,
        character: u32,
        trigger: Option<String>,
    },
    /// The right-click menu opened or closed; it is drawn in-window, under native web views.
    ContextMenu {
        open: bool,
    },
    /// A file that could not be opened opened after all, so language servers can now be told.
    Opened,
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
    /// Width of one column of the monospace font.
    pub cell: Pixels,
    pub rows: Vec<LayoutRow>,
    /// Scope headers pinned over the top rows by sticky scroll, outermost first.
    pub sticky: Vec<usize>,
    /// Left and right edge of the gutter column holding fold chevrons.
    pub fold_column: (Pixels, Pixels),
}

impl EditorLayout {
    /// The row drawn on visual row `row`, if it was on screen.
    pub fn row(&self, row: usize) -> Option<&LayoutRow> {
        self.rows.iter().find(|r| r.row == row)
    }

    /// The row a caret at char `col` of `line` is drawn on, else that line's last row shown.
    pub fn row_holding(&self, line: usize, col: usize) -> Option<&LayoutRow> {
        let mut rows = self.rows.iter().filter(|r| r.line == line);
        rows.clone()
            .find(|r| r.holds(col))
            .or_else(|| rows.next_back())
    }
}

/// One visual row drawn last frame: a whole line, or one wrapped piece of it.
pub(crate) struct LayoutRow {
    pub line: usize,
    pub row: usize,
    /// The chars of the line this row shows.
    pub chars: Range<usize>,
    /// The whole line, which `chars` index into.
    pub display: Rc<DisplayLine>,
    pub shaped: ShapedLine,
    /// How far right a wrapped line's continuation row starts.
    pub indent: Pixels,
    pub last: bool,
}

impl LayoutRow {
    fn byte(&self, col: usize) -> usize {
        let map = &self.display.char_to_byte;
        map.get(col).or(map.last()).copied().unwrap_or(0)
    }

    /// X of char `col` of the line, from the left of the text.
    pub fn x_for(&self, col: usize) -> Pixels {
        let col = col.clamp(self.chars.start, self.chars.end);
        let byte = self.byte(col).saturating_sub(self.byte(self.chars.start));
        self.indent + self.shaped.x_for_index(byte)
    }

    /// The char of the line nearest `x`; past a wrapped row's end it stays on that row.
    pub fn col_for(&self, x: Pixels) -> usize {
        let byte = self.byte(self.chars.start) + self.shaped.closest_index_for_x(x - self.indent);
        let end = if self.last {
            self.chars.end
        } else {
            self.chars.end.saturating_sub(1).max(self.chars.start)
        };
        self.display
            .char_for_byte(byte)
            .clamp(self.chars.start, end)
    }

    /// The char of the line under `x`, if `x` is over text.
    pub fn col_under(&self, x: Pixels) -> Option<usize> {
        let index = self.shaped.index_for_x(x - self.indent)?;
        Some(
            self.display
                .char_for_byte(self.byte(self.chars.start) + index),
        )
    }

    /// Whether a caret at char `col` of the line is drawn on this row.
    pub fn holds(&self, col: usize) -> bool {
        self.chars.contains(&col) || (self.last && col == self.chars.end)
    }
}

struct FindBar {
    input: Entity<TextInput>,
    replace: Entity<TextInput>,
    /// The replace row is shown, as Cmd+Alt+F or the bar's chevron reveals it.
    replacing: bool,
    matches: Vec<Range<usize>>,
    current: usize,
    /// The regex last compiled, kept while the query and toggles stay the same.
    compiled: Option<(String, FindOptions, Result<regex::Regex, String>)>,
    _subscriptions: [Subscription; 2],
}

impl FindBar {
    fn regex(&mut self, query: &str, opts: FindOptions) -> &Result<regex::Regex, String> {
        if self
            .compiled
            .as_ref()
            .is_none_or(|(q, o, _)| q != query || *o != opts)
        {
            self.compiled = Some((query.to_string(), opts, find::compile(query, opts)));
        }
        &self.compiled.as_ref().expect("just compiled").2
    }

    /// Why the query matches nothing because it is not a valid regex.
    fn error(&self, opts: FindOptions) -> Option<&str> {
        match &self.compiled {
            Some((_, o, Err(e))) if opts.regex && *o == opts => Some(e),
            _ => None,
        }
    }
}

/// Files larger than this are reloaded from disk off the UI thread.
const BACKGROUND_RELOAD: u64 = 1024 * 1024;

pub struct EditorView {
    /// Shared with every other tab on the same file; the cursor and folds stay per view.
    pub(crate) buffer: Option<Rc<SharedBuffer>>,
    pub(crate) cursor: Cursors,
    /// The buffer version this view's cursor and folds have followed up to.
    seen: u64,
    _buffer_watch: Vec<Subscription>,
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
    pub(crate) signing: Signing,
    /// Whether Cmd+S formats first, from the workspace; `None` means the language decides.
    pub(crate) format_setting: Option<bool>,
    /// The format request a save waits for, and the buffer version it was asked about.
    pub(crate) formatting: Option<(u64, u64)>,
    pub(crate) format_requests: u64,
    pub(crate) save_settings: crate::SaveSettings,
    /// Set around an edit that typing made, which narrows the suggestion list instead of closing it.
    typing: bool,
    pub(crate) marked: Option<String>,
    find: Option<FindBar>,
    find_opening: Option<Opening>,
    /// A dismissed find bar, still drawn while it fades out.
    find_closing: Option<(FindBar, Closing)>,
    find_generation: u64,
    /// The find bar's toggles, kept while it is closed as VS Code keeps them.
    pub(crate) find_options: FindOptions,
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
    reloading: Option<Task<()>>,
    /// The file changed on disk while this buffer had unsaved edits, or was deleted; the bar asks
    /// what to keep.
    conflict: Option<DiskState>,
    pub(crate) gutter_marks: Vec<GutterMark>,
    /// A caption drawn after the cursor's line while the cursor stays on that zero-based line.
    pub(crate) blame: Option<(usize, String)>,
    cursor_line: usize,
    context_menu: Option<(Entity<ContextMenu>, Subscription)>,
    pub(crate) rename: Option<crate::lsp_ui::RenameBox>,
    /// The zero-based line showing the code action lightbulb.
    pub(crate) lightbulb: Option<usize>,
    /// A restored first line to scroll to once the line height is known.
    pub(crate) pending_top: Option<usize>,
    /// Where a Shift+Alt drag started, as a line and a column with tabs expanded.
    pub(crate) column_select: Option<(usize, usize)>,
    /// The text Cmd+D last added an occurrence of, and whether it matches whole words only.
    pub(crate) occurrence: Option<(String, bool)>,
    /// This tab's word wrap choice; `None` follows `wrap_default`.
    pub(crate) wrap: Option<bool>,
    pub(crate) wrap_default: bool,
}

/// Where a view stood in its file, for restoring a tab across launches; positions are zero-based.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ViewState {
    /// The cursor's line and UTF-16 column, as language servers count them.
    pub cursor: (u32, u32),
    /// The buffer line at the top of the viewport; `None` scrolls the cursor into view.
    pub top_line: Option<usize>,
    /// The header lines of folded regions, which fold whatever region they head when restored.
    pub folds: Vec<usize>,
    /// This tab's word wrap choice; `None` follows the workspace default.
    pub wrap: Option<bool>,
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
        let watch = buffer
            .as_ref()
            .map(|b| Self::watch(b, cx))
            .unwrap_or_default();
        Self {
            buffer,
            cursor: Cursors::default(),
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
            signing: Signing::default(),
            typing: false,
            format_setting: None,
            formatting: None,
            format_requests: 0,
            save_settings: crate::SaveSettings::default(),
            marked: None,
            find: None,
            find_opening: None,
            find_closing: None,
            find_generation: 0,
            find_options: FindOptions::default(),
            save_error: None,
            selecting: false,
            was_dirty: false,
            markers: Vec::new(),
            display: DisplayMap::default(),
            fold_cache: RefCell::default(),
            gutter_hover: false,
            autosave: None,
            autosave_task: None,
            reloading: None,
            conflict: None,
            gutter_marks: Vec::new(),
            blame: None,
            cursor_line: 0,
            context_menu: None,
            rename: None,
            lightbulb: None,
            pending_top: None,
            column_select: None,
            occurrence: None,
            wrap: None,
            wrap_default: false,
        }
    }

    /// The cursor, scroll position and folds, to hand back to [`Self::restore_view_state`].
    pub fn view_state(&self) -> Option<ViewState> {
        let b = self.buf()?;
        // A tab never drawn has no scroll of its own yet.
        let top_line = match (self.pending_top, self.layout.as_ref()) {
            (Some(line), _) => Some(line),
            (None, Some(layout)) => {
                let row = (self.scroll.y / f32::from(layout.line_height))
                    .floor()
                    .max(0.) as usize;
                let rows = self.display.row_count(b.len_lines());
                Some(self.display.line_of(row.min(rows.saturating_sub(1))))
            }
            (None, None) => None,
        };
        Some(ViewState {
            cursor: b.utf16_position(self.cursor.head()),
            top_line,
            folds: self.display.folds().map(|f| f.header()).collect(),
            wrap: self.wrap,
        })
    }

    /// Puts the cursor, folds and scroll back where [`Self::view_state`] found them; a header that
    /// no longer heads a region is skipped and positions past the end are clamped.
    pub fn restore_view_state(&mut self, state: &ViewState, cx: &mut Context<Self>) {
        let Some(lines) = self.buf().map(|b| b.len_lines()) else {
            return;
        };
        self.follow_edits();
        self.set_word_wrap(state.wrap, cx);
        self.display.clear();
        for &header in &state.folds {
            if let Some(fold) = self.fold_at(header) {
                self.display.fold(fold);
            }
        }
        if let Some(shared) = self.buffer.clone() {
            let b = shared.buffer.borrow();
            let at = b.char_at_utf16(state.cursor.0, state.cursor.1);
            self.cursor.collapse();
            b.move_to(self.cursor.primary_mut(), at, false);
        }
        self.cursor_out_of_folds();
        self.pending_top = state.top_line.map(|top| top.min(lines.saturating_sub(1)));
        self.autoscroll = self.pending_top.is_none();
        self.center_cursor = self.autoscroll;
        self.note_cursor_line(false, cx);
        cx.notify();
    }

    fn watch(shared: &SharedBuffer, cx: &mut Context<Self>) -> Vec<Subscription> {
        vec![
            cx.observe(&shared.signal, |this, _, cx| this.buffer_changed(cx)),
            cx.observe(&shared.parsed, |this, _, cx| {
                // Fold regions come from the tree's bracket pairs.
                *this.fold_cache.borrow_mut() = Default::default();
                cx.notify();
            }),
        ]
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
        if self.conflict.is_some()
            && self
                .buf()
                .is_some_and(|b| b.disk_state() == DiskState::Unchanged)
        {
            self.conflict = None;
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
                self.display.reset_wrap();
                self.cursor.follow(std::iter::empty(), b.len_chars());
            }
        }
        self.seen = b.version();
        drop(b);
        self.sync_wrap();
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

    pub(crate) fn note_cursor_line(&mut self, edited: bool, cx: &mut Context<Self>) {
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
            c.collapse();
            b.move_to(c.primary_mut(), at, false);
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
        let selection =
            (!self.cursor.selection().is_empty()).then(|| b.selected_text(self.cursor.primary()));
        Some((line as u32 + 1, b.column_of(head) as u32 + 1, selection))
    }

    /// Zero-based cursor line and UTF-16 column, as [`Self::go_to_position`] takes them.
    pub fn cursor_utf16(&self) -> Option<(u32, u32)> {
        let b = self.buf()?;
        Some(b.utf16_position(self.cursor.head()))
    }

    /// Where [`Self::go_to_position`] would put the cursor, a position past the text clamped.
    pub fn landing_utf16(&self, line: u32, character: u32) -> Option<(u32, u32)> {
        let b = self.buf()?;
        Some(b.utf16_position(b.char_at_utf16(line, character)))
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
            c.collapse();
            b.move_to(c.primary_mut(), at, false);
        });
    }

    pub fn is_dirty(&self) -> bool {
        self.buf().is_some_and(|b| b.is_dirty())
    }

    pub fn save(&mut self, cx: &mut Context<Self>) -> bool {
        self.save_with(false, cx)
    }

    fn save_with(&mut self, auto: bool, cx: &mut Context<Self>) -> bool {
        let Some(shared) = self.buffer.clone() else {
            return false;
        };
        self.tidy_for_save(auto, cx);
        let checked = shared.buffer.borrow_mut().save_checked();
        let result = match checked {
            Err(SaveError::Conflict) => {
                self.conflict = Some(DiskState::Changed);
                cx.notify();
                return false;
            }
            Err(SaveError::Deleted) => {
                self.conflict = Some(DiskState::Deleted);
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
        if let Some(shared) = &self.buffer {
            shared.note_recovery();
        }
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
        f: impl FnOnce(&mut Buffer, &mut Cursors),
    ) {
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        self.follow_edits();
        let mut cursor = std::mem::take(&mut self.cursor);
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
            self.hide_signature(cx);
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

    /// Runs a move at every caret.
    fn move_each(&mut self, cx: &mut Context<Self>, op: impl FnMut(&Buffer, &mut Cursor)) {
        self.with_buffer(cx, |b, c| b.move_each(c, op));
    }

    /// Runs an edit at every caret, as one undo step.
    fn edit_each(&mut self, cx: &mut Context<Self>, op: impl FnMut(&mut Buffer, &mut Cursor)) {
        self.with_buffer(cx, |b, c| b.edit_each(c, op));
    }

    fn page_lines(&self) -> isize {
        let lh = self.layout.as_ref().map_or(px(20.), |l| l.line_height);
        ((self.viewport.height / lh) as isize - 2).max(1)
    }

    /// Buffer char under a window position, from last frame's layout.
    pub(crate) fn char_at_position(&self, position: Point<Pixels>) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        let buffer = self.buf()?;
        let y = position.y - layout.origin.y + px(self.scroll.y);
        let rows = self.display.row_count(buffer.len_lines());
        let row = ((y / layout.line_height).floor().max(0.) as usize).min(rows.saturating_sub(1));
        let line = self.display.line_of(row);
        let x = position.x - layout.text_left + px(self.scroll.x);
        let col = layout.row(row).map_or(0, |r| r.col_for(x));
        Some(buffer.char_at(line, col))
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        self.hide_hover(cx);
        self.dismiss_completion(cx);
        if self.click_sticky(event.position, cx) {
            return;
        }
        if self.click_lightbulb(event.position, window, cx) {
            return;
        }
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
        let m = event.modifiers;
        self.column_select = None;
        let mut drag = true;
        if m.alt && m.shift {
            if let Some(col) = self.column_at(event.position) {
                self.column_select = Some((b.line_of(at), col));
            }
            self.cursor = Cursors::new(Cursor::at(at));
        } else {
            // Alt+click adds a caret, or removes the one clicked, as VS Code does.
            let adding = m.alt && !m.platform;
            if !adding {
                self.cursor.collapse();
            }
            let c = &mut self.cursor;
            match event.click_count {
                2 => b.select_word_at(c.primary_mut(), at),
                n if n >= 3 => b.select_line_at(c.primary_mut(), at),
                _ if adding => drag = c.toggle(at),
                _ => b.move_to(c.primary_mut(), at, m.shift),
            }
            c.normalize();
        }
        drop(b);
        self.note_cursor_line(false, cx);
        if event.modifiers.platform && event.click_count == 1 {
            self.definition_at(at, cx);
            cx.notify();
            return;
        }
        // Dragging after removing a caret would stretch the primary instead.
        self.selecting = drag;
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
        if self.column_select.is_some() {
            self.select_columns(event.position);
            self.autoscroll = true;
            self.note_cursor_line(false, cx);
            cx.notify();
        } else if let Some(at) = self.char_at_position(event.position)
            && let Some(buffer) = self.buffer.clone()
        {
            buffer
                .buffer
                .borrow()
                .move_to(self.cursor.primary_mut(), at, true);
            self.cursor.normalize();
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
        if self.display.wrap_cols().is_none() {
            self.scroll.x = (self.scroll.x - f32::from(delta.x)).max(0.);
        }
        self.autoscroll = false;
        self.hide_hover(cx);
        self.dismiss_completion(cx);
        cx.notify();
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.buf().map(|b| copied_text(&b, &self.cursor)) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Cmd+F opens the find bar alone; Cmd+Alt+F (`replace`) opens it with the replace row.
    fn open_find(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        let seed = self
            .buf()
            .map(|b| b.selected_text(self.cursor.primary()))
            .filter(|s| !s.is_empty() && !s.contains('\n'));
        if self.find.is_none() {
            let input = cx.new(|cx| TextInput::new("Find", cx));
            let replace = cx.new(|cx| TextInput::new("Replace", cx));
            let find_events =
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
            let replace_events = cx.subscribe_in(
                &replace,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    match event {
                        InputEvent::Submit => this.replace_one(cx),
                        InputEvent::SubmitBeside => this.replace_all(cx),
                        InputEvent::Cancel => {
                            this.close_find(cx);
                            window.focus(&this.focus);
                        }
                        InputEvent::Changed | InputEvent::Up | InputEvent::Down => {}
                    }
                    cx.notify();
                },
            );
            self.find = Some(FindBar {
                input,
                replace,
                replacing: false,
                matches: Vec::new(),
                current: 0,
                compiled: None,
                _subscriptions: [find_events, replace_events],
            });
            self.find_closing = None;
            self.find_opening = Some(Opening::now());
        }
        let Some(find) = self.find.as_mut() else {
            return;
        };
        find.replacing = replace;
        let (input, replace_input) = (find.input.clone(), find.replace.clone());
        if let Some(seed) = seed {
            input.update(cx, |i, cx| i.set_text(seed, cx));
        }
        // With a search to replace already typed, the replace field is the one to fill in.
        let target = if replace && !input.read(cx).text().is_empty() {
            replace_input
        } else {
            input
        };
        window.focus(&target.focus_handle(cx));
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
    pub(crate) fn refresh_find(&mut self, jump: bool, cx: &App) {
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
        let opts = self.find_options;
        let b = buffer.buffer.borrow();
        find.matches = if !opts.regex {
            b.find(&query, opts.case, opts.word)
        } else if query.is_empty() {
            Vec::new()
        } else {
            match find.regex(&query, opts) {
                Ok(re) => b.find_regex(re, opts.word),
                Err(_) => Vec::new(),
            }
        };
        drop(b);
        let head = self.cursor.selection().range().start;
        find.current = find
            .matches
            .iter()
            .position(|m| m.start >= head)
            .unwrap_or(0);
        if jump && let Some(m) = find.matches.get(find.current) {
            self.cursor = Cursors::new(Cursor {
                selection: Selection {
                    anchor: m.start,
                    head: m.end,
                },
                ..Cursor::default()
            });
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
        self.cursor = Cursors::new(Cursor {
            selection: Selection {
                anchor: m.start,
                head: m.end,
            },
            ..Cursor::default()
        });
        self.autoscroll = true;
        self.reveal_selection();
    }

    pub(crate) fn find_open(&self) -> bool {
        self.find.is_some()
    }

    pub(crate) fn find_matches(&self) -> &[Range<usize>] {
        self.find.as_ref().map_or(&[], |f| f.matches.as_slice())
    }

    fn render_find(&self, window: &Window, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (find, closing) = match (&self.find, &self.find_closing) {
            (Some(find), _) => (find, None),
            (None, Some((find, closing))) => (find, Some(*closing)),
            (None, None) => return None,
        };
        let t = cx.theme().clone();
        let opts = self.find_options;
        let error = find.error(opts);
        let count = match find.matches.len() {
            _ if error.is_some() => "Invalid regular expression".to_string(),
            0 => "No results".to_string(),
            n => format!("{} of {n}", find.current + 1),
        };
        let field = |input: &Entity<TextInput>, invalid: bool| {
            let focused = input.focus_handle(cx).is_focused(window);
            div()
                .w(px(280.))
                .h(px(24.))
                .pl(px(8.))
                .pr(px(2.))
                .flex()
                .items_center()
                .gap(px(2.))
                .bg(t.color.surface_sunken)
                .border_1()
                .border_color(if invalid {
                    t.color.danger
                } else if focused {
                    t.color.accent
                } else {
                    t.color.border_strong
                })
                .rounded(t.shape.radius_control)
                .child(div().flex_1().min_w_0().child(input.clone()))
        };
        let toggle = |id: &'static str,
                      label: &'static str,
                      tip: &'static str,
                      on: bool,
                      flip: fn(&mut FindOptions)| {
            div()
                .id(id)
                .flex_none()
                .h(px(18.))
                .px(px(4.))
                .flex()
                .items_center()
                .rounded(t.shape.radius_control)
                .cursor_pointer()
                .text_color(if on {
                    t.color.content
                } else {
                    t.color.content_muted
                })
                .when(on, |el| {
                    el.bg(t.color.surface_accent)
                        .border_1()
                        .border_color(t.color.accent)
                })
                .when(!on, |el| el.hover(|s| s.bg(t.color.surface_hover)))
                .tooltip(move |_, cx| athena_ui::Tooltip::view(tip, cx))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_find_option(flip, cx)))
                .child(label)
        };
        let toggles = [
            toggle(
                "find-match-case",
                "Aa",
                "Match Case  ⌥⌘C",
                opts.case,
                |o| o.case = !o.case,
            ),
            toggle(
                "find-whole-word",
                "ab",
                "Match Whole Word  ⌥⌘W",
                opts.word,
                |o| o.word = !o.word,
            ),
            toggle(
                "find-regex",
                ".*",
                "Use Regular Expression  ⌥⌘R",
                opts.regex,
                |o| o.regex = !o.regex,
            ),
        ];
        let chevron = div()
            .id("find-toggle-replace")
            .w(px(16.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(t.shape.radius_control)
            .text_color(t.color.content_muted)
            .cursor_pointer()
            .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
            .tooltip(|_, cx| athena_ui::Tooltip::view("Toggle Replace  ⌥⌘F", cx))
            .on_click(cx.listener(|this, _, window, cx| this.toggle_replace(window, cx)))
            .child(if find.replacing { "⌄" } else { "›" });
        let find_row = div()
            .h(px(24.))
            .flex()
            .items_center()
            .gap(px(8.))
            .child(chevron)
            .child(field(&find.input, error.is_some()).children(toggles))
            .child(
                div()
                    .id("find-count")
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(if error.is_some() {
                        t.color.danger
                    } else {
                        t.color.content_muted
                    })
                    .when_some(error.map(str::to_string), |el, error| {
                        el.tooltip(move |_, cx| athena_ui::Tooltip::view(error.clone(), cx))
                    })
                    .child(count),
            );
        let replace_row = find.replacing.then(|| {
            div()
                .h(px(24.))
                .flex()
                .items_center()
                .gap(px(8.))
                .pl(px(24.))
                .child(field(&find.replace, false))
                .child(
                    Button::new("find-replace-one", "Replace", ButtonKind::Ghost)
                        .on_click(cx.listener(|this, _, _, cx| this.replace_one(cx))),
                )
                .child(
                    Button::new("find-replace-all", "Replace All", ButtonKind::Ghost)
                        .on_click(cx.listener(|this, _, _, cx| this.replace_all(cx))),
                )
        });
        let row = div()
            .size_full()
            .flex()
            .flex_col()
            .justify_center()
            .gap(px(4.))
            .child(find_row)
            .children(replace_row);
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
                .key_context("FindBar")
                .on_action(cx.listener(|this, _: &ToggleMatchCase, _, cx| {
                    this.toggle_find_option(|o| o.case = !o.case, cx)
                }))
                .on_action(cx.listener(|this, _: &ToggleWholeWord, _, cx| {
                    this.toggle_find_option(|o| o.word = !o.word, cx)
                }))
                .on_action(cx.listener(|this, _: &ToggleRegex, _, cx| {
                    this.toggle_find_option(|o| o.regex = !o.regex, cx)
                }))
                .flex_none()
                .h(px(if find.replacing { 64. } else { 36. }))
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
    fn toggle_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let replacing = self.find.as_ref().is_some_and(|f| f.replacing);
        self.open_find(!replacing, window, cx);
    }

    fn select_current_match(&mut self) {
        self.step_find(0);
    }

    fn toggle_find_option(&mut self, flip: fn(&mut FindOptions), cx: &mut Context<Self>) {
        flip(&mut self.find_options);
        self.refresh_find(true, cx);
        cx.notify();
    }

    /// The text each of `matches` becomes, with regex groups filled in when that toggle is on.
    fn replacements(
        &mut self,
        matches: &[Range<usize>],
        cx: &App,
    ) -> Option<Vec<(Range<usize>, String)>> {
        let opts = self.find_options;
        let find = self.find.as_mut()?;
        let with = find.replace.read(cx).text().to_string();
        if !opts.regex {
            return Some(matches.iter().map(|m| (m.clone(), with.clone())).collect());
        }
        let query = find.input.read(cx).text().to_string();
        let re = find.regex(&query, opts).as_ref().ok()?.clone();
        let b = self.buffer.as_ref()?.buffer.borrow();
        let text = b.rope().to_string();
        let byte = |at: usize| b.rope().char_to_byte(at);
        Some(
            matches
                .iter()
                .map(|m| {
                    let range = byte(m.start)..byte(m.end);
                    (
                        m.clone(),
                        find::replacement(&re, &text, &range, &with, opts),
                    )
                })
                .collect(),
        )
    }

    /// Replaces the selected match and selects the next one; a selection off the matches first
    /// moves to the current match, as VS Code's Replace does.
    fn replace_one(&mut self, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_ref() else {
            return;
        };
        let Some(current) = find.matches.get(find.current).cloned() else {
            return;
        };
        if self.cursor.selection().range() != current {
            self.select_current_match();
            cx.notify();
            return;
        }
        let Some((_, with)) = self
            .replacements(std::slice::from_ref(&current), cx)
            .and_then(|mut r| r.pop())
        else {
            return;
        };
        self.with_buffer(cx, |b, c| {
            b.edit_primary(c, |b, c| b.replace_range(c, current, &with))
        });
        self.select_current_match();
    }

    /// Replaces every match as one undo step.
    fn replace_all(&mut self, cx: &mut Context<Self>) {
        let Some(matches) = self.find.as_ref().map(|f| f.matches.clone()) else {
            return;
        };
        let Some(edits) = self.replacements(&matches, cx) else {
            return;
        };
        if !edits.is_empty() {
            self.with_buffer(cx, |b, c| {
                b.edit_primary(c, |b, c| b.apply_edits(c, &edits, None))
            });
        }
    }

    fn on_line_actions(
        el: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        el.on_action(cx.listener(|this, _: &IndentLines, _, cx| {
            this.with_buffer(cx, |b, c| b.indent_lines_all(c, false))
        }))
        .on_action(cx.listener(|this, _: &OutdentLines, _, cx| {
            this.with_buffer(cx, |b, c| b.indent_lines_all(c, true))
        }))
        .on_action(cx.listener(|this, _: &MoveLinesUp, _, cx| {
            this.with_buffer(cx, |b, c| b.move_lines_all(c, false))
        }))
        .on_action(cx.listener(|this, _: &MoveLinesDown, _, cx| {
            this.with_buffer(cx, |b, c| b.move_lines_all(c, true))
        }))
        .on_action(cx.listener(|this, _: &CopyLinesUp, _, cx| {
            this.with_buffer(cx, |b, c| b.copy_lines_all(c, false))
        }))
        .on_action(cx.listener(|this, _: &CopyLinesDown, _, cx| {
            this.with_buffer(cx, |b, c| b.copy_lines_all(c, true))
        }))
        .on_action(cx.listener(|this, _: &DeleteLines, _, cx| {
            this.with_buffer(cx, |b, c| b.delete_lines_all(c))
        }))
        .on_action(cx.listener(|this, _: &InsertLineBelow, _, cx| {
            this.edit_each(cx, |b, c| b.insert_line(c, true))
        }))
        .on_action(cx.listener(|this, _: &InsertLineAbove, _, cx| {
            this.edit_each(cx, |b, c| b.insert_line(c, false))
        }))
        .on_action(cx.listener(|this, _: &FindReplace, w, cx| this.open_find(true, w, cx)))
        .on_action(
            cx.listener(|this, _: &AddNextOccurrence, _, cx| this.add_next_occurrence(false, cx)),
        )
        .on_action(
            cx.listener(|this, _: &SkipOccurrence, _, cx| this.add_next_occurrence(true, cx)),
        )
        .on_action(
            cx.listener(|this, _: &SelectAllOccurrences, _, cx| this.select_all_occurrences(cx)),
        )
        .on_action(cx.listener(|this, _: &AddCursorAbove, _, cx| this.add_caret_vertically(-1, cx)))
        .on_action(cx.listener(|this, _: &AddCursorBelow, _, cx| this.add_caret_vertically(1, cx)))
        .on_action(cx.listener(|this, _: &ToggleWordWrap, _, cx| this.toggle_word_wrap(cx)))
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

/// The opened value once `open` succeeds; until then `error` holds why it failed this time.
fn retry_open<T>(
    error: &mut Option<String>,
    open: impl FnOnce() -> anyhow::Result<T>,
) -> Option<T> {
    match open() {
        Ok(value) => {
            *error = None;
            Some(value)
        }
        Err(e) => {
            *error = Some(format!("{e:#}"));
            None
        }
    }
}

/// Replaces `b`'s text with `text` through one edit covering only the changed span, keeping the selections.
fn replace_differing(b: &mut Buffer, c: &mut Cursors, text: &str) {
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
    let before: Vec<Selection> = c.all().iter().map(|c| c.selection).collect();
    let text: String = new[prefix..new_end].iter().collect();
    b.edit_primary(c, |b, c| b.replace_range(c, prefix..old_end, &text));
    let all = before.into_iter().map(|s| Cursor {
        selection: Selection {
            anchor: map(s.anchor),
            head: map(s.head),
        },
        ..Cursor::default()
    });
    c.set(all.collect(), c.primary_index());
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
        if !focused {
            self.dismiss_completion(cx);
        }
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
            .children(self.render_find(window, cx))
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
                        this.move_each(cx, |b, c| b.move_left(c, false))
                    }))
                    .on_action(cx.listener(|this, _: &MoveRight, _, cx| {
                        this.move_each(cx, |b, c| b.move_right(c, false))
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
                        this.move_each(cx, |b, c| b.move_left(c, true))
                    }))
                    .on_action(cx.listener(|this, _: &SelectRight, _, cx| {
                        this.move_each(cx, |b, c| b.move_right(c, true))
                    }))
                    .on_action(
                        cx.listener(|this, _: &SelectUp, _, cx| this.move_rows(-1, true, cx)),
                    )
                    .on_action(
                        cx.listener(|this, _: &SelectDown, _, cx| this.move_rows(1, true, cx)),
                    )
                    .on_action(cx.listener(|this, _: &MoveWordLeft, _, cx| {
                        this.move_each(cx, |b, c| b.move_word(c, false, false))
                    }))
                    .on_action(cx.listener(|this, _: &MoveWordRight, _, cx| {
                        this.move_each(cx, |b, c| b.move_word(c, true, false))
                    }))
                    .on_action(cx.listener(|this, _: &SelectWordLeft, _, cx| {
                        this.move_each(cx, |b, c| b.move_word(c, false, true))
                    }))
                    .on_action(cx.listener(|this, _: &SelectWordRight, _, cx| {
                        this.move_each(cx, |b, c| b.move_word(c, true, true))
                    }))
                    .on_action(cx.listener(|this, _: &MoveLineStart, _, cx| {
                        this.move_to_row_edge(false, false, cx)
                    }))
                    .on_action(cx.listener(|this, _: &MoveLineEnd, _, cx| {
                        this.move_to_row_edge(true, false, cx)
                    }))
                    .on_action(cx.listener(|this, _: &SelectLineStart, _, cx| {
                        this.move_to_row_edge(false, true, cx)
                    }))
                    .on_action(cx.listener(|this, _: &SelectLineEnd, _, cx| {
                        this.move_to_row_edge(true, true, cx)
                    }))
                    .on_action(cx.listener(|this, _: &MoveDocStart, _, cx| {
                        this.move_each(cx, |b, c| b.move_to(c, 0, false))
                    }))
                    .on_action(cx.listener(|this, _: &MoveDocEnd, _, cx| {
                        this.move_each(cx, |b, c| b.move_to(c, usize::MAX, false))
                    }))
                    .on_action(cx.listener(|this, _: &SelectDocStart, _, cx| {
                        this.move_each(cx, |b, c| b.move_to(c, 0, true))
                    }))
                    .on_action(cx.listener(|this, _: &SelectDocEnd, _, cx| {
                        this.move_each(cx, |b, c| b.move_to(c, usize::MAX, true))
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
                        let signing = this.signing_shown();
                        this.typing = open || signing;
                        this.edit_each(cx, |b, c| b.backspace(c));
                        if open {
                            this.refilter_completion(cx);
                        }
                        if signing {
                            this.request_signature(cx);
                        }
                    }))
                    .on_action(cx.listener(|this, _: &Delete, _, cx| {
                        this.edit_each(cx, |b, c| b.delete_forward(c))
                    }))
                    .on_action(cx.listener(|this, _: &DeleteWordBack, _, cx| {
                        this.edit_each(cx, |b, c| b.delete_word_back(c))
                    }))
                    .on_action(cx.listener(|this, _: &DeleteToLineStart, _, cx| {
                        this.edit_each(cx, |b, c| b.delete_to_line_start(c))
                    }))
                    .on_action(cx.listener(|this, _: &Newline, _, cx| {
                        if this.completion_open() {
                            return this.accept_completion(None, cx);
                        }
                        this.edit_each(cx, |b, c| b.newline(c))
                    }))
                    .on_action(cx.listener(|this, _: &Tab, _, cx| {
                        if this.completion_open() {
                            return this.accept_completion(None, cx);
                        }
                        this.with_buffer(cx, |b, c| b.tab_all(c))
                    }))
                    .on_action(
                        cx.listener(|this, _: &ShowCompletions, _, cx| this.complete_now(cx)),
                    )
                    .on_action(cx.listener(|this, _: &ShowHover, _, cx| this.hover_at_cursor(cx)))
                    .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                        this.with_buffer(cx, |b, c| {
                            c.collapse();
                            b.select_all(c.primary_mut())
                        })
                    }))
                    .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy(cx)))
                    .on_action(cx.listener(|this, _: &Cut, _, cx| {
                        this.copy(cx);
                        this.with_buffer(cx, cut);
                    }))
                    .on_action(cx.listener(|this, _: &Paste, _, cx| {
                        if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                            this.with_buffer(cx, |b, c| b.paste_all(c, &text));
                        }
                    }))
                    .on_action(cx.listener(|this, _: &Undo, _, cx| {
                        this.with_buffer(cx, |b, c| {
                            b.undo_all(c);
                        })
                    }))
                    .on_action(cx.listener(|this, _: &Redo, _, cx| {
                        this.with_buffer(cx, |b, c| {
                            b.redo_all(c);
                        })
                    }))
                    .on_action(cx.listener(|this, _: &Save, _, cx| this.save_formatted(cx)))
                    .on_action(cx.listener(|this, _: &Find, w, cx| this.open_find(false, w, cx)))
                    .on_action(cx.listener(|this, _: &FindNext, _, cx| {
                        this.step_find(1);
                        cx.notify();
                    }))
                    .on_action(cx.listener(|this, _: &FindPrev, _, cx| {
                        this.step_find(-1);
                        cx.notify();
                    }))
                    .on_action(cx.listener(|this, _: &ToggleComment, _, cx| {
                        this.with_buffer(cx, |b, c| b.toggle_comment_all(c))
                    }))
                    .on_action(cx.listener(|this, _: &Escape, _, cx| {
                        if this.dismiss_completion(cx)
                            || this.hide_signature(cx)
                            || this.hide_hover(cx)
                        {
                            return;
                        }
                        if this.close_find(cx) {
                            return;
                        }
                        if this.cursor.is_multi() {
                            this.with_buffer(cx, |_, c| c.collapse());
                        } else {
                            let head = this.cursor.head();
                            this.move_each(cx, |b, c| b.move_to(c, head, false));
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
                    .map(|el| Self::on_line_actions(el, cx))
                    .child(EditorElement::new(cx.entity(), focused)),
            )
            .children(self.render_line_jump(cx))
            .children(self.render_hover(cx))
            .children(self.render_rename(cx))
            .children(focused.then(|| self.render_signature(cx)).flatten())
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

    /// Unfolds whatever hides a selection's ends, as a caret never sits inside a fold.
    pub(crate) fn reveal_selection(&mut self) {
        if self.display.is_empty() {
            return;
        }
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        let b = shared.buffer.borrow();
        for c in self.cursor.all() {
            self.display.reveal(b.line_of(c.selection.anchor));
            self.display.reveal(b.line_of(c.selection.head));
        }
    }

    /// Vertical moves count visual rows, so folded blocks are stepped over.
    fn move_rows(&mut self, rows: isize, extend: bool, cx: &mut Context<Self>) {
        self.follow_edits();
        if self.display.wrap_cols().is_some() {
            let Some(targets) = self.buf().map(|b| {
                let all = self.cursor.all();
                all.iter()
                    .map(|c| self.row_target(&b, c, rows))
                    .collect::<Vec<_>>()
            }) else {
                return;
            };
            let mut targets = targets.into_iter();
            return self.move_each(cx, |b, c| match targets.next().flatten() {
                Some((at, goal)) => {
                    b.move_to(c, at, extend);
                    c.goal_column = Some(goal);
                }
                None => b.move_to(c, if rows < 0 { 0 } else { usize::MAX }, extend),
            });
        }
        let Some(targets) = self.buf().map(|b| {
            let count = self.display.row_count(b.len_lines()) as isize;
            let all = self.cursor.all();
            all.iter()
                .map(|c| {
                    let row = self.display.row_of(b.line_of(c.head())) as isize + rows;
                    (0..count)
                        .contains(&row)
                        .then(|| self.display.line_of(row as usize))
                })
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        let mut targets = targets.into_iter();
        self.move_each(cx, |b, c| match targets.next().flatten() {
            Some(line) => b.move_to_line(c, line, extend),
            None => {
                let len = b.len_lines() as isize;
                b.move_vertical(c, if rows < 0 { -len } else { len }, extend)
            }
        });
    }

    fn move_to_row_edge(&mut self, end: bool, extend: bool, cx: &mut Context<Self>) {
        self.follow_edits();
        let Some(targets) = self.buf().map(|b| {
            let all = self.cursor.all();
            all.iter()
                .map(|c| self.row_edge(&b, c, end))
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        let mut targets = targets.into_iter();
        self.move_each(cx, |b, c| match (targets.next().flatten(), end) {
            (Some(at), _) => b.move_to(c, at, extend),
            (None, true) => b.move_line_end(c, extend),
            (None, false) => b.move_line_start(c, extend),
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

    /// Moves carets that a new fold swallowed up to that fold's header.
    fn cursor_out_of_folds(&mut self) {
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        let b = shared.buffer.borrow();
        let display = &self.display;
        b.move_each(&mut self.cursor, |b, c| {
            let line = b.line_of(c.head());
            if let Some(fold) = display.fold_containing(line) {
                let col = b.column_of(c.head());
                b.move_to(c, b.char_at(fold.header(), col), false);
            }
        });
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
        if self.is_dirty()
            && self.error.is_none()
            && self.save_error.is_none()
            && self.conflict.is_none()
            // Tidying now would move the version and make the pending format's edits be dropped.
            && self.formatting.is_none()
        {
            self.save_with(true, cx);
        }
    }

    /// Picks up a change made on disk by another program: a clean buffer reloads, a dirty one asks,
    /// and a deleted file keeps its text and asks too.
    pub fn check_disk(&mut self, cx: &mut Context<Self>) {
        if self.error.is_some() {
            self.retry_open(cx);
            return;
        }
        let Some((state, dirty)) = self.buf().map(|b| (b.disk_state(), b.is_dirty())) else {
            return;
        };
        match state {
            DiskState::Unchanged => return,
            DiskState::Changed if !dirty => return self.reload(cx),
            DiskState::Changed | DiskState::Deleted => self.conflict = Some(state),
        }
        cx.notify();
    }

    /// A file that could not be opened may have been created, fixed or made readable since.
    fn retry_open(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let Some(shared) = retry_open(&mut self.error, || shared::open(&path, cx)) else {
            cx.notify();
            return;
        };
        self.seen = shared.buffer.borrow().version();
        self._buffer_watch = Self::watch(&shared, cx);
        self.cursor = Cursors::default();
        self.buffer = Some(shared);
        self.changed(cx);
        cx.emit(EditorEvent::Opened);
    }

    /// Replaces the buffer with the file on disk, as a step that undo can take back; a large file
    /// is read and compared off the UI thread.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some((path, rope, version)) = self
            .buf()
            .and_then(|b| Some((b.path.clone()?, b.rope().clone(), b.version())))
        else {
            return;
        };
        if std::fs::metadata(&path).is_ok_and(|m| m.len() <= BACKGROUND_RELOAD) {
            let disk = read_disk_text(&path, &rope);
            self.finish_reload(version, disk, cx);
            return;
        }
        self.reloading = Some(cx.spawn(async move |this, cx| {
            let disk = cx
                .background_executor()
                .spawn(async move { read_disk_text(&path, &rope) })
                .await;
            this.update(cx, |this, cx| this.finish_reload(version, disk, cx))
                .ok();
        }));
    }

    fn finish_reload(
        &mut self,
        version: u64,
        disk: anyhow::Result<DiskText>,
        cx: &mut Context<Self>,
    ) {
        self.reloading = None;
        if self.version() != Some(version) {
            // The text the file was compared with has changed since, so look again.
            self.check_disk(cx);
            return;
        }
        let mut result = Ok(());
        self.with_buffer(cx, |b, c| match disk {
            Ok(disk) => b.take_disk_text(c, disk),
            Err(e) => result = Err(e),
        });
        if result.is_ok() {
            self.conflict = None;
        }
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
        self.conflict = None;
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
        self.tidy_for_save(false, cx);
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
            self._buffer_watch = Self::watch(&fork, cx);
            self.cursor
                .follow(std::iter::empty(), fork.buffer.borrow().len_chars());
            self.buffer = Some(fork);
        } else {
            shared.buffer.borrow_mut().save_as(path.clone())?;
            shared::register(&shared, &path, cx);
        }
        // A new language folds differently, and the fold cache is keyed only on the text version.
        self.display.clear();
        *self.fold_cache.borrow_mut() = Default::default();
        self.path = path;
        self.conflict = None;
        self.save_error = None;
        cx.emit(EditorEvent::Saved);
        self.changed(cx);
        Ok(())
    }

    /// Closes this tab through the shell's Close Tab, which asks about unsaved edits first.
    fn close_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        if let Ok(action) = cx.build_action("athena::CloseTab", None) {
            window.dispatch_action(action, cx);
        }
    }

    fn render_conflict(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let deleted = match self.conflict? {
            DiskState::Unchanged => return None,
            DiskState::Changed => false,
            DiskState::Deleted => true,
        };
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
                        .child(if deleted {
                            format!("{name} was deleted on disk.")
                        } else {
                            format!("{name} changed on disk while you were editing it.")
                        }),
                )
                .map(|bar| {
                    if deleted {
                        bar.child(
                            Button::new("deleted-close", "Close", ButtonKind::Ghost).on_click(
                                cx.listener(|this, _, window, cx| this.close_tab(window, cx)),
                            ),
                        )
                        .child(
                            Button::new("deleted-save", "Save", ButtonKind::Secondary)
                                .on_click(cx.listener(|this, _, _, cx| this.overwrite(cx))),
                        )
                    } else {
                        bar.child(
                            Button::new("conflict-reload", "Reload", ButtonKind::Ghost)
                                .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                        )
                        .child(
                            Button::new("conflict-overwrite", "Overwrite", ButtonKind::Secondary)
                                .on_click(cx.listener(|this, _, _, cx| this.overwrite(cx))),
                        )
                    }
                }),
        )
    }
}

/// The UTF-16 `range` of `text`, clamped and put in order since the input system may send either.
fn utf16_slice(text: &str, range: Range<usize>) -> (Option<String>, Range<usize>) {
    let units: Vec<u16> = text.encode_utf16().collect();
    let (a, b) = (range.start.min(units.len()), range.end.min(units.len()));
    let r = a.min(b)..a.max(b);
    (String::from_utf16(&units[r.clone()]).ok(), r)
}

impl EntityInputHandler for EditorView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let (text, r) = utf16_slice(self.marked.as_deref().unwrap_or_default(), range);
        *actual = Some(r);
        text
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
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => self.edit_each(cx, |b, c| b.type_char(c, ch)),
                _ => self.edit_each(cx, |b, c| b.insert(c, &text)),
            }
            self.completion_after_typing(&text, cx);
            self.signature_after_typing(&text, cx);
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
        let origin = self.char_origin(self.cursor.head())?;
        let height = self.layout.as_ref()?.line_height;
        Some(Bounds::new(origin, gpui::size(px(2.), height)))
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
            && !self
                .cursor
                .all()
                .iter()
                .any(|c| c.selection.range().contains(&at))
        {
            self.cursor.collapse();
            buffer
                .buffer
                .borrow()
                .move_to(self.cursor.primary_mut(), at, false);
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
            item(
                "Go to Implementations",
                "⌘F12",
                Box::new(crate::GoToImplementation),
            ),
            item(
                "Go to Type Definition",
                "",
                Box::new(crate::GoToTypeDefinition),
            ),
            MenuItem::separator(),
            item("Rename Symbol", "F2", Box::new(crate::RenameSymbol)),
            item("Quick Fix…", "⌘.", Box::new(crate::ShowCodeActions)),
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
                cx.emit(EditorEvent::ContextMenu { open: false });
                cx.notify();
            }
        });
        self.context_menu = Some((menu, subscription));
        cx.emit(EditorEvent::ContextMenu { open: true });
        cx.notify();
    }
}

/// What the status bar shows about an editor.
#[derive(Clone, Debug, PartialEq)]
pub struct EditorStatus {
    /// 1-based cursor line.
    pub line: usize,
    /// 1-based cursor column with tabs expanded, as VS Code counts it.
    pub column: usize,
    /// Characters selected, over every caret.
    pub selected: usize,
    /// How many carets there are.
    pub carets: usize,
    pub lang: Option<crate::Lang>,
    pub indent: crate::Indent,
    pub line_ending: crate::LineEnding,
}

impl EditorView {
    pub fn status(&self) -> Option<EditorStatus> {
        let b = self.buf()?;
        let head = self.cursor.head().min(b.len_chars());
        let line = b.line_of(head);
        let before = b.text(b.line_start(line)..head);
        let column = before.chars().fold(0, |col, c| match c {
            '\t' => col + crate::display::TAB_WIDTH - col % crate::display::TAB_WIDTH,
            _ => col + 1,
        });
        Some(EditorStatus {
            line: line + 1,
            column: column + 1,
            selected: self
                .cursor
                .all()
                .iter()
                .map(|c| c.selection.range().len())
                .sum(),
            carets: self.cursor.len(),
            lang: b.lang(),
            indent: b.indent,
            line_ending: b.line_ending(),
        })
    }

    /// Highlights this file as `lang` (plain text for `None`), in every tab showing it.
    pub fn set_language(&mut self, lang: Option<crate::Lang>, cx: &mut Context<Self>) {
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        shared.buffer.borrow_mut().set_lang(lang);
        self.display.clear();
        *self.fold_cache.borrow_mut() = Default::default();
        shared.changed(cx);
        self.changed(cx);
    }

    /// Indents with `indent` from now on, leaving existing lines as they are.
    pub fn set_indent(&mut self, indent: crate::Indent, cx: &mut Context<Self>) {
        if let Some(shared) = self.buffer.clone() {
            shared.buffer.borrow_mut().indent = indent;
            shared.changed(cx);
            self.changed(cx);
        }
    }

    /// Rewrites every line's indentation as `indent` and keeps using it; one undo step.
    pub fn convert_indentation(&mut self, indent: crate::Indent, cx: &mut Context<Self>) {
        self.with_buffer(cx, |b, c| b.convert_indentation_all(c, indent));
    }
}

/// A 0-based line and UTF-16 column.
type Utf16 = (u32, u32);

impl EditorView {
    /// The primary selection as 0-based (line, UTF-16 column) start and end, in document order,
    /// with its text; start equals end for a bare cursor.
    pub fn selection_utf16(&self) -> Option<(Utf16, Utf16, String)> {
        let b = self.buf()?;
        let range = self.cursor.selection().range();
        let text = b.selected_text(self.cursor.primary());
        Some((
            b.utf16_position(range.start),
            b.utf16_position(range.end),
            text,
        ))
    }
}

/// Every caret's selection, one per line; with no selection anywhere, every caret's whole line.
fn copied_text(b: &Buffer, cs: &Cursors) -> String {
    let all = cs.all();
    if all.iter().all(|c| c.selection.is_empty()) {
        let mut lines: Vec<usize> = all.iter().map(|c| b.line_of(c.head())).collect();
        lines.dedup();
        lines.iter().map(|&l| format!("{}\n", b.line(l))).collect()
    } else {
        let parts: Vec<String> = all.iter().map(|c| b.selected_text(c)).collect();
        parts.join("\n")
    }
}

/// Deletes exactly what [`copied_text`] copies, so a cut never loses text.
fn cut(b: &mut Buffer, cs: &mut Cursors) {
    if cs.all().iter().all(|c| c.selection.is_empty()) {
        // An empty last line selects nothing, and backspace then takes the break before it.
        b.move_each(cs, |b, c| b.select_line_at(c, c.head()));
        b.edit_each(cs, Buffer::backspace);
    } else {
        b.edit_each(cs, |b, c| {
            if !c.selection.is_empty() {
                b.backspace(c);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cutting_with_mixed_carets_deletes_only_what_was_copied() {
        let mut b = Buffer::new("keep one\nsel two\n", None);
        let mut cs = Cursors::new(Cursor::at(2));
        cs.add(Cursor {
            selection: Selection {
                anchor: 9,
                head: 12,
            },
            ..Cursor::default()
        });
        assert_eq!(copied_text(&b, &cs), "\nsel");
        cut(&mut b, &mut cs);
        assert_eq!(
            b.full_text(),
            "keep one\n two\n",
            "the empty caret's line stays"
        );

        let mut b = Buffer::new("a\nb\n", None);
        let mut cs = Cursors::new(Cursor::at(0));
        cs.add(Cursor::at(2));
        assert_eq!(copied_text(&b, &cs), "a\nb\n");
        cut(&mut b, &mut cs);
        assert_eq!(b.full_text(), "", "with no selection whole lines go");

        let mut b = Buffer::new("a\n", None);
        let mut cs = Cursors::new(Cursor::at(2));
        cut(&mut b, &mut cs);
        assert_eq!(b.full_text(), "a", "an empty last line goes with its break");
    }

    #[test]
    fn alt_clicking_a_caret_removes_it_and_does_not_start_a_drag() {
        let mut cs = Cursors::new(Cursor::at(0));
        assert!(cs.toggle(4), "a new caret starts a drag");
        assert!(!cs.toggle(4));
        assert_eq!(cs.len(), 1);
        assert!(!cs.toggle(0), "the last caret stays");
        assert_eq!(cs.len(), 1);
    }

    #[test]
    fn marked_text_ranges_are_clamped_and_ordered() {
        let reversed = |start, end| Range { start, end };
        assert_eq!(
            utf16_slice("かな", reversed(2, 0)),
            (Some("かな".into()), 0..2)
        );
        assert_eq!(utf16_slice("ab", reversed(5, 1)), (Some("b".into()), 1..2));
        assert_eq!(utf16_slice("", 3..7), (Some(String::new()), 0..0));
    }

    #[test]
    fn replacing_text_is_one_undo_step_that_keeps_the_cursor() {
        let mut b = Buffer::new("let old = 1;\nkeep\nold();\n", None);
        let mut c = Cursors::new(Cursor::at(b.line_start(1) + 2));
        replace_differing(&mut b, &mut c, "let new = 1;\nkeep\nnew();\n");
        assert_eq!(b.full_text(), "let new = 1;\nkeep\nnew();\n");
        assert_eq!(c.head(), b.line_start(1) + 2);
        b.undo_all(&mut c);
        assert_eq!(b.full_text(), "let old = 1;\nkeep\nold();\n");
        replace_differing(&mut b, &mut c, "let old = 1;\nkeep\nold();\n");
        assert!(!b.undo_all(&mut c), "an unchanged text adds no undo step");
    }

    #[test]
    fn line_and_replace_keystrokes_parse() {
        let parse = |s: &str| gpui::Keystroke::parse(s).unwrap();
        assert_eq!(parse("cmd-]").key, "]");
        assert_eq!(parse("cmd-[").key, "[");
        let k = parse("alt-shift-up");
        assert!(k.modifiers.alt && k.modifiers.shift && k.key == "up");
        let k = parse("cmd-shift-k");
        assert!(k.modifiers.platform && k.modifiers.shift && k.key == "k");
        let k = parse("cmd-alt-f");
        assert!(k.modifiers.platform && k.modifiers.alt && k.key == "f");
        assert!(parse("shift-tab").modifiers.shift);
        assert_eq!(parse("cmd-enter").key, "enter");
    }

    #[test]
    fn replacing_every_match_is_one_undo_step() {
        let mut b = Buffer::new("foo Foo bar foo\n", None);
        let mut c = Cursor::default();
        let edits: Vec<_> = b
            .find_all("foo")
            .into_iter()
            .map(|m| (m, "qux".to_string()))
            .collect();
        b.apply_edits(&mut c, &edits, None);
        assert_eq!(b.full_text(), "qux qux bar qux\n");
        assert!(b.undo(&mut c));
        assert_eq!(b.full_text(), "foo Foo bar foo\n");
        assert!(!b.undo(&mut c), "one step");
    }

    #[test]
    fn a_file_that_failed_to_open_opens_once_it_is_readable() {
        let dir = std::env::temp_dir().join(format!("athena-retry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("late.txt");
        let mut error = None;
        assert!(retry_open(&mut error, || Buffer::open(&path)).is_none());
        assert!(error.is_some(), "a missing file shows why");
        std::fs::write(&path, [0u8, 1, 2]).unwrap();
        assert!(retry_open(&mut error, || Buffer::open(&path)).is_none());
        assert!(error.as_deref().unwrap().contains("binary"), "{error:?}");
        std::fs::write(&path, "now text\n").unwrap();
        let b = retry_open(&mut error, || Buffer::open(&path)).unwrap();
        assert_eq!(b.full_text(), "now text\n");
        assert_eq!(error, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
