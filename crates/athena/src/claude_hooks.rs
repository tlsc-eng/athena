//! Claude Code hooks that tell Athena when a session starts working, stops, needs input, or
//! edits a file.
//! Written only on request, into the project's `.claude/settings.local.json`, which Claude Code
//! keeps out of version control.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

/// Every Athena hook command contains this, which is how they are found again.
const MARKER: &str = "athena\" notify --event";

pub fn settings_path(root: &Path) -> PathBuf {
    root.join(".claude/settings.local.json")
}

/// Claude's file-writing tools; their hook input names the file in `tool_input.file_path`.
const EDIT_TOOLS: &str = "Edit|MultiEdit|Write";

fn hooks(athena: &Path) -> [(&'static str, Option<&'static str>, String); 5] {
    let cmd = |event: &str| format!("\"{}\" notify --event {event}", athena.display());
    [
        ("UserPromptSubmit", None, cmd("claude-running")),
        ("Stop", None, cmd("claude-stop")),
        (
            "Notification",
            Some("permission_prompt|idle_prompt"),
            cmd("claude-needs-input"),
        ),
        // Before the first edit, to keep the file as it was; after each, to offer the diff.
        ("PreToolUse", Some(EDIT_TOOLS), cmd("claude-will-edit")),
        ("PostToolUse", Some(EDIT_TOOLS), cmd("claude-edited")),
    ]
}

fn is_ours_command(hook: &Value) -> bool {
    hook["command"].as_str().is_some_and(|c| c.contains(MARKER))
}

fn is_ours(entry: &Value) -> bool {
    entry["hooks"]
        .as_array()
        .is_some_and(|hs| hs.iter().any(is_ours_command))
}

/// Settings with Athena's hooks added (`enable`) or removed, leaving everything else as it was.
/// Fails on settings whose shape it does not understand rather than guess.
pub fn merge(mut settings: Value, athena: &Path, enable: bool) -> Result<Value> {
    let Value::Object(top) = &mut settings else {
        bail!("the settings are not a JSON object");
    };
    let had_hooks = top.contains_key("hooks");
    let mut dropped_event = false;
    let all_hooks = match top.entry("hooks").or_insert_with(|| json!({})) {
        Value::Object(m) => m,
        _ => bail!("\"hooks\" is not a JSON object"),
    };
    for (event, matcher, command) in hooks(athena) {
        let had_event = all_hooks.contains_key(event);
        let mut stripped = false;
        let entries = match all_hooks.entry(event).or_insert_with(|| json!([])) {
            Value::Array(a) => a,
            _ => bail!("\"hooks.{event}\" is not a JSON array"),
        };
        // Strip only our own commands so a user's command sharing an entry with ours survives.
        entries.retain_mut(
            |entry| match entry.get_mut("hooks").and_then(Value::as_array_mut) {
                Some(hs) if hs.iter().any(is_ours_command) => {
                    hs.retain(|h| !is_ours_command(h));
                    stripped = true;
                    !hs.is_empty()
                }
                _ => true,
            },
        );
        if enable {
            let mut entry = json!({ "hooks": [{ "type": "command", "command": command }] });
            if let Some(m) = matcher {
                entry["matcher"] = json!(m);
            }
            entries.push(entry);
        }
        if entries.is_empty() && (stripped || !had_event) {
            all_hooks.shift_remove(event);
            dropped_event |= had_event;
        }
    }
    if all_hooks.is_empty() && (dropped_event || !had_hooks) {
        top.shift_remove("hooks");
    }
    Ok(settings)
}

/// Whether any Athena hook is installed, so there is something to remove.
pub fn installed(root: &Path) -> bool {
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

/// Whether every current Athena hook is installed; an install from an older Athena is not, so
/// enabling again adds the hooks it lacks.
pub fn enabled(root: &Path) -> bool {
    let Some(settings) = fs::read_to_string(settings_path(root))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return false;
    };
    hooks(Path::new("athena"))
        .iter()
        .all(|(event, _, command)| {
            let event_flag = command.rsplit(' ').next().unwrap_or_default();
            settings["hooks"][event].as_array().is_some_and(|entries| {
                entries.iter().any(|e| {
                    is_ours(e)
                        && e["hooks"].as_array().is_some_and(|hs| {
                            hs.iter().any(|h| {
                                h["command"]
                                    .as_str()
                                    .is_some_and(|c| c.ends_with(&format!(" {event_flag}")))
                            })
                        })
                })
            })
        })
}

