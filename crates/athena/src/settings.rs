use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use athena_editor::Lang;
use athena_workspace::{Preferences, ThemeChoice};
use serde_json::{Map, Value, json};

/// What a new settings.json holds: an empty object, with every setting shown commented out.
const TEMPLATE: &str = include_str!("settings-template.jsonc");

/// The gopls hints Toggle Inlay Hints turns on when the user has chosen none.
const GOPLS_HINTS: &[&str] = &[
    "assignVariableTypes",
    "compositeLiteralFields",
    "constantValues",
    "functionTypeParameters",
    "parameterNames",
    "rangeVariableTypes",
];

/// The user's settings.json; a field left out is `None` and falls back.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub editor: EditorSettings,
    /// Overrides by language, from `"[go]": {...}` blocks keyed by VS Code's language id.
    pub languages: HashMap<String, EditorSettings>,
    pub theme: Option<ThemeChoice>,
    pub ide_integration: Option<bool>,
    /// VS Code's `git.autofetch`: fetch the active project every three minutes.
    pub autofetch: Option<bool>,
    /// Each language server's settings by program name, sent as it starts and when they change.
    pub lsp: HashMap<String, Value>,
    /// VS Code's `explorer.confirmDragAndDrop`: ask before a file dragged in the tree moves.
    pub confirm_drag_and_drop: Option<bool>,
    /// `window.zoom_level`: interface text and spacing in 10% steps from the default.
    pub zoom_level: Option<i32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EditorSettings {
    pub format_on_save: Option<bool>,
    pub trim_trailing_whitespace: Option<bool>,
    pub insert_final_newline: Option<bool>,
    pub word_wrap: Option<bool>,
    pub font_size: Option<f32>,
    pub tab_size: Option<usize>,
    pub autosave_delay_ms: Option<u64>,
    pub inlay_hints: Option<bool>,
}

impl EditorSettings {
    /// These settings with `over`'s wherever it has one.
    fn overlaid(&self, over: &Self) -> Self {
        Self {
            format_on_save: over.format_on_save.or(self.format_on_save),
            trim_trailing_whitespace: over
                .trim_trailing_whitespace
                .or(self.trim_trailing_whitespace),
            insert_final_newline: over.insert_final_newline.or(self.insert_final_newline),
            word_wrap: over.word_wrap.or(self.word_wrap),
            font_size: over.font_size.or(self.font_size),
            tab_size: over.tab_size.or(self.tab_size),
            autosave_delay_ms: over.autosave_delay_ms.or(self.autosave_delay_ms),
            inlay_hints: over.inlay_hints.or(self.inlay_hints),
        }
    }

    fn set(&mut self, key: &str, value: &Value) -> Result<(), String> {
        let flag = || value.as_bool().ok_or("must be true or false");
        match editor_key(key) {
            "format_on_save" => self.format_on_save = Some(flag()?),
            "trim_trailing_whitespace" => self.trim_trailing_whitespace = Some(flag()?),
            "insert_final_newline" => self.insert_final_newline = Some(flag()?),
            "word_wrap" => self.word_wrap = Some(flag()?),
            "inlay_hints" => self.inlay_hints = Some(flag()?),
            "font_size" => {
                let size = value.as_f64().filter(|s| (6.0..=40.0).contains(s));
                self.font_size = Some(size.ok_or("must be a number from 6 to 40")? as f32);
            }
            "tab_size" => {
                let size = value.as_u64().filter(|s| (1..=16).contains(s));
                self.tab_size = Some(size.ok_or("must be a whole number from 1 to 16")? as usize);
            }
            "autosave_delay_ms" => {
                self.autosave_delay_ms = Some(
                    value
                        .as_u64()
                        .ok_or("must be a whole number of milliseconds")?,
                );
            }
            _ => return Err("is not a setting".into()),
        }
        Ok(())
    }
}

/// The setting an editor key names; VS Code's spellings work too, so its settings can be pasted.
fn editor_key(key: &str) -> &str {
    match key {
        "formatOnSave" => "format_on_save",
        "trimTrailingWhitespace" => "trim_trailing_whitespace",
        "insertFinalNewline" => "insert_final_newline",
        "fontSize" => "font_size",
        "tabSize" => "tab_size",
        "autoSaveDelay" => "autosave_delay_ms",
        other => other,
    }
}

impl Settings {
    pub fn git_autofetch(&self) -> bool {
        self.autofetch.unwrap_or(false)
    }

    pub fn confirm_drag_and_drop(&self) -> bool {
        self.confirm_drag_and_drop.unwrap_or(true)
    }

    /// The word wrap set in `lang`'s own block, if any.
    pub fn language_word_wrap(&self, lang: Option<Lang>) -> Option<bool> {
        lang.and_then(|l| self.languages.get(language_id(l)))
            .and_then(|over| over.word_wrap)
    }

    /// The editor settings for files of `lang`, its language block applied.
    pub fn editor_for(&self, lang: Option<Lang>) -> EditorSettings {
        match lang.and_then(|l| self.languages.get(language_id(l))) {
            Some(over) => self.editor.overlaid(over),
            None => self.editor.clone(),
        }
    }

