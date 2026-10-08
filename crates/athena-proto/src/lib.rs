//! Wire protocol between the Athena GUI and the `athena-mux` session daemon.

mod client;
mod codec;
mod paths;

pub use client::{ConnectError, Connection, connect, connect_or_spawn};
pub use codec::{MAX_FRAME, read_frame, write_frame};
pub use paths::{data_dir, lock_path, log_path, socket_path};

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to the messages below.
pub const PROTO_VERSION: u32 = 1;

/// Largest `Output` payload the daemon sends in one frame.
pub const MAX_OUTPUT_CHUNK: usize = 64 * 1024;

pub type PaneId = u64;

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
