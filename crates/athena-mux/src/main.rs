//! `athena-mux`: owns the shells so they outlive the Athena window.

mod pane;
mod ring;
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
        eprintln!("athena-mux: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    // SAFETY: setsid/signal are async-signal-safe and called before any threads exist.
    unsafe {
        libc::setsid();
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }

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
    eprintln!(
        "athena-mux: pid {} listening on {}",
        std::process::id(),
        socket.display()
    );

    let server = Arc::new(server::Server::new(socket));
    server.clone().start_idle_reaper();
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
