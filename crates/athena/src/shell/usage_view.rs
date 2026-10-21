use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use athena_proto::NoticeKind;
use athena_ui::motion::{self, Closing};
use athena_ui::{ActiveTheme, ButtonKind, IconButton, Space};
use gpui::{
    Animation, AnyElement, Context, FontWeight, Hsla, MouseButton, MouseDownEvent, Pixels, Point,
    PromptLevel, Window, div, prelude::*,
};

use super::Shell;
use crate::transcripts;
use crate::usage::{self, Profile, Status};

const POLL: Duration = Duration::from_secs(5 * 60);
/// Refresh presses closer together than this reuse the last reading.
const MIN_REFRESH: Duration = Duration::from_secs(60);
const LEVELS: [f32; 2] = [75., 90.];

#[derive(Default)]
pub(super) struct UsageState {
    readings: Vec<(Profile, Status)>,
    /// Profile of each open project's newest transcript, by project root.
    latest: HashMap<PathBuf, String>,
    fetched: Option<Instant>,
    open: bool,
    /// Generation of the current opening, keying its enter animation.
    opened: u64,
    closing: Option<Closing>,
    dismissed_by: Option<Point<Pixels>>,
    /// Highest threshold already announced per profile and window, reset when usage drops.
    announced: HashMap<(String, String), f32>,
}

fn peak(status: &Status) -> f32 {
    match status {
        Status::Windows(w) => w.iter().map(|x| x.used).fold(0., f32::max),
        _ => 0.,
    }
}

