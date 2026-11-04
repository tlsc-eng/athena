use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use daedalus_ui::{ActiveTheme, Button, ButtonKind, Space};
use futures::future::Either;
use gpui::{
    AnyElement, App, Context, FocusHandle, FontWeight, KeyDownEvent, MouseButton, PromptLevel,
    Task, Window, div, prelude::*, px,
};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use qrcode::{Color, QrCode};

use super::{Shell, windows};
use crate::companion::devices::PairingCode;
use crate::companion::server::{PairPeer, Verdict};
use crate::companion::tailnet;

/// Shorter than the server's 60 s wait, so the phone is told no before it gives up.
const ANSWER_FOR: Duration = Duration::from_secs(55);
const MODULE: f32 = 4.;
const QUIET_MODULES: usize = 4;

/// Everything in a URL's unreserved set stays readable; the rest is escaped.
const QUERY_VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

struct Grid {
    width: usize,
    dark: Vec<bool>,
}

/// The pairing modal's state; the code it shows is the one the server accepts while it is open.
pub(super) struct Pairing {
    ip: Option<Ipv4Addr>,
    port: u16,
    sid: String,
    /// Set once the code is made and the MagicDNS lookup has finished, whether or not it found a name.
    ready: Option<Ready>,
    focus: FocusHandle,
    restore: Option<FocusHandle>,
    _tasks: Vec<Task<()>>,
}

struct Ready {
    code: String,
    expires: Instant,
    cli_missing: bool,
    grid: Option<Grid>,
}

/// What the background lookup finds; the code and the CLI search touch the disk.
struct Looked {
    code: PairingCode,
    cli_missing: bool,
    host: Option<String>,
}

fn pair_url(host: Option<&str>, ip: Option<Ipv4Addr>, port: u16, code: &str, sid: &str) -> String {
    let escape = |value: &str| utf8_percent_encode(value, QUERY_VALUE).to_string();
    let mut parts = Vec::new();
    if let Some(host) = host {
        parts.push(format!("host={}", escape(host)));
    }
    if let Some(ip) = ip {
        parts.push(format!("ip={}", escape(&ip.to_string())));
    }
    parts.push(format!("port={port}"));
    parts.push(format!("code={}", escape(code)));
    parts.push(format!("sid={}", escape(sid)));
    format!("daedalus://pair?{}", parts.join("&"))
}

fn grid(url: &str) -> Option<Grid> {
    let code = QrCode::new(url.as_bytes()).ok()?;
    Some(Grid {
        width: code.width(),
        dark: code
            .to_colors()
            .into_iter()
            .map(|c| c == Color::Dark)
            .collect(),
    })
}

/// A row as runs of (dark, length), so a row is a few boxes instead of one per module.
fn runs(row: &[bool]) -> Vec<(bool, usize)> {
    let mut runs: Vec<(bool, usize)> = Vec::new();
    for &dark in row {
        match runs.last_mut() {
            Some((last, len)) if *last == dark => *len += 1,
            _ => runs.push((dark, 1)),
        }
    }
    runs
}

