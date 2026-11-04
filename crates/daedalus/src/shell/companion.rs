use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{App, Context};

use super::Shell;
use super::windows;
use crate::companion::devices::Devices;
use crate::companion::server::{self, Config, Server};
use crate::companion::tailnet;

const PORT: u16 = 47123;
const WAITING_POLL: Duration = Duration::from_secs(10);
const RUNNING_POLL: Duration = Duration::from_secs(30);

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
            let mut told = false;
            loop {
                let Ok(wait) = cx.update(|cx| step(&name, &mut told, cx)) else {
                    return;
                };
                cx.background_executor().timer(wait).await;
            }
        });
        windows::watch_companion(task, cx);
    }
}

/// Brings the server in line with the Tailscale addresses now, and says how long until the next look.
fn step(name: &str, told: &mut bool, cx: &mut App) -> Duration {
    let (v4, v6) = tailnet::pick(&tailnet::local_addrs());
    let addrs = bind_addrs(v4, v6, PORT);
    if let Some(running) = windows::companion(cx) {
        if running.local_addrs() == addrs {
            return RUNNING_POLL;
        }
        windows::drop_companion_server(cx);
    }
    if addrs.is_empty() {
        if !std::mem::replace(told, true) {
            notice(
                "Waiting for Tailscale",
                "The phone companion starts when Tailscale is connected.".into(),
                cx,
            );
        }
        return WAITING_POLL;
    }
    match start(addrs, name) {
        Ok((server, commands)) => {
            *told = false;
            windows::set_companion(server, commands, cx);
            RUNNING_POLL
        }
        Err(e) => {
            if !std::mem::replace(told, true) {
                tracing::error!("could not start the phone companion: {e}");
                notice(
                    "Phone companion could not start",
                    format!("{e}. Daedalus keeps trying."),
                    cx,
                );
            }
            WAITING_POLL
        }
    }
}

fn start(
    addrs: Vec<SocketAddr>,
    name: &str,
) -> io::Result<(Server, async_channel::Receiver<server::Command>)> {
    let dir = daedalus_proto::data_dir()
        .map_err(io::Error::other)?
        .join("companion");
    Server::start(Config {
        addrs,
        devices: Arc::new(Mutex::new(Devices::load(&dir))),
        data_dir: dir,
        cli: tailnet::cli(),
        allow_loopback_for_tests: false,
        name: name.to_string(),
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
