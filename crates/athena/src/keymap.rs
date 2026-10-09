use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Result, anyhow};
use gpui::{
    Action, App, DummyKeyboardMapper, Global, KeyBinding, KeyBindingContextPredicate,
    KeyBindingMetaIndex, Keystroke,
};
use serde::Deserialize;
use serde_json::{Value, json};

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
/// every valid entry and describing each invalid one. A binding's meta is its entry's index.
pub fn parse(
    text: &str,
    build: impl Fn(&str, Option<serde_json::Value>) -> Result<Box<dyn Action>>,
) -> (Vec<Rule>, Vec<String>) {
    let (rules, problems) = parse_entries(text, build);
    let problems = problems
        .into_iter()
        .map(|(entry, why)| match entry {
            Some(i) => format!("entry {}: {why}", i + 1),
            None => why,
        })
        .collect();
    (rules, problems)
}

/// [`parse`], with each problem's entry index, or `None` when the file is not a list at all.
fn parse_entries(
    text: &str,
    build: impl Fn(&str, Option<serde_json::Value>) -> Result<Box<dyn Action>>,
) -> (Vec<Rule>, Vec<(Option<usize>, String)>) {
    let json = strip_jsonc(text);
    if json.trim().is_empty() {
        return (Vec::new(), Vec::new());
    }
    let values: Vec<serde_json::Value> = match serde_json::from_str(&json) {
        Ok(values) => values,
        Err(e) => {
            let why = format!("keymap.json is not a list of bindings: {e}");
            return (Vec::new(), vec![(None, why)]);
        }
    };
    let mut rules = Vec::new();
    let mut problems = Vec::new();
    for (i, value) in values.into_iter().enumerate() {
        match rule(value, &build) {
            Ok(Rule::Bind(b)) => rules.push(Rule::Bind(b.with_meta(KeyBindingMetaIndex(i as u32)))),
            Ok(rule) => rules.push(rule),
            Err(e) => problems.push((Some(i), e.to_string())),
        }
    }
    (rules, problems)
}

/// Each problem keymap.json has, at the bytes of the entry it is about, or of the line JSON
/// could not be read at, marked `true` as then none of the file applies.
pub fn problems_at(
    text: &str,
    build: impl Fn(&str, Option<serde_json::Value>) -> Result<Box<dyn Action>>,
) -> Vec<(Range<usize>, String, bool)> {
    let (_, problems) = parse_entries(text, build);
    let items = crate::settings::array_layout(text)
        .ok()
        .flatten()
        .map(|l| l.items)
        .unwrap_or_default();
    problems
        .into_iter()
        .map(|(entry, why)| match entry.and_then(|i| items.get(i)) {
            Some(range) => (range.clone(), why, false),
            None => {
                let line = crate::settings::error_line(&why).unwrap_or(1);
                (crate::settings::line_span(text, line), why, true)
            }
        })
        .collect()
}

/// Each entry's command, `athena::`-qualified and with its `-` kept; `None` for an entry with none.
pub fn entry_commands(text: &str) -> Vec<Option<String>> {
    let json = strip_jsonc(text);
    let Ok(entries) = serde_json::from_str::<Vec<Value>>(&json) else {
        return Vec::new();
    };
    entries
        .iter()
        .map(|e| {
            let command = e.get("command")?.as_str()?;
            Some(match command.strip_prefix('-') {
                Some(name) => format!("-{}", qualified(name)),
                None => qualified(command),
            })
        })
        .collect()
}

/// One binding as keymap.json spells it, `key` in Athena's spelling (`cmd-k cmd-t`).
#[derive(Clone, Debug, PartialEq)]
pub struct NewEntry {
    pub key: String,
    /// The command, or `-command` to remove a default.
    pub command: String,
    pub when: Option<String>,
}

