mod blame;
mod breakpoints;
mod buffer;
mod code_lens;
mod completion;
pub mod diff;
mod diff_view;
mod display;
pub mod editorconfig;
mod element;
mod encoding;
pub mod find;
mod format;
mod hover;
mod image;
mod large;
mod line_jump;
mod lines;
mod lsp_ui;
mod merge_conflicts;
mod minimap;
mod multi;
mod pairs;
mod peek;
pub mod recovery;
mod run_marks;
mod save;
mod shared;
mod signature;
mod smart_select;
mod snippet;
mod sticky;
mod syntax;
mod view;
mod wrap;

pub use blame::{BlameCommit, GitGutterEvent, GutterBlame};
pub use breakpoints::{
    Breakpoint, DebugTestAt, EnableBreakpointAt, SetBreakpointAt, ToggleBreakpoint,
    ToggleBreakpointAt,
};
pub use buffer::{
    Buffer, Cursor, Cursors, DiskState, Edit, Indent, LineEnding, SaveError, Selection,
};
pub use code_lens::Lens;
pub use completion::{Completion, Resolved, ServerEdit};
pub use diff_view::{DiffEvent, DiffView, HunkActions};
pub use encoding::{Decoded, FileEncoding, decode as decode_text};
pub use hover::HoverBlock;
pub use image::{ImageView, is_image_path};
pub use large::{LargeFileView, is_large_file};
pub use lsp_ui::{
    ConfirmRename, FormatSelection, GoToImplementation, GoToTypeDefinition, Inlay, LspRequest,
    Occurrence, RenameSymbol, SemanticSpan, ShowCallHierarchy, ShowCodeActions, ShowTypeHierarchy,
};
pub use merge_conflicts::{
    CompareMergeConflicts, ConflictBlock, Resolution, count_merge_conflicts, find_merge_conflicts,
    resolve_all,
};
pub use peek::{ShowNextChange, ShowPreviousChange};
pub use run_marks::{Coverage, RunMark, RunState, RunTestAt, TestSymbol, find_tests, is_test_file};
pub use save::{SaveSettings, Tidy, resolve_tidy};
pub use signature::Signature;
pub use smart_select::{ExpandSelection, ShrinkSelection};
pub use syntax::{Lang, Token, semantic_token};
pub use view::{
    AddCursorAbove, AddCursorBelow, AddNextOccurrence, CopyLinesDown, CopyLinesUp, EditorEvent,
    EditorStatus, EditorView, GutterMark, Marker, MarkerSeverity, MoveLinesDown, MoveLinesUp,
    SelectAll, SelectAllOccurrences, SkipOccurrence, ToggleMatchCase, ToggleRegex, ToggleWholeWord,
    ToggleWordWrap, ViewState,
};

/// Registers the editor's and image viewer's key bindings.
pub fn init(cx: &mut gpui::App) {
    view::init(cx);
    diff_view::init(cx);
    image::init(cx);
    large::init(cx);
    lsp_ui::init(cx);
    smart_select::init(cx);
    peek::init(cx);
    breakpoints::init(cx);
}
