use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{App, AsyncApp, Context, Task};

use super::Shell;
use super::windows;
use crate::companion::devices::Devices;
use crate::companion::server::{self, Config, Server};
use crate::companion::tailnet;

mod snapshot;

const PORT: u16 = 47123;
const WAITING_POLL: Duration = Duration::from_secs(10);
const RUNNING_POLL: Duration = Duration::from_secs(30);
const TICK: Duration = Duration::from_millis(250);
const REBUILD: Duration = Duration::from_secs(1);
const SESSIONS_REFRESH: Duration = Duration::from_secs(5);

impl Shell {
    /// Starts the phone companion's watch when `companion.enabled` is on and stops everything when it is off.
    pub(super) fn sync_companion(&mut self, cx: &mut Context<Self>) {
        if !self.settings.file.companion_enabled() {
            windows::stop_companion(cx);
            return;
        }
        if windows::companion_watching(cx) {
            return;
        }
        // App-wide, so it outlives the window that turned the setting on.
        let app: &mut App = cx;
        let task = app.spawn(async move |cx| {
            let name = cx
                .background_executor()
                .spawn(async { computer_name() })
                .await;
            let mut told = Told::default();
            loop {
                let Some(wait) = look(&name, &mut told, cx).await else {
                    return;
                };
                cx.background_executor().timer(wait).await;
            }
        });
        windows::watch_companion(task, cx);
    }
}

/// Which notices the watch has shown, so a retry loop does not repeat them.
#[derive(Default)]
struct Told {
    waiting: bool,
    failed: bool,
}

/// Brings the server in line with the Tailscale addresses now, and says how long until the next look.
/// Starting runs on the background executor; `None` means the app is gone.
async fn look(name: &str, told: &mut Told, cx: &mut AsyncApp) -> Option<Duration> {
    let background = cx.background_executor().clone();
    if let Some(closing) = cx.update(windows::take_companion_closing).ok()? {
        let _ = closing.await;
    }
    let addrs = background
        .spawn(async {
            let (v4, v6) = tailnet::pick(&tailnet::local_addrs());
            bind_addrs(v4, v6, PORT)
        })
        .await;
    let running = cx
        .update(|cx| windows::companion(cx).map(|server| server.local_addrs() == addrs))
        .ok()?;
    match running {
        Some(true) => return Some(RUNNING_POLL),
        // The old server holds the port until its shutdown resolves.
        Some(false) => {
            if let Some(server) = cx.update(windows::take_companion).ok()? {
                let _ = server.shutdown().await;
            }
        }
        None => {}
    }
    if addrs.is_empty() {
        if !std::mem::replace(&mut told.waiting, true) {
            cx.update(|cx| {
                notice(
                    "Waiting for Tailscale",
                    "The phone companion starts when Tailscale is connected.".into(),
                    cx,
                )
            })
            .ok()?;
        }
        return Some(WAITING_POLL);
    }
    let name = name.to_string();
    match background.spawn(async move { start(addrs, &name) }).await {
        Ok((server, commands)) => {
            told.failed = false;
            cx.update(|cx| windows::set_companion(server, commands, cx))
                .ok()?;
            Some(RUNNING_POLL)
        }
        Err(e) => {
            if !std::mem::replace(&mut told.failed, true) {
                tracing::error!("could not start the phone companion: {e}");
                cx.update(|cx| {
                    notice(
                        "Phone companion could not start",
                        format!("{e}. Daedalus keeps trying."),
                        cx,
                    )
                })
                .ok()?;
            }
            Some(WAITING_POLL)
        }
    }
}

