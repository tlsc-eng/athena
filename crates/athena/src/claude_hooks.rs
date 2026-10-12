//! Claude Code hooks that tell Athena when a session starts working, stops, needs input, or
//! edits a file.
//! Kept in each Claude Code config folder's `settings.json` unless `claude.hooks` is off; they do
//! nothing in a terminal Athena did not start.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

/// Every Athena hook command contains this, which is how they are found again.
const MARKER: &str = "athena\" notify --event";

/// Where Athena 0.10 and earlier put its hooks, one project at a time.
pub fn settings_path(root: &Path) -> PathBuf {
    root.join(".claude/settings.local.json")
}

/// The `settings.json` of `~/.claude` and of every `~/.claude-*` folder Claude Code has run from.
pub fn global_settings(home: &Path) -> Vec<PathBuf> {
    let default = home.join(".claude");
    let mut dirs: Vec<PathBuf> = default.is_dir().then_some(default).into_iter().collect();
    dirs.extend(
        crate::transcripts::profiles(home)
            .into_iter()
            .filter_map(|p| p.config_dir),
    );
    dirs.dedup();
    dirs.into_iter().map(|d| d.join("settings.json")).collect()
}

/// Claude's file-writing tools; their hook input names the file in `tool_input.file_path`.
const EDIT_TOOLS: &str = "Edit|MultiEdit|Write";
/// Tools whose input is the session's todo list or its plan.
const PLAN_TOOLS: &str = "TodoWrite|ExitPlanMode";
/// Notifications that wait on the user; `idle_prompt` is only a reminder after Claude finished.
const ASKS: &str = "permission_prompt|elicitation_dialog|elicitation_url_dialog|agent_needs_input|worker_permission_prompt";

