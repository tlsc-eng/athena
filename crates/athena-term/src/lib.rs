mod colors;
mod element;
mod glyphs;
mod keys;
mod links;
mod terminal;
mod view;

pub use view::{
    ClaudeState, TerminalEvent, TerminalView, init, is_shell, open_connection, read_messages,
};
