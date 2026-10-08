use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crate::{ClientMsg, PROTO_VERSION, ServerMsg, read_frame, write_frame};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const SPAWN_WAIT: Duration = Duration::from_secs(3);
const SPAWN_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("session daemon is not running")]
    NotRunning,
    #[error(
        "session daemon speaks protocol {daemon} but this Athena needs {PROTO_VERSION}; \
         run `athena mux stop` to restart it, which ends the shells it runs ({panes})"
    )]
    VersionMismatch { daemon: u32, panes: u32, pid: u32 },
    #[error("session daemon: {0}")]
    Io(#[from] io::Error),
}

/// The write half of a daemon connection; the read half is returned separately by `connect`.
pub struct Connection {
    writer: Mutex<UnixStream>,
    pub daemon_pid: u32,
}

impl Connection {
    pub fn send(&self, msg: &ClientMsg) -> io::Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        write_frame(&mut *writer, msg)
    }
}

/// Connects and completes the `Hello` exchange; the returned stream yields `ServerMsg` frames.
pub fn connect(socket: &Path) -> Result<(Connection, UnixStream), ConnectError> {
    let mut stream = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(ConnectError::NotRunning);
        }
        Err(e) => return Err(e.into()),
    };
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    write_frame(
        &mut stream,
        &ClientMsg::Hello {
            proto: PROTO_VERSION,
        },
    )?;
    let hello: Option<ServerMsg> = read_frame(&mut stream)?;
    // A daemon of another version hangs up after its Hello, and macOS then rejects setsockopt,
    // so the version is checked before touching the socket again.
    let pid = match hello {
        Some(ServerMsg::Hello { proto, pid, .. }) if proto == PROTO_VERSION => pid,
        Some(ServerMsg::Hello { proto, pid, panes }) => {
            return Err(ConnectError::VersionMismatch {
                daemon: proto,
                panes,
                pid,
            });
        }
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "bad handshake").into()),
    };
    stream.set_read_timeout(None)?;
    let reader = stream.try_clone()?;
    Ok((
        Connection {
            writer: Mutex::new(stream),
            daemon_pid: pid,
        },
        reader,
    ))
}

/// Connects, starting `daemon` first if no daemon is listening.
///
/// A daemon from an older build with no live sessions is replaced silently; one that still has
/// sessions is left alone and reported, so restarting it is the user's call.
pub fn connect_or_spawn(
    socket: &Path,
    daemon: &Path,
    log: &Path,
) -> Result<(Connection, UnixStream), ConnectError> {
    match connect(socket) {
        Ok(pair) => return Ok(pair),
        Err(ConnectError::NotRunning) => {}
        Err(ConnectError::VersionMismatch { panes: 0, pid, .. }) => {
            // SAFETY: plain kill(2) on a pid the daemon reported for itself.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            thread::sleep(Duration::from_millis(200));
        }
        Err(e) => return Err(e),
    }
    tracing::info!("spawning athena-mux at {}", daemon.display());
    spawn_daemon(daemon, log)?;
    let deadline = Instant::now() + SPAWN_WAIT;
    loop {
        match connect(socket) {
            Err(ConnectError::NotRunning) if Instant::now() < deadline => thread::sleep(SPAWN_POLL),
            Err(ConnectError::NotRunning) => {
                tracing::warn!("athena-mux did not listen within {SPAWN_WAIT:?}");
                return Err(ConnectError::NotRunning);
            }
            other => return other,
        }
    }
}

fn spawn_daemon(daemon: &Path, log: &Path) -> io::Result<()> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log)?;
    let mut child = Command::new(daemon)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?;
    // Reap it if it exits while we are still running; after we exit, launchd adopts it.
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