fn hooks(athena: &Path) -> [(&'static str, Option<&'static str>, String); 9] {
    let athena = athena.display();
    // Settings are shared with Claude sessions outside Athena, where the hook must stay silent.
    let cmd = |event: &str| {
        format!(
            "[ -n \"$ATHENA_PANE_ID\" ] && [ -x \"{athena}\" ] && \"{athena}\" notify --event {event} || true"
        )
    };
    [
        ("UserPromptSubmit", None, cmd("claude-running")),
        ("Stop", None, cmd("claude-stop")),
        ("Notification", Some(ASKS), cmd("claude-needs-input")),
        // Every tool call, so a question or plan reads as waiting and the rest as working.
        ("PreToolUse", None, cmd("claude-tool")),
        ("PostToolUse", None, cmd("claude-running")),
        ("PostToolUseFailure", None, cmd("claude-running")),
        // Before the first edit, to keep the file as it was; after each, to offer the diff.
        ("PreToolUse", Some(EDIT_TOOLS), cmd("claude-will-edit")),
        ("PostToolUse", Some(EDIT_TOOLS), cmd("claude-edited")),
        ("PostToolUse", Some(PLAN_TOOLS), cmd("claude-plan")),
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
    let ours = hooks(athena);
    let mut events: Vec<&str> = ours.iter().map(|(event, ..)| *event).collect();
    events.dedup();
    for event in events {
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
            for (_, matcher, command) in ours.iter().filter(|(e, ..)| *e == event) {
                let mut entry = json!({ "hooks": [{ "type": "command", "command": command }] });
                if let Some(m) = matcher {
                    entry["matcher"] = json!(m);
                }
                entries.push(entry);
            }
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

/// Whether the settings at `path` hold any Athena hook, so there is something to remove.
pub fn installed(path: &Path) -> bool {
    let Some(settings) = fs::read_to_string(path)
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

/// Whether the settings at `path` hold every current Athena hook; an install from an older Athena
/// does not, so enabling again brings it up to date.
pub fn enabled(path: &Path) -> bool {
    let Some(settings) = fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return false;
    };
    hooks(Path::new("athena"))
        .iter()
        .all(|(event, _, command)| {
            let event_flag = command
                .split("--event ")
                .nth(1)
                .and_then(|rest| rest.split(' ').next())
                .unwrap_or_default();
            settings["hooks"][event].as_array().is_some_and(|entries| {
                entries.iter().any(|e| {
                    is_ours(e)
                        && e["hooks"].as_array().is_some_and(|hs| {
                            hs.iter().any(|h| {
                                h["command"].as_str().is_some_and(|c| {
                                    // Hooks from before the guard ran in every Claude session.
                                    c.contains("$ATHENA_PANE_ID")
                                        && c.contains(&format!("--event {event_flag} "))
                                })
                            })
                        })
                })
            })
        })
}

/// Adds or removes Athena's hooks in the Claude settings at `path`, keeping a copy of the
/// file as it was before Athena first changed it when `backup` is set.
pub fn write(path: &Path, athena: &Path, enable: bool, backup: bool) -> Result<()> {
    let path = path.to_path_buf();
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
    let mut saved = path
        .file_name()
        .expect("settings path has a name")
        .to_owned();
    saved.push(".athena-backup");
    let saved = path.with_file_name(saved);
    if backup && perms.is_some() && !saved.exists() {
        fs::copy(&path, &saved)?;
    }
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

/// Brings Athena's hooks in every Claude Code config folder under `home` in line with `enable`,
/// and takes the per-project hooks of older Athenas out of `projects`.
pub fn sync(home: &Path, athena: &Path, enable: bool, projects: &[PathBuf]) -> Result<()> {
    let mut failed = Vec::new();
    for path in global_settings(home) {
        if let Err(e) = write(&path, athena, enable, true) {
            failed.push(format!("{}: {e:#}", path.display()));
        }
    }
    for path in projects.iter().map(|root| settings_path(root)) {
        if installed(&path)
            && let Err(e) = write(&path, athena, false, false)
        {
            failed.push(format!("{}: {e:#}", path.display()));
        }
    }
    if !failed.is_empty() {
        bail!("{}", failed.join("; "));
    }
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

    fn command(event: &str) -> String {
        format!(
            "[ -n \"$ATHENA_PANE_ID\" ] && [ -x \"{ATHENA}\" ] && \"{ATHENA}\" notify --event {event} || true"
        )
    }

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
        assert_eq!(stop[1]["hooks"][0]["command"], command("claude-stop"));
        let asks = out["hooks"]["Notification"][0]["matcher"].as_str().unwrap();
        assert!(asks.contains("permission_prompt") && asks.contains("elicitation_dialog"));
        assert!(
            !asks.contains("idle_prompt"),
            "a reminder after finishing is not a question"
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
        assert!(!enabled(&settings_path(&dir)));
        write(&settings_path(&dir), Path::new(ATHENA), true, false).unwrap();
        assert!(enabled(&settings_path(&dir)));
        write(&settings_path(&dir), Path::new(ATHENA), false, false).unwrap();
        assert!(!enabled(&settings_path(&dir)) && !installed(&settings_path(&dir)));
        fs::write(settings_path(&dir), "{ not json").unwrap();
        assert!(
            write(&settings_path(&dir), Path::new(ATHENA), true, false).is_err(),
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
        assert_eq!(pre.len(), 3);
        assert_eq!(pre[0]["hooks"][0]["command"], "guard.sh");
        assert_eq!(
            pre[1].get("matcher"),
            None,
            "every tool reports the session busy"
        );
        assert_eq!(pre[1]["hooks"][0]["command"], command("claude-tool"));
        assert_eq!(pre[2]["matcher"], "Edit|MultiEdit|Write");
        assert_eq!(pre[2]["hooks"][0]["command"], command("claude-will-edit"));
        let post = out["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(post[0]["hooks"][0]["command"], "prettier --write");
        assert_eq!(post[1]["hooks"][0]["command"], command("claude-running"));
        assert_eq!(post[2]["hooks"][0]["command"], command("claude-edited"));
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
    fn the_plan_hook_sits_beside_the_edit_hook_and_both_survive_enabling_twice() {
        let out = merge(user_settings(), Path::new(ATHENA), true).unwrap();
        let post = out["hooks"]["PostToolUse"].as_array().unwrap();
        let matchers: Vec<&str> = post
            .iter()
            .map(|e| e["matcher"].as_str().unwrap_or("*"))
            .collect();
        assert_eq!(
            matchers,
            [
                "Edit|Write",
                "*",
                "Edit|MultiEdit|Write",
                "TodoWrite|ExitPlanMode"
            ]
        );
        assert_eq!(post[3]["hooks"][0]["command"], command("claude-plan"));
        assert_eq!(merge(out.clone(), Path::new(ATHENA), true).unwrap(), out);
        assert_eq!(
            merge(out, Path::new(ATHENA), false).unwrap(),
            user_settings()
        );
    }

    #[test]
    fn an_install_without_the_plan_hook_is_upgraded_without_duplicating_the_edit_hook() {
        let dir = temp("plan-upgrade");
        let mut old = merge(user_settings(), Path::new(ATHENA), true).unwrap();
        old["hooks"]["PostToolUse"]
            .as_array_mut()
            .unwrap()
            .retain(|e| e["matcher"] != PLAN_TOOLS);
        fs::write(settings_path(&dir), old.to_string()).unwrap();
        assert!(!enabled(&settings_path(&dir)) && installed(&settings_path(&dir)));
        write(&settings_path(&dir), Path::new(ATHENA), true, false).unwrap();
        assert!(enabled(&settings_path(&dir)));
        let written: Value =
            serde_json::from_str(&fs::read_to_string(settings_path(&dir)).unwrap()).unwrap();
        let post = written["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(post.len(), 4, "{post:?}");
        assert_eq!(post.iter().filter(|e| is_ours(e)).count(), 3);
        fs::remove_dir_all(dir).unwrap();
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
        assert!(!enabled(&settings_path(&dir)));
        assert!(
            installed(&settings_path(&dir)),
            "an older install can still be removed"
        );
        write(&settings_path(&dir), Path::new(ATHENA), true, false).unwrap();
        assert!(enabled(&settings_path(&dir)));
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
        let ours = command("claude-stop");
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

        let odd = json!({ "hooks": { "Stop": ["x", { "hooks": "y" }] } });
        let round_trip = merge(odd.clone(), Path::new(ATHENA), true).unwrap();
        assert_eq!(merge(round_trip, Path::new(ATHENA), false).unwrap(), odd);
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
            assert!(
                write(&settings_path(&dir), Path::new(ATHENA), true, false).is_err(),
                "{text}"
            );
            assert!(
                write(&settings_path(&dir), Path::new(ATHENA), false, false).is_err(),
                "{text}"
            );
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
        write(&settings_path(&dir), Path::new(ATHENA), true, false).unwrap();
        let on = fs::read_to_string(settings_path(&dir)).unwrap();
        let order = |t: &str| {
            ["zeta", "hooks", "SessionStart", "Stop", "alpha"]
                .map(|k| t.find(&format!("\"{k}\"")).unwrap())
        };
        assert!(order(&on).is_sorted(), "{on}");
        write(&settings_path(&dir), Path::new(ATHENA), false, false).unwrap();
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
        write(&settings_path(&dir), Path::new(ATHENA), true, false).unwrap();
        assert!(settings_path(&dir).is_symlink(), "the link is kept");
        assert!(enabled(&settings_path(&dir)));
        let written: Value = serde_json::from_str(&fs::read_to_string(&real).unwrap()).unwrap();
        assert_eq!(written["model"], "opus");
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        write(&settings_path(&dir), Path::new(ATHENA), false, false).unwrap();
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

    #[test]
    fn hooks_from_before_the_guard_are_replaced() {
        let dir = temp("unguarded");
        let old = merge(json!({}), Path::new(ATHENA), true)
            .unwrap()
            .to_string()
            .replace(
                "[ -n \\\"$ATHENA_PANE_ID\\\" ] && [ -x \\\"/opt/homebrew/bin/athena\\\" ] && ",
                "",
            );
        assert!(!old.contains("ATHENA_PANE_ID"), "{old}");
        fs::write(settings_path(&dir), &old).unwrap();
        assert!(!enabled(&settings_path(&dir)));
        write(&settings_path(&dir), Path::new(ATHENA), true, false).unwrap();
        assert!(enabled(&settings_path(&dir)));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn global_install_covers_every_config_folder_keeps_a_backup_and_cleans_projects() {
        use std::os::unix::fs::PermissionsExt;
        let home = temp("home");
        for dir in [".claude", ".claude-work/projects", ".claude-mem"] {
            fs::create_dir_all(home.join(dir)).unwrap();
        }
        let user = home.join(".claude/settings.json");
        fs::write(&user, user_settings().to_string()).unwrap();
        fs::set_permissions(&user, fs::Permissions::from_mode(0o600)).unwrap();
        let project = temp("project");
        write(&settings_path(&project), Path::new(ATHENA), true, false).unwrap();

        sync(
            &home,
            Path::new(ATHENA),
            true,
            std::slice::from_ref(&project),
        )
        .unwrap();
        let work = home.join(".claude-work/settings.json");
        assert!(enabled(&user) && enabled(&work));
        assert!(
            !home.join(".claude-mem/settings.json").exists(),
            "not a config folder"
        );
        assert_eq!(
            fs::metadata(&user).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        let backup = home.join(".claude/settings.json.athena-backup");
        let saved: Value = serde_json::from_str(&fs::read_to_string(&backup).unwrap()).unwrap();
        assert_eq!(saved, user_settings());
        assert!(!installed(&settings_path(&project)));

        sync(&home, Path::new(ATHENA), false, &[]).unwrap();
        let off: Value = serde_json::from_str(&fs::read_to_string(&user).unwrap()).unwrap();
        assert_eq!(off, user_settings());
        assert!(!installed(&work));
        fs::remove_dir_all(home).unwrap();
        fs::remove_dir_all(project).unwrap();
    }
}