/// The preferred profile's reading even when it has no figures, else the busiest one's.
fn choose<'a>(
    readings: &'a [(Profile, Status)],
    preferred: Option<&str>,
) -> Option<&'a (Profile, Status)> {
    readings
        .iter()
        .find(|(p, _)| Some(p.name.as_str()) == preferred)
        .or_else(|| {
            readings
                .iter()
                .filter(|(_, s)| matches!(s, Status::Windows(w) if !w.is_empty()))
                .max_by(|(_, a), (_, b)| peak(a).total_cmp(&peak(b)))
        })
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
                let Ok(roots) = this.update(cx, |this, _| {
                    this.workspace
                        .projects
                        .iter()
                        .map(|p| p.root.clone())
                        .collect::<Vec<_>>()
                }) else {
                    return;
                };
                let (readings, latest) = cx
                    .background_executor()
                    .spawn(async move {
                        let readings = usage::profiles(fresh)
                            .into_iter()
                            .map(|p| {
                                let s = usage::fetch(&p);
                                (p, s)
                            })
                            .collect::<Vec<_>>();
                        let latest = std::env::home_dir()
                            .map(|home| {
                                roots
                                    .into_iter()
                                    .filter_map(|root| {
                                        let p = transcripts::latest_profile(&home, &root)?;
                                        Some((root, p.name))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        (readings, latest)
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
                    .update(cx, |this, cx| this.usage_arrived(readings, latest, cx))
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
        latest: HashMap<PathBuf, String>,
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
        self.usage.latest = latest;
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
        div().w(t.ui(width)).h(t.ui(2.)).bg(t.color.border).child(
            div()
                .h_full()
                .w(t.ui(width * (used / 100.).clamp(0., 1.)))
                .bg(self.level_color(used, cx)),
        )
    }

    pub(super) fn render_usage_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        if !self.workspace.usage_indicator {
            return IconButton::new("usage-enable")
                .label("Usage")
                .tooltip("Show Claude plan usage")
                .on_click(cx.listener(|this, _, window, cx| this.enable_usage(window, cx)))
                .into_any_element();
        }
        let preferred = self.preferred_profile();
        let shown = choose(&self.usage.readings, preferred);
        let windows = shown.and_then(|(_, s)| match s {
            Status::Windows(w) if !w.is_empty() => Some(w),
            _ => None,
        });
        let tag = shown
            .filter(|_| self.usage.readings.len() > 1)
            .map(|(p, _)| {
                p.name
                    .strip_prefix("claude-")
                    .unwrap_or(&p.name)
                    .to_string()
            });
        let used = |label: &str| {
            windows
                .and_then(|w| w.iter().find(|x| x.label == label))
                .map(|x| x.used)
        };
        let (five, week) = (used("5h"), used("7d"));
        let part = |label: &str, used: Option<f32>| match used {
            Some(u) => div()
                .text_color(self.level_color(u, cx))
                .child(format!("{label} {u:.0}%")),
            None => div()
                .text_color(t.color.content_muted)
                .child(format!("{label} n/a")),
        };
        let summary = if windows.is_some() {
            div()
                .flex()
                .gap(t.space(Space::Xs))
                .children(tag.map(|tag| div().text_color(t.color.content_muted).child(tag)))
                .child(part("5h", five))
                .child(div().text_color(t.color.content_muted).child("·"))
                .child(part("7d", week))
        } else if self.usage.readings.is_empty() {
            div().text_color(t.color.content_muted).child("Usage…")
        } else {
            let label = match tag {
                Some(tag) => format!("{tag} usage unavailable"),
                None => "Usage unavailable".into(),
            };
            div().text_color(t.color.content_muted).child(label)
        };
        let button = IconButton::new("usage")
            .icon(
                div()
                    .flex()
                    .items_center()
                    .gap(t.space(Space::Md))
                    .when(five.is_some() || week.is_some(), |el| {
                        el.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(t.space(Space::Xxs))
                                .children(five.map(|f| self.bar(f, 24., cx)))
                                .children(week.map(|w| self.bar(w, 24., cx))),
                        )
                    })
                    .child(summary),
            )
            .fill()
            .active(self.usage_open())
            .on_click(cx.listener(|this, click, _, cx| {
                if super::notices::is_dismissing_click(click, &mut this.usage.dismissed_by) {
                    return;
                }
                this.usage.open = true;
                this.usage.opened = this.next_generation();
                cx.notify();
            }));
        if self.usage_open() {
            button.into_any_element()
        } else {
            button
                .tooltip("Claude plan usage, 5-hour and weekly")
                .into_any_element()
        }
    }

    /// The profile `claude.profiles` names for the active project, else its newest session's.
    fn preferred_profile(&self) -> Option<&str> {
        let root = &self.workspace.active_project()?.root;
        self.settings
            .file
            .claude_profile_for(root)
            .or_else(|| self.usage.latest.get(root).map(String::as_str))
    }

    /// Open or still fading out; either way it covers web previews.
    pub(super) fn usage_open(&self) -> bool {
        self.usage.open || self.usage.closing.is_some()
    }

    fn close_usage(&mut self, cx: &mut Context<Self>) {
        if !self.usage.open {
            return;
        }
        self.usage.open = false;
        let generation = self.next_generation();
        self.usage.closing = Some(Closing::new(generation));
        let t = cx.theme();
        let delay = motion::exit_delay(t.motion.reduced, t.motion.fast);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .usage
                    .closing
                    .is_some_and(|c| c.generation == generation)
                {
                    this.usage.closing = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_usage_popover(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.usage_open() {
            return None;
        }
        let t = cx.theme().clone();
        let step = t.space(Space::Xs);
        let rise = t.ui(super::status_bar::HEIGHT) + step;
        let preferred = self.preferred_profile();
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
                            .gap(t.space(Space::Md))
                            .children(windows.iter().map(|w| {
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(t.space(Space::Xs))
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
                        .gap(t.space(Space::Md))
                        .child(
                            div()
                                .flex()
                                .gap(t.space(Space::Sm))
                                .font_weight(FontWeight::MEDIUM)
                                .child(profile.name.clone())
                                .when(Some(profile.name.as_str()) == preferred, |el| {
                                    el.child(
                                        div()
                                            .font_weight(FontWeight::NORMAL)
                                            .text_color(t.color.content_muted)
                                            .child("this project"),
                                    )
                                }),
                        )
                        .child(body)
                        .into_any_element()
                })
                .collect();
        let empty = sections.is_empty();
        let refresh = athena_ui::Button::new("usage-refresh", "Refresh", ButtonKind::Ghost)
            .on_click(cx.listener(|this, _, window, cx| this.refresh_usage(window, cx)));
        let popover = div()
            .id("usage-popover")
            .absolute()
            .bottom(rise)
            .right(t.space(Space::Md))
            .w(t.ui(300.))
            .p(t.space(Space::Xl))
            .flex()
            .flex_col()
            .gap(t.space(Space::Xl))
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_panel)
            .shadow(vec![t.popover_shadow()])
            .text_size(t.typography.caption)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                this.usage.dismissed_by = Some(event.position);
                this.close_usage(cx)
            }))
            .when(self.usage.closing.is_some(), |el| {
                el.capture_any_mouse_down(|_, _, cx| cx.stop_propagation())
            })
            .children(sections)
            .when(empty, |el| {
                el.child(
                    div()
                        .text_color(t.color.content_muted)
                        .child("No Claude Code sign-in found."),
                )
            })
            .child(div().flex().justify_end().child(refresh));
        Some(match self.usage.closing {
            Some(closing) => motion::animate_exit(
                t.motion.reduced,
                popover,
                ("usage-close", closing.generation),
                t.motion.fast,
                move |el, d| el.opacity(1. - d).bottom(rise - step * d),
            ),
            None => motion::animate_if(
                t.motion.reduced,
                popover,
                ("usage-open", self.usage.opened),
                Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
                move |el, d| el.opacity(d).bottom(rise - step * (1. - d)),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(name: &str, used: Option<f32>) -> (Profile, Status) {
        let status = match used {
            Some(used) => Status::Windows(vec![usage::Window {
                label: "5h".into(),
                used,
                resets_at: None,
            }]),
            None => Status::Stale,
        };
        (Profile::named(name), status)
    }

    #[test]
    fn the_projects_profile_is_shown_even_when_quieter_or_without_figures() {
        let readings = [
            reading("claude-ai", Some(80.)),
            reading("claude-tlsc", Some(10.)),
            reading("claude-work", None),
        ];
        let name = |p: Option<&str>| choose(&readings, p).map(|(p, _)| p.name.as_str());
        assert_eq!(name(Some("claude-tlsc")), Some("claude-tlsc"));
        assert_eq!(name(Some("claude-work")), Some("claude-work"));
        assert_eq!(name(Some("claude-gone")), Some("claude-ai"), "busiest");
        assert_eq!(name(None), Some("claude-ai"));
        assert_eq!(choose(&[reading("claude-ai", None)], None), None);
    }
}
