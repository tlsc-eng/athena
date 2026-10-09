//! A small Language Server Protocol client on plain threads, for gopls, typescript-language-server
//! and the ESLint and Biome servers a project installs.

mod call;
mod client;
mod code_action;
mod completion;
mod edit;
mod env;
mod file_ops;
mod local;
mod markup;
mod protocol;
mod ranges;
mod signature;
mod symbol;

pub use call::{Call, CallItem};
pub use client::{Client, Config, EditReply, Event, FileEvent, RenameTarget};
pub use code_action::{CodeAction, Command};
pub use completion::{CompletionItem, CompletionList, TextEdit};
pub use edit::{EditError, FileChange, WorkspaceEdit, apply_text_edits};
pub use env::{find_program, server_env};
pub use local::{
    eslint_settings, global_typescript, project_server, project_typescript, reachable_typescript,
};
pub use markup::{Hover, MarkupBlock, expand_snippet, markdown_blocks, snippet_stops};
pub use protocol::{
    Diagnostic, Highlight, InlayHint, Location, Position, Range, Severity, path_from_uri,
    uri_from_path,
};
pub use ranges::LinkedRanges;
pub use signature::SignatureHelp;
pub use symbol::{Symbol, symbol_kind_label};

/// The language servers Athena knows how to start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServerKind {
    Go,
    TypeScript,
    /// Started only from the project's own node_modules; see [`project_server`].
    Eslint,
    Biome,
}

impl ServerKind {
    pub fn program(self) -> &'static str {
        match self {
            Self::Go => "gopls",
            Self::TypeScript => "typescript-language-server",
            Self::Eslint => "vscode-eslint-language-server",
            Self::Biome => "biome",
        }
    }

    pub fn args(self) -> &'static [&'static str] {
        match self {
            Self::Go => &[],
            Self::TypeScript | Self::Eslint => &["--stdio"],
            Self::Biome => &["lsp-proxy"],
        }
    }

    /// What to tell the user when the server is not installed.
    pub fn install_hint(self) -> &'static str {
        match self {
            Self::Go => "go install golang.org/x/tools/gopls@latest",
            Self::TypeScript => "npm install -g typescript-language-server typescript",
            Self::Eslint => "npm install -D vscode-langservers-extracted",
            Self::Biome => "npm install -D @biomejs/biome",
        }
    }

    /// A linter that runs beside a file's main server and is found in the project, never on PATH.
    pub fn is_project_local(self) -> bool {
        matches!(self, Self::Eslint | Self::Biome)
    }

    /// The name its diagnostics are labelled with when they carry none.
    pub fn label(self) -> &'static str {
        match self {
            Self::Eslint => "eslint",
            other => other.program(),
        }
    }
}
