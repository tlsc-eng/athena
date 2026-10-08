//! Claude plan limits (the 5-hour and weekly windows) for the title bar.
//!
//! Reads Claude Code's own sign-in from the Keychain the way Claude Code does (`security`), asks
//! api.anthropic.com for usage with it, and keeps nothing: the token lives only for the request.
//! The endpoint is the one Claude Code's `/usage` uses; it is undocumented, so every failure
//! degrades to "unavailable" rather than an error.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};

const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage?at_wall=1&skip_spend=1";
/// `security` exit status when the item does not exist.
const NOT_FOUND: i32 = 44;
/// Sign-ins rarely appear or vanish, so the Keychain is not asked on every poll.
const PROFILE_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    /// Shown in the popover: the config folder name, such as `claude-tlsc`.
    pub name: String,
    service: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    /// `5h`, `7d`, or a model name for per-model weekly caps.
    pub label: String,
    /// Percent used, 0 to 100.
    pub used: f32,
    /// Seconds since the epoch.
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Windows(Vec<Window>),
    /// The saved sign-in has expired; running Claude Code refreshes it.
    Stale,
    /// The Keychain refused (the user chose Deny).
    NoAccess,
    RateLimited(Duration),
    Unavailable(String),
}

/// Keychain service name Claude Code uses for a config folder (`None` for the default one).
pub fn service_for(config_dir: Option<&Path>) -> String {
    match config_dir {
        None => "Claude Code-credentials".into(),
        Some(dir) => {
            let digest = Sha256::digest(dir.to_string_lossy().as_bytes());
            let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
            format!("Claude Code-credentials-{hex}")
        }
    }
}

/// Config folders that may hold a Claude Code sign-in: `~/.claude` and `~/.claude-*`.
fn candidates() -> Vec<(String, Option<PathBuf>)> {
    let Some(home) = std::env::home_dir() else {
        return Vec::new();
    };
    let mut out = vec![("claude".to_string(), None)];
    if let Ok(entries) = std::fs::read_dir(&home) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".claude-") && entry.path().is_dir() {
                out.push((name.trim_start_matches('.').to_string(), Some(entry.path())));
            }
        }
    }
    out.sort();
    out
}

fn user() -> String {
    std::env::var("USER").unwrap_or_else(|_| "claude-code-user".into())
}

fn security(service: &str, want_secret: bool) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new("/usr/bin/security");
    cmd.args(["find-generic-password", "-a", &user(), "-s", service]);
    if want_secret {
        cmd.arg("-w");
    }
    cmd.stdin(Stdio::null()).output()
}

/// Profiles with a stored Claude Code sign-in, probed at most every 30 minutes.
/// Checking existence does not reveal the secret.
pub fn profiles() -> Vec<Profile> {
    static CACHE: Mutex<Option<(Instant, Vec<Profile>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((at, list)) = cache.as_ref()
        && at.elapsed() < PROFILE_TTL
    {
        return list.clone();
    }
    let list = probe_profiles();
    *cache = Some((Instant::now(), list.clone()));
    list
}

fn probe_profiles() -> Vec<Profile> {
    candidates()
        .into_iter()
        .map(|(name, dir)| Profile {
            name,
            service: service_for(dir.as_deref()),
        })
        .filter(|p| {
            security(&p.service, false)
                .is_ok_and(|o| o.status.code() != Some(NOT_FOUND) && o.status.success())
        })
        .collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn fetch(profile: &Profile) -> Status {
    let output = match security(&profile.service, true) {
        Ok(o) if o.status.success() => o,
        Ok(_) => return Status::NoAccess,
        Err(e) => return Status::Unavailable(e.to_string()),
    };
    let secret: Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(_) => return Status::Unavailable("unrecognised sign-in format".into()),
    };
    let oauth = &secret["claudeAiOauth"];
    let Some(token) = oauth["accessToken"].as_str() else {
        return Status::Unavailable("not signed in with a Claude account".into());
    };
    if oauth["expiresAt"].as_u64().is_some_and(|t| t <= now_ms()) {
        return Status::Stale;
    }
    request(token)
}

/// GET the usage endpoint with curl, handing the token over stdin so it never shows in `ps`.
fn request(token: &str) -> Status {
    let child = Command::new("/usr/bin/curl")
        .args([
            "-sS",
            "--max-time",
            "10",
            "--proto",
            "=https",
            "-D",
            "-",
            "-H",
            "@-",
        ])
        .args([
            "-H",
            "anthropic-beta: oauth-2025-04-20",
            "-H",
            "accept: application/json",
            ENDPOINT,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return Status::Unavailable(e.to_string()),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = writeln!(stdin, "Authorization: Bearer {token}");
    }
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => return Status::Unavailable(e.to_string()),
    };
    if !output.status.success() {
        return Status::Unavailable("network error".into());
    }
    interpret(&String::from_utf8_lossy(&output.stdout))
}

/// Splits curl's `-D -` output (headers, blank line, body) and maps it to a status.
fn interpret(raw: &str) -> Status {
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
    let code: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    match code {
        200 => match serde_json::from_str::<Value>(body) {
            Ok(v) => Status::Windows(parse(&v)),
            Err(_) => Status::Unavailable("unexpected response".into()),
        },
        401 => Status::Stale,
        429 => {
            let retry = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("retry-after:")
                        .map(|v| v.trim().to_string())
                })
                .and_then(|v| v.parse().ok())
                .unwrap_or(600);
            Status::RateLimited(Duration::from_secs(retry))
        }
        other => Status::Unavailable(format!("HTTP {other}")),
    }
}

