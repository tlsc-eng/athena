use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::{ClientMsg, PROTO_VERSION, ServerMsg, read_frame, write_frame};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const SPAWN_WAIT: Duration = Duration::from_secs(10);
const SPAWN_POLL: Duration = Duration::from_millis(50);
const EXITED_GRACE: Duration = Duration::from_millis(500);
const EXIT_WAIT: Duration = Duration::from_secs(1);

static SPAWN: Mutex<()> = Mutex::new(());

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("session daemon is not running")]
    NotRunning,
    #[error(
        "session daemon speaks protocol {daemon} but this Athena needs {PROTO_VERSION}; \
         run `athena mux stop` to restart it, which ends the shells it runs ({panes})"
    )]
    VersionMismatch { daemon: u32, panes: u32, pid: u32 },
    #[error("session daemon exited while starting; see mux.log")]
    DaemonExited,
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
        Err(ConnectError::NotRunning | ConnectError::VersionMismatch { panes: 0, .. }) => {}
        other => return other,
    }
    // Every restored terminal connects at once on launch; only the first may start a daemon.
    let _spawning = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
    match connect(socket) {
        Ok(pair) => return Ok(pair),
        Err(ConnectError::NotRunning) => {}
        Err(ConnectError::VersionMismatch { panes: 0, pid, .. }) => {
            tracing::info!("replacing idle session daemon {pid} from an older build");
            terminate(pid);
        }
        Err(e) => return Err(e),
    }
    tracing::info!("spawning athena-mux at {}", daemon.display());
    let exited = spawn_daemon(daemon, log)?;
    let started = Instant::now();
    let mut exited_at = None;
    loop {
        match connect(socket) {
            Err(ConnectError::NotRunning) => {}
            other => return other,
        }
        if exited_at.is_none() && exited.load(Ordering::Acquire) {
            exited_at = Some(Instant::now());
        }
        // Ours may have lost the lock to a daemon another process is starting, so wait briefly.
        if exited_at.is_some_and(|t| t.elapsed() >= EXITED_GRACE) {
            tracing::error!("athena-mux exited before it was listening");
            return Err(ConnectError::DaemonExited);
        }
        if started.elapsed() >= SPAWN_WAIT {
            tracing::warn!("athena-mux did not listen within {SPAWN_WAIT:?}");
            return Err(ConnectError::NotRunning);
        }
        thread::sleep(SPAWN_POLL);
    }
}

/// Ends the daemon on `socket` and the shells it runs, whatever its version; false if none ran.
pub fn stop_daemon(socket: &Path) -> Result<bool, ConnectError> {
    match connect(socket) {
        Ok((conn, _)) => {
            conn.send(&ClientMsg::Shutdown)?;
            wait_for_exit(conn.daemon_pid);
            Ok(true)
        }
        Err(ConnectError::NotRunning) => Ok(false),
        Err(ConnectError::VersionMismatch { pid, .. }) => {
            tracing::info!("stopping session daemon {pid} from an older build");
            terminate(pid);
            Ok(true)
        }
        Err(e) => Err(e),
    }
}

fn terminate(pid: u32) {
    // SAFETY: plain kill(2) on a pid the daemon reported for itself.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    // Daemons before 0.2 started with SIGTERM blocked when the app spawned them.
    if !wait_for_exit(pid) {
        // SAFETY: as above.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        wait_for_exit(pid);
    }
}

/// Whether `pid` is gone within `EXIT_WAIT`.
fn wait_for_exit(pid: u32) -> bool {
    let deadline = Instant::now() + EXIT_WAIT;
    loop {
        // SAFETY: signal 0 only checks that the pid exists.
        if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(SPAWN_POLL);
    }
}

/// Starts the daemon; the flag turns true if it exits while this process is still running.
fn spawn_daemon(daemon: &Path, log: &Path) -> io::Result<Arc<AtomicBool>> {
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
    let exited = Arc::new(AtomicBool::new(false));
    let flag = exited.clone();
    // Reap it if it exits while we are still running; after we exit, launchd adopts it.
    thread::spawn(move || {
        let _ = child.wait();
        flag.store(true, Ordering::Release);
    });
    Ok(exited)
}
