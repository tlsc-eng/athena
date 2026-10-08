//! A small Language Server Protocol client on plain threads, for gopls and typescript-language-server.

mod client;
mod env;
mod protocol;

pub use client::{Client, Event};
pub use env::{find_program, server_env};
pub use protocol::{Diagnostic, Location, Position, Range, Severity, path_from_uri, uri_from_path};

/// The language servers Athena knows how to start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServerKind {
    Go,
    TypeScript,
}

impl ServerKind {
    pub fn program(self) -> &'static str {
        match self {
            Self::Go => "gopls",
            Self::TypeScript => "typescript-language-server",
        }
    }

    pub fn args(self) -> &'static [&'static str] {
        match self {
            Self::Go => &[],
            Self::TypeScript => &["--stdio"],
        }
    }

    /// What to tell the user when the server is not installed.
    pub fn install_hint(self) -> &'static str {
        match self {
            Self::Go => "go install golang.org/x/tools/gopls@latest",
            Self::TypeScript => "npm install -g typescript-language-server typescript",
        }
    }
}
