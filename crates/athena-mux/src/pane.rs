use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use athena_proto::{MAX_OUTPUT_CHUNK, PaneId, Process};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::process;
use crate::ring::Ring;

pub const SCROLLBACK_BYTES: usize = 8 * 1024 * 1024;
/// How long programs get to exit after a hangup before they are killed.
pub const KILL_GRACE: Duration = Duration::from_secs(2);
const REAP_POLL: Duration = Duration::from_millis(50);

pub struct Pane {
    pub cwd: PathBuf,
    pub rows: u16,
    pub cols: u16,
    pub ring: Ring,
    pub attached: Vec<u64>,
    pub exit: Option<Option<i32>>,
    pub foreground: Option<Process>,
    pub spawned_at: Instant,
    /// Whether any client ever attached; a pane nobody attached to has no tab to come back to.
    pub ever_attached: bool,
    /// When a client was dropped from this pane for falling behind.
    pub lag_dropped: Option<Instant>,
    /// Device number of the pane's terminal; `None` once it has been hung up.
    tty: Option<u32>,
    master: Box<dyn MasterPty + Send>,
    input: mpsc::Sender<Vec<u8>>,
}

/// Output side of a freshly spawned shell, consumed by the pane's reader thread.
pub struct PaneOutput {
    pub reader: Box<dyn Read + Send>,
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl Pane {
    pub fn spawn(
        id: PaneId,
        cwd: &Path,
        rows: u16,
        cols: u16,
        zdotdir: Option<&Path>,
    ) -> Result<(Self, PaneOutput)> {
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
        // The daemon inherits whatever launched it (an IDE, an agent session with tokens), so
        // shells get only what login(1) would give them and rebuild the rest from their profile.
        cmd.env_clear();
        for (key, value) in std::env::vars_os() {
            if inherited(&key) {
                cmd.env(key, value);
            }
        }
        if cmd.get_env("PATH").is_none() {
            cmd.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "athena");
        cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        cmd.env("ATHENA_PANE_ID", id.to_string());
        let zsh = cmd
            .get_env("SHELL")
            .is_some_and(|s| Path::new(s).ends_with("zsh"));
        if let Some(dir) = zdotdir.filter(|_| zsh) {
            if let Some(original) = std::env::var_os("ZDOTDIR") {
                cmd.env("ATHENA_ORIG_ZDOTDIR", original);
            }
            cmd.env("ZDOTDIR", dir);
        }
        // Lets Claude Code find the window's IDE server; the file is absent while that is off.
        if let Ok(path) = athena_proto::ide_env_path() {
            for (key, value) in athena_proto::read_ide_env(&path) {
                cmd.env(key, value);
            }
        }
        // Apps launched from Finder get no locale, which breaks UTF-8 in most shells.
        if std::env::var_os("LANG").is_none() {
            cmd.env("LANG", "en_US.UTF-8");
        }

        let tty = pair
            .master
            .tty_name()
            .and_then(|path| std::fs::metadata(path).ok())
            .context("pty device")?
            .rdev() as u32;
        let child = pair.slave.spawn_command(cmd).context("spawn shell")?;
        drop(pair.slave);
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
            foreground: None,
            spawned_at: Instant::now(),
            ever_attached: false,
            lag_dropped: None,
            tty: Some(tty),
            master: pair.master,
            input,
        };
        Ok((pane, PaneOutput { reader, child }))
    }

    /// Process group currently in the foreground of the PTY, as the kernel reports it.
    pub fn leader(&self) -> Option<i32> {
        self.master.process_group_leader()
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

    /// Hangs up everything running on the pane's terminal, as closing a terminal window does.
    /// The returned handle kills what ignores the hangup.
    pub fn hang_up(&mut self) -> Option<Hangup> {
        let tty = self.tty.take()?;
        // An exited pane's terminal had no process left on it, so there is nothing to signal.
        if self.exit.is_some() {
            return None;
        }
        let hold = self.master.try_clone_reader().ok();
        process::signal_tty(tty, libc::SIGHUP);
        // Stopped jobs only act on the hangup once continued, as the kernel's own hangup does.
        process::signal_tty(tty, libc::SIGCONT);
        hold.map(|hold| Hangup { tty, _hold: hold })
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        if let Some(hangup) = self.hang_up() {
            let deadline = Instant::now() + KILL_GRACE;
            let _ = thread::Builder::new()
                .name("pane-reap".into())
                .spawn(move || hangup.reap(deadline));
        }
    }
}

/// A hung-up terminal, held open so its device cannot pass to a new pane before the kill.
pub struct Hangup {
    tty: u32,
    _hold: Box<dyn Read + Send>,
}

impl Hangup {
    /// Waits until `deadline` for everything on the terminal to exit, then kills what is left.
    pub fn reap(self, deadline: Instant) {
        while !process::on_tty(self.tty).is_empty() {
            if Instant::now() >= deadline {
                let killed = process::signal_tty(self.tty, libc::SIGKILL);
                tracing::info!("killed {killed} processes that ignored the hangup");
                return;
            }
            thread::sleep(REAP_POLL);
        }
    }
}

pub const READ_CHUNK: usize = MAX_OUTPUT_CHUNK;

const INHERITED: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PATH",
    "TMPDIR",
    "LANG",
    "SSH_AUTH_SOCK",
    "__CF_USER_TEXT_ENCODING",
];

fn inherited(key: &std::ffi::OsStr) -> bool {
    let key = key.to_string_lossy();
    INHERITED.contains(&key.as_ref()) || key.starts_with("LC_")
}