impl NewEntry {
    fn render(&self) -> String {
        let quote = |s: &str| serde_json::to_string(s).unwrap_or_default();
        let when = match &self.when {
            Some(w) => format!(", \"when\": {}", quote(w)),
            None => String::new(),
        };
        format!(
            "{{\"key\": {}, \"command\": {}{when}}}",
            quote(&self.key),
            quote(&self.command)
        )
    }
}

/// `text` with `entries` added at the end of the list, each on a line of its own; comments and
/// the other entries are kept. Refuses a file that is not a list.
pub fn append_entries(text: &str, entries: &[NewEntry]) -> Result<String, String> {
    let mut out = text.to_string();
    for entry in entries {
        out = append_entry(&out, entry)?;
    }
    Ok(out)
}

fn append_entry(text: &str, entry: &NewEntry) -> Result<String, String> {
    let rendered = entry.render();
    let Some(layout) = readable_layout(text)? else {
        let mut out = text.trim_end().to_string();
        if !out.is_empty() {
            out.push('\n');
        }
        return Ok(format!("{out}[\n  {rendered}\n]\n"));
    };
    if let Some(last) = layout.items.last() {
        let line_start = text[..last.start].rfind('\n').map_or(0, |i| i + 1);
        let indent = &text[line_start..last.start];
        let indent = match indent.trim().is_empty() && !indent.is_empty() {
            true => indent,
            false => "  ",
        };
        let at = last.end;
        return Ok(format!(
            "{},\n{indent}{rendered}{}",
            &text[..at],
            &text[at..]
        ));
    }
    let close = layout.close;
    let line_start = text[..close].rfind('\n').map_or(0, |i| i + 1);
    Ok(
        match text[line_start..close].trim().is_empty() && line_start > layout.open {
            true => format!(
                "{}  {rendered}\n{}",
                &text[..line_start],
                &text[line_start..]
            ),
            false => format!("{}\n  {rendered}\n{}", &text[..close], &text[close..]),
        },
    )
}

/// `text` without the entries at `indices`, each with the comma that separated it.
pub fn remove_entries(text: &str, indices: &[usize]) -> Result<String, String> {
    let mut sorted = indices.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = text.to_string();
    for &i in sorted.iter().rev() {
        let Some(layout) = readable_layout(&out)? else {
            return Err("keymap.json has no list of bindings".into());
        };
        let Some(span) = layout.items.get(i).cloned() else {
            return Err(format!("keymap.json has no entry {}", i + 1));
        };
        let prev_end = i.checked_sub(1).map(|p| layout.items[p].end);
        out = crate::settings::remove_span(&out, span, prev_end);
    }
    Ok(out)
}

/// `text` with entry `index` bound to `key` instead, its command, `when` and args kept.
pub fn rebind_entry(text: &str, index: usize, key: &str) -> Result<String, String> {
    let Some(layout) = readable_layout(text)? else {
        return Err("keymap.json has no list of bindings".into());
    };
    let span = layout
        .items
        .get(index)
        .cloned()
        .ok_or_else(|| format!("keymap.json has no entry {}", index + 1))?;
    let mut entry: Value = serde_json::from_str(&strip_jsonc(&text[span.clone()]))
        .map_err(|e| format!("entry {} could not be read: {e}", index + 1))?;
    let Some(fields) = entry.as_object_mut() else {
        return Err(format!("entry {} is not a binding", index + 1));
    };
    fields.insert("key".into(), json!(key));
    let mut rendered = NewEntry {
        key: key.to_string(),
        command: fields
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        when: fields
            .get("when")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
    .render();
    if let Some(args) = fields.get("args") {
        rendered.insert_str(rendered.len() - 1, &format!(", \"args\": {args}"));
    }
    Ok(format!(
        "{}{rendered}{}",
        &text[..span.start],
        &text[span.end..]
    ))
}

/// The list's layout, refusing a file that is not a list of entries, as a write would break it.
fn readable_layout(text: &str) -> Result<Option<crate::settings::ArrayLayout>, String> {
    let json = strip_jsonc(text);
    if !json.trim().is_empty() {
        serde_json::from_str::<Vec<Value>>(&json).map_err(|e| {
            format!("keymap.json is not a list of bindings: {e}; fix it, then try again")
        })?;
    }
    crate::settings::array_layout(text)
        .map_err(|_| "keymap.json could not be read; fix it, then try again".to_string())
}

/// A binding's keys in Athena's spelling, as keymap.json takes them: `cmd-k cmd-t`.
pub fn key_text(binding: &KeyBinding) -> String {
    let strokes: Vec<String> = binding
        .keystrokes()
        .iter()
        .map(|k| k.inner().unparse())
        .collect();
    strokes.join(" ")
}

/// A keystroke as menus show it: `⌘⇧P`.
pub fn symbols(k: &Keystroke) -> String {
    let m = k.modifiers;
    let mut s = String::new();
    if m.control {
        s.push('⌃');
    }
    if m.alt {
        s.push('⌥');
    }
    if m.shift {
        s.push('⇧');
    }
    if m.platform {
        s.push('⌘');
    }
    let key = match k.key.as_str() {
        "enter" => "↩".to_string(),
        "left" => "←".into(),
        "right" => "→".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        other => other.to_uppercase(),
    };
    s + &key
}

/// What a press means while recording a binding.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Recording,
    /// Enter, with keys recorded: these.
    Accept(String),
    Cancel,
}

