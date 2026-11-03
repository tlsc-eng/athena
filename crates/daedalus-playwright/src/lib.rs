//! Playwright for Athena: finding a project's config, reading the JSON reporter's output, and the
//! optional `.mcp.json` entry that lets Claude Code drive a browser.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

/// The Playwright MCP server version written into `.mcp.json`; pinned so a project never runs
/// whatever npm serves that day.
pub const MCP_PACKAGE: &str = "@playwright/mcp@0.0.83";

const CONFIG_NAMES: &[&str] = &[
    "playwright.config.ts",
    "playwright.config.mts",
    "playwright.config.js",
    "playwright.config.mjs",
    "playwright.config.cjs",
];

/// The config file and the folder tests run from: the project root, or `e2e/` / `web/` under it.
pub fn find_config(root: &Path) -> Option<PathBuf> {
    ["", "e2e", "web", "tests"].iter().find_map(|sub| {
        let dir = root.join(sub);
        CONFIG_NAMES
            .iter()
            .map(|n| dir.join(n))
            .find(|p| p.is_file())
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed,
    Flaky,
    Skipped,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Attachment {
    pub name: String,
    pub content_type: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TestResult {
    /// `file › describe › test`.
    pub title: String,
    pub file: String,
    pub line: u32,
    pub project: String,
    pub outcome: Outcome,
    pub duration_ms: u64,
    pub error: Option<String>,
    /// From the last attempt, which is the one that decided the outcome.
    pub attachments: Vec<Attachment>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub tests: Vec<TestResult>,
    pub duration_ms: u64,
}

impl Report {
    pub fn count(&self, outcome: Outcome) -> usize {
        self.tests.iter().filter(|t| t.outcome == outcome).count()
    }
}

/// Reads the JSON reporter's file (`--reporter=json`, `PLAYWRIGHT_JSON_OUTPUT_NAME`).
pub fn read_report(path: &Path) -> Result<Report> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let value: Value =
        serde_json::from_str(&text).context("Playwright report is not valid JSON")?;
    Ok(parse_report(&value))
}

pub fn parse_report(v: &Value) -> Report {
    let mut tests = Vec::new();
    for suite in v["suites"].as_array().into_iter().flatten() {
        walk_suite(suite, &[], &mut tests);
    }
    Report {
        tests,
        duration_ms: v["stats"]["duration"].as_f64().unwrap_or(0.) as u64,
    }
}

fn walk_suite(suite: &Value, path: &[String], out: &mut Vec<TestResult>) {
    let mut path = path.to_vec();
    if let Some(title) = suite["title"].as_str().filter(|t| !t.is_empty()) {
        path.push(title.to_string());
    }
    for spec in suite["specs"].as_array().into_iter().flatten() {
        let mut title = path.clone();
        title.push(spec["title"].as_str().unwrap_or("").to_string());
        for test in spec["tests"].as_array().into_iter().flatten() {
            let results = test["results"].as_array().cloned().unwrap_or_default();
            let last = results.last().cloned().unwrap_or(Value::Null);
            let outcome = match test["status"].as_str() {
                Some("expected") => Outcome::Passed,
                Some("flaky") => Outcome::Flaky,
                Some("skipped") => Outcome::Skipped,
                _ => Outcome::Failed,
            };
            let error = results
                .iter()
                .rev()
                .find_map(|r| {
                    r["error"]["message"]
                        .as_str()
                        .or_else(|| r["errors"][0]["message"].as_str())
                })
                .map(strip_ansi);
            let attachments = last["attachments"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| {
                    Some(Attachment {
                        name: a["name"].as_str()?.to_string(),
                        content_type: a["contentType"].as_str().unwrap_or("").to_string(),
                        path: PathBuf::from(a["path"].as_str()?),
                    })
                })
                .collect();
            out.push(TestResult {
                title: title.join(" › "),
                file: spec["file"].as_str().unwrap_or("").to_string(),
                line: spec["line"].as_u64().unwrap_or(0) as u32,
                project: test["projectName"].as_str().unwrap_or("").to_string(),
                outcome,
                duration_ms: results.iter().filter_map(|r| r["duration"].as_u64()).sum(),
                error,
                attachments,
            });
        }
    }
    for child in suite["suites"].as_array().into_iter().flatten() {
        walk_suite(child, &path, out);
    }
}

/// Playwright colours its error messages; the panel shows plain text.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

pub fn mcp_json_path(root: &Path) -> PathBuf {
    root.join(".mcp.json")
}

/// `.mcp.json` with the Playwright MCP server added (`enable`) or removed; other servers kept.
pub fn merge_mcp_json(current: Value, enable: bool) -> Value {
    let mut root = match current {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let mut servers = match root.remove("mcpServers") {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    servers.remove("playwright");
    if enable {
        servers.insert(
            "playwright".into(),
            json!({ "command": "npx", "args": [MCP_PACKAGE, "--isolated"] }),
        );
    }
    if !servers.is_empty() {
        root.insert("mcpServers".into(), Value::Object(servers));
    }
    Value::Object(root)
}

pub fn mcp_enabled(root: &Path) -> bool {
    fs::read_to_string(mcp_json_path(root))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .is_some_and(|v| v["mcpServers"]["playwright"].is_object())
}

/// Writes the merged `.mcp.json`; with `keep_local`, also lists it in `.git/info/exclude` so it
/// is not committed. A file that is not valid JSON is left alone.
pub fn write_mcp_json(root: &Path, enable: bool, keep_local: bool) -> Result<()> {
    let path = mcp_json_path(root);
    let current = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(e) => return Err(e.into()),
    };
    let merged = merge_mcp_json(current, enable);
    if merged == json!({}) {
        fs::remove_file(&path).ok();
    } else {
        fs::write(&path, serde_json::to_string_pretty(&merged)? + "\n")?;
    }
    if keep_local && enable {
        exclude_from_git(root, ".mcp.json")?;
    }
    Ok(())
}

fn exclude_from_git(root: &Path, entry: &str) -> Result<()> {
    let info = root.join(".git/info");
    if !info.is_dir() {
        return Ok(());
    }
    let exclude = info.join("exclude");
    let existing = fs::read_to_string(&exclude).unwrap_or_default();
    if existing
        .lines()
        .any(|l| l.trim() == entry || l.trim() == format!("/{entry}"))
    {
        return Ok(());
    }
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    fs::write(&exclude, format!("{existing}{separator}/{entry}\n"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Value {
        json!({
            "stats": { "duration": 4321.5 },
            "suites": [{
                "title": "login.spec.ts", "file": "login.spec.ts",
                "specs": [{
                    "title": "signs in", "file": "login.spec.ts", "line": 4,
                    "tests": [{ "projectName": "phone", "status": "expected",
                        "results": [{ "status": "passed", "duration": 800, "attachments": [] }] }]
                }],
                "suites": [{
                    "title": "admin",
                    "specs": [{
                        "title": "rejects bad password", "file": "login.spec.ts", "line": 12,
                        "tests": [{ "projectName": "phone", "status": "unexpected",
                            "results": [
                                { "status": "failed", "duration": 500 },
                                { "status": "failed", "duration": 600,
                                  "error": { "message": "\u{1b}[31mExpected\u{1b}[39m 401" },
                                  "attachments": [
                                    { "name": "screenshot", "contentType": "image/png", "path": "/r/shot.png" },
                                    { "name": "trace", "contentType": "application/zip", "path": "/r/trace.zip" }
                                  ] }
                            ] }]
                    }]
                }]
            }]
        })
    }

    #[test]
    fn flattens_nested_suites_with_retries() {
        let r = parse_report(&report());
        assert_eq!(r.tests.len(), 2);
        assert_eq!(r.count(Outcome::Passed), 1);
        let failed = &r.tests[1];
        assert_eq!(failed.title, "login.spec.ts › admin › rejects bad password");
        assert_eq!(failed.outcome, Outcome::Failed);
        assert_eq!(failed.duration_ms, 1100, "all attempts counted");
        assert_eq!(failed.error.as_deref(), Some("Expected 401"));
        assert_eq!(failed.attachments.len(), 2);
        assert_eq!(r.duration_ms, 4321);
    }

    #[test]
    fn mcp_json_keeps_other_servers() {
        let current = json!({ "mcpServers": { "db": { "command": "db-mcp" } }, "x": 1 });
        let on = merge_mcp_json(current, true);
        assert_eq!(on["mcpServers"]["db"]["command"], "db-mcp");
        assert_eq!(on["mcpServers"]["playwright"]["args"][0], MCP_PACKAGE);
        let off = merge_mcp_json(on, false);
        assert_eq!(
            off,
            json!({ "mcpServers": { "db": { "command": "db-mcp" } }, "x": 1 })
        );
        assert_eq!(merge_mcp_json(json!({}), false), json!({}));
    }

    #[test]
    fn finds_configs_and_excludes_from_git() {
        let dir = std::env::temp_dir().join(format!("athena-pw-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("e2e")).unwrap();
        fs::create_dir_all(dir.join(".git/info")).unwrap();
        assert_eq!(find_config(&dir), None);
        fs::write(dir.join("e2e/playwright.config.ts"), "").unwrap();
        assert_eq!(
            find_config(&dir),
            Some(dir.join("e2e/playwright.config.ts"))
        );
        write_mcp_json(&dir, true, true).unwrap();
        write_mcp_json(&dir, true, true).unwrap();
        assert!(mcp_enabled(&dir));
        assert_eq!(
            fs::read_to_string(dir.join(".git/info/exclude")).unwrap(),
            "/.mcp.json\n"
        );
        write_mcp_json(&dir, false, false).unwrap();
        assert!(
            !dir.join(".mcp.json").exists(),
            "removed when nothing else was in it"
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