    /// The workspace.json preferences as settings.json overrides them.
    pub fn over(&self, fallback: Preferences) -> Preferences {
        Preferences {
            autosave_delay_ms: self
                .editor
                .autosave_delay_ms
                .unwrap_or(fallback.autosave_delay_ms),
            format_on_save: self.editor.format_on_save.or(fallback.format_on_save),
            word_wrap: self.editor.word_wrap.unwrap_or(fallback.word_wrap),
            ide_integration: self.ide_integration.unwrap_or(fallback.ide_integration),
            theme: self.theme.unwrap_or(fallback.theme),
        }
    }

    /// What the server named `program` is configured with: the user's `lsp` section, plus a
    /// curated set of inlay hints when `editor.inlay_hints` is on and the user chose none.
    pub fn server_config(&self, program: &str) -> Value {
        let mut config = self.lsp.get(program).cloned().unwrap_or(Value::Null);
        if self.editor.inlay_hints != Some(true) {
            return config;
        }
        if !config.is_object() {
            config = json!({});
        }
        let hints: Map<String, Value> = GOPLS_HINTS
            .iter()
            .map(|h| (h.to_string(), json!(true)))
            .collect();
        match program {
            "gopls" if config.get("hints").is_none() => config["hints"] = Value::Object(hints),
            "typescript-language-server" => {
                let prefs = json!({
                    "includeInlayParameterNameHints": "literals",
                    "includeInlayFunctionLikeReturnTypeHints": true,
                    "includeInlayEnumMemberValueHints": true,
                });
                // Older servers read the preferences, newer ones each language's inlayHints.
                for (key, value) in [
                    ("/preferences", prefs.clone()),
                    ("/typescript/inlayHints", prefs.clone()),
                    ("/javascript/inlayHints", prefs),
                ] {
                    if config.pointer(key).is_none() {
                        set_pointer(&mut config, key, value);
                    }
                }
            }
            _ => {}
        }
        config
    }
}

fn set_pointer(root: &mut Value, pointer: &str, value: Value) {
    let mut at = root;
    let keys: Vec<&str> = pointer.trim_start_matches('/').split('/').collect();
    for (i, key) in keys.iter().enumerate() {
        if !at.is_object() {
            *at = json!({});
        }
        let map = at.as_object_mut().expect("just made an object");
        if i + 1 == keys.len() {
            map.insert(key.to_string(), value);
            return;
        }
        at = map.entry(key.to_string()).or_insert_with(|| json!({}));
    }
}

/// VS Code's id for a language, as `"[id]"` blocks name it.
pub fn language_id(lang: Lang) -> &'static str {
    match lang {
        Lang::Go => "go",
        Lang::TypeScript => "typescript",
        Lang::Tsx => "typescriptreact",
        Lang::JavaScript => "javascript",
        Lang::Yaml => "yaml",
        Lang::Json => "json",
        Lang::Toml => "toml",
        Lang::Shell => "shellscript",
        Lang::Rust => "rust",
        Lang::Python => "python",
        Lang::Css => "css",
        Lang::Html => "html",
        Lang::Markdown => "markdown",
        Lang::Swift => "swift",
        Lang::Dockerfile => "dockerfile",
        Lang::DotEnv => "dotenv",
        Lang::GoMod => "go.mod",
        Lang::GoSum => "go.sum",
        Lang::Makefile => "makefile",
        Lang::Sql => "sql",
        Lang::Protobuf => "proto",
    }
}

fn server_name(name: &str) -> &str {
    match name {
        "tsserver" | "typescript" => "typescript-language-server",
        other => other,
    }
}

pub fn path() -> Result<PathBuf> {
    Ok(athena_proto::data_dir()?.join("settings.json"))
}

/// Writes the commented template unless the file exists, and returns its path.
pub fn ensure_file() -> Result<PathBuf> {
    let path = path()?;
    if !path.exists() {
        std::fs::write(&path, TEMPLATE)?;
    }
    Ok(path)
}