/// Adds or removes Athena's hooks in the project's local Claude settings.
pub fn write(root: &Path, athena: &Path, enable: bool) -> Result<()> {
    let path = settings_path(root);
    // Write through a symlinked settings file to its target instead of replacing the link.
    let path = match fs::canonicalize(&path) {
        Ok(real) => real,
        Err(_) if path.is_symlink() => bail!("{} is a broken symlink", path.display()),
        Err(_) => path,
    };
    let (current, perms) = match fs::read_to_string(&path) {
        Ok(text) => (
            serde_json::from_str(&text)
                .with_context(|| format!("{} is not valid JSON", path.display()))?,
            Some(fs::metadata(&path)?.permissions()),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Value::Object(Map::new()), None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let updated = merge(current.clone(), athena, enable)
        .with_context(|| format!("{} has an unexpected shape", path.display()))?;
    if updated == current {
        return Ok(());
    }
    fs::create_dir_all(path.parent().expect("settings path has a parent"))?;
    let mut name = path
        .file_name()
        .expect("settings path has a name")
        .to_owned();
    name.push(".athena-tmp");
    let tmp = path.with_file_name(name);
    let mut out = fs::File::create(&tmp)?;
    if let Some(perms) = perms {
        out.set_permissions(perms)?;
    }
    out.write_all((serde_json::to_string_pretty(&updated)? + "\n").as_bytes())?;
    out.sync_all()?;
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
        let out = merge(existing, Path::new(ATHENA), true).unwrap();
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
        let once = merge(json!({}), Path::new(ATHENA), true).unwrap();
        let twice = merge(once.clone(), Path::new(ATHENA), true).unwrap();
        assert_eq!(once, twice);
        let removed = merge(twice, Path::new(ATHENA), false).unwrap();
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
        assert!(!enabled(&dir) && !installed(&dir));
        fs::write(settings_path(&dir), "{ not json").unwrap();
        assert!(
            write(&dir, Path::new(ATHENA), true).is_err(),
            "never overwrite a file we can't parse"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    /// A settings file as a user might have it: their own hooks on the same events Athena uses.
    fn user_settings() -> Value {
        json!({
            "model": "opus",
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "guard.sh" }] }
                ],
                "PostToolUse": [
                    { "matcher": "Edit|Write", "hooks": [{ "type": "command", "command": "prettier --write" }] }
                ],
                "SessionStart": [{ "hooks": [{ "type": "command", "command": "echo hi" }] }]
            }
        })
    }

    #[test]
    fn edit_hooks_are_appended_after_the_users_own_and_removed_cleanly() {
        let out = merge(user_settings(), Path::new(ATHENA), true).unwrap();
        let pre = out["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0]["hooks"][0]["command"], "guard.sh");
        assert_eq!(pre[1]["matcher"], "Edit|MultiEdit|Write");
        assert_eq!(
            pre[1]["hooks"][0]["command"],
            "\"/opt/homebrew/bin/athena\" notify --event claude-will-edit"
        );
        let post = out["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(post[0]["hooks"][0]["command"], "prettier --write");
        assert_eq!(
            post[1]["hooks"][0]["command"],
            "\"/opt/homebrew/bin/athena\" notify --event claude-edited"
        );
        assert_eq!(
            out["hooks"]["SessionStart"],
            user_settings()["hooks"]["SessionStart"]
        );
        assert_eq!(out["model"], "opus");

        let again = merge(out.clone(), Path::new(ATHENA), true).unwrap();
        assert_eq!(again, out, "enabling twice changes nothing");
        assert_eq!(
            merge(again, Path::new(ATHENA), false).unwrap(),
            user_settings()
        );
    }

    #[test]
    fn an_install_from_an_older_athena_reads_as_not_enabled_until_upgraded() {
        let dir = std::env::temp_dir().join(format!("athena-hooks-old-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".claude")).unwrap();
        let mut old = merge(user_settings(), Path::new(ATHENA), true).unwrap();
        let hooks = old["hooks"].as_object_mut().unwrap();
        for event in ["PreToolUse", "PostToolUse"] {
            let kept: Vec<Value> = hooks[event]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| !is_ours(e))
                .cloned()
                .collect();
            hooks.insert(event.into(), Value::Array(kept));
        }
        fs::write(settings_path(&dir), old.to_string()).unwrap();
        assert!(!enabled(&dir));
        assert!(installed(&dir), "an older install can still be removed");
        write(&dir, Path::new(ATHENA), true).unwrap();
        assert!(enabled(&dir));
        let written: Value =
            serde_json::from_str(&fs::read_to_string(settings_path(&dir)).unwrap()).unwrap();
        assert_eq!(
            written["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "guard.sh"
        );
        assert_eq!(written["hooks"]["Stop"].as_array().unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("athena-hooks-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".claude")).unwrap();
        dir
    }

    #[test]
    fn a_users_command_sharing_an_entry_with_ours_survives_uninstall_and_reinstall() {
        let ours = "\"/opt/homebrew/bin/athena\" notify --event claude-stop";
        let shared = json!({
            "hooks": { "Stop": [{ "hooks": [
                { "type": "command", "command": ours },
                { "type": "command", "command": "lint.sh" }
            ] }] }
        });
        let removed = merge(shared, Path::new(ATHENA), false).unwrap();
        assert_eq!(
            removed,
            json!({ "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "lint.sh" }] }] } })
        );
        let again = merge(removed, Path::new(ATHENA), true).unwrap();
        let stop = again["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"][0]["command"], "lint.sh");
        assert_eq!(stop[1]["hooks"][0]["command"], ours);
    }

    #[test]
    fn settings_of_an_unexpected_shape_are_refused_and_left_untouched() {
        let dir = temp("shape");
        for text in [
            "[]",
            "{\"hooks\": \"yes\"}",
            "{\"hooks\": {\"Stop\": {}}}",
            "{\"hooks\": {\"PreToolUse\": 3}}",
        ] {
            fs::write(settings_path(&dir), text).unwrap();
            assert!(write(&dir, Path::new(ATHENA), true).is_err(), "{text}");
            assert!(write(&dir, Path::new(ATHENA), false).is_err(), "{text}");
            assert_eq!(fs::read_to_string(settings_path(&dir)).unwrap(), text);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn key_order_survives_install_and_uninstall() {
        let dir = temp("order");
        let settings = json!({
            "zeta": 1,
            "hooks": {
                "SessionStart": [],
                "Stop": [{ "hooks": [{ "type": "command", "command": "say done" }] }]
            },
            "alpha": true
        });
        let text = serde_json::to_string_pretty(&settings).unwrap() + "\n";
        fs::write(settings_path(&dir), &text).unwrap();
        write(&dir, Path::new(ATHENA), true).unwrap();
        let on = fs::read_to_string(settings_path(&dir)).unwrap();
        let order = |t: &str| {
            ["zeta", "hooks", "SessionStart", "Stop", "alpha"]
                .map(|k| t.find(&format!("\"{k}\"")).unwrap())
        };
        assert!(order(&on).is_sorted(), "{on}");
        write(&dir, Path::new(ATHENA), false).unwrap();
        assert_eq!(fs::read_to_string(settings_path(&dir)).unwrap(), text);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_symlinked_settings_file_is_written_through_and_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("link");
        let real = dir.join("dotfiles-settings.json");
        fs::write(&real, "{\"model\": \"opus\"}").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&real, settings_path(&dir)).unwrap();
        write(&dir, Path::new(ATHENA), true).unwrap();
        assert!(settings_path(&dir).is_symlink(), "the link is kept");
        assert!(enabled(&dir));
        let written: Value = serde_json::from_str(&fs::read_to_string(&real).unwrap()).unwrap();
        assert_eq!(written["model"], "opus");
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        write(&dir, Path::new(ATHENA), false).unwrap();
        assert!(settings_path(&dir).is_symlink());
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert!(fs::read_dir(&dir).unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with("athena-tmp")
        }));
        fs::remove_dir_all(dir).unwrap();
    }
}
