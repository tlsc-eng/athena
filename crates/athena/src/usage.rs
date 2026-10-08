//! Claude plan limits (the 5-hour and weekly windows) for the title bar.
//!
//! Reads Claude Code's own sign-in from the Keychain the way Claude Code does (`security`), asks
//! api.anthropic.com for usage with it, and keeps nothing: the token lives only for the request.
//! The endpoint is the one Claude Code's `/usage` uses; it is undocumented, so every failure
//! degrades to "unavailable" rather than an error.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};

const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
/// `security` exit status when the item does not exist.
const NOT_FOUND: i32 = 44;

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
    pub resets_at: Option<String>,
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

/// Profiles with a stored Claude Code sign-in. Checking existence does not reveal the secret.
pub fn profiles() -> Vec<Profile> {
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

/// Reads the `limits` list, or the older `five_hour` / `seven_day` fields.
pub fn parse(v: &Value) -> Vec<Window> {
    let window = |label: String, w: &Value| Window {
        label,
        used: w["utilization"].as_f64().unwrap_or(0.) as f32,
        resets_at: w["resets_at"].as_str().map(str::to_string),
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
                Some(window(label, l))
            })
            .collect();
        out.sort_by_key(|w| match w.label.as_str() {
            "5h" => 0,
            "7d" => 1,
            _ => 2,
        });
        return out;
    }
    [("5h", "five_hour"), ("7d", "seven_day")]
        .into_iter()
        .filter(|(_, key)| v[*key].is_object())
        .map(|(label, key)| window(label.to_string(), &v[key]))
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

    #[test]
    fn reads_limits_and_legacy_shapes() {
        let limits = json!({ "limits": [
            { "kind": "weekly_all", "utilization": 18.0, "resets_at": "2026-10-12T00:00:00Z" },
            { "kind": "session", "utilization": 42.5 },
            { "kind": "weekly_scoped", "utilization": 5.0, "scope": { "model": { "display_name": "Fable" } } }
        ]});
        let w = parse(&limits);
        assert_eq!(
            w.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(),
            ["5h", "7d", "Fable"]
        );
        assert_eq!(w[0].used, 42.5);
        let legacy = json!({ "five_hour": { "utilization": 10 }, "seven_day": { "utilization": 3, "resets_at": "x" } });
        assert_eq!(
            parse(&legacy)[1],
            Window {
                label: "7d".into(),
                used: 3.,
                resets_at: Some("x".into())
            }
        );
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
