use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use athena_proto::NoticeKind;
use athena_ui::{ActiveTheme, ButtonKind};
use gpui::{
    AnyElement, Context, FontWeight, Hsla, MouseButton, PromptLevel, Window, div, prelude::*, px,
};

use super::Shell;
use crate::usage::{self, Profile, Status};

const POLL: Duration = Duration::from_secs(5 * 60);
/// Refresh presses closer together than this reuse the last reading.
const MIN_REFRESH: Duration = Duration::from_secs(60);
const LEVELS: [f32; 2] = [75., 90.];

#[derive(Default)]
pub(super) struct UsageState {
    readings: Vec<(Profile, Status)>,
    fetched: Option<Instant>,
    open: bool,
    /// Highest threshold already announced per profile and window, reset when usage drops.
    announced: HashMap<(String, String), f32>,
}

/// Seconds since the epoch for `YYYY-MM-DDTHH:MM:SS[.fff](Z|±HH:MM)`.
fn parse_rfc3339(s: &str) -> Option<i64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let (clock, offset) = match time.find(['Z', '+', '-']) {
        Some(i) => (&time[..i], &time[i..]),
        None => (time, "Z"),
    };
    let mut c = clock.split(':');
    let (hh, mm) = (
        c.next()?.parse::<i64>().ok()?,
        c.next()?.parse::<i64>().ok()?,
    );
    let ss = c
        .next()
        .and_then(|s| s.split('.').next()?.parse::<i64>().ok())
        .unwrap_or(0);
    let shift = match offset.as_bytes().first() {
        Some(b'+' | b'-') => {
            let sign = if offset.starts_with('-') { -1 } else { 1 };
            let mut o = offset[1..].split(':');
            sign * (o.next()?.parse::<i64>().ok()? * 3600
                + o.next().and_then(|m| m.parse::<i64>().ok()).unwrap_or(0) * 60)
        }
        _ => 0,
    };
    // Days from civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss - shift)
}

fn resets_in(at: &str) -> Option<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let secs = (parse_rfc3339(at)? - now).max(0);
    Some(match secs {
        0..3600 => format!("resets in {}m", secs / 60),
        3600..86_400 => format!("resets in {}h {}m", secs / 3600, secs % 3600 / 60),
        _ => format!("resets in {}d {}h", secs / 86_400, secs % 86_400 / 3600),
    })
}

