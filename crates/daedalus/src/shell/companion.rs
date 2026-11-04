use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use gpui::{App, AsyncApp, Context, Task};

use super::Shell;
use super::item::ItemView;
use super::windows;
use crate::companion::devices::Devices;
use crate::companion::server::{self, Config, Server};
use crate::companion::tailnet;

pub(super) mod act;
pub(super) mod answer;
mod dialog;
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
    let started = match load_devices(cx).await? {
        Ok(devices) => {
            background
                .spawn(async move { start(addrs, &name, devices) })
                .await
        }
        Err(e) => Err(e),
    };
    match started {
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

fn data_dir() -> io::Result<PathBuf> {
    Ok(daedalus_proto::data_dir()
        .map_err(io::Error::other)?
        .join("companion"))
}

/// The shared device store, read from disk once on the background executor; `None` means the app is gone.
async fn load_devices(cx: &mut AsyncApp) -> Option<io::Result<Arc<Mutex<Devices>>>> {
    if let Some(devices) = cx.update(|cx| windows::companion_devices(cx)).ok()? {
        return Some(Ok(devices));
    }
    let loaded = cx
        .background_executor()
        .spawn(async { data_dir().map(|dir| Devices::load(&dir)) })
        .await;
    Some(match loaded {
        Ok(devices) => Ok(cx
            .update(|cx| windows::store_companion_devices(devices, cx))
            .ok()?),
        Err(e) => Err(e),
    })
}

/// Reads the device list for Settings, which shows it while the server is off.
pub(super) fn show_devices(cx: &mut App) {
    if windows::companion_devices(cx).is_some() {
        return;
    }
    cx.spawn(async move |cx| {
        if let Some(Ok(_)) = load_devices(cx).await {
            let _ = cx.update(redraw_windows);
        }
    })
    .detach();
}

/// Removes a paired device; the file write runs on the background executor.
pub(super) fn revoke_device(id: String, cx: &mut App) {
    let Some(devices) = windows::companion_devices(cx) else {
        return;
    };
    cx.spawn(async move |cx| {
        let target = id.clone();
        let saved = cx
            .background_executor()
            .spawn(async move {
                devices
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .revoke(&target)
            })
            .await;
        let _ = cx.update(|cx| {
            if let Some(server) = windows::companion(cx) {
                server.close_connections_of(&id);
            }
            if let Err(e) = saved {
                notice("Could not save the device list", format!("{e}"), cx);
            }
            redraw_windows(cx);
        });
    })
    .detach();
}

/// Settings is a cached view, so the windows' own redraw does not reach its device list.
pub(super) fn redraw_windows(cx: &mut App) {
    for handle in windows::handles(cx) {
        let _ = handle.update(cx, |shell, _, cx| {
            for item in shell.items.values() {
                if let ItemView::Settings(view) = item {
                    view.update(cx, |_, cx| cx.notify());
                }
            }
            cx.notify();
        });
    }
}

fn start(
    addrs: Vec<SocketAddr>,
    name: &str,
    devices: Arc<Mutex<Devices>>,
) -> io::Result<(Server, async_channel::Receiver<server::Command>)> {
    let dir = data_dir()?;
    let mux_socket = daedalus_proto::socket_path().map_err(io::Error::other)?;
    let home = std::env::home_dir();
    if home.is_none() {
        tracing::warn!("companion: no home directory, so transcripts cannot be read");
    }
    Server::start(Config {
        addrs,
        devices,
        data_dir: dir,
        cli: tailnet::cli(),
        allow_loopback_for_tests: false,
        name: name.to_string(),
        mux_socket,
        home,
    })
}

pub(super) fn notice(title: &str, body: String, cx: &mut App) {
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
