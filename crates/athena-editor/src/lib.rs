mod blame;
mod buffer;
mod completion;
pub mod diff;
mod diff_view;
mod display;
pub mod editorconfig;
mod element;
pub mod find;
mod format;
mod hover;
mod image;
mod line_jump;
mod lines;
mod lsp_ui;
mod merge_conflicts;
mod multi;
mod pairs;
mod peek;
pub mod recovery;
mod run_marks;
mod save;
mod shared;
mod signature;
mod snippet;
mod sticky;
mod syntax;
mod view;
mod wrap;

pub use blame::{BlameCommit, GitGutterEvent, GutterBlame};
pub use buffer::{
    Buffer, Cursor, Cursors, DiskState, Edit, Indent, LineEnding, SaveError, Selection,
};
pub use completion::{Completion, Resolved, ServerEdit};
pub use diff_view::{DiffEvent, DiffView, HunkActions};
pub use hover::HoverBlock;
pub use image::{ImageView, is_image_path};
pub use lsp_ui::{
    ConfirmRename, GoToImplementation, GoToTypeDefinition, Inlay, LspRequest, Occurrence,
    RenameSymbol, ShowCallHierarchy, ShowCodeActions,
};
pub use merge_conflicts::{
    CompareMergeConflicts, ConflictBlock, Resolution, count_merge_conflicts, find_merge_conflicts,
    resolve_all,
};
pub use peek::{ShowNextChange, ShowPreviousChange};
pub use run_marks::{Coverage, RunMark, RunState, RunTestAt, TestSymbol, find_tests, is_test_file};
pub use save::{SaveSettings, Tidy, resolve_tidy};
pub use signature::Signature;
pub use syntax::{Lang, Token};
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
    lsp_ui::init(cx);
    peek::init(cx);
}
