use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Result, anyhow};
use gpui::{
    Action, App, DummyKeyboardMapper, Global, KeyBinding, KeyBindingContextPredicate, Keystroke,
};
use serde::Deserialize;

/// What a new keymap.json holds: an empty list, with an example of each kind of entry.
const TEMPLATE: &str = include_str!("keymap-template.jsonc");

/// The bindings Athena registers itself, kept so the user's file can be re-applied over them.
struct Defaults(Vec<KeyBinding>);

impl Global for Defaults {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    #[serde(default)]
    key: Option<String>,
    command: String,
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    args: Option<serde_json::Value>,
}

/// One usable entry of the user's keymap.
pub enum Rule {
    Bind(KeyBinding),
    /// Removes default bindings of `command`, only those on `key` and in `when` if given.
    Unbind {
        command: String,
        key: Option<KeyBinding>,
        when: Option<KeyBindingContextPredicate>,
    },
}

pub fn path() -> Result<PathBuf> {
    Ok(athena_proto::data_dir()?.join("keymap.json"))
}

/// The file keymap.json links to, when that is in another folder whose changes then matter too.
pub fn link_target(path: &Path) -> Option<PathBuf> {
    let real = path.canonicalize().ok()?;
    let dir = path.parent()?.canonicalize().ok()?;
    (real.parent() != Some(dir.as_path())).then_some(real)
}

/// Writes the commented template unless the file exists, and returns its path.
pub fn ensure_file() -> Result<PathBuf> {
    let path = path()?;
    if !path.exists() {
        std::fs::write(&path, TEMPLATE)?;
    }
    Ok(path)
}

/// Re-applies Athena's bindings plus the user's keymap.json; returns what was wrong with the file.
pub fn reload(cx: &mut App) -> Vec<String> {
    if !cx.has_global::<Defaults>() {
        let defaults = cx.key_bindings().borrow().bindings().cloned().collect();
        cx.set_global(Defaults(defaults));
    }
    let text = match path().map(|p| std::fs::read_to_string(&p)) {
        Ok(Ok(text)) => text,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Ok(Err(e)) => return vec![format!("keymap.json could not be read: {e}")],
        Err(e) => return vec![format!("{e:#}")],
    };
    let (rules, problems) = parse(&text, |name, args| {
        cx.build_action(name, args).map_err(|e| anyhow!("{e}"))
    });
    let bindings = merge(&cx.global::<Defaults>().0, rules);
    cx.clear_key_bindings();
    cx.bind_keys(bindings);
    problems
}

/// Parses keymap.json (comments and trailing commas allowed, as VS Code allows them), keeping
/// every valid entry and describing each invalid one.
pub fn parse(
    text: &str,
    build: impl Fn(&str, Option<serde_json::Value>) -> Result<Box<dyn Action>>,
) -> (Vec<Rule>, Vec<String>) {
    let json = strip_jsonc(text);
    if json.trim().is_empty() {
        return (Vec::new(), Vec::new());
    }
    let values: Vec<serde_json::Value> = match serde_json::from_str(&json) {
        Ok(values) => values,
        Err(e) => {
            return (
                Vec::new(),
                vec![format!("keymap.json is not a list of bindings: {e}")],
            );
        }
    };
    let mut rules = Vec::new();
    let mut problems = Vec::new();
    for (i, value) in values.into_iter().enumerate() {
        match rule(value, &build) {
            Ok(rule) => rules.push(rule),
            Err(e) => problems.push(format!("entry {}: {e}", i + 1)),
        }
    }
    (rules, problems)
}

fn rule(
    value: serde_json::Value,
    build: &impl Fn(&str, Option<serde_json::Value>) -> Result<Box<dyn Action>>,
) -> Result<Rule> {
    let entry: Entry = serde_json::from_value(value)?;
    let when = entry
        .when
        .as_deref()
        .map(|w| KeyBindingContextPredicate::parse(w).map_err(|e| anyhow!("when \"{w}\": {e}")))
        .transpose()?;
    if let Some(name) = entry.command.strip_prefix('-') {
        let key = entry
            .key
            .as_deref()
            .map(|k| binding(k, gpui::NoAction.boxed_clone(), None))
            .transpose()?;
        return Ok(Rule::Unbind {
            command: qualified(name),
            key,
            when,
        });
    }
    let Some(key) = entry.key.as_deref() else {
        return Err(anyhow!("\"{}\" has no \"key\"", entry.command));
    };
    let action = build(&qualified(&entry.command), entry.args)
        .map_err(|e| anyhow!("\"{}\" is not an Athena command ({e})", entry.command))?;
    Ok(Rule::Bind(binding(key, action, when)?))
}