impl Shell {
    pub(super) fn start_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.workspace.usage_indicator {
            return;
        }
        self._usage = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let readings = cx
                    .background_executor()
                    .spawn(async {
                        usage::profiles()
                            .into_iter()
                            .map(|p| {
                                let s = usage::fetch(&p);
                                (p, s)
                            })
                            .collect::<Vec<_>>()
                    })
                    .await;
                let wait = readings
                    .iter()
                    .filter_map(|(_, s)| match s {
                        Status::RateLimited(d) => Some(*d),
                        _ => None,
                    })
                    .max()
                    .map_or(POLL, |d| d.max(POLL));
                if this
                    .update_in(cx, |this, window, cx| {
                        this.usage_arrived(readings, window, cx)
                    })
                    .is_err()
                {
                    return;
                }
                cx.background_executor().timer(wait).await;
            }
        }));
    }

    fn usage_arrived(
        &mut self,
        readings: Vec<(Profile, Status)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut crossings = Vec::new();
        for (profile, status) in &readings {
            let Status::Windows(windows) = status else {
                continue;
            };
            for w in windows
                .iter()
                .filter(|w| w.label == "5h" || w.label == "7d")
            {
                let key = (profile.name.clone(), w.label.clone());
                let level = LEVELS
                    .iter()
                    .copied()
                    .filter(|l| w.used >= *l)
                    .fold(0., f32::max);
                let previous = self.usage.announced.get(&key).copied().unwrap_or(0.);
                if level > previous {
                    crossings.push(format!(
                        "{} {} limit at {:.0}%",
                        profile.name, w.label, w.used
                    ));
                }
                self.usage.announced.insert(key, level);
            }
        }
        self.usage.readings = readings;
        self.usage.fetched = Some(Instant::now());
        for title in crossings {
            self.local_notice(
                NoticeKind::Message {
                    title,
                    body: "Claude plan usage".into(),
                },
                window,
                cx,
            );
        }
        cx.notify();
    }

    fn enable_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Info,
            "Show Claude plan usage?",
            Some(
                "Athena reads the sign-in Claude Code keeps in your Keychain (macOS may ask you to allow \
                 this) and asks api.anthropic.com for your 5-hour and weekly usage every 5 minutes. \
                 Nothing is stored. This uses the same undocumented endpoint as Claude Code's /usage, \
                 so it may stop working after a Claude Code update.",
            ),
            &["Show usage", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                this.workspace.usage_indicator = true;
                this.schedule_save(cx);
                this.start_usage(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .usage
            .fetched
            .is_none_or(|t| t.elapsed() >= MIN_REFRESH)
        {
            self.usage.fetched = Some(Instant::now());
            self.start_usage(window, cx);
        }
    }

    fn level_color(&self, used: f32, cx: &Context<Self>) -> Hsla {
        let c = &cx.theme().color;
        if used >= 90. {
            c.danger
        } else if used >= 75. {
            c.warning
        } else {
            c.content_muted
        }
    }

    fn bar(&self, used: f32, width: f32, cx: &Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div().w(px(width)).h(px(2.)).bg(t.color.border).child(
            div()
                .h_full()
                .w(px(width * (used / 100.).clamp(0., 1.)))
                .bg(self.level_color(used, cx)),
        )
    }

    pub(super) fn render_usage_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        if !self.workspace.usage_indicator {
            return div()
                .id("usage-enable")
                .h(px(24.))
                .px(px(8.))
                .flex()
                .items_center()
                .rounded(t.shape.radius_control)
                .cursor_pointer()
                .text_color(t.color.content_muted)
                .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                .on_click(cx.listener(|this, _, window, cx| this.enable_usage(window, cx)))
                .child("Usage")
                .into_any_element();
        }
        let busiest = self
            .usage
            .readings
            .iter()
            .filter_map(|(_, s)| match s {
                Status::Windows(w) => Some(w),
                _ => None,
            })
            .max_by(|a, b| {
                let five = |w: &&Vec<usage::Window>| w.first().map_or(0., |x| x.used);
                five(a).total_cmp(&five(b))
            });
        let used = |label: &str| {
            busiest
                .and_then(|w| w.iter().find(|x| x.label == label))
                .map(|x| x.used)
        };
        let (five, week) = (used("5h"), used("7d"));
        let summary = match (five, week) {
            (Some(f), Some(w)) => format!("5h {f:.0}% · 7d {w:.0}%"),
            (Some(f), None) => format!("5h {f:.0}%"),
            _ if self.usage.readings.is_empty() => "Usage…".into(),
            _ => "Usage unavailable".into(),
        };
        let color = self.level_color(five.unwrap_or(0.).max(week.unwrap_or(0.)), cx);
        div()
            .id("usage")
            .h(px(24.))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(8.))
            .rounded(t.shape.radius_control)
            .cursor_pointer()
            .hover(|s| s.bg(t.color.surface_hover))
            .on_click(cx.listener(|this, _, _, cx| {
                this.usage.open = !this.usage.open;
                cx.notify();
            }))
            .when(five.is_some(), |el| {
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(self.bar(five.unwrap_or(0.), 24., cx))
                        .child(self.bar(week.unwrap_or(0.), 24., cx)),
                )
            })
            .child(div().text_color(color).child(summary))
            .into_any_element()
    }

    pub(super) fn render_usage_popover(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.usage.open {
            return None;
        }
        let t = cx.theme().clone();
        let sections: Vec<AnyElement> =
            self.usage
                .readings
                .iter()
                .map(|(profile, status)| {
                    let body: AnyElement = match status {
                        Status::Windows(windows) => div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .children(windows.iter().map(|w| {
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.))
                                    .child(
                                        div()
                                            .flex()
                                            .justify_between()
                                            .child(
                                                div()
                                                    .text_color(t.color.content)
                                                    .child(w.label.clone()),
                                            )
                                            .child(
                                                div()
                                                    .text_color(self.level_color(w.used, cx))
                                                    .child(format!("{:.0}%", w.used)),
                                            ),
                                    )
                                    .child(self.bar(w.used, 260., cx))
                                    .children(w.resets_at.as_deref().and_then(resets_in).map(|r| {
                                        div().text_color(t.color.content_disabled).child(r)
                                    }))
                            }))
                            .into_any_element(),
                        Status::Stale => div()
                            .text_color(t.color.content_muted)
                            .child("Sign-in expired. Run Claude Code to refresh it.")
                            .into_any_element(),
                        Status::NoAccess => div()
                            .text_color(t.color.content_muted)
                            .child("Keychain access was denied.")
                            .into_any_element(),
                        Status::RateLimited(d) => div()
                            .text_color(t.color.content_muted)
                            .child(format!(
                                "Rate limited; trying again in {} min.",
                                d.as_secs().div_ceil(60)
                            ))
                            .into_any_element(),
                        Status::Unavailable(why) => div()
                            .text_color(t.color.content_muted)
                            .child(format!("Unavailable: {why}"))
                            .into_any_element(),
                    };
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .child(profile.name.clone()),
                        )
                        .child(body)
                        .into_any_element()
                })
                .collect();
        let empty = sections.is_empty();
        let refresh = athena_ui::Button::new("usage-refresh", "Refresh", ButtonKind::Ghost)
            .on_click(cx.listener(|this, _, window, cx| this.refresh_usage(window, cx)));
        Some(
            div()
                .id("usage-popover")
                .absolute()
                .top(px(40.))
                .right(px(8.))
                .w(px(300.))
                .p(px(16.))
                .flex()
                .flex_col()
                .gap(px(16.))
                .bg(t.color.surface)
                .border_1()
                .border_color(t.color.border)
                .rounded(t.shape.radius_panel)
                .shadow(vec![t.popover_shadow()])
                .text_size(t.typography.caption)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .children(sections)
                .when(empty, |el| {
                    el.child(
                        div()
                            .text_color(t.color.content_muted)
                            .child("No Claude Code sign-in found."),
                    )
                })
                .child(div().flex().justify_end().child(refresh))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339("2026-10-08T12:00:00.123Z"),
            Some(1_791_460_800)
        );
        assert_eq!(
            parse_rfc3339("2026-10-08T14:00:00+02:00"),
            Some(1_791_460_800)
        );
        assert_eq!(parse_rfc3339("garbage"), None);
    }
}
