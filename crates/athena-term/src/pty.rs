use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

const READ_CHUNK: usize = 64 * 1024;
const OUTPUT_QUEUE: usize = 64;

pub enum PtyEvent {
    Output(Vec<u8>),
    Exited(Option<i32>),
}

/// A login shell on a local PTY; dropping it hangs up the shell.
pub struct LocalPty {
    master: Box<dyn MasterPty + Send>,
    input: mpsc::Sender<Vec<u8>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

impl LocalPty {
    pub fn spawn(
        cwd: &Path,
        rows: u16,
        cols: u16,
    ) -> Result<(Self, async_channel::Receiver<PtyEvent>)> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open pty")?;

        let mut cmd = CommandBuilder::new_default_prog();
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "athena");
        cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        // Apps launched from Finder get no locale, which breaks UTF-8 in most shells.
        if std::env::var_os("LANG").is_none() {
            cmd.env("LANG", "en_US.UTF-8");
        }

        let mut child = pair.slave.spawn_command(cmd).context("spawn shell")?;
        drop(pair.slave);
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;

        // Bounded so a flooding program blocks on the PTY instead of growing memory.
        let (out_tx, out_rx) = async_channel::bounded(OUTPUT_QUEUE);
        thread::Builder::new()
            .name("pty-read".into())
            .spawn(move || {
                let mut buf = vec![0u8; READ_CHUNK];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if out_tx
                                .send_blocking(PtyEvent::Output(buf[..n].to_vec()))
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
                let code = child.wait().ok().map(|s| s.exit_code() as i32);
                let _ = out_tx.send_blocking(PtyEvent::Exited(code));
            })?;

        let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>();
        thread::Builder::new()
            .name("pty-write".into())
            .spawn(move || {
                for bytes in in_rx {
                    if writer
                        .write_all(&bytes)
                        .and_then(|_| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            })?;

        Ok((
            Self {
                master: pair.master,
                input: in_tx,
                killer,
            },
            out_rx,
        ))
    }

    pub fn write(&self, bytes: Vec<u8>) {
        let _ = self.input.send(bytes);
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }
}

impl Drop for LocalPty {
    fn drop(&mut self) {
        let _ = self.killer.kill();
    }
}