/// Athena's own commands may be named without their `athena::` prefix.
fn qualified(name: &str) -> String {
    if name.contains("::") {
        name.to_string()
    } else {
        format!("athena::{name}")
    }
}

/// Named keys a binding may use besides single characters and F1–F24.
const NAMED_KEYS: &[&str] = &[
    "enter",
    "escape",
    "backspace",
    "delete",
    "tab",
    "space",
    "up",
    "down",
    "left",
    "right",
    "home",
    "end",
    "pageup",
    "pagedown",
    "insert",
];

/// A VS Code keystroke (`cmd+shift+p`) in Athena's spelling (`cmd-shift-p`).
fn athena_keystroke(vscode: &str) -> String {
    if !vscode.contains('+') || vscode == "+" {
        return vscode.to_string();
    }
    let (mods, key) = match vscode.strip_suffix("++") {
        Some(mods) => (mods, "+"),
        None => vscode.rsplit_once('+').unwrap_or(("", vscode)),
    };
    let mut out: Vec<&str> = mods.split('+').filter(|m| !m.is_empty()).collect();
    out.push(key);
    out.join("-")
}

fn binding(
    key: &str,
    action: Box<dyn Action>,
    when: Option<KeyBindingContextPredicate>,
) -> Result<KeyBinding> {
    let strokes: Vec<String> = key.split_whitespace().map(athena_keystroke).collect();
    if strokes.is_empty() {
        return Err(anyhow!("\"key\" is empty"));
    }
    for stroke in &strokes {
        let parsed =
            Keystroke::parse(stroke).map_err(|_| anyhow!("\"{stroke}\" is not a keystroke"))?;
        let name = parsed.key.as_str();
        let known = name.chars().count() == 1
            || NAMED_KEYS.contains(&name)
            || name
                .strip_prefix('f')
                .and_then(|n| n.parse::<u8>().ok())
                .is_some_and(|n| (1..=24).contains(&n));
        if !known {
            return Err(anyhow!("\"{name}\" in \"{stroke}\" is not a key"));
        }
    }
    KeyBinding::load(
        &strokes.join(" "),
        action,
        when.map(Rc::new),
        false,
        None,
        &DummyKeyboardMapper,
    )
    .map_err(|e| anyhow!("\"{key}\": {e}"))
}

/// Athena's bindings minus the ones the user removed or rebound everywhere, then the user's,
/// which win at equal depth because they come later.
pub fn merge(defaults: &[KeyBinding], rules: Vec<Rule>) -> Vec<KeyBinding> {
    let mut out: Vec<KeyBinding> = defaults.to_vec();
    let mut added = Vec::new();
    for rule in rules {
        match rule {
            Rule::Bind(binding) => {
                // gpui falls through to later matches when one is unhandled; VS Code never does.
                if binding.predicate().is_none() {
                    out.retain(|b| b.keystrokes() != binding.keystrokes());
                }
                added.push(binding);
            }
            Rule::Unbind { command, key, when } => out.retain(|b| {
                !(b.action().name() == command
                    && key
                        .as_ref()
                        .is_none_or(|k| k.keystrokes() == b.keystrokes())
                    && when
                        .as_ref()
                        .is_none_or(|w| b.predicate().as_deref() == Some(w)))
            }),
        }
    }
    out.extend(added);
    out
}