/// Reads settings.json; a missing file is empty settings, and `Err` says why none could be read.
pub fn load() -> Result<(Settings, Vec<String>), String> {
    match path().map(|p| std::fs::read_to_string(&p)) {
        Ok(Ok(text)) => parse(&text),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Ok(Err(e)) => Err(format!("settings.json could not be read: {e}")),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// Parses settings.json (comments and trailing commas allowed), keeping every valid setting and
/// describing each invalid one; `Err` when the file is not a JSON object at all.
/// `"editor.word_wrap": true` is read as `"editor": {"word_wrap": true}`.
pub fn parse(text: &str) -> Result<(Settings, Vec<String>), String> {
    let mut settings = Settings::default();
    let json = crate::keymap::strip_jsonc(text);
    if json.trim().is_empty() {
        return Ok((settings, Vec::new()));
    }
    let root = match serde_json::from_str(&json) {
        Ok(Value::Object(root)) => root,
        Ok(_) => return Err("settings.json is not an object".into()),
        Err(e) => return Err(format!("settings.json is not valid JSON: {e}")),
    };
    let mut problems = Vec::new();
    for (key, value) in &root {
        let result = match key.as_str() {
            "theme" => match value.as_str() {
                Some("system") => Ok(ThemeChoice::System),
                Some("light") => Ok(ThemeChoice::Light),
                Some("dark") => Ok(ThemeChoice::Dark),
                _ => Err("must be \"system\", \"light\" or \"dark\"".to_string()),
            }
            .map(|theme| settings.theme = Some(theme)),
            "ide_integration" => value
                .as_bool()
                .map(|on| settings.ide_integration = Some(on))
                .ok_or_else(|| "must be true or false".to_string()),
            "editor" => editor_block(&mut settings.editor, value, key, &mut problems),
            "git" => match value.get("autofetch").map(Value::as_bool) {
                Some(Some(on)) => {
                    settings.autofetch = Some(on);
                    Ok(())
                }
                _ => Err("must be {\"autofetch\": true or false}".into()),
            },
            "explorer" => match value.get("confirmDragAndDrop").map(Value::as_bool) {
                Some(Some(on)) => {
                    settings.confirm_drag_and_drop = Some(on);
                    Ok(())
                }
                _ => Err("must be {\"confirmDragAndDrop\": true or false}".into()),
            },
            "window" => match value.get("zoom_level").map(zoom_level) {
                Some(Ok(level)) => {
                    settings.zoom_level = Some(level);
                    Ok(())
                }
                Some(Err(why)) => Err(format!("\"zoom_level\" {why}")),
                None => Err("must be {\"zoom_level\": a whole number}".into()),
            },
            "lsp" => match value.as_object() {
                Some(servers) => {
                    for (name, config) in servers {
                        settings
                            .lsp
                            .insert(server_name(name).into(), config.clone());
                    }
                    Ok(())
                }
                None => Err("must be an object of language server settings".into()),
            },
            k if k.len() > 2 && k.starts_with('[') && k.ends_with(']') => {
                let lang = settings
                    .languages
                    .entry(k[1..k.len() - 1].into())
                    .or_default();
                editor_block(lang, value, key, &mut problems)
            }
            k => match k.split_once('.') {
                Some(("editor" | "files", rest)) => settings.editor.set(rest, value),
                Some(("git", "autofetch")) => value
                    .as_bool()
                    .map(|on| settings.autofetch = Some(on))
                    .ok_or_else(|| "must be true or false".to_string()),
                Some(("explorer", "confirmDragAndDrop")) => value
                    .as_bool()
                    .map(|on| settings.confirm_drag_and_drop = Some(on))
                    .ok_or_else(|| "must be true or false".to_string()),
                Some(("window", "zoom_level")) => {
                    zoom_level(value).map(|level| settings.zoom_level = Some(level))
                }
                Some(("lsp", name)) => {
                    settings.lsp.insert(server_name(name).into(), value.clone());
                    Ok(())
                }
                _ => Err("is not a setting".into()),
            },
        };
        if let Err(why) = result {
            problems.push(format!("\"{key}\" {why}"));
        }
    }
    Ok((settings, problems))
}

fn zoom_level(value: &Value) -> Result<i32, String> {
    let range = athena_ui::UI_ZOOM;
    value
        .as_i64()
        .and_then(|l| i32::try_from(l).ok())
        .filter(|l| range.contains(l))
        .ok_or_else(|| {
            format!(
                "must be a whole number from {} to {}",
                range.start(),
                range.end()
            )
        })
}

/// Reads an object of editor settings, spelled bare or `editor.`-prefixed.
fn editor_block(
    into: &mut EditorSettings,
    value: &Value,
    block: &str,
    problems: &mut Vec<String>,
) -> Result<(), String> {
    let map = value.as_object().ok_or("must be an object")?;
    for (key, value) in map {
        let bare = key
            .strip_prefix("editor.")
            .or_else(|| key.strip_prefix("files."))
            .unwrap_or(key);
        if let Err(why) = into.set(bare, value) {
            problems.push(format!("\"{block}\": \"{key}\" {why}"));
        }
    }
    Ok(())
}

/// Sets `keys` (a top-level key and the keys inside it) to `value` in settings.json, creating
/// it from the template if needed; returns the new text. Comments and layout elsewhere are kept.
pub fn write(keys: &[&str], value: &Value) -> Result<String> {
    write_at(&ensure_file()?, keys, value)
}

fn write_at(path: &Path, keys: &[&str], value: &Value) -> Result<String> {
    // A settings.json linked from a dotfiles checkout is updated there, not replaced by a copy.
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text =
        std::fs::read_to_string(&target).with_context(|| format!("read {}", target.display()))?;
    let updated = set_value(&text, keys, value).map_err(|why| anyhow!("{why}"))?;
    if updated != text {
        let tmp = target.with_file_name(format!(".settings.json.{}", std::process::id()));
        std::fs::write(&tmp, &updated)?;
        if let Ok(meta) = std::fs::metadata(&target) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, &target)?;
    }
    Ok(updated)
}

/// `text` with `keys` set to `value`: the value in place where the setting is read from, else a
/// new member at the end of its object, indented like its neighbours.
/// Refuses text that [`parse`] cannot read, so a broken file is never rewritten.
pub fn set_value(text: &str, keys: &[&str], value: &Value) -> Result<String, String> {
    parse(text)?;
    let out = set_parsed(text, keys, value)?;
    parse(&out).map_err(|why| format!("settings.json would be left unreadable: {why}"))?;
    Ok(out)
}

fn set_parsed(text: &str, keys: &[&str], value: &Value) -> Result<String, String> {
    let mut scan = Scan {
        s: text.as_bytes(),
        i: 0,
    };
    scan.skip_blank();
    if scan.i == text.len() {
        let mut out = text.trim_end().to_string();
        if !out.is_empty() {
            out.push('\n');
        }
        let nested = nest(&keys[1..], value);
        out.push_str(&format!(
            "{{\n  {}: {}\n}}\n",
            quote(keys[0]),
            render(&nested, "  ", "  ")
        ));
        return Ok(out);
    }
    let root = scan.object()?;
    scan.skip_blank();
    if scan.i != text.len() {
        return Err("settings.json has text after its closing brace".into());
    }
    let unit = root
        .members
        .first()
        .map(|m| line_indent(text, m.key_start))
        .filter(|i| !i.is_empty())
        .unwrap_or("  ")
        .to_string();
    if let Some(m) = read_from(&root, keys) {
        return Ok(replace(text, m, value, &unit));
    }
    let mut object = &root;
    for (depth, key) in keys.iter().enumerate() {
        let rest = &keys[depth + 1..];
        // Of a repeated key serde keeps the last value, so that is the one to change.
        match object.members.iter().rfind(|m| m.key == *key) {
            Some(m) if rest.is_empty() => return Ok(replace(text, m, value, &unit)),
            Some(Member {
                object: Some(inner),
                ..
            }) => object = inner,
            Some(m) => return Ok(replace(text, m, &nest(rest, value), &unit)),
            None => return Ok(insert(text, object, key, &nest(rest, value), &unit)),
        }
    }
    unreachable!("keys is never empty")
}

/// The member whose value [`parse`] ends up with for `keys`, whichever spelling or place it has.
fn read_from<'a>(root: &'a Object, keys: &[&str]) -> Option<&'a Member> {
    let target = match keys {
        [key] => setting_name(None, key),
        [block, rest @ ..] => setting_name(Some(block), &rest.join(".")),
        [] => return None,
    };
    let mut found = None;
    for m in as_read(root) {
        if setting_name(None, &m.key) == target {
            found = Some(m);
        }
        if keys.len() > 1
            && m.key == keys[0]
            && let Some(inner) = &m.object
        {
            for im in as_read(inner) {
                if setting_name(Some(keys[0]), &im.key) == target {
                    found = Some(im);
                }
            }
        }
    }
    found
}

