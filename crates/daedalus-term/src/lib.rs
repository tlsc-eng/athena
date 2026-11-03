mod colors;
mod element;
mod glyphs;
mod keys;
mod links;
mod marks;
mod mouse;
mod search;
mod terminal;
mod view;

pub use view::{
    ClaudeState, TerminalEvent, TerminalView, init, is_shell, kill_sessions, open_connection,
    read_messages,
};
