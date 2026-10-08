use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result};
use athena_proto::{MAX_OUTPUT_CHUNK, PaneId};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::ring::Ring;

const SCROLLBACK_BYTES: usize = 8 * 1024 * 1024;

pub struct Pane {
    pub cwd: PathBuf,
    pub rows: u16,
    pub cols: u16,
    pub ring: Ring,
    pub attached: Vec<u64>,
    pub exit: Option<Option<i32>>,
    master: Box<dyn MasterPty + Send>,
    input: mpsc::Sender<Vec<u8>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

/// Output side of a freshly spawned shell, consumed by the pane's reader thread.
pub struct PaneOutput {
    pub reader: Box<dyn Read + Send>,
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl Pane {
    pub fn spawn(id: PaneId, cwd: &Path, rows: u16, cols: u16) -> Result<(Self, PaneOutput)> {
        let cwd = if cwd.is_dir() {
            cwd.to_path_buf()
        } else {
            std::env::home_dir().unwrap_or_else(|| "/".into())
        };
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open pty")?;

        let mut cmd = CommandBuilder::new_default_prog();
        cmd.cwd(&cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "athena");
        cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        cmd.env("ATHENA_PANE_ID", id.to_string());
        // Apps launched from Finder get no locale, which breaks UTF-8 in most shells.
        if std::env::var_os("LANG").is_none() {
            cmd.env("LANG", "en_US.UTF-8");
        }

        let child = pair.slave.spawn_command(cmd).context("spawn shell")?;
        drop(pair.slave);
        let killer = child.clone_killer();
        let reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;

        // Input goes through its own thread so a child that stops reading cannot stall the daemon.
        let (input, input_rx) = mpsc::channel::<Vec<u8>>();
        thread::Builder::new()
            .name("pane-write".into())
            .spawn(move || {
                for bytes in input_rx {
                    if writer
                        .write_all(&bytes)
                        .and_then(|_| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            })?;

        let pane = Self {
            cwd,
            rows,
            cols,
            ring: Ring::new(SCROLLBACK_BYTES),
            attached: Vec::new(),
            exit: None,
            master: pair.master,
            input,
            killer,
        };
        Ok((pane, PaneOutput { reader, child }))
    }

    pub fn write(&self, data: Vec<u8>) {
        let _ = self.input.send(data);
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        let _ = self.killer.kill();
    }
}

pub const READ_CHUNK: usize = MAX_OUTPUT_CHUNK;