/// `object`'s members in the order serde_json reads them: a repeated key keeps the place of its
/// first appearance and the value of its last.
fn as_read(object: &Object) -> Vec<&Member> {
    let mut out: Vec<&Member> = Vec::new();
    for m in &object.members {
        match out.iter_mut().find(|e| e.key == m.key) {
            Some(slot) => *slot = m,
            None => out.push(m),
        }
    }
    out
}

/// The dotted name of what a member spelled `key`, inside `block` if any, sets.
fn setting_name(block: Option<&str>, key: &str) -> String {
    let key = match block {
        Some("editor") => {
            let bare = key
                .strip_prefix("editor.")
                .or_else(|| key.strip_prefix("files."))
                .unwrap_or(key);
            return format!("editor.{}", editor_key(bare));
        }
        Some(block) => format!("{block}.{key}"),
        None => key.to_string(),
    };
    match key.split_once('.') {
        Some(("editor" | "files", rest)) => format!("editor.{}", editor_key(rest)),
        _ => key,
    }
}

fn nest(keys: &[&str], value: &Value) -> Value {
    keys.iter()
        .rev()
        .fold(value.clone(), |inner, key| json!({ *key: inner }))
}

fn quote(key: &str) -> String {
    serde_json::to_string(key).unwrap_or_default()
}

/// `value` as JSON, objects one member per line at `indent` plus `unit`.
fn render(value: &Value, indent: &str, unit: &str) -> String {
    match value {
        Value::Object(map) if !map.is_empty() => {
            let inner = format!("{indent}{unit}");
            let members: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{inner}{}: {}", quote(k), render(v, &inner, unit)))
                .collect();
            format!("{{\n{}\n{indent}}}", members.join(",\n"))
        }
        other => other.to_string(),
    }
}

fn replace(text: &str, member: &Member, value: &Value, unit: &str) -> String {
    let indent = line_indent(text, member.key_start);
    format!(
        "{}{}{}",
        &text[..member.value.start],
        render(value, indent, unit),
        &text[member.value.end..]
    )
}

