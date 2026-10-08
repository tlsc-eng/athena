//! `athena-mux`: owns the shells so they outlive the Athena window.

mod notices;
mod pane;
mod process;
mod ring;
mod scanner;
mod server;

use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result, bail};

fn main() {
    if let Err(err) = run() {
        if tracing::dispatcher::has_been_set() {
            tracing::error!("{err:#}");
        } else {
            eprintln!("athena-mux: {err:#}");
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    // SAFETY: setsid/signal are async-signal-safe and called before any threads exist.
    unsafe {
        libc::setsid();
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        // Spawned from an AppKit app, which ignores SIGTERM; ignored signals survive exec.
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
        libc::signal(libc::SIGINT, libc::SIG_DFL);
    }
    athena_proto::logging::init(&athena_proto::log_path()?, "ATHENA_LOG")?;

    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(athena_proto::lock_path()?)?;
    // SAFETY: flock on a descriptor we own; held for the life of the process.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another athena-mux is already running");
    }

    let socket = athena_proto::socket_path()?;
    // Holding the lock means any socket file left behind belongs to a dead daemon.
    let _ = fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    tracing::info!(
        "pid {} listening on {}",
        std::process::id(),
        socket.display()
    );

    let server = Arc::new(server::Server::new(
        socket,
        install_zsh_integration(),
        notify_after(),
    ));
    server.clone().start_idle_reaper();
    server.clone().start_foreground_poller();
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let server = server.clone();
        thread::Builder::new()
            .name("mux-client".into())
            .spawn(move || server.serve(stream))?;
    }
    drop(lock);
    Ok(())
}

/// Writes the zsh startup shim; `None` disables integration (opted out or not writable).
fn install_zsh_integration() -> Option<std::path::PathBuf> {
    if std::env::var_os("ATHENA_SHELL_INTEGRATION").is_some_and(|v| v == "0") {
        return None;
    }
    let dir = athena_proto::data_dir().ok()?.join("shell/zsh");
    fs::create_dir_all(&dir).ok()?;
    fs::write(dir.join(".zshenv"), include_str!("zshenv")).ok()?;
    Some(dir)
}

/// Commands shorter than this finish silently.
fn notify_after() -> std::time::Duration {
    let secs = std::env::var("ATHENA_NOTIFY_AFTER_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    std::time::Duration::from_secs(secs)
}