/// Records a binding from the keys pressed: up to two keystrokes, as in `cmd-k cmd-t`; Enter
/// accepts and Escape cancels, so those two cannot be bound bare, as in VS Code.
#[derive(Clone, Debug, Default)]
pub struct Recorder {
    strokes: Vec<Keystroke>,
}

impl Recorder {
    pub fn press(&mut self, pressed: &Keystroke) -> Step {
        let bare = !pressed.modifiers.modified();
        match pressed.key.as_str() {
            "enter" if bare && !self.strokes.is_empty() => Step::Accept(self.text()),
            "escape" if bare => Step::Cancel,
            "enter" if bare => Step::Recording,
            _ => {
                if self.strokes.len() == 2 {
                    self.strokes.clear();
                }
                self.strokes.push(Keystroke {
                    key: pressed.key.clone(),
                    modifiers: pressed.modifiers,
                    key_char: None,
                });
                Step::Recording
            }
        }
    }

    pub fn strokes(&self) -> &[Keystroke] {
        &self.strokes
    }

    /// The keys so far in keymap.json's spelling.
    pub fn text(&self) -> String {
        let strokes: Vec<String> = self.strokes.iter().map(Keystroke::unparse).collect();
        strokes.join(" ")
    }

    /// Why the keys so far could not be saved, if they could not.
    pub fn problem(&self) -> Option<String> {
        binding(&self.text(), gpui::NoAction.boxed_clone(), None)
            .err()
            .map(|e| e.to_string())
    }
}

/// The bindings `key` (Athena's spelling) would also trigger, in any context.
pub fn conflicts<'a>(bindings: &'a [KeyBinding], key: &str) -> Vec<&'a KeyBinding> {
    let Ok(wanted) = key
        .split_whitespace()
        .map(Keystroke::parse)
        .collect::<Result<Vec<_>, _>>()
    else {
        return Vec::new();
    };
    bindings
        .iter()
        .filter(|b| {
            b.keystrokes().len() == wanted.len()
                && b.keystrokes().iter().zip(&wanted).all(|(have, want)| {
                    have.inner().key == want.key && have.inner().modifiers == want.modifiers
                })
        })
        .collect()
}