/// Keeps the server's snapshot current while a phone is connected; it does nothing without one.
pub(super) fn tick_snapshots(cx: &mut App) -> Task<()> {
    cx.spawn(async move |cx| {
        let mut built: Option<Instant> = None;
        let mut refreshed: Option<Instant> = None;
        loop {
            cx.background_executor().timer(TICK).await;
            let ticked = cx.update(|cx| {
                if windows::companion_clients(cx) == 0 {
                    (built, refreshed) = (None, None);
                    return;
                }
                if refreshed.is_none_or(|at| at.elapsed() >= SESSIONS_REFRESH) {
                    refresh_sessions(cx);
                    refreshed = Some(Instant::now());
                }
                let dirty = windows::take_companion_dirty(cx);
                if dirty || built.is_none_or(|at| at.elapsed() >= REBUILD) {
                    push_snapshot(cx);
                    built = Some(Instant::now());
                }
            });
            if ticked.is_err() {
                return;
            }
        }
    })
}

/// Builds the snapshot and hands it to the server, which skips one equal to what it holds.
pub(super) fn push_snapshot(cx: &mut App) {
    windows::take_companion_dirty(cx);
    let snapshot = snapshot::to_snapshot(snapshot::gather(cx));
    windows::prune_companion_needs(&snapshot, cx);
    if let Some(server) = windows::companion(cx) {
        server.set_snapshot(Arc::new(snapshot));
    }
}

/// Loads each window's session list even when its Sessions panel is hidden, so labels are there.
fn refresh_sessions(cx: &mut App) {
    for handle in windows::handles(cx) {
        let _ = handle.update(cx, |shell, _, cx| {
            if let Some(root) = shell.workspace.active_project().map(|p| p.root.clone()) {
                shell.refresh_sessions(&root, cx);
            }
        });
    }
}

fn start(
    addrs: Vec<SocketAddr>,
    name: &str,
) -> io::Result<(Server, async_channel::Receiver<server::Command>)> {
    let dir = daedalus_proto::data_dir()
        .map_err(io::Error::other)?
        .join("companion");
    let mux_socket = daedalus_proto::socket_path().map_err(io::Error::other)?;
    Server::start(Config {
        addrs,
        devices: Arc::new(Mutex::new(Devices::load(&dir))),
        data_dir: dir,
        cli: tailnet::cli(),
        allow_loopback_for_tests: false,
        name: name.to_string(),
        mux_socket,
    })
}

fn notice(title: &str, body: String, cx: &mut App) {
    if let Some(window) = windows::focused(cx) {
        let _ = window.update(cx, |shell, _, cx| shell.transient_notice(title, body, cx));
    }
}

/// Only the Tailscale addresses, never a wildcard, so nothing off the tailnet can connect.
fn bind_addrs(v4: Option<Ipv4Addr>, v6: Option<Ipv6Addr>, port: u16) -> Vec<SocketAddr> {
    v4.map(|ip| SocketAddr::from((ip, port)))
        .into_iter()
        .chain(v6.map(|ip| SocketAddr::from((ip, port))))
        .collect()
}

/// The name System Settings shows for this Mac, else the hostname without `.local`.
fn computer_name() -> String {
    let run = |program: &str, args: &[&str]| {
        let out = Command::new(program).args(args).output().ok()?;
        let text = String::from_utf8(out.stdout).ok()?;
        let text = text.trim();
        (out.status.success() && !text.is_empty()).then(|| text.to_string())
    };
    run("/usr/sbin/scutil", &["--get", "ComputerName"])
        .or_else(|| {
            run("/bin/hostname", &[]).map(|host| host.trim_end_matches(".local").to_string())
        })
        .unwrap_or_else(|| "Mac".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_addrs_hold_only_the_tailscale_addresses_on_the_fixed_port() {
        let v4 = Ipv4Addr::new(100, 64, 1, 2);
        let v6 = "fd7a:115c:a1e0::1".parse::<Ipv6Addr>().unwrap();
        assert_eq!(
            bind_addrs(Some(v4), Some(v6), PORT),
            vec![SocketAddr::from((v4, PORT)), SocketAddr::from((v6, PORT))]
        );
        assert_eq!(
            bind_addrs(Some(v4), None, PORT),
            vec![SocketAddr::from((v4, PORT))]
        );
        assert_eq!(
            bind_addrs(None, Some(v6), PORT),
            vec![SocketAddr::from((v6, PORT))]
        );
        assert!(bind_addrs(None, None, PORT).is_empty());
    }
}
