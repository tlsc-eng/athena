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

fn resets_in(at: i64) -> Option<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let secs = (at - now).max(0);
    Some(match secs {
        0..3600 => format!("resets in {}m", secs / 60),
        3600..86_400 => format!("resets in {}h {}m", secs / 3600, secs % 3600 / 60),
        _ => format!("resets in {}d {}h", secs / 86_400, secs % 86_400 / 3600),
    })
}

impl Shell {
    pub(super) fn start_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.poll_usage(false, window, cx);
    }

    /// Restarts polling; `fresh` re-probes the Keychain for sign-ins on the first round.
    fn poll_usage(&mut self, fresh: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.workspace.usage_indicator {
            return;
        }
        self._usage = Some(cx.spawn_in(window, async move |this, cx| {
            let mut fresh = fresh;
            loop {
                let readings = cx
                    .background_executor()
                    .spawn(async move {
                        usage::profiles(fresh)
                            .into_iter()
                            .map(|p| {
                                let s = usage::fetch(&p);
                                (p, s)
                            })
                            .collect::<Vec<_>>()
                    })
                    .await;
                fresh = false;
                let wait = readings
                    .iter()
                    .filter_map(|(_, s)| match s {
                        Status::RateLimited(d) => Some(*d),
                        _ => None,
                    })
                    .max()
                    .map_or(POLL, |d| d.max(POLL));
                if this
                    .update(cx, |this, cx| this.usage_arrived(readings, cx))
                    .is_err()
                {
                    return;
                }
                cx.background_executor().timer(wait).await;
            }
        }));
    }

    fn usage_arrived(&mut self, readings: Vec<(Profile, Status)>, cx: &mut Context<Self>) {
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
            self.poll_usage(true, window, cx);
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
                Status::Windows(w) if !w.is_empty() => Some(w),
                _ => None,
            })
            .max_by(|a, b| {
                let peak = |w: &&Vec<usage::Window>| w.iter().map(|x| x.used).fold(0., f32::max);
                peak(a).total_cmp(&peak(b))
            });
        let used = |label: &str| {
            busiest
                .and_then(|w| w.iter().find(|x| x.label == label))
                .map(|x| x.used)
        };
        let (five, week) = (used("5h"), used("7d"));
        let color = self.level_color(five.unwrap_or(0.).max(week.unwrap_or(0.)), cx);
        let part = |label: &str, used: Option<f32>| match used {
            Some(u) => div().text_color(color).child(format!("{label} {u:.0}%")),
            None => div()
                .text_color(t.color.content_muted)
                .child(format!("{label} n/a")),
        };
        let summary = if busiest.is_some() {
            div()
                .flex()
                .gap(px(4.))
                .child(part("5h", five))
                .child(div().text_color(t.color.content_muted).child("·"))
                .child(part("7d", week))
        } else if self.usage.readings.is_empty() {
            div().text_color(t.color.content_muted).child("Usage…")
        } else {
            div()
                .text_color(t.color.content_muted)
                .child("Usage unavailable")
        };
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
            .when(five.is_some() || week.is_some(), |el| {
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .children(five.map(|f| self.bar(f, 24., cx)))
                        .children(week.map(|w| self.bar(w, 24., cx))),
                )
            })
            .child(summary)
            .into_any_element()
    }

    pub(super) fn usage_open(&self) -> bool {
        self.usage.open
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
                        Status::Windows(windows) if windows.is_empty() => div()
                            .text_color(t.color.content_muted)
                            .child("No usage figures in the response.")
                            .into_any_element(),
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
                                    .children(w.resets_at.and_then(resets_in).map(|r| {
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