/// The JSON Schema of keymap.json for a JSON language server; `commands` are the action names
/// a binding may run.
pub fn json_schema(commands: &[&str]) -> Value {
    let mut names: Vec<String> = commands.iter().map(|c| c.to_string()).collect();
    names.extend(commands.iter().map(|c| format!("-{c}")));
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "Athena keyboard shortcuts",
        "type": "array",
        "allowComments": true,
        "allowTrailingCommas": true,
        "items": {
            "type": "object",
            "required": ["command"],
            "additionalProperties": false,
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Keys such as \"cmd-k cmd-t\" or VS Code's \"cmd+k cmd+t\"."
                },
                "command": {
                    "description": "The command to run; \"-command\" removes Athena's binding of it.",
                    "anyOf": [{"enum": names}, {"type": "string"}]
                },
                "when": {
                    "type": "string",
                    "description": "Where it applies, such as \"Editor\" or \"!Terminal\"."
                },
                "args": {"description": "What the command takes, for the few that take any."}
            }
        }
    })
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

    fn entry(key: &str, command: &str, when: Option<&str>) -> NewEntry {
        NewEntry {
            key: key.into(),
            command: command.into(),
            when: when.map(str::to_string),
        }
    }

    #[test]
    fn user_bindings_carry_their_entry_index_and_problems_point_at_their_entry() {
        let text = "[\n  {\"key\": \"cmd-k\", \"command\": \"athena::Nope\"},\n  // two\n  {\"key\": \"cmd-k cmd-t\", \"command\": \"NewTerminal\"}\n]";
        let (rules, _) = parse(text, build);
        let Rule::Bind(b) = &rules[0] else {
            panic!("expected a binding");
        };
        assert_eq!(b.meta(), Some(KeyBindingMetaIndex(1)));
        let found = problems_at(text, build);
        assert_eq!(found.len(), 1);
        assert_eq!(
            &text[found[0].0.clone()],
            "{\"key\": \"cmd-k\", \"command\": \"athena::Nope\"}"
        );
        assert!(!found[0].2);
        let broken = "[\n  {\"key\": \"cmd-k\"\n   \"command\": \"x\"}\n]";
        let found = problems_at(broken, build);
        assert!(found[0].2 && found[0].1.contains("not a list"), "{found:?}");
        assert_eq!(&broken[found[0].0.clone()], "\"command\": \"x\"}");
    }

    #[test]
    fn new_entries_go_at_the_end_keeping_the_header_and_comments() {
        let out = append_entries(
            TEMPLATE,
            &[entry("cmd-k cmd-t", "athena::NewTerminal", None)],
        )
        .unwrap();
        assert!(out.starts_with(&TEMPLATE[..TEMPLATE.rfind('[').unwrap()]));
        assert!(
            out.ends_with(
                "[\n  {\"key\": \"cmd-k cmd-t\", \"command\": \"athena::NewTerminal\"}\n]\n"
            ),
            "{out}"
        );
        let out = append_entries(
            &out,
            &[
                entry("cmd-e", "athena::QuickOpen", Some("Editor")),
                entry("cmd-d", "-athena::SplitRight", None),
            ],
        )
        .unwrap();
        let (rules, problems) = parse(&out, build);
        assert!(problems.is_empty(), "{problems:?}\n{out}");
        assert_eq!(rules.len(), 3);
        assert!(out.contains(
            "NewTerminal\"},\n  {\"key\": \"cmd-e\", \"command\": \"athena::QuickOpen\", \"when\": \"Editor\"},\n  {"
        ), "{out}");
        for (text, want) in [
            ("", "[\n  {\"key\": \"f1\", \"command\": \"x\"}\n]\n"),
            ("[]", "[\n  {\"key\": \"f1\", \"command\": \"x\"}\n]"),
            (
                "[{\"key\": \"f2\", \"command\": \"y\"},]",
                "[{\"key\": \"f2\", \"command\": \"y\"},\n  {\"key\": \"f1\", \"command\": \"x\"},]",
            ),
        ] {
            assert_eq!(
                append_entries(text, &[entry("f1", "x", None)]).unwrap(),
                want
            );
        }
        assert!(append_entries("{\"not\": \"a list\"}", &[entry("f1", "x", None)]).is_err());
        assert!(append_entries("[{", &[entry("f1", "x", None)]).is_err());
    }

    #[test]
    fn entries_are_removed_and_rebound_in_place() {
        let text = "// mine\n[\n  {\"key\": \"cmd-1\", \"command\": \"NewTerminal\"}, // one\n  {\"key\": \"cmd-2\", \"command\": \"QuickOpen\", \"when\": \"Editor\", \"args\": null},\n  {\"key\": \"cmd-3\", \"command\": \"SplitRight\"}\n]\n";
        let out = rebind_entry(text, 1, "cmd-k cmd-2").unwrap();
        assert!(out.contains("{\"key\": \"cmd-k cmd-2\", \"command\": \"QuickOpen\", \"when\": \"Editor\", \"args\": null},"), "{out}");
        assert_eq!(parse(&out, build).1, Vec::<String>::new());
        let out = remove_entries(&out, &[0, 2]).unwrap();
        assert_eq!(
            out,
            "// mine\n[\n  {\"key\": \"cmd-k cmd-2\", \"command\": \"QuickOpen\", \"when\": \"Editor\", \"args\": null}\n]\n"
        );
        let out = remove_entries(&out, &[0]).unwrap();
        assert_eq!(out, "// mine\n[\n]\n");
        assert!(remove_entries(&out, &[0]).is_err());
    }

    #[test]
    fn the_recorder_takes_a_chord_and_enter_or_escape_ends_it() {
        let k = |s: &str| Keystroke::parse(s).unwrap();
        let mut r = Recorder::default();
        assert_eq!(
            r.press(&k("enter")),
            Step::Recording,
            "nothing to accept yet"
        );
        assert_eq!(r.press(&k("cmd-k")), Step::Recording);
        assert_eq!(r.press(&k("cmd-shift-t")), Step::Recording);
        assert_eq!(r.text(), "cmd-k cmd-shift-t");
        assert_eq!(r.problem(), None);
        assert_eq!(
            r.press(&k("f5")),
            Step::Recording,
            "a third key starts over"
        );
        assert_eq!(r.text(), "f5");
        assert_eq!(
            r.press(&k("cmd-enter")),
            Step::Recording,
            "modified Enter is a key"
        );
        assert_eq!(r.press(&k("enter")), Step::Accept("f5 cmd-enter".into()));
        assert_eq!(r.press(&k("escape")), Step::Cancel);
        let mut odd = Recorder::default();
        odd.press(&k("cmd-capslock"));
        assert!(odd.problem().is_some());
    }

    #[test]
    fn conflicts_are_the_bindings_on_the_same_keys_in_any_context() {
        let bindings = defaults();
        let names = |found: Vec<&KeyBinding>| -> Vec<&'static str> {
            found.iter().map(|b| b.action().name()).collect()
        };
        assert_eq!(
            names(conflicts(&bindings, "cmd-t")),
            ["athena::NewTerminal", "athena::NewTerminal"]
        );
        assert_eq!(
            names(conflicts(&bindings, "cmd-shift-d")),
            ["athena::SplitDown"]
        );
        assert!(conflicts(&bindings, "cmd-k cmd-t").is_empty());
        assert!(conflicts(&bindings, "cmd-alt-t").is_empty());
        let (rules, _) = parse(r#"[{"key": "cmd+k cmd+t", "command": "QuickOpen"}]"#, build);
        let merged = merge(&bindings, rules);
        assert_eq!(
            names(conflicts(&merged, "cmd-k cmd-t")),
            ["athena::QuickOpen"]
        );
        assert_eq!(
            key_text(conflicts(&merged, "cmd-k cmd-t")[0]),
            "cmd-k cmd-t"
        );
    }

    #[test]
    fn the_keymap_schema_lists_commands_and_their_removals() {
        let schema = json_schema(&["athena::NewTerminal"]);
        let names = &schema["items"]["properties"]["command"]["anyOf"][0]["enum"];
        assert_eq!(
            names,
            &json!(["athena::NewTerminal", "-athena::NewTerminal"])
        );
        assert_eq!(schema["allowComments"], true);
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