/// Percent used from `percent`, else `utilization`; `None` when neither is present.
fn used(w: &Value, fraction: bool) -> Option<f32> {
    if let Some(p) = w["percent"].as_f64() {
        return Some(p as f32);
    }
    let u = w["utilization"].as_f64()?;
    // `limits` rows may carry the raw 0–1 fraction; legacy fields are already percentages.
    Some(if fraction && u <= 1. { u * 100. } else { u } as f32)
}

/// `resets_at` as epoch seconds, given an RFC 3339 string or a number.
fn resets_at(v: &Value) -> Option<i64> {
    if let Some(s) = v.as_str() {
        return parse_rfc3339(s);
    }
    let n = v.as_f64()?;
    // Epoch milliseconds passed 1e11 in 1973; epoch seconds won't until the year 5138.
    Some(if n > 1e11 { n / 1000. } else { n } as i64)
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

/// Reads the `limits` list, or the older `five_hour` / `seven_day` fields.
/// A window without a usage figure is left out rather than shown as 0%.
pub fn parse(v: &Value) -> Vec<Window> {
    let window = |label: String, w: &Value, fraction: bool| {
        Some(Window {
            label,
            used: used(w, fraction)?,
            resets_at: resets_at(&w["resets_at"]),
        })
    };
    if let Some(limits) = v["limits"].as_array() {
        let mut out: Vec<Window> = limits
            .iter()
            .filter_map(|l| {
                let label = match l["kind"].as_str()? {
                    "session" => "5h".to_string(),
                    "weekly_all" => "7d".to_string(),
                    "weekly_scoped" => l["scope"]["model"]["display_name"]
                        .as_str()
                        .unwrap_or("model")
                        .to_string(),
                    _ => return None,
                };
                window(label, l, true)
            })
            .collect();
        out.sort_by_key(|w| match w.label.as_str() {
            "5h" => 0,
            "7d" => 1,
            _ => 2,
        });
        if !out.is_empty() {
            return out;
        }
    }
    [("5h", "five_hour"), ("7d", "seven_day")]
        .into_iter()
        .filter(|(_, key)| v[*key].is_object())
        .filter_map(|(label, key)| window(label.to_string(), &v[key], false))
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn service_names_match_claude_code() {
        assert_eq!(service_for(None), "Claude Code-credentials");
        // sha256("/Users/json/.claude-tlsc")[..8], as Claude Code 2.1.294 derives it.
        assert_eq!(
            service_for(Some(Path::new("/Users/json/.claude-tlsc"))),
            "Claude Code-credentials-99dd0ebe"
        );
    }

    /// `/api/oauth/usage?at_wall=1&skip_spend=1` as it answered on 2026-10-08, trimmed.
    const LIVE: &str = r#"{
        "five_hour": { "utilization": 39.0, "resets_at": "2026-10-08T17:59:59.885547+00:00",
                       "limit_dollars": null, "used_dollars": null, "locked_reason": null },
        "seven_day": { "utilization": 31.0, "resets_at": "2026-10-13T19:59:59.885573+00:00",
                       "limit_dollars": null, "used_dollars": null, "locked_reason": null },
        "seven_day_oauth_apps": null,
        "seven_day_opus": null,
        "seven_day_sonnet": null,
        "extra_usage": null,
        "limits": [
            { "kind": "session", "group": "session", "percent": 39, "severity": "normal",
              "resets_at": "2026-10-08T17:59:59.885547+00:00", "scope": null, "is_active": true },
            { "kind": "weekly_all", "group": "weekly", "percent": 31, "severity": "normal",
              "resets_at": "2026-10-13T19:59:59.885573+00:00", "scope": null, "is_active": false },
            { "kind": "weekly_scoped", "group": "weekly", "percent": 14, "severity": "normal",
              "resets_at": "2026-10-13T19:59:59.885802+00:00",
              "scope": { "model": { "id": null, "display_name": "Fable" }, "surface": null },
              "is_active": false }
        ],
        "spend": null
    }"#;

    #[test]
    fn reads_the_live_response() {
        let w = parse(&serde_json::from_str(LIVE).unwrap());
        assert_eq!(
            w,
            [
                Window {
                    label: "5h".into(),
                    used: 39.,
                    resets_at: Some(1_791_482_399)
                },
                Window {
                    label: "7d".into(),
                    used: 31.,
                    resets_at: Some(1_791_921_599)
                },
                Window {
                    label: "Fable".into(),
                    used: 14.,
                    resets_at: Some(1_791_921_599)
                },
            ]
        );
    }

    #[test]
    fn percent_wins_and_fractional_utilization_is_scaled() {
        let row = |extra: Value| {
            let mut l = json!({ "kind": "session" });
            l.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            parse(&json!({ "limits": [l] }))
        };
        assert_eq!(row(json!({ "percent": 42 }))[0].used, 42.);
        assert_eq!(
            row(json!({ "percent": 42, "utilization": 0.9 }))[0].used,
            42.
        );
        assert_eq!(row(json!({ "utilization": 0.42 }))[0].used, 42.);
        assert_eq!(row(json!({ "utilization": 42.0 }))[0].used, 42.);
    }

    #[test]
    fn window_without_usage_is_left_out() {
        let v = json!({ "limits": [
            { "kind": "session", "resets_at": "2026-10-08T17:59:59Z" },
            { "kind": "weekly_all", "percent": 31 }
        ]});
        assert_eq!(
            parse(&v)
                .iter()
                .map(|w| w.label.as_str())
                .collect::<Vec<_>>(),
            ["7d"]
        );
        let legacy =
            json!({ "limits": null, "five_hour": { "resets_at": null }, "seven_day": null });
        assert!(parse(&legacy).is_empty());
    }

    #[test]
    fn empty_limits_fall_back_to_legacy_percentages() {
        let v = json!({ "limits": [], "five_hour": { "utilization": 0.5 }, "seven_day": null });
        assert_eq!(
            parse(&v),
            [Window {
                label: "5h".into(),
                used: 0.5,
                resets_at: None
            }]
        );
    }

    #[test]
    fn resets_at_accepts_epoch_numbers() {
        let v = json!({ "limits": [
            { "kind": "session", "percent": 1, "resets_at": 1_791_482_399 },
            { "kind": "weekly_all", "percent": 1, "resets_at": 1_791_921_599_000_u64 },
            { "kind": "weekly_scoped", "percent": 1, "resets_at": "soon" }
        ]});
        let resets: Vec<_> = parse(&v).iter().map(|w| w.resets_at).collect();
        assert_eq!(resets, [Some(1_791_482_399), Some(1_791_921_599), None]);
    }

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
        assert_eq!(
            parse_rfc3339("2026-10-08T12:00:00.885547+00:00"),
            Some(1_791_460_800)
        );
        assert_eq!(parse_rfc3339("garbage"), None);
    }

    #[test]
    fn maps_http_statuses() {
        assert_eq!(interpret("HTTP/2 401\r\n\r\n{}"), Status::Stale);
        assert_eq!(
            interpret("HTTP/2 429\r\nretry-after: 120\r\n\r\n"),
            Status::RateLimited(Duration::from_secs(120))
        );
        assert!(
            matches!(interpret("HTTP/2 200\r\n\r\n{\"five_hour\":{\"utilization\":1}}"), Status::Windows(w) if w.len() == 1)
        );
        assert!(matches!(
            interpret("HTTP/2 500\r\n\r\n"),
            Status::Unavailable(_)
        ));
    }
}