/// JSON with `//` and `/* */` comments and trailing commas removed, strings left alone.
pub(crate) fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    if c == '\n' {
                        out.push('\n');
                    }
                    last = c;
                }
            }
            (']' | '}', _) => {
                let trimmed = out.trim_end().len();
                if out[..trimmed].ends_with(',') {
                    out.truncate(trimmed - 1);
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Tells real edits of keymap.json from the folder's other traffic, which FSEvents often reports
/// only as "something in this folder changed".
pub struct FileChange {
    path: PathBuf,
    last: Option<Vec<u8>>,
}

impl FileChange {
    pub fn new(path: PathBuf) -> Self {
        let last = std::fs::read(&path).ok();
        Self { path, last }
    }

    /// Whether the file's bytes differ from the last call (or creation); deleting it counts.
    pub fn changed(&mut self) -> bool {
        let now = std::fs::read(&self.path).ok();
        if now == self.last {
            return false;
        }
        self.last = now;
        true
    }
}

#[cfg(test)]
mod tests {
    use gpui::KeyContext;

    use super::*;
    use crate::actions::{NewTerminal, QuickOpen, SplitDown, SplitRight};

    fn build(name: &str, _: Option<serde_json::Value>) -> Result<Box<dyn Action>> {
        match name {
            "athena::NewTerminal" => Ok(Box::new(NewTerminal)),
            "athena::QuickOpen" => Ok(Box::new(QuickOpen)),
            "athena::SplitRight" => Ok(Box::new(SplitRight)),
            _ => Err(anyhow!("unknown")),
        }
    }

    fn defaults() -> Vec<KeyBinding> {
        vec![
            KeyBinding::new("cmd-d", SplitRight, None),
            KeyBinding::new("cmd-shift-d", SplitDown, None),
            KeyBinding::new("cmd-t", NewTerminal, None),
            KeyBinding::new("cmd-t", NewTerminal, Some("Editor")),
        ]
    }

    fn summary(bindings: &[KeyBinding]) -> Vec<(String, &'static str)> {
        bindings
            .iter()
            .map(|b| {
                let keys: Vec<String> =
                    b.keystrokes().iter().map(|k| k.inner().unparse()).collect();
                (keys.join(" "), b.action().name())
            })
            .collect()
    }

    #[test]
    fn comments_and_trailing_commas_are_allowed_and_strings_kept() {
        let text = r#"// header
        [ /* one */ {"key": "cmd-k cmd-t", "command": "NewTerminal",},
          {"key": "cmd-p", "command": "athena::QuickOpen", "when": "Editor"}, // tail
        ]"#;
        let (rules, problems) = parse(text, build);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(rules.len(), 2);
        assert_eq!(
            strip_jsonc(r#"{"a": "x // y, ]"}"#),
            r#"{"a": "x // y, ]"}"#
        );
        let (rules, problems) = parse(TEMPLATE, build);
        assert!(rules.is_empty() && problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn invalid_entries_are_reported_and_the_rest_kept() {
        let text = r#"[
            {"key": "cmd-k cmd-t", "command": "athena::NewTerminal"},
            {"key": "cmd-k", "command": "athena::Nope"},
            {"key": "cmd-shiftt-p", "command": "athena::QuickOpen"},
            {"key": "cmd-p", "command": "athena::QuickOpen", "when": "Editor &&"},
            {"command": "athena::QuickOpen"},
            {"key": "cmd-p", "command": "athena::QuickOpen", "typo": 1},
            "cmd-p"
        ]"#;
        let (rules, problems) = parse(text, build);
        assert_eq!(rules.len(), 1);
        assert_eq!(problems.len(), 6, "{problems:#?}");
        assert!(problems[0].starts_with("entry 2:") && problems[0].contains("athena::Nope"));
        assert!(problems[1].starts_with("entry 3:") && problems[1].contains("keystroke"));
        assert!(problems[2].contains("when"));
        assert!(problems[3].contains("no \"key\""));
        assert!(problems[4].contains("typo"));
        let (_, broken) = parse("{", build);
        assert_eq!(broken.len(), 1);
    }

    #[test]
    fn vs_code_key_spelling_is_accepted_and_unknown_keys_are_not() {
        assert_eq!(athena_keystroke("cmd+shift+p"), "cmd-shift-p");
        assert_eq!(athena_keystroke("ctrl+-"), "ctrl--");
        assert_eq!(athena_keystroke("cmd++"), "cmd-+");
        assert_eq!(athena_keystroke("cmd-k"), "cmd-k");
        let (rules, problems) = parse(
            r#"[{"key": "cmd+k cmd+t", "command": "NewTerminal"},
                {"key": "shift+f12", "command": "NewTerminal"},
                {"key": "cmd-foo", "command": "NewTerminal"}]"#,
            build,
        );
        assert_eq!(rules.len(), 2);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("\"foo\""), "{problems:?}");
        let Rule::Bind(chord) = &rules[0] else {
            panic!("expected a binding");
        };
        assert_eq!(summary(std::slice::from_ref(chord))[0].0, "cmd-k cmd-t");
    }

    #[test]
    fn user_bindings_come_after_defaults_and_minus_removes_a_default() {
        let text = r#"[
            {"key": "cmd-k cmd-t", "command": "NewTerminal"},
            {"key": "cmd-d", "command": "-athena::SplitRight"},
            {"command": "-athena::NewTerminal", "when": "Editor"},
            {"key": "cmd-d", "command": "athena::QuickOpen"}
        ]"#;
        let (rules, problems) = parse(text, build);
        assert!(problems.is_empty(), "{problems:?}");
        let merged = merge(&defaults(), rules);
        assert_eq!(
            summary(&merged),
            [
                ("cmd-shift-d".to_string(), "athena::SplitDown"),
                ("cmd-t".into(), "athena::NewTerminal"),
                ("cmd-k cmd-t".into(), "athena::NewTerminal"),
                ("cmd-d".into(), "athena::QuickOpen"),
            ]
        );
        assert!(merged[1].predicate().is_none(), "only the Editor one went");
    }

    #[test]
    fn a_user_binding_on_a_default_key_wins() {
        let (rules, _) = parse(
            r#"[{"key": "cmd-t", "command": "athena::QuickOpen"}]"#,
            build,
        );
        let keymap = gpui::Keymap::new(merge(&defaults()[..3], rules));
        let typed = [Keystroke::parse("cmd-t").unwrap()];
        let shell = [KeyContext::parse("Shell").unwrap()];
        let (found, _) = keymap.bindings_for_input(&typed, &shell);
        assert_eq!(found[0].action().name(), "athena::QuickOpen");
    }

    #[test]
    fn a_user_binding_without_when_beats_a_default_in_a_deeper_context() {
        let (rules, _) = parse(
            r#"[{"key": "cmd-t", "command": "athena::QuickOpen"},
                {"key": "cmd-d", "command": "athena::NewTerminal", "when": "Editor"}]"#,
            build,
        );
        let keymap = gpui::Keymap::new(merge(&defaults(), rules));
        let editor = [
            KeyContext::parse("Shell").unwrap(),
            KeyContext::parse("Editor").unwrap(),
        ];
        let actions = |keys: &str| -> Vec<&'static str> {
            let typed = [Keystroke::parse(keys).unwrap()];
            let (found, _) = keymap.bindings_for_input(&typed, &editor);
            found.iter().map(|b| b.action().name()).collect()
        };
        assert_eq!(actions("cmd-t"), ["athena::QuickOpen"]);
        assert_eq!(
            actions("cmd-d")[0],
            "athena::NewTerminal",
            "one with when wins in its context"
        );
        let shell = [KeyContext::parse("Shell").unwrap()];
        let typed = [Keystroke::parse("cmd-d").unwrap()];
        let (found, _) = keymap.bindings_for_input(&typed, &shell);
        assert_eq!(found[0].action().name(), "athena::SplitRight");
    }

    #[test]
    fn a_symlinked_keymap_names_its_target_folder_to_watch() {
        let dir = std::env::temp_dir().join(format!("athena-keymap-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::create_dir_all(dir.join("dotfiles")).unwrap();
        let (data, real) = (
            dir.join("data/keymap.json"),
            dir.join("dotfiles/keymap.json"),
        );
        std::fs::write(&real, "[]").unwrap();
        assert_eq!(link_target(&data), None, "missing");
        std::fs::write(&data, "[]").unwrap();
        assert_eq!(link_target(&data), None, "a plain file");
        std::fs::remove_file(&data).unwrap();
        std::os::unix::fs::symlink(&real, &data).unwrap();
        assert_eq!(link_target(&data), Some(real.canonicalize().unwrap()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_edits_to_keymap_json_count_as_changes() {
        let dir = std::env::temp_dir().join(format!("athena-keymap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("keymap.json");
        std::fs::write(&file, "[]").unwrap();
        let mut watch = FileChange::new(file.clone());
        assert!(!watch.changed(), "nothing happened yet");
        std::fs::write(dir.join("app.log"), "log line").unwrap();
        assert!(!watch.changed(), "another file in the folder");
        std::fs::write(&file, "[]").unwrap();
        assert!(!watch.changed(), "saved without edits");
        std::fs::write(&file, "[{}]").unwrap();
        assert!(watch.changed());
        assert!(!watch.changed());
        std::fs::remove_file(&file).unwrap();
        assert!(watch.changed(), "deleting it brings the defaults back");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
