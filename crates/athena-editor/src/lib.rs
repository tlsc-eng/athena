mod buffer;
mod completion;
pub mod diff;
mod display;
mod element;
mod format;
mod hover;
mod image;
mod line_jump;
mod lines;
mod pairs;
pub mod recovery;
mod shared;
mod signature;
mod syntax;
mod view;

pub use buffer::{Buffer, Cursor, DiskState, Edit, Indent, LineEnding, SaveError, Selection};
pub use completion::{Completion, ServerEdit};
pub use hover::HoverBlock;
pub use image::{ImageView, is_image_path};
pub use signature::Signature;
pub use syntax::{Lang, Token};
pub use view::{EditorEvent, EditorStatus, EditorView, GutterMark, Marker, MarkerSeverity};

/// Registers the editor's and image viewer's key bindings.
pub fn init(cx: &mut gpui::App) {
    view::init(cx);
    image::init(cx);
}
