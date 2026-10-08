mod buffer;
mod completion;
pub mod diff;
mod diff_view;
mod display;
mod element;
mod format;
mod hover;
mod image;
mod line_jump;
mod lines;
mod lsp_ui;
mod multi;
mod pairs;
pub mod recovery;
mod shared;
mod signature;
mod syntax;
mod view;
mod wrap;

pub use buffer::{
    Buffer, Cursor, Cursors, DiskState, Edit, Indent, LineEnding, SaveError, Selection,
};
pub use completion::{Completion, ServerEdit};
pub use diff_view::{DiffEvent, DiffView, HunkActions};
pub use hover::HoverBlock;
pub use image::{ImageView, is_image_path};
pub use lsp_ui::{
    ConfirmRename, GoToImplementation, GoToTypeDefinition, RenameSymbol, ShowCodeActions,
};
pub use signature::Signature;
pub use syntax::{Lang, Token};
pub use view::{
    EditorEvent, EditorStatus, EditorView, GutterMark, Marker, MarkerSeverity, ViewState,
};

/// Registers the editor's and image viewer's key bindings.
pub fn init(cx: &mut gpui::App) {
    view::init(cx);
    diff_view::init(cx);
    image::init(cx);
    lsp_ui::init(cx);
}
