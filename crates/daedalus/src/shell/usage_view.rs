use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use daedalus_proto::NoticeKind;
use daedalus_ui::motion::{self, Closing};
use daedalus_ui::{ActiveTheme, ButtonKind, IconButton, Space};
use gpui::{
    Animation, AnyElement, Context, FontWeight, Hsla, MouseButton, MouseDownEvent, Pixels, Point,
    PromptLevel, Window, div, prelude::*,
};

use super::notices::{send_push, usage_push_request};
use super::{Shell, windows};
use crate::transcripts;
use crate::usage::{self, Profile, Status};

const POLL: Duration = Duration::from_secs(5 * 60);
/// Refresh presses closer together than this reuse the last reading.
const MIN_REFRESH: Duration = Duration::from_secs(60);
const LEVELS: [f32; 2] = [75., 90.];
/// Lower levels stay in the app; a phone buzz is for a limit that is about to bite.
const PUSH_LEVEL: f32 = 90.;

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

/// A refresh waits out `MIN_REFRESH` and any rate-limit retry-after since the last fetch.
fn refresh_allowed(since_fetch: Option<Duration>, retry_after: Duration) -> bool {
    since_fetch.is_none_or(|elapsed| elapsed >= MIN_REFRESH.max(retry_after))
}

struct Crossing {
    profile: String,
    label: String,
    used: f32,
    level: f32,
    resets_at: Option<i64>,
}

impl Crossing {
    fn pushes(&self) -> bool {
        self.level >= PUSH_LEVEL
    }
}

/// The levels newly passed since the last reading, recording what was announced.
fn crossings(
    announced: &mut HashMap<(String, String), f32>,
    readings: &[(Profile, Status)],
) -> Vec<Crossing> {
    let mut crossings = Vec::new();
    for (profile, status) in readings {
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
            let previous = announced.get(&key).copied().unwrap_or(0.);
            if level > previous {
                crossings.push(Crossing {
                    profile: profile.name.clone(),
                    label: w.label.clone(),
                    used: w.used,
                    level,
                    resets_at: w.resets_at,
                });
            }
            announced.insert(key, level);
        }
    }
    crossings
}

/// The crossings that push, tracked in `pushed` so windows sharing it never push one twice.
fn pushes_due(
    pushed: &mut HashMap<(String, String), f32>,
    readings: &[(Profile, Status)],
) -> Vec<Crossing> {
    let mut due = crossings(pushed, readings);
    due.retain(Crossing::pushes);
    due
}

/// The API reports a reset a fraction of a second before the minute, so it rounds up to read as 18:00.
fn round_up_minute(at: i64) -> i64 {
    (at + 59) / 60 * 60
}

/// The weekday is only worth its width on a window that resets days away.
fn reset_text(weekly: bool, weekday: i32, hour: i32, minute: i32) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let day = if weekly {
        format!("{} ", DAYS[weekday.rem_euclid(7) as usize])
    } else {
        String::new()
    };
    format!("{day}{hour:02}:{minute:02}")
}

