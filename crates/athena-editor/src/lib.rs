mod buffer;
mod completion;
mod display;
mod element;
mod hover;
mod image;
mod line_jump;
mod lines;
mod shared;
mod syntax;
mod view;

pub use buffer::{Buffer, Cursor, Edit, Indent, SaveError, Selection};
pub use completion::{Completion, ServerEdit};
pub use hover::HoverBlock;
pub use image::{ImageView, is_image_path};
pub use syntax::{Lang, Token};
pub use view::{EditorEvent, EditorView, GutterMark, Marker, MarkerSeverity};

/// Registers the editor's and image viewer's key bindings.
pub fn init(cx: &mut gpui::App) {
    view::init(cx);
    image::init(cx);
}
