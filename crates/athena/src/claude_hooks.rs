//! Claude Code hooks that tell Athena when a session starts working, stops, or needs input.
//! Written only on request, into the project's `.claude/settings.local.json`, which Claude Code
//! keeps out of version control.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

/// Every Athena hook command contains this, which is how they are found again.
const MARKER: &str = "athena\" notify --event";

pub fn settings_path(root: &Path) -> PathBuf {
    root.join(".claude/settings.local.json")
}

fn hooks(athena: &Path) -> [(&'static str, Option<&'static str>, String); 3] {
    let cmd = |event: &str| format!("\"{}\" notify --event {event}", athena.display());
    [
        ("UserPromptSubmit", None, cmd("claude-running")),
        ("Stop", None, cmd("claude-stop")),
        (
            "Notification",
            Some("permission_prompt|idle_prompt"),
            cmd("claude-needs-input"),
        ),
    ]
}

fn is_ours(entry: &Value) -> bool {
    entry["hooks"].as_array().is_some_and(|hs| {
        hs.iter()
            .any(|h| h["command"].as_str().is_some_and(|c| c.contains(MARKER)))
    })
}

/// Settings with Athena's hooks added (`enable`) or removed, leaving everything else as it was.
pub fn merge(settings: Value, athena: &Path, enable: bool) -> Value {
    let mut settings = match settings {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let mut all_hooks = match settings.remove("hooks") {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    for (event, matcher, command) in hooks(athena) {
        let mut entries: Vec<Value> = match all_hooks.remove(event) {
            Some(Value::Array(a)) => a.into_iter().filter(|e| !is_ours(e)).collect(),
            _ => Vec::new(),
        };
        if enable {
            let mut entry = json!({ "hooks": [{ "type": "command", "command": command }] });
            if let Some(m) = matcher {
                entry["matcher"] = json!(m);
            }
            entries.push(entry);
        }
        if !entries.is_empty() {
            all_hooks.insert(event.to_string(), Value::Array(entries));
        }
    }
    if !all_hooks.is_empty() {
        settings.insert("hooks".into(), Value::Object(all_hooks));
    }
    Value::Object(settings)
}

pub fn enabled(root: &Path) -> bool {
    let Some(settings) = fs::read_to_string(settings_path(root))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return false;
    };
    settings["hooks"].as_object().is_some_and(|events| {
        events
            .values()
            .filter_map(Value::as_array)
            .flatten()
            .any(is_ours)
    })
}

/// Adds or removes Athena's hooks in the project's local Claude settings.
pub fn write(root: &Path, athena: &Path, enable: bool) -> Result<()> {
    let path = settings_path(root);
    let current = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let updated = merge(current, athena, enable);
    fs::create_dir_all(path.parent().expect("settings path has a parent"))?;
    let tmp = path.with_extension("json.athena-tmp");
    fs::write(&tmp, serde_json::to_string_pretty(&updated)? + "\n")?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// The `athena` command hooks should run: the Homebrew link if installed, else this executable.
pub fn athena_command() -> PathBuf {
    let brew = Path::new("/opt/homebrew/bin/athena");
    if brew.exists() {
        return brew.to_path_buf();
    }
    std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .unwrap_or_else(|_| PathBuf::from("athena"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ATHENA: &str = "/opt/homebrew/bin/athena";

    #[test]
    fn adds_hooks_alongside_existing_settings() {
        let existing = json!({
            "permissions": { "allow": ["Bash(ls)"] },
            "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "say done" }] }] }
        });
        let out = merge(existing, Path::new(ATHENA), true);
        assert_eq!(out["permissions"]["allow"][0], "Bash(ls)");
        let stop = out["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "the user's own Stop hook is kept");
        assert_eq!(
            stop[1]["hooks"][0]["command"],
            "\"/opt/homebrew/bin/athena\" notify --event claude-stop"
        );
        assert_eq!(
            out["hooks"]["Notification"][0]["matcher"],
            "permission_prompt|idle_prompt"
        );
    }

    #[test]
    fn is_idempotent_and_removable() {
        let once = merge(json!({}), Path::new(ATHENA), true);
        let twice = merge(once.clone(), Path::new(ATHENA), true);
        assert_eq!(once, twice);
        let removed = merge(twice, Path::new(ATHENA), false);
        assert_eq!(removed, json!({}));
    }

    #[test]
    fn writes_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("athena-hooks-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(!enabled(&dir));
        write(&dir, Path::new(ATHENA), true).unwrap();
        assert!(enabled(&dir));
        write(&dir, Path::new(ATHENA), false).unwrap();
        assert!(!enabled(&dir));
        fs::write(settings_path(&dir), "{ not json").unwrap();
        assert!(
            write(&dir, Path::new(ATHENA), true).is_err(),
            "never overwrite a file we can't parse"
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