fn countdown(left: Duration) -> String {
    let secs = left.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// The title and detail of the question asked before a phone is paired.
fn pair_prompt(device: &str, peer: &PairPeer) -> (String, String) {
    let detail = match &peer.tailnet {
        Some(p) => format!(
            "{device} ({}, {}) will see Claude sessions and can type in terminals.",
            p.node, p.login
        ),
        None => format!(
            "{device} will see Claude sessions and can type in terminals. \
             Tailscale identity could not be verified."
        ),
    };
    (format!("Allow {device} to pair?"), detail)
}

/// Asks in the window showing the pairing modal, else the focused one; no window means no.
pub(super) fn ask_to_pair(
    device: String,
    peer: PairPeer,
    reply: tokio::sync::oneshot::Sender<Verdict>,
    cx: &mut App,
) {
    let modal = windows::pairing_window(cx).and_then(|w| {
        windows::handles(cx)
            .into_iter()
            .find(|h| gpui::AnyWindowHandle::from(*h) == w)
    });
    let Some(target) = modal.or_else(|| windows::focused(cx)) else {
        let _ = reply.send(Verdict::Deny);
        return;
    };
    let _ = target.update(cx, |shell, window, cx| {
        shell.confirm_pair(device, peer, reply, window, cx)
    });
}

impl Shell {
    pub(super) fn open_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(open) = &self.pairing {
            window.focus(&open.focus);
            return;
        }
        if !self.settings.file.companion_enabled() {
            self.transient_notice(
                "Pair iPhone",
                "Turn on Phone Companion in Settings to pair an iPhone.",
                cx,
            );
            return;
        }
        let Some(server) = windows::companion(cx) else {
            self.transient_notice(
                "Waiting for Tailscale",
                "The phone companion starts when Tailscale is connected.",
                cx,
            );
            return;
        };
        let addrs = server.local_addrs();
        let ip = addrs.iter().find_map(|a| match a {
            SocketAddr::V4(a) => Some(*a.ip()),
            SocketAddr::V6(_) => None,
        });
        let port = addrs.first().map_or(0, SocketAddr::port);
        let sid = server.id().to_string();
        // Another window's modal would otherwise clear this code when it closes.
        let previous = windows::pairing_window(cx).filter(|w| *w != self.window_handle);
        windows::set_pairing_window(Some(self.window_handle), cx);
        if let Some(previous) = previous.and_then(|w| w.downcast::<Shell>()) {
            cx.defer(move |cx| {
                let _ = previous.update(cx, |shell, window, cx| shell.close_pairing(window, cx));
            });
        }

        let lookup = cx.spawn_in(window, async move |this, cx| {
            let looked = cx
                .background_executor()
                .spawn(async {
                    let code = PairingCode::new(Instant::now())?;
                    let cli = tailnet::cli();
                    Ok::<_, io::Error>(Looked {
                        code,
                        cli_missing: cli.is_none(),
                        host: cli.and_then(|cli| tailnet::self_dns(&cli)),
                    })
                })
                .await;
            let _ = this.update_in(cx, |shell, window, cx| {
                shell.pairing_resolved(looked, window, cx)
            });
        });
        let tick = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this
                    .update_in(cx, |shell, window, cx| shell.pairing_tick(window, cx))
                    .is_err()
                {
                    return;
                }
            }
        });
        let focus = cx.focus_handle();
        let restore = window.focused(cx);
        window.focus(&focus);
        self.pairing = Some(Pairing {
            ip,
            port,
            sid,
            ready: None,
            focus,
            restore,
            _tasks: vec![lookup, tick],
        });
        cx.notify();
    }

    fn pairing_resolved(
        &mut self,
        looked: io::Result<Looked>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pairing.is_none() {
            return;
        }
        let looked = match looked {
            Ok(looked) => looked,
            Err(e) => {
                self.transient_notice("Could not make a pairing code", e.to_string(), cx);
                self.close_pairing(window, cx);
                return;
            }
        };
        let Some(server) = windows::companion(cx) else {
            self.close_pairing(window, cx);
            return;
        };
        let (expires, text) = (looked.code.expires(), looked.code.code().to_string());
        server.set_pairing_code(Some(looked.code));
        let Some(open) = self.pairing.as_mut() else {
            return;
        };
        let grid = (looked.host.is_some() || open.ip.is_some())
            .then(|| {
                let url = pair_url(looked.host.as_deref(), open.ip, open.port, &text, &open.sid);
                grid(&url)
            })
            .flatten();
        open.ready = Some(Ready {
            code: text,
            expires,
            cli_missing: looked.cli_missing,
            grid,
        });
        cx.notify();
    }

    fn pairing_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.pairing.as_ref().map(|open| open.ready.as_ref()) {
            Some(Some(ready)) if Instant::now() >= ready.expires => self.close_pairing(window, cx),
            Some(_) => cx.notify(),
            None => {}
        }
    }

    /// Closes the modal and, if it is still the one pairing, stops the server accepting its code.
    fn close_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(closed) = self.pairing.take() else {
            return;
        };
        if windows::pairing_window(cx) == Some(self.window_handle) {
            windows::set_pairing_window(None, cx);
            if let Some(server) = windows::companion(cx) {
                server.set_pairing_code(None);
            }
        }
        if let Some(handle) = closed.restore {
            window.focus(&handle);
        }
        cx.notify();
    }

    fn confirm_pair(
        &mut self,
        device: String,
        peer: PairPeer,
        reply: tokio::sync::oneshot::Sender<Verdict>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, detail) = pair_prompt(&device, &peer);
        let answer = window.prompt(
            PromptLevel::Warning,
            &title,
            Some(&detail),
            &["Allow", "Don't Allow"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let timeout = cx.background_executor().timer(ANSWER_FOR);
            let choice = futures::future::select(answer, timeout).await;
            let allowed = matches!(choice, Either::Left((Ok(0), _)));
            let verdict = if allowed {
                Verdict::Allow
            } else {
                Verdict::Deny
            };
            let _ = reply.send(verdict);
            let _ = this.update_in(cx, |shell, window, cx| shell.close_pairing(window, cx));
            // The sheet stays up after the timeout, and an answer to it pairs nothing.
            if let Either::Right((_, late)) = choice
                && late.await.is_ok()
            {
                let _ = this.update(cx, |shell, cx| {
                    shell.transient_notice("Pairing request timed out", "Scan the code again.", cx)
                });
            }
        })
        .detach();
    }

    pub(super) fn render_pairing(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.pairing.as_ref()?;
        let t = cx.theme().clone();
        let ready = open.ready.as_ref();
        let module = px(MODULE);
        // The Camera app may not read an inverted code, so the dark theme still gets dark on light.
        let light = daedalus_ui::Theme::light(false).color;
        let code = match ready.and_then(|r| r.grid.as_ref()) {
            Some(grid) => div()
                .p(px(QUIET_MODULES as f32 * MODULE))
                .flex()
                .flex_col()
                .bg(light.surface)
                .border_1()
                .border_color(t.color.border)
                .children(grid.dark.chunks(grid.width).map(|row| {
                    div()
                        .flex()
                        .h(module)
                        .children(runs(row).into_iter().map(|(dark, len)| {
                            div()
                                .flex_none()
                                .h(module)
                                .w(px(len as f32 * MODULE))
                                .when(dark, |el| el.bg(light.content))
                        }))
                }))
                .into_any_element(),
            None => div()
                .h(t.ui(120.))
                .flex()
                .items_center()
                .justify_center()
                .text_color(t.color.content_muted)
                .child(match (ready, open.ip) {
                    (None, _) => "Preparing the code…",
                    (Some(_), Some(_)) => "Could not draw the code.",
                    (Some(_), None) => "This Mac has no Tailscale IPv4 address to put in the code.",
                })
                .into_any_element(),
        };
        let panel = div()
            .id("pairing")
            .track_focus(&open.focus)
            .key_context("Pairing")
            .occlude()
            .w(t.ui(340.))
            .p(t.space(Space::Xl))
            .flex()
            .flex_col()
            .items_center()
            .gap(t.space(Space::Lg))
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_panel)
            .shadow(vec![t.popover_shadow()])
            .text_size(t.typography.caption)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.close_pairing(window, cx);
                }
            }))
            .child(
                div()
                    .text_size(t.typography.body)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.color.content)
                    .child("Pair iPhone"),
            )
            .child(
                div()
                    .text_color(t.color.content_muted)
                    .child("Scan this code with Daedalus on your iPhone."),
            )
            .child(code)
            .children(ready.map(|r| {
                div()
                    .font_family(t.typography.mono.clone())
                    .text_color(t.color.content)
                    .child(r.code.clone())
            }))
            .children(ready.map(|r| {
                let left = r.expires.saturating_duration_since(Instant::now());
                div()
                    .text_color(t.color.content_muted)
                    .child(format!("Expires in {}", countdown(left)))
            }))
            .when(ready.is_some_and(|r| r.cli_missing), |el| {
                el.child(
                    div()
                        .text_color(t.color.warning)
                        .child("Tailscale CLI not found; identity not verified."),
                )
            })
            .child(
                Button::new("pairing-cancel", "Cancel", ButtonKind::Secondary)
                    .on_click(cx.listener(|this, _, window, cx| this.close_pairing(window, cx))),
            );
        Some(
            div()
                .id("pairing-layer")
                .absolute()
                .inset_0()
                .occlude()
                .bg(t.color.overlay)
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(panel)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::tailnet::Peer;

    #[test]
    fn companion_pair_url_carries_every_part_in_order() {
        assert_eq!(
            pair_url(
                Some("mac.tail1234.ts.net"),
                Some(Ipv4Addr::new(100, 64, 1, 2)),
                47123,
                "0123456789abcdef0123456789abcdef",
                "a1b2c3d4e5f60718"
            ),
            "daedalus://pair?host=mac.tail1234.ts.net&ip=100.64.1.2&port=47123\
             &code=0123456789abcdef0123456789abcdef&sid=a1b2c3d4e5f60718"
        );
    }

    #[test]
    fn companion_pair_url_leaves_out_a_host_or_ip_it_does_not_have() {
        let ip = Some(Ipv4Addr::new(100, 64, 1, 2));
        assert_eq!(
            pair_url(None, ip, 47123, "c0de", "5id"),
            "daedalus://pair?ip=100.64.1.2&port=47123&code=c0de&sid=5id"
        );
        assert_eq!(
            pair_url(Some("mac"), None, 47123, "c0de", "5id"),
            "daedalus://pair?host=mac&port=47123&code=c0de&sid=5id"
        );
        assert_eq!(
            pair_url(None, None, 1, "c", "s"),
            "daedalus://pair?port=1&code=c&sid=s"
        );
    }

    #[test]
    fn companion_pair_url_escapes_what_would_end_a_value() {
        assert_eq!(
            pair_url(Some("a b&c=d#é"), None, 1, "x/y", "s"),
            "daedalus://pair?host=a%20b%26c%3Dd%23%C3%A9&port=1&code=x%2Fy&sid=s"
        );
    }

    #[test]
    fn companion_qr_grid_is_square_and_holds_the_finder_patterns() {
        let url = pair_url(
            Some("mac.tail1234.ts.net"),
            Some(Ipv4Addr::new(100, 64, 1, 2)),
            47123,
            "0123456789abcdef0123456789abcdef",
            "a1b2c3d4e5f60718",
        );
        let g = grid(&url).unwrap();
        assert_eq!(g.dark.len(), g.width * g.width);
        assert!((g.width as f32 + 2. * QUIET_MODULES as f32) * MODULE <= 260.);
        // Each corner but the bottom-right opens with a dark module of a finder pattern.
        assert!(g.dark[0] && g.dark[g.width - 1] && g.dark[g.width * (g.width - 1)]);
    }

    #[test]
    fn companion_qr_rows_merge_neighbours_of_one_colour() {
        assert_eq!(
            runs(&[true, true, false, true, false, false, false]),
            vec![(true, 2), (false, 1), (true, 1), (false, 3)]
        );
        assert!(runs(&[]).is_empty());
    }

    #[test]
    fn companion_countdown_shows_minutes_and_padded_seconds() {
        assert_eq!(countdown(Duration::from_secs(300)), "5:00");
        assert_eq!(countdown(Duration::from_secs(61)), "1:01");
        assert_eq!(countdown(Duration::ZERO), "0:00");
    }

    #[test]
    fn companion_pair_prompt_names_the_tailnet_device_or_says_it_is_unverified() {
        let ip = "100.64.1.9".parse().unwrap();
        let verified = PairPeer {
            ip,
            tailnet: Some(Peer {
                login: "jason@example.com".into(),
                node: "jasons-iphone".into(),
                os: "iOS".into(),
            }),
        };
        let (title, detail) = pair_prompt("Jason's iPhone", &verified);
        assert_eq!(title, "Allow Jason's iPhone to pair?");
        assert_eq!(
            detail,
            "Jason's iPhone (jasons-iphone, jason@example.com) will see Claude sessions and can type in terminals."
        );
        let (_, detail) = pair_prompt("Jason's iPhone", &PairPeer { ip, tailnet: None });
        assert!(detail.ends_with("Tailscale identity could not be verified."));
    }
}