fn insert(text: &str, object: &Object, key: &str, value: &Value, unit: &str) -> String {
    if let Some(last) = object.members.last() {
        let indent = line_indent(text, last.key_start);
        let member = format!(",\n{indent}{}: {}", quote(key), render(value, indent, unit));
        let at = last.value.end;
        return format!("{}{member}{}", &text[..at], &text[at..]);
    }
    let outer = line_indent(text, object.open);
    let indent = format!("{outer}{unit}");
    let member = format!("{indent}{}: {}", quote(key), render(value, &indent, unit));
    let line_start = text[..object.close].rfind('\n').map_or(0, |i| i + 1);
    if text[line_start..object.close].trim().is_empty() {
        // The closing brace has a line of its own, so the member goes on the line above it.
        format!("{}{member}\n{}", &text[..line_start], &text[line_start..])
    } else {
        let at = object.close;
        format!("{}\n{member}\n{outer}{}", &text[..at], &text[at..])
    }
}

/// The whitespace starting the line that holds byte `at`.
fn line_indent(text: &str, at: usize) -> &str {
    let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let line = &text[start..];
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

struct Member {
    key: String,
    key_start: usize,
    value: Range<usize>,
    object: Option<Object>,
}

struct Object {
    open: usize,
    close: usize,
    members: Vec<Member>,
}

/// Just enough of a JSONC reader to find where each key and value of the objects sits.
struct Scan<'a> {
    s: &'a [u8],
    i: usize,
}

impl Scan<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn skip_blank(&mut self) {
        loop {
            match (self.peek(), self.s.get(self.i + 1)) {
                (Some(c), _) if c.is_ascii_whitespace() => self.i += 1,
                (Some(b'/'), Some(b'/')) => {
                    while self.peek().is_some_and(|c| c != b'\n') {
                        self.i += 1;
                    }
                }
                (Some(b'/'), Some(b'*')) => {
                    self.i += 2;
                    while self.i < self.s.len() && !self.s[self.i..].starts_with(b"*/") {
                        self.i += 1;
                    }
                    self.i = (self.i + 2).min(self.s.len());
                }
                _ => return,
            }
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.peek() != Some(c) {
            return Err(format!(
                "settings.json could not be read near byte {}; fix it, then try again",
                self.i
            ));
        }
        self.i += 1;
        Ok(())
    }

    fn string(&mut self) -> Result<Range<usize>, String> {
        let start = self.i;
        self.expect(b'"')?;
        while let Some(c) = self.peek() {
            self.i += 1;
            match c {
                b'\\' => self.i += 1,
                b'"' => return Ok(start..self.i),
                _ => {}
            }
        }
        Err("settings.json has a string that never ends".into())
    }

    fn value(&mut self) -> Result<(Range<usize>, Option<Object>), String> {
        let start = self.i;
        match self.peek() {
            Some(b'{') => {
                let object = self.object()?;
                Ok((start..self.i, Some(object)))
            }
            Some(b'[') => {
                self.i += 1;
                loop {
                    self.skip_blank();
                    match self.peek() {
                        Some(b']') => break,
                        Some(b',') => self.i += 1,
                        _ => {
                            self.value()?;
                        }
                    }
                }
                self.i += 1;
                Ok((start..self.i, None))
            }
            Some(b'"') => Ok((self.string()?, None)),
            _ => {
                while self.peek().is_some_and(|c| !b",}] \t\r\n/".contains(&c)) {
                    self.i += 1;
                }
                if self.i == start {
                    return Err(format!("settings.json has no value at byte {start}"));
                }
                Ok((start..self.i, None))
            }
        }
    }

