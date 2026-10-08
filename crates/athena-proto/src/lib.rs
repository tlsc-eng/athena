//! Wire protocol between the Athena GUI and the `athena-mux` session daemon.

mod client;
mod codec;
pub mod logging;
mod paths;

pub use client::{ConnectError, Connection, connect, connect_or_spawn, stop_daemon};
pub use codec::{MAX_FRAME, read_frame, write_frame};
pub use paths::{
    app_log_path, app_socket_path, data_dir, lock_path, log_path, recovery_dir, socket_path,
};

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to the messages below.
pub const PROTO_VERSION: u32 = 3;

/// Largest `Output` payload the daemon sends in one frame.
pub const MAX_OUTPUT_CHUNK: usize = 64 * 1024;

pub type PaneId = u64;

/// Requests to the running Athena window over `app.sock`, from the command line and the MCP
/// bridge. Each gets one `AppReply`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum AppMsg {
    OpenProject {
        path: PathBuf,
    },
    ListProjects,
    ActiveFile,
    OpenFile {
        path: PathBuf,
        line: Option<u32>,
    },
    ListTerminals,
    ReadTerminal {
        session: PaneId,
        lines: u32,
    },
    /// Which pane and project the connecting process runs in, as the window determined it.
    WhoAmI,
    /// Types `text` into a terminal, only after the user approves it in the window.
    RunInTerminal {
        session: PaneId,
        text: String,
        newline: bool,
    },
    /// Sent first by clients running in an Athena terminal (`ATHENA_PANE_ID`). The window only
    /// believes it if that pane's foreground program is an ancestor of the client.
    Identify {
        session: PaneId,
    },
    /// Language server diagnostics for one file, or for every open project when `path` is None.
    Diagnostics {
        path: Option<PathBuf>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum AppReply {
    Ok,
    Error(String),
    Projects(Vec<ProjectInfo>),
    ActiveFile(Option<ActiveFile>),
    Terminals(Vec<TerminalInfo>),
    Lines(Vec<String>),
    Caller {
        session: Option<PaneId>,
        project: Option<PathBuf>,
    },
    Diagnostics(Vec<DiagnosticInfo>),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DiagnosticInfo {
    pub path: PathBuf,
    /// 1-based line; the column counts UTF-16 units, as language servers do.
    pub line: u32,
    pub column: u32,
    pub severity: String,
    pub message: String,
    pub source: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ProjectInfo {
    pub root: PathBuf,
    pub name: String,
    pub active: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ActiveFile {
    pub path: PathBuf,
    /// 1-based, as editors show them.
    pub line: u32,
    pub column: u32,
    pub selection: Option<String>,
    pub modified: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TerminalInfo {
    pub session: Option<PaneId>,
    pub project: PathBuf,
    pub title: String,
    pub cwd: Option<PathBuf>,
    pub program: Option<String>,
    /// `running` or `waiting_input` for a Claude Code session.
    pub claude: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ClientMsg {
    Hello {
        proto: u32,
    },
    ListPanes,
    Spawn {
        cwd: PathBuf,
        rows: u16,
        cols: u16,
    },
    /// Replays the pane's scrollback, then streams live output.
    Attach {
        pane: PaneId,
    },
    Input {
        pane: PaneId,
        data: Vec<u8>,
    },
    Resize {
        pane: PaneId,
        rows: u16,
        cols: u16,
    },
    Kill {
        pane: PaneId,
    },
    Shutdown,
    /// Receive every `Notice`, including ones buffered while no window was connected.
    Subscribe,
    /// Raise a notice, as `athena notify` does from Claude Code hooks.
    Notify {
        pane: Option<PaneId>,
        kind: NoticeKind,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ServerMsg {
    Hello {
        proto: u32,
        pid: u32,
        panes: u32,
    },
    Panes {
        panes: Vec<PaneInfo>,
    },
    Spawned {
        pane: PaneId,
    },
    /// Followed by `Output` frames of the scrollback, then `ReplayDone`.
    Attached {
        pane: PaneId,
        rows: u16,
        cols: u16,
    },
    ReplayDone {
        pane: PaneId,
    },
    Output {
        pane: PaneId,
        data: Vec<u8>,
    },
    Exited {
        pane: PaneId,
        code: Option<i32>,
    },
    Error {
        kind: ErrorKind,
    },
    /// The pane's foreground process changed; also sent right after an attach.
    Foreground {
        pane: PaneId,
        process: Option<Process>,
    },
    Notice(Notice),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Notice {
    pub pane: Option<PaneId>,
    pub kind: NoticeKind,
    /// Milliseconds since the Unix epoch, stamped by the daemon.
    pub at: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum NoticeKind {
    CommandFinished {
        exit_code: i32,
        elapsed_ms: u64,
        command: Option<String>,
    },
    ClaudeRunning,
    ClaudeStopped,
    ClaudeNeedsInput {
        message: String,
    },
    Message {
        title: String,
        body: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Process {
    pub pid: i32,
    /// Executable name as the kernel records it; some CLIs install under a version number.
    pub name: String,
    pub path: PathBuf,
    pub cwd: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PaneInfo {
    pub id: PaneId,
    pub cwd: PathBuf,
    pub rows: u16,
    pub cols: u16,
    pub alive: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, thiserror::Error)]
pub enum ErrorKind {
    #[error("no such pane {0}")]
    NoSuchPane(PaneId),
    #[error("could not start shell: {0}")]
    SpawnFailed(String),
    #[error("protocol violation: {0}")]
    Protocol(String),
}