fn local_reset(label: &str, at: i64) -> String {
    let t = round_up_minute(at) as libc::time_t;
    // SAFETY: localtime_r only writes the tm it is given.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    reset_text(label == "7d", tm.tm_wday, tm.tm_hour, tm.tm_min)
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
        let crossings = crossings(&mut self.usage.announced, &readings);
        let due = pushes_due(windows::usage_pushed(cx), &readings);
        self.usage.readings = readings;
        self.usage.latest = latest;
        self.usage.fetched = Some(Instant::now());
        windows::mark_companion_dirty(cx);
        if let Some(url) = self.settings.file.companion_push_url() {
            for c in &due {
                let resets = c.resets_at.map(|at| local_reset(&c.label, at));
                if let Some(push) =
                    usage_push_request(&c.label, c.used, &c.profile, resets.as_deref(), url)
                {
                    send_push(push);
                }
            }
        }
        for c in crossings {
            self.local_notice(
                NoticeKind::Message {
                    title: format!("{} {} limit at {:.0}%", c.profile, c.label, c.used),
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
                "Daedalus reads the sign-in Claude Code keeps in your Keychain (macOS may ask you to allow \
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

    /// The readings the phone may see, none while the indicator is off.
    pub(super) fn usage_readings(&self) -> Vec<(Profile, Status)> {
        if self.workspace.usage_indicator {
            self.usage.readings.clone()
        } else {
            Vec::new()
        }
    }

    pub(super) fn refresh_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let retry_after = self
            .usage
            .readings
            .iter()
            .filter_map(|(_, s)| match s {
                Status::RateLimited(d) => Some(*d),
                _ => None,
            })
            .max()
            .unwrap_or_default();
        if refresh_allowed(self.usage.fetched.map(|t| t.elapsed()), retry_after) {
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
        let refresh = daedalus_ui::Button::new("usage-refresh", "Refresh", ButtonKind::Ghost)
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

    #[test]
    fn a_refresh_waits_for_the_minimum_gap_and_any_retry_after() {
        let secs = Duration::from_secs;
        assert!(refresh_allowed(None, secs(300)));
        assert!(!refresh_allowed(Some(secs(30)), Duration::ZERO));
        assert!(!refresh_allowed(Some(secs(120)), secs(300)));
        assert!(refresh_allowed(Some(secs(61)), Duration::ZERO));
        assert!(refresh_allowed(Some(secs(300)), secs(300)));
    }

    #[test]
    fn only_a_crossing_into_ninety_pushes_and_not_again_until_usage_drops_and_rises() {
        let mut announced = HashMap::new();
        let mut pushed = |used: f32| {
            crossings(&mut announced, &[reading("claude-ai", Some(used))])
                .iter()
                .map(Crossing::pushes)
                .collect::<Vec<_>>()
        };
        assert_eq!(pushed(50.), Vec::<bool>::new());
        assert_eq!(pushed(76.), [false], "75 is a local notice only");
        assert_eq!(pushed(91.), [true]);
        assert_eq!(pushed(95.), Vec::<bool>::new());
        assert_eq!(pushed(10.), Vec::<bool>::new());
        assert_eq!(pushed(92.), [true], "usage reset, so it crosses again");
    }

    #[test]
    fn windows_sharing_the_pushed_levels_push_a_crossing_once_between_them() {
        let (mut shared, mut first, mut second) = (HashMap::new(), HashMap::new(), HashMap::new());
        let readings = [reading("claude-ai", Some(91.))];
        assert_eq!(pushes_due(&mut shared, &readings).len(), 1);
        assert_eq!(pushes_due(&mut shared, &readings).len(), 0);
        assert_eq!(crossings(&mut first, &readings).len(), 1);
        assert_eq!(
            crossings(&mut second, &readings).len(),
            1,
            "each window still tells its user"
        );
        let low = [reading("claude-ai", Some(20.))];
        assert_eq!(pushes_due(&mut shared, &low).len(), 0);
        assert_eq!(pushes_due(&mut shared, &readings).len(), 1);
    }

    #[test]
    fn a_reset_a_second_before_the_minute_reads_as_the_minute_and_only_a_week_shows_its_day() {
        // "2026-10-08T17:59:59.885Z" as the API sends it, truncated to seconds.
        let at = 1_791_482_399;
        assert_eq!(round_up_minute(at), 1_791_482_400);
        assert_eq!(round_up_minute(1_791_482_400), 1_791_482_400);
        assert_eq!(reset_text(false, 4, 18, 0), "18:00");
        assert_eq!(reset_text(true, 1, 18, 0), "Mon 18:00");
    }

    #[test]
    fn a_jump_straight_past_ninety_pushes_once_and_other_windows_are_left_out() {
        let mut announced = HashMap::new();
        let readings = [(
            Profile::named("claude-ai"),
            Status::Windows(vec![
                usage::Window {
                    label: "7d".into(),
                    used: 93.,
                    resets_at: Some(1_791_921_599),
                },
                usage::Window {
                    label: "Fable".into(),
                    used: 99.,
                    resets_at: None,
                },
            ]),
        )];
        let found = crossings(&mut announced, &readings);
        assert_eq!(found.len(), 1);
        assert!(found[0].pushes());
        assert_eq!((found[0].label.as_str(), found[0].used), ("7d", 93.));
    }
}