    fn object(&mut self) -> Result<Object, String> {
        let open = self.i;
        self.expect(b'{')?;
        let mut members = Vec::new();
        loop {
            self.skip_blank();
            match self.peek() {
                Some(b'}') => break,
                Some(b',') => {
                    self.i += 1;
                    continue;
                }
                None => return Err("settings.json ends inside an object".into()),
                _ => {}
            }
            let key = self.string()?;
            let key_start = key.start;
            let key: String = serde_json::from_slice(&self.s[key]).map_err(|e| e.to_string())?;
            self.skip_blank();
            self.expect(b':')?;
            self.skip_blank();
            let (value, object) = self.value()?;
            members.push(Member {
                key,
                key_start,
                value,
                object,
            });
        }
        let close = self.i;
        self.i += 1;
        Ok(Object {
            open,
            close,
            members,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(text: &str, keys: &[&str], value: Value) -> String {
        let out = set_value(text, keys, &value).unwrap();
        let (_, problems) = parse(&out).unwrap();
        assert!(problems.is_empty(), "{problems:?} in\n{out}");
        out
    }

    #[test]
    fn a_new_key_goes_into_the_template_below_its_comments() {
        let out = set(TEMPLATE, &["editor", "word_wrap"], json!(true));
        assert!(
            out.starts_with(&TEMPLATE[..TEMPLATE.rfind('}').unwrap()]),
            "{out}"
        );
        assert!(
            out.ends_with("  \"editor\": {\n    \"word_wrap\": true\n  }\n}\n"),
            "{out}"
        );
        assert_eq!(parse(&out).unwrap().0.editor.word_wrap, Some(true));
    }

    #[test]
    fn an_existing_value_is_replaced_in_place_keeping_comments_and_trailing_commas() {
        let text = "{\n    // mine\n    \"theme\": \"dark\", /* keep */\n    \"editor\": {\n        \"word_wrap\": false, // why\n    },\n}\n";
        let out = set(text, &["editor", "word_wrap"], json!(true));
        assert_eq!(
            out,
            text.replace("\"word_wrap\": false", "\"word_wrap\": true")
        );
        let out = set(&out, &["editor", "tab_size"], json!(4));
        assert!(
            out.contains("\"word_wrap\": true,\n        \"tab_size\": 4, // why\n    },"),
            "{out}"
        );
        let out = set(&out, &["ide_integration"], json!(true));
        assert!(
            out.contains("    },\n    \"ide_integration\": true,\n}"),
            "{out}"
        );
    }

    #[test]
    fn the_template_parses_with_every_example_uncommented() {
        let uncommented: String = TEMPLATE
            .lines()
            .map(|l| match l.trim_start().strip_prefix("// ") {
                Some(rest) if l.starts_with("  ") => format!("  {rest}\n"),
                _ => format!("{l}\n"),
            })
            .collect();
        let (s, problems) = parse(&uncommented).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(s.editor.tab_size, Some(4));
        assert_eq!(s.theme, Some(ThemeChoice::System));
        assert!(s.lsp.contains_key("gopls"));
        assert!(s.git_autofetch());
        assert_eq!(parse(TEMPLATE).unwrap().0, Settings::default());
        assert!(
            !Settings::default().git_autofetch(),
            "off by default, as in VS Code"
        );
        let (dotted, _) = parse(r#"{"git.autofetch": true}"#).unwrap();
        assert!(dotted.git_autofetch());
        assert!(!s.confirm_drag_and_drop() && s.zoom_level == Some(1));
        assert!(
            Settings::default().confirm_drag_and_drop(),
            "on by default, as in VS Code"
        );
    }

    #[test]
    fn drag_confirmation_and_window_zoom_read_in_either_spelling_and_write_back() {
        let (s, problems) =
            parse(r#"{"explorer.confirmDragAndDrop": false, "window.zoom_level": -2}"#).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            (s.confirm_drag_and_drop, s.zoom_level),
            (Some(false), Some(-2))
        );
        let out = set("{}", &["explorer", "confirmDragAndDrop"], json!(false));
        assert!(!parse(&out).unwrap().0.confirm_drag_and_drop());
        let out = set(&out, &["window", "zoom_level"], json!(3));
        assert_eq!(parse(&out).unwrap().0.zoom_level, Some(3));
        let (_, problems) = parse(r#"{"window": {"zoom_level": 99}}"#).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        let (_, problems) = parse(r#"{"window.zoom_level": 1.5}"#).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
    }

    #[test]
    fn dotted_keys_are_updated_where_they_are() {
        let text = "{\"editor.format_on_save\": false}";
        assert_eq!(
            set(text, &["editor", "format_on_save"], json!(true)),
            "{\"editor.format_on_save\": true}"
        );
    }

    #[test]
    fn strings_holding_comment_and_brace_marks_are_left_alone() {
        let text = "{\n  \"lsp\": {\"gopls\": {\"x\": \"// } {\"}},\n  \"theme\": \"light\"\n}";
        let out = set(text, &["theme"], json!("dark"));
        assert!(out.contains("\"x\": \"// } {\""), "{out}");
        assert!(out.contains("\"theme\": \"dark\""), "{out}");
    }

    #[test]
    fn empty_objects_and_empty_files_get_the_key() {
        assert_eq!(
            set("{}", &["editor", "word_wrap"], json!(true)),
            "{\n  \"editor\": {\n    \"word_wrap\": true\n  }\n}"
        );
        assert_eq!(
            set("// just a note\n", &["theme"], json!("light")),
            "// just a note\n{\n  \"theme\": \"light\"\n}\n"
        );
    }

    #[test]
    fn a_file_that_is_not_an_object_is_an_error_and_an_empty_one_is_not() {
        assert!(parse("[1]").is_err());
        assert!(parse("{\"a\": ").is_err());
        assert_eq!(parse("// nothing yet\n").unwrap().0, Settings::default());
    }

    #[test]
    fn a_symlinked_file_is_written_through_and_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("athena-settings-{}", std::process::id()));
        let dots = dir.join("dotfiles");
        std::fs::create_dir_all(&dots).unwrap();
        let real = dots.join("settings.json");
        std::fs::write(&real, "{\n  // mine\n}\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.join("settings.json");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_at(&link, &["theme"], &json!("dark")).unwrap();
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        let text = std::fs::read_to_string(&real).unwrap();
        assert_eq!(text, "{\n  // mine\n  \"theme\": \"dark\"\n}\n");
        let mode = std::fs::metadata(&real).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_file_is_not_rewritten() {
        for broken in [
            "{\"theme\": ",
            "{\"a\": 1} x",
            "{\"a\": 1 \"b\": 2}",
            "{\"a\": tru}",
            "{\"a\": [1 2]}",
            "{\"editor\": {\"word_wrap\": false \"tab_size\": 2}}",
        ] {
            assert!(parse(broken).is_err(), "{broken}");
            assert!(
                set_value(broken, &["editor", "word_wrap"], &json!(true)).is_err(),
                "{broken}"
            );
        }
        let dir = std::env::temp_dir().join(format!("athena-settings-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("settings.json");
        std::fs::write(&file, "{\"theme\": \"light\" \"x\": 1}").unwrap();
        assert!(write_at(&file, &["theme"], &json!("dark")).is_err());
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "{\"theme\": \"light\" \"x\": 1}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_with_problems_is_still_written() {
        let out = set(r#"{"theme": "purple"}"#, &["theme"], json!("dark"));
        assert_eq!(out, r#"{"theme": "dark"}"#);
    }

    #[test]
    fn the_occurrence_serde_reads_last_is_the_one_written() {
        let cases: &[(&str, &[&str], Value, &str)] = &[
            (
                r#"{"theme": "light", "theme": "dark"}"#,
                &["theme"],
                json!("system"),
                r#"{"theme": "light", "theme": "system"}"#,
            ),
            (
                r#"{"editor": {"word_wrap": true}, "editor.word_wrap": true, "editor": {"word_wrap": true}}"#,
                &["editor", "word_wrap"],
                json!(false),
                r#"{"editor": {"word_wrap": true}, "editor.word_wrap": false, "editor": {"word_wrap": true}}"#,
            ),
            (
                r#"{"editor.word_wrap": true, "editor": {"word_wrap": true}}"#,
                &["editor", "word_wrap"],
                json!(false),
                r#"{"editor.word_wrap": true, "editor": {"word_wrap": false}}"#,
            ),
            (
                r#"{"editor": {"tab_size": 3}, "editor": {"word_wrap": true}}"#,
                &["editor", "format_on_save"],
                json!(false),
                "{\"editor\": {\"tab_size\": 3}, \"editor\": {\"word_wrap\": true,\n\"format_on_save\": false}}",
            ),
            (
                r#"{"editor": {"tab_size": 3, "tabSize": 2}}"#,
                &["editor", "tab_size"],
                json!(8),
                r#"{"editor": {"tab_size": 3, "tabSize": 8}}"#,
            ),
            (
                r#"{"files.insertFinalNewline": true, "editor": {"editor.insert_final_newline": true}}"#,
                &["editor", "insert_final_newline"],
                json!(false),
                r#"{"files.insertFinalNewline": true, "editor": {"editor.insert_final_newline": false}}"#,
            ),
        ];
        for (text, keys, value, want) in cases {
            assert_eq!(set_value(text, keys, value).unwrap(), *want, "{text}");
        }
    }

    /// One random settings.json; every member is a valid setting, some spelled twice or more.
    fn random_settings(next: &mut impl FnMut(usize) -> usize) -> String {
        let b = |n: usize| if n.is_multiple_of(2) { "true" } else { "false" };
        let blank = |n: usize| [" ", "\n  ", " /* c */ ", "\n  // c\n  "][n % 4];
        let inner = |next: &mut dyn FnMut(usize) -> usize| {
            let mut members = Vec::new();
            for _ in 0..next(5) {
                let v = next(16);
                members.push(match next(9) {
                    0 => format!("\"word_wrap\": {}", b(v)),
                    1 => format!("\"editor.word_wrap\": {}", b(v)),
                    2 => format!("\"files.word_wrap\": {}", b(v)),
                    3 => format!("\"tab_size\": {}", v + 1),
                    4 => format!("\"tabSize\": {}", v + 1),
                    5 => format!("\"editor.tabSize\": {}", v + 1),
                    6 => format!("\"formatOnSave\": {}", b(v)),
                    7 => format!("\"format_on_save\": {}", b(v)),
                    _ => format!("\"autosave_delay_ms\": {}", v * 100),
                });
            }
            members
        };
        let mut members = Vec::new();
        for _ in 0..next(9) {
            let v = next(16);
            members.push(match next(12) {
                0 | 1 => {
                    let m = inner(next);
                    let sep = format!(",{}", blank(next(4)));
                    format!("\"editor\": {{{}}}", m.join(&sep))
                }
                2 => format!("\"editor.word_wrap\": {}", b(v)),
                3 => format!("\"files.word_wrap\": {}", b(v)),
                4 => format!("\"editor.tabSize\": {}", v + 1),
                5 => format!("\"editor.tab_size\": {}", v + 1),
                6 => format!("\"editor.formatOnSave\": {}", b(v)),
                7 => format!("\"theme\": \"{}\"", ["light", "dark", "system"][v % 3]),
                8 => format!("\"ide_integration\": {}", b(v)),
                9 => format!("\"[go]\": {{{}}}", inner(next).join(", ")),
                10 => format!("\"git\": {{\"autofetch\": {}}}", b(v)),
                _ => format!("\"git.autofetch\": {}", b(v)),
            });
        }
        let mut text = String::from("{");
        for (i, m) in members.iter().enumerate() {
            text.push_str(blank(next(4)));
            text.push_str(m);
            if i + 1 < members.len() || next(2) == 0 {
                text.push(',');
            }
        }
        text.push_str(if next(2) == 0 { "\n}\n" } else { "}" });
        text
    }

    #[test]
    fn writes_land_where_parse_reads_across_many_shapes() {
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n.max(1) as u64) as usize
        };
        for _ in 0..3000 {
            let text = random_settings(&mut next);
            let (before, problems) = parse(&text).unwrap();
            assert!(problems.is_empty(), "{problems:?} in\n{text}");
            let v = next(16);
            let on = v.is_multiple_of(2);
            let mut want = before;
            let (keys, value): (&[&str], Value) = match next(7) {
                0 => {
                    want.editor.word_wrap = Some(on);
                    (&["editor", "word_wrap"], json!(on))
                }
                1 => {
                    want.editor.tab_size = Some(v + 1);
                    (&["editor", "tab_size"], json!(v + 1))
                }
                2 => {
                    want.editor.format_on_save = Some(on);
                    (&["editor", "format_on_save"], json!(on))
                }
                3 => {
                    want.editor.autosave_delay_ms = Some(v as u64 * 7);
                    (&["editor", "autosave_delay_ms"], json!(v * 7))
                }
                4 => {
                    want.editor.inlay_hints = Some(on);
                    (&["editor", "inlay_hints"], json!(on))
                }
                5 => {
                    want.theme = Some(ThemeChoice::Dark);
                    (&["theme"], json!("dark"))
                }
                _ => {
                    want.ide_integration = Some(on);
                    (&["ide_integration"], json!(on))
                }
            };
            let out = set_value(&text, keys, &value).unwrap();
            let (after, problems) = parse(&out).unwrap();
            assert!(problems.is_empty(), "{problems:?} in\n{out}");
            assert_eq!(after, want, "{keys:?} = {value} in\n{text}\nbecame\n{out}");
        }
    }

    #[test]
    fn settings_parse_with_language_blocks_dotted_keys_and_problems() {
        let (s, problems) = parse(
            r#"{
              "editor": {"format_on_save": true, "tab_size": 4, "bogus": 1},
              "editor.word_wrap": true,
              "[markdown]": {"files.trimTrailingWhitespace": false, "word_wrap": false},
              "files.insertFinalNewline": false,
              "theme": "purple",
              "lsp": {"gopls": {"staticcheck": true}, "tsserver": {"preferences": {}}},
              "nope": 1,
            }"#,
        )
        .unwrap();
        assert_eq!(s.editor.format_on_save, Some(true));
        assert_eq!(s.editor.tab_size, Some(4));
        assert_eq!(s.editor.word_wrap, Some(true));
        let md = s.editor_for(Some(Lang::Markdown));
        assert_eq!(md.trim_trailing_whitespace, Some(false));
        assert_eq!(md.word_wrap, Some(false));
        assert_eq!(
            md.format_on_save,
            Some(true),
            "falls back to the editor block"
        );
        assert_eq!(md.insert_final_newline, Some(false));
        assert_eq!(s.editor_for(Some(Lang::Go)).word_wrap, Some(true));
        assert_eq!(s.theme, None);
        assert!(s.lsp.contains_key("typescript-language-server"));
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("\"bogus\"")));
        assert!(problems.iter().any(|p| p.starts_with("\"theme\"")));
        assert!(problems.iter().any(|p| p.starts_with("\"nope\"")));
    }

    #[test]
    fn settings_override_the_workspace_and_a_missing_key_keeps_its_toggle() {
        let fallback = Preferences {
            autosave_delay_ms: 0,
            format_on_save: Some(true),
            word_wrap: true,
            ide_integration: true,
            theme: ThemeChoice::Light,
        };
        assert_eq!(Settings::default().over(fallback), fallback);
        let (s, _) = parse(r#"{"editor": {"word_wrap": false}, "theme": "dark"}"#).unwrap();
        let p = s.over(fallback);
        assert!(!p.word_wrap);
        assert_eq!(p.theme, ThemeChoice::Dark);
        assert_eq!(p.format_on_save, Some(true));
        assert!(p.ide_integration);
    }

    #[test]
    fn inlay_hints_add_curated_server_settings_unless_the_user_chose_some() {
        let (mut s, _) = parse(r#"{"lsp": {"gopls": {"staticcheck": true}}}"#).unwrap();
        assert_eq!(s.server_config("gopls"), json!({"staticcheck": true}));
        s.editor.inlay_hints = Some(true);
        let go = s.server_config("gopls");
        assert_eq!(go["staticcheck"], json!(true));
        assert_eq!(go["hints"]["parameterNames"], json!(true));
        s.lsp
            .insert("gopls".into(), json!({"hints": {"constantValues": true}}));
        assert_eq!(
            s.server_config("gopls"),
            json!({"hints": {"constantValues": true}})
        );
        let ts = s.server_config("typescript-language-server");
        assert_eq!(
            ts["preferences"]["includeInlayParameterNameHints"],
            json!("literals")
        );
        assert_eq!(
            ts["typescript"]["inlayHints"]["includeInlayFunctionLikeReturnTypeHints"],
            json!(true)
        );
    }
}
