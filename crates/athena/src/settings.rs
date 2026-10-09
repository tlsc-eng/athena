use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use athena_editor::Lang;
use athena_workspace::{Preferences, ThemeChoice};
use serde_json::{Map, Value, json};

pub mod schema;

/// What a new settings.json holds: an empty object, with every setting shown commented out.
pub(crate) const TEMPLATE: &str = include_str!("settings-template.jsonc");

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
    /// VS Code ESLint's `eslint.fixOnSave`-style switch: Cmd+S applies ESLint's fixes first.
    pub eslint_fix_on_save: Option<bool>,
    /// `claude.prices`: USD per million tokens by model id, over the built-in estimates.
    pub claude_prices: HashMap<String, crate::transcripts::Price>,
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
    pub bracket_pair_colorization: Option<bool>,
    /// From VS Code's `editor.codeActionsOnSave`: `source.organizeImports` on Cmd+S.
    pub organize_imports_on_save: Option<bool>,
    /// From `editor.codeActionsOnSave`: `source.fixAll.eslint` on Cmd+S.
    pub fix_all_on_save: Option<bool>,
    pub lightbulb: Option<Lightbulb>,
    /// VS Code's `editor.linkedEditing`: typing in a tag name renames its matching tag.
    pub linked_editing: Option<bool>,
    /// VS Code's `editor.minimap.enabled`; on unless turned off.
    pub minimap: Option<bool>,
    /// VS Code's `editor.semanticHighlighting.enabled`: colour names as the language server
    /// classifies them.
    pub semantic_highlighting: Option<bool>,
    /// VS Code's `editor.codeLens`: show the commands language servers offer with lines.
    pub code_lens: Option<bool>,
}

/// Which code actions put a lightbulb beside the cursor's line; the rest wait for Cmd+.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lightbulb {
    Off,
    QuickFixes,
    /// Refactorings too, as VS Code's `"editor.lightbulb.enabled": "onCode"`.
    All,
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
            bracket_pair_colorization: over
                .bracket_pair_colorization
                .or(self.bracket_pair_colorization),
            organize_imports_on_save: over
                .organize_imports_on_save
                .or(self.organize_imports_on_save),
            fix_all_on_save: over.fix_all_on_save.or(self.fix_all_on_save),
            lightbulb: over.lightbulb.or(self.lightbulb),
            linked_editing: over.linked_editing.or(self.linked_editing),
            minimap: over.minimap.or(self.minimap),
            semantic_highlighting: over.semantic_highlighting.or(self.semantic_highlighting),
            code_lens: over.code_lens.or(self.code_lens),
        }
    }

    fn set(&mut self, key: &str, value: &Value) -> Result<(), String> {
        let flag = || value.as_bool().ok_or("must be true or false");
        match editor_key(key) {
            "format_on_save" => self.format_on_save = Some(flag()?),
            "trim_trailing_whitespace" => self.trim_trailing_whitespace = Some(flag()?),
            "insert_final_newline" => self.insert_final_newline = Some(flag()?),
            // VS Code spells it "on", "off", "wordWrapColumn" or "bounded".
            "word_wrap" => {
                self.word_wrap = Some(match value.as_str() {
                    Some("off") => false,
                    Some("on" | "wordWrapColumn" | "bounded") => true,
                    _ => flag()?,
                });
            }
            "linked_editing" => self.linked_editing = Some(flag()?),
            // `"editor": {"minimap": {"enabled": false}}` nests the flag VS Code keys flat.
            "minimap" => {
                let flag = value.get("enabled").unwrap_or(value).as_bool();
                self.minimap = Some(flag.ok_or("must be true or false")?);
            }
            "code_lens" => self.code_lens = Some(flag()?),
            // VS Code's "configuredByTheme" is on: every Athena theme colours semantic tokens.
            "semantic_highlighting" => {
                self.semantic_highlighting = Some(match value.as_str() {
                    Some("configuredByTheme") => true,
                    _ => flag()?,
                });
            }
            "lightbulb" => {
                self.lightbulb = Some(match (value.as_str(), value.as_bool()) {
                    (Some("off"), _) | (_, Some(false)) => Lightbulb::Off,
                    (Some("quickfix"), _) => Lightbulb::QuickFixes,
                    (Some("all" | "onCode" | "on"), _) | (_, Some(true)) => Lightbulb::All,
                    _ => return Err("must be \"off\", \"quickfix\" or \"all\"".into()),
                });
            }
            "code_actions_on_save" => {
                let (organize, fix_all) = code_actions_on_save(value)?;
                self.organize_imports_on_save = Some(organize);
                self.fix_all_on_save = Some(fix_all);
            }
            "inlay_hints" => self.inlay_hints = Some(flag()?),
            "bracket_pair_colorization" => self.bracket_pair_colorization = Some(flag()?),
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
        "bracketPairColorization" | "bracketPairColorization.enabled" => {
            "bracket_pair_colorization"
        }
        "wordWrap" => "word_wrap",
        "linkedEditing" => "linked_editing",
        "minimap.enabled" => "minimap",
        "codeLens" => "code_lens",
        "semanticHighlighting" | "semanticHighlighting.enabled" => "semantic_highlighting",
        "lightbulb.enabled" => "lightbulb",
        "codeActionsOnSave" => "code_actions_on_save",
        other => other,
    }
}

/// The `editor.codeActionsOnSave` kinds Athena runs: organize imports and ESLint's fix-all.
/// VS Code takes a list of kinds, or an object of kinds to `true`, `"explicit"`, `"always"`
/// or `false`/`"never"`; Cmd+S is an explicit save, so all but the off values turn a kind on.
fn code_actions_on_save(value: &Value) -> Result<(bool, bool), String> {
    let on: Vec<&str> = match value {
        Value::Array(kinds) => kinds.iter().filter_map(Value::as_str).collect(),
        Value::Object(kinds) => kinds
            .iter()
            .filter(|(_, v)| !matches!(v, Value::Bool(false)) && v.as_str() != Some("never"))
            .map(|(k, _)| k.as_str())
            .collect(),
        _ => return Err("must be an object of code action kinds".into()),
    };
    let wants = |kind: &str| {
        on.iter()
            .any(|k| *k == kind || kind.starts_with(&format!("{k}.")))
    };
    Ok((
        wants("source.organizeImports"),
        wants("source.fixAll.eslint"),
    ))
}

impl Settings {
    pub fn git_autofetch(&self) -> bool {
        self.autofetch.unwrap_or(false)
    }

    pub fn confirm_drag_and_drop(&self) -> bool {
        self.confirm_drag_and_drop.unwrap_or(true)
    }

    /// Off unless chosen, as fixes that rewrite code on save should be.
    pub fn eslint_fix_on_save(&self) -> bool {
        self.eslint_fix_on_save.unwrap_or(false)
    }

    /// The word wrap `lang` chooses: its own block's, else its built-in default.
    pub fn language_word_wrap(&self, lang: Option<Lang>) -> Option<bool> {
        self.language_block(lang?).word_wrap
    }

    /// The editor settings for files of `lang`: the global ones, then the language's built-in
    /// defaults, then the user's `"[lang]"` block, which is VS Code's order.
    pub fn editor_for(&self, lang: Option<Lang>) -> EditorSettings {
        match lang {
            Some(lang) => self.editor.overlaid(&self.language_block(lang)),
            None => self.editor.clone(),
        }
    }

    fn language_block(&self, lang: Lang) -> EditorSettings {
        let defaults = language_defaults(lang);
        match self.languages.get(language_id(lang)) {
            Some(over) => defaults.overlaid(over),
            None => defaults,
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
        // gopls sends no semantic tokens unless asked, yet VS Code shows them by default.
        if program == "gopls" && self.editor.semantic_highlighting != Some(false) {
            if !config.is_object() {
                config = json!({});
            }
            if config.get("semanticTokens").is_none() {
                config["semanticTokens"] = json!(true);
            }
        }
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

impl Settings {
    /// These settings with a project's laid over them: its editor and language blocks win key by
    /// key and its `lsp` entries merge into these; app-wide settings stay these.
    pub fn overlaid(&self, project: &Settings) -> Settings {
        let mut out = self.clone();
        out.editor = self.editor.overlaid(&project.editor);
        for (id, block) in &project.languages {
            let merged = match self.languages.get(id) {
                Some(base) => base.overlaid(block),
                None => block.clone(),
            };
            out.languages.insert(id.clone(), merged);
        }
        for (server, config) in &project.lsp {
            merge_json(out.lsp.entry(server.clone()).or_insert(Value::Null), config);
        }
        out.eslint_fix_on_save = project.eslint_fix_on_save.or(self.eslint_fix_on_save);
        out
    }

    /// Whether these project settings choose what language servers run or load, which only a
    /// trusted project may: server settings can name programs, plugins, flags and toolchains.
    pub fn changes_programs(&self) -> bool {
        !self.lsp.is_empty()
    }

    /// These settings without what [`Self::changes_programs`] covers.
    pub fn without_programs(&self) -> Settings {
        Settings {
            lsp: HashMap::new(),
            ..self.clone()
        }
    }
}

/// `over` laid onto `base`, objects merged key by key.
pub fn merge_json(base: &mut Value, over: &Value) {
    match (base.as_object_mut(), over.as_object()) {
        (Some(base), Some(over)) => {
            for (key, value) in over {
                merge_json(base.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        _ if !over.is_null() => *base = over.clone(),
        _ => {}
    }
}

/// Where a project keeps its own settings, beside VS Code's.
pub const PROJECT_FILE: &str = ".athena/settings.json";
pub const VSCODE_FILE: &str = ".vscode/settings.json";

/// A project's `.athena/settings.json`, read like the global file; settings that are app-wide
/// (theme, zoom, autofetch and the like) are reported and left out.
pub fn parse_project(text: &str) -> Result<(Settings, Vec<String>), String> {
    let (mut s, mut problems) = parse(text)?;
    let app_wide = [
        ("theme", s.theme.take().is_some()),
        ("ide_integration", s.ide_integration.take().is_some()),
        ("git.autofetch", s.autofetch.take().is_some()),
        (
            "explorer.confirmDragAndDrop",
            s.confirm_drag_and_drop.take().is_some(),
        ),
        ("window.zoom_level", s.zoom_level.take().is_some()),
        ("editor.font_size", s.editor.font_size.take().is_some()),
        (
            "claude.prices",
            !std::mem::take(&mut s.claude_prices).is_empty(),
        ),
    ];
    for (key, set) in app_wide {
        if set {
            problems.push(format!(
                "\"{key}\" applies only in the global settings.json"
            ));
        }
    }
    Ok((s, problems))
}

/// The settings Athena understands in a project's `.vscode/settings.json`: editor and `[lang]`
/// keys, `editor.codeActionsOnSave`, `gopls`, `go.toolsEnvVars` (gopls's `env`),
/// `typescript.tsdk` and the `typescript.*` / `javascript.*` preferences typescript-language-server
/// reads. Everything else, and any value Athena cannot use, is ignored, as the file is shared
/// with VS Code and its extensions.
pub fn parse_vscode(text: &str, root: &Path) -> Settings {
    let mut settings = Settings::default();
    let json = crate::keymap::strip_jsonc(text);
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&json) else {
        return settings;
    };
    const TS: &str = "typescript-language-server";
    for (key, value) in &map {
        match key.as_str() {
            k if k.len() > 2 && k.starts_with('[') && k.ends_with(']') => {
                let block = settings
                    .languages
                    .entry(k[1..k.len() - 1].into())
                    .or_default();
                for (inner, value) in value.as_object().into_iter().flatten() {
                    if let Some(rest) = inner
                        .strip_prefix("editor.")
                        .or_else(|| inner.strip_prefix("files."))
                    {
                        let _ = block.set(rest, value);
                    }
                }
            }
            "gopls" if value.is_object() => {
                merge_json(
                    settings.lsp.entry("gopls".into()).or_insert(Value::Null),
                    value,
                );
            }
            "go.toolsEnvVars" if value.is_object() => {
                let gopls = settings.lsp.entry("gopls".into()).or_insert(json!({}));
                merge_json(gopls, &json!({ "env": value }));
            }
            "typescript.tsdk" => {
                if let Some(dir) = value.as_str().filter(|d| !d.is_empty()) {
                    let dir = root.join(dir);
                    let ts = settings.lsp.entry(TS.into()).or_insert(json!({}));
                    set_pointer(ts, "/tsserver/path", json!(dir));
                }
            }
            k if k.starts_with("typescript.") || k.starts_with("javascript.") => {
                let ts = settings.lsp.entry(TS.into()).or_insert(json!({}));
                set_pointer(ts, &format!("/{}", k.replace('.', "/")), value.clone());
            }
            k => {
                if let Some(rest) = k
                    .strip_prefix("editor.")
                    .or_else(|| k.strip_prefix("files."))
                {
                    let _ = settings.editor.set(rest, value);
                }
            }
        }
    }
    settings.editor.font_size = None;
    settings
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

/// The settings VS Code ships for a language, which beat the user's global ones.
/// Go's tab indentation is the editor's own default for Go files.
fn language_defaults(lang: Lang) -> EditorSettings {
    match lang {
        Lang::Markdown => EditorSettings {
            word_wrap: Some(true),
            trim_trailing_whitespace: Some(false),
            ..EditorSettings::default()
        },
        // VS Code's Go extension also sets `codeActionsOnSave` to organize imports.
        Lang::Go => EditorSettings {
            format_on_save: Some(true),
            organize_imports_on_save: Some(true),
            ..EditorSettings::default()
        },
        _ => EditorSettings::default(),
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
        Lang::Mermaid => "mermaid",
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
            "eslint" => match value.get("fixOnSave").map(Value::as_bool) {
                Some(Some(on)) => {
                    settings.eslint_fix_on_save = Some(on);
                    Ok(())
                }
                _ => Err("must be {\"fixOnSave\": true or false}".into()),
            },
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
            "claude" => match value.get("prices") {
                Some(prices) => claude_prices(&mut settings, prices),
                None => Err("must be {\"prices\": {model id: prices}}".into()),
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
                Some(("eslint", "fixOnSave")) => value
                    .as_bool()
                    .map(|on| settings.eslint_fix_on_save = Some(on))
                    .ok_or_else(|| "must be true or false".to_string()),
                Some(("lsp", name)) => {
                    settings.lsp.insert(server_name(name).into(), value.clone());
                    Ok(())
                }
                Some(("claude", "prices")) => claude_prices(&mut settings, value),
                _ => Err("is not a setting".into()),
            },
        };
        if let Err(why) = result {
            problems.push(format!("\"{key}\" {why}"));
        }
    }
    Ok((settings, problems))
}

/// Reads `{"claude-opus-5": {"input": 5, "output": 25}}`; cache prices default to the usual
/// multiples of the input price.
fn claude_prices(settings: &mut Settings, value: &Value) -> Result<(), String> {
    let models = value
        .as_object()
        .ok_or("must be an object of prices by model id")?;
    for (model, price) in models {
        let field = |name: &str| match price.get(name) {
            None => Ok(None),
            Some(v) => v.as_f64().filter(|n| *n >= 0.).map(Some).ok_or(format!(
                "\"{model}\".\"{name}\" must be a number of USD per million tokens"
            )),
        };
        let (Some(input), Some(output)) = (field("input")?, field("output")?) else {
            return Err(format!("\"{model}\" needs \"input\" and \"output\" prices"));
        };
        let mut p = crate::transcripts::Price::of(input, output);
        p.cache_write_5m = field("cache_write")?.unwrap_or(p.cache_write_5m);
        p.cache_write_1h = field("cache_write_1h")?.unwrap_or(p.cache_write_1h);
        p.cache_read = field("cache_read")?.unwrap_or(p.cache_read);
        settings.claude_prices.insert(model.clone(), p);
    }
    Ok(())
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

/// Sets `keys` to `value` in the settings file at `path`, as [`write`] does settings.json.
pub fn write_at(path: &Path, keys: &[&str], value: &Value) -> Result<String> {
    edit_at(path, |text| set_value(text, keys, value))
}

/// Removes every spelling of `keys` from the settings file at `path`, so it falls back.
pub fn unset_at(path: &Path, keys: &[&str]) -> Result<String> {
    edit_at(path, |text| unset_value(text, keys))
}

/// Creates a project's `.athena/settings.json` as an empty object unless it exists.
pub fn ensure_project_file(root: &Path) -> Result<PathBuf> {
    let path = root.join(PROJECT_FILE);
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, "{\n}\n")?;
    }
    Ok(path)
}

/// Rewrites the file at `path` with `edit`, through a symlink and keeping its mode, replacing it
/// whole so a reader never sees half of it.
pub(crate) fn edit_at(
    path: &Path,
    edit: impl FnOnce(&str) -> Result<String, String>,
) -> Result<String> {
    // A settings.json linked from a dotfiles checkout is updated there, not replaced by a copy.
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text =
        std::fs::read_to_string(&target).with_context(|| format!("read {}", target.display()))?;
    let updated = edit(&text).map_err(|why| anyhow!("{why}"))?;
    if updated != text {
        let name = target.file_name().unwrap_or_default().to_string_lossy();
        let tmp = target.with_file_name(format!(".{name}.{}", std::process::id()));
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

/// `text` without the members that set `keys`, in any spelling, so the setting falls back; a
/// block such as `"git": {}` left empty goes too. Refuses text [`parse`] cannot read.
pub fn unset_value(text: &str, keys: &[&str]) -> Result<String, String> {
    parse(text)?;
    let mut out = text.to_string();
    // Each pass removes the spelling parse reads, which may uncover an earlier one.
    while let Some(root) = root_object(&out)? {
        let Some((object, member)) = read_from(&root, keys) else {
            break;
        };
        let at = object
            .members
            .iter()
            .position(|m| std::ptr::eq(m, member))
            .expect("the member is in its object");
        out = remove_member(&out, object, at);
        let Some(root) = root_object(&out)? else {
            break;
        };
        let read = root
            .members
            .iter()
            .rposition(|m| keys.len() > 1 && m.key == keys[0]);
        let emptied = read.filter(|&at| {
            let m = &root.members[at];
            m.object.as_ref().is_some_and(|o| {
                o.members.is_empty()
                    && (m.key != "editor" || out[o.open + 1..o.close].trim().is_empty())
            })
        });
        if let Some(at) = emptied {
            let key = root.members[at].key.clone();
            out = remove_member(&out, &root, at);
            // Earlier blocks of that name were shadowed by this one, and would apply without it.
            while let Some(root) = root_object(&out)?
                && let Some(i) = root.members.iter().position(|m| m.key == key)
            {
                out = remove_member(&out, &root, i);
            }
        }
    }
    parse(&out).map_err(|why| format!("settings.json would be left unreadable: {why}"))?;
    Ok(out)
}

/// The file's top-level object, or `None` for a file with nothing but comments.
fn root_object(text: &str) -> Result<Option<Object>, String> {
    let mut scan = Scan {
        s: text.as_bytes(),
        i: 0,
    };
    scan.skip_blank();
    if scan.i == text.len() {
        return Ok(None);
    }
    scan.object().map(Some)
}

/// `text` without `object`'s member `at` and the comma that separated it from its neighbours;
/// a member on a line of its own takes the line, and the comment ending it, with it.
fn remove_member(text: &str, object: &Object, at: usize) -> String {
    let m = &object.members[at];
    remove_span(
        text,
        m.key_start..m.value.end,
        at.checked_sub(1).map(|i| object.members[i].value.end),
    )
}

/// `text` without the list entry at `span`; `prev_end` is where the entry before it ends.
pub(crate) fn remove_span(text: &str, span: Range<usize>, prev_end: Option<usize>) -> String {
    let line_start = text[..span.start].rfind('\n').map_or(0, |i| i + 1);
    let own_line = text[line_start..span.start].trim().is_empty();
    let mut scan = Scan {
        s: text.as_bytes(),
        i: span.end,
    };
    scan.skip_blank();
    if scan.peek() == Some(b',') {
        let start = if own_line { line_start } else { span.start };
        let after = scan.i + 1;
        let line_end = text[after..].find('\n').map_or(text.len(), |i| after + i);
        let rest = text[after..line_end].trim_start();
        let end = if own_line && (rest.is_empty() || rest.starts_with("//")) {
            (line_end + 1).min(text.len())
        } else {
            after + (text[after..line_end].len() - text[after..line_end].trim_start().len())
        };
        return format!("{}{}", &text[..start], &text[end..]);
    }
    // The last entry: only the comma before it goes, not the comments between them.
    let comma = prev_end.and_then(|end| {
        let mut scan = Scan {
            s: text.as_bytes(),
            i: end,
        };
        scan.skip_blank();
        (scan.peek() == Some(b',')).then_some(scan.i)
    });
    let before = &text[line_start..span.start];
    let own_line = own_line || comma.is_some_and(|c| c >= line_start) && before.trim() == ",";
    let line_end = text[span.end..]
        .find('\n')
        .map_or(text.len(), |i| span.end + i);
    let rest = &text[span.end..line_end];
    let after_blank = span.end + (rest.len() - rest.trim_start().len());
    let (start, end) = match own_line {
        true if rest.trim().is_empty() || rest.trim_start().starts_with("//") => {
            (line_start, (line_end + 1).min(text.len()))
        }
        true => (line_start, after_blank),
        false => match comma {
            Some(c) if text[c + 1..span.start].trim().is_empty() => (c, after_blank),
            _ if rest.trim().is_empty() => (line_start + before.trim_end().len(), after_blank),
            _ => (span.start, after_blank),
        },
    };
    match comma.filter(|&c| c < start) {
        Some(c) => format!("{}{}{}", &text[..c], &text[c + 1..start], &text[end..]),
        None => format!("{}{}", &text[..start], &text[end..]),
    }
}

/// Where a top-level JSON array's entries sit: its brackets and each entry's bytes.
pub(crate) struct ArrayLayout {
    pub(crate) open: usize,
    pub(crate) close: usize,
    pub(crate) items: Vec<Range<usize>>,
}

/// Finds the entries of the JSONC array `text` holds; `None` for a file with only comments.
pub(crate) fn array_layout(text: &str) -> Result<Option<ArrayLayout>, String> {
    let mut scan = Scan {
        s: text.as_bytes(),
        i: 0,
    };
    scan.skip_blank();
    if scan.i == text.len() {
        return Ok(None);
    }
    let open = scan.i;
    scan.expect(b'[')?;
    let mut items = Vec::new();
    loop {
        scan.skip_blank();
        match scan.peek() {
            Some(b']') => break,
            Some(b',') => scan.i += 1,
            None => return Err("the list never ends".into()),
            _ => items.push(scan.value()?.0),
        }
    }
    Ok(Some(ArrayLayout {
        open,
        close: scan.i,
        items,
    }))
}

/// Each problem `text` has, at the bytes it is about: a setting's key, or the line JSON could
/// not be read at, marked `true` as nothing in the file then applies.
pub fn problems_at(text: &str, project: bool) -> Vec<(Range<usize>, String, bool)> {
    let read = |t: &str| match project {
        true => parse_project(t),
        false => parse(t),
    };
    match read(text) {
        Err(why) => {
            let line = error_line(&why).unwrap_or(1);
            return vec![(line_span(text, line), why, true)];
        }
        Ok((_, problems)) if problems.is_empty() => return Vec::new(),
        Ok(_) => {}
    }
    let Ok(Some(root)) = root_object(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for m in &root.members {
        let own = format!("{{{}}}", &text[m.key_start..m.value.end]);
        let problems = read(&own).map(|(_, p)| p).unwrap_or_default();
        if problems.is_empty() {
            continue;
        }
        // Inside a block, each problem goes on the key it is about, where one is.
        let mut inside = Vec::new();
        for im in m.object.iter().flat_map(|o| &o.members) {
            let one = format!(
                "{{{}: {{{}}}}}",
                quote(&m.key),
                &text[im.key_start..im.value.end]
            );
            for why in read(&one).map(|(_, p)| p).unwrap_or_default() {
                inside.push((im.key_start..im.key_end, why, false));
            }
        }
        match inside.is_empty() {
            true => out.extend(
                problems
                    .into_iter()
                    .map(|why| (m.key_start..m.key_end, why, false)),
            ),
            false => out.extend(inside),
        }
    }
    out
}

/// The 1-based line serde_json names in an error such as "... at line 3 column 5".
pub(crate) fn error_line(why: &str) -> Option<usize> {
    let rest = &why[why.rfind(" at line ")? + " at line ".len()..];
    rest.split_whitespace().next()?.parse().ok()
}

/// The bytes of 1-based `line` without its indentation, or the whole text past its end.
pub(crate) fn line_span(text: &str, line: usize) -> Range<usize> {
    let mut start = 0;
    for _ in 1..line {
        match text[start..].find('\n') {
            Some(i) => start += i + 1,
            None => break,
        }
    }
    let end = text[start..].find('\n').map_or(text.len(), |i| start + i);
    let indent = text[start..end].len() - text[start..end].trim_start().len();
    (start + indent).min(end)..end
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
    if let Some((_, m)) = read_from(&root, keys) {
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

/// The member whose value [`parse`] ends up with for `keys`, whichever spelling or place it has,
/// and the object holding it.
fn read_from<'a>(root: &'a Object, keys: &[&str]) -> Option<(&'a Object, &'a Member)> {
    let target = match keys {
        [key] => setting_name(None, key),
        [block, rest @ ..] => setting_name(Some(block), &rest.join(".")),
        [] => return None,
    };
    let mut found = None;
    for m in as_read(root) {
        if setting_name(None, &m.key) == target {
            found = Some((root, m));
        }
        if keys.len() > 1
            && m.key == keys[0]
            && let Some(inner) = &m.object
        {
            for im in as_read(inner) {
                if setting_name(Some(keys[0]), &im.key) == target {
                    found = Some((inner, im));
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
    key_end: usize,
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
            let (key_start, key_end) = (key.start, key.end);
            let key: String = serde_json::from_slice(&self.s[key]).map_err(|e| e.to_string())?;
            self.skip_blank();
            self.expect(b':')?;
            self.skip_blank();
            let (value, object) = self.value()?;
            members.push(Member {
                key,
                key_start,
                key_end,
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
pub(crate) mod tests {
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
        assert!(s.eslint_fix_on_save());
        assert!(!Settings::default().eslint_fix_on_save(), "off by default");
        let (dotted, _) = parse(r#"{"eslint.fixOnSave": true}"#).unwrap();
        assert!(dotted.eslint_fix_on_save());
        assert_eq!(s.claude_prices["claude-sonnet-5"].cache_read, 0.3);
    }

    #[test]
    fn claude_prices_fill_cache_prices_from_the_input_price_and_reject_bad_ones() {
        let (s, problems) =
            parse(r#"{"claude.prices": {"m-1": {"input": 2, "output": 10, "cache_write_1h": 3}}}"#)
                .unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        let p = s.claude_prices["m-1"];
        assert_eq!(
            (
                p.input,
                p.output,
                p.cache_write_5m,
                p.cache_write_1h,
                p.cache_read
            ),
            (2., 10., 2.5, 3., 0.2)
        );
        for bad in [
            r#"{"claude": {"prices": {"m": {"input": 1}}}}"#,
            r#"{"claude": {"prices": {"m": {"input": -1, "output": 1}}}}"#,
            r#"{"claude": {"prices": {"m": {"input": 1, "output": "x"}}}}"#,
            r#"{"claude": {"prices": []}}"#,
            r#"{"claude": {"price": {}}}"#,
        ] {
            let (_, problems) = parse(bad).unwrap();
            assert_eq!(problems.len(), 1, "{bad}");
        }
    }

    #[test]
    fn the_minimap_reads_vs_codes_flat_and_nested_keys_and_by_language() {
        let (s, problems) =
            parse(r#"{"editor.minimap.enabled": false, "[go]": {"editor.minimap.enabled": true}}"#)
                .unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(s.editor.minimap, Some(false));
        assert_eq!(s.editor_for(Some(Lang::Go)).minimap, Some(true));
        let (s, _) = parse(r#"{"editor": {"minimap": {"enabled": false}}}"#).unwrap();
        assert_eq!(s.editor.minimap, Some(false));
        let (s, _) = parse(r#"{"editor": {"minimap": true}}"#).unwrap();
        assert_eq!(s.editor.minimap, Some(true));
        let (_, problems) = parse(r#"{"editor": {"minimap": "on"}}"#).unwrap();
        assert_eq!(problems.len(), 1);
        assert_eq!(Settings::default().editor.minimap, None);
    }

    #[test]
    fn toggling_the_minimap_rewrites_whichever_spelling_is_there() {
        let keys = &["editor", "minimap", "enabled"];
        let cases = [
            (
                r#"{"editor.minimap.enabled": true}"#,
                r#"{"editor.minimap.enabled": false}"#,
            ),
            (
                r#"{"editor": {"minimap": true}}"#,
                r#"{"editor": {"minimap": false}}"#,
            ),
        ];
        for (text, want) in cases {
            assert_eq!(set_value(text, keys, &json!(false)).unwrap(), want);
        }
        let out = set_value("{}", keys, &json!(false)).unwrap();
        assert_eq!(parse(&out).unwrap().0.editor.minimap, Some(false), "{out}");
    }

    #[test]
    fn bracket_pair_colorization_reads_vs_codes_key_and_athenas_and_by_language() {
        let (s, problems) = parse(
            r#"{"editor.bracketPairColorization.enabled": false,
                "[markdown]": {"bracket_pair_colorization": true}}"#,
        )
        .unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(s.editor.bracket_pair_colorization, Some(false));
        let md = s.editor_for(Some(Lang::Markdown));
        assert_eq!(md.bracket_pair_colorization, Some(true));
        let (s, _) = parse(r#"{"editor": {"bracketPairColorization": false}}"#).unwrap();
        assert_eq!(s.editor.bracket_pair_colorization, Some(false));
        assert_eq!(Settings::default().editor.bracket_pair_colorization, None);
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
    fn unsetting_removes_every_spelling_and_keeps_comments_and_neighbours() {
        let text = "{\n  // mine\n  \"theme\": \"dark\", // why\n  \"editor.tabSize\": 2,\n  \"editor\": {\n    \"tab_size\": 3,\n    \"word_wrap\": true\n  },\n  \"git\": {\"autofetch\": true}\n}\n";
        let out = unset_value(text, &["editor", "tab_size"]).unwrap();
        assert_eq!(
            out,
            "{\n  // mine\n  \"theme\": \"dark\", // why\n  \"editor\": {\n    \"word_wrap\": true\n  },\n  \"git\": {\"autofetch\": true}\n}\n"
        );
        let out = unset_value(&out, &["theme"]).unwrap();
        assert!(out.starts_with("{\n  // mine\n  \"editor\""), "{out}");
        let out = unset_value(&out, &["git", "autofetch"]).unwrap();
        assert!(!out.contains("git"), "an emptied block goes too:\n{out}");
        let out = unset_value(&out, &["editor", "word_wrap"]).unwrap();
        assert_eq!(out, "{\n  // mine\n}\n");
        assert_eq!(parse(&out).unwrap().0, Settings::default());
        assert_eq!(unset_value("{}", &["theme"]).unwrap(), "{}");
        assert_eq!(
            unset_value(
                r#"{"a.b": 1, "theme": "dark", "editor.minimap.enabled": false}"#,
                &["editor", "minimap"]
            )
            .unwrap(),
            r#"{"a.b": 1, "theme": "dark"}"#,
            "a file with problems is still written"
        );
    }

    #[test]
    fn unsetting_the_last_member_takes_the_comma_before_it() {
        assert_eq!(
            unset_value(
                r#"{"theme": "dark", "ide_integration": true}"#,
                &["ide_integration"]
            )
            .unwrap(),
            r#"{"theme": "dark"}"#
        );
        assert_eq!(
            unset_value(
                r#"{"theme": "dark", "ide_integration": true, "git.autofetch": true}"#,
                &["ide_integration"]
            )
            .unwrap(),
            r#"{"theme": "dark", "git.autofetch": true}"#
        );
        assert_eq!(
            unset_value(
                "{\n  \"editor\": { /* keep */ \"word_wrap\": true }\n}",
                &["editor", "word_wrap"]
            )
            .unwrap(),
            "{\n  \"editor\": { /* keep */ }\n}",
            "an editor block with a comment in it stays"
        );
    }

    #[test]
    fn unsetting_the_last_member_keeps_the_comments_before_it() {
        for (text, want) in [
            (
                "{\n  \"ide_integration\": true,\n  // note\n  // \"theme\": \"light\",\n  \"theme\": \"dark\" // mine\n}\n",
                "{\n  \"ide_integration\": true\n  // note\n  // \"theme\": \"light\",\n}\n",
            ),
            (
                "{\n  \"ide_integration\": true, // about it\n  \"theme\": \"dark\"\n}",
                "{\n  \"ide_integration\": true // about it\n}",
            ),
            (
                "{\"ide_integration\": true /* x */, \"theme\": \"dark\"}",
                "{\"ide_integration\": true /* x */}",
            ),
            (
                "{\n  \"ide_integration\": true\n  , \"theme\": \"dark\"\n}",
                "{\n  \"ide_integration\": true\n}",
            ),
            (
                "{\n  \"ide_integration\": true,\n  /* c */ \"theme\": \"dark\"\n}",
                "{\n  \"ide_integration\": true\n  /* c */\n}",
            ),
            (
                "{\n  // only\n  \"theme\": \"dark\" // mine\n}",
                "{\n  // only\n}",
            ),
        ] {
            assert_eq!(unset_value(text, &["theme"]).unwrap(), want, "from\n{text}");
        }
    }

    /// The numbered comments in `text`: `c` ones stand alone or comment out an entry, `t` ones
    /// end an entry's line.
    pub(crate) fn comment_marks(text: &str) -> Vec<&str> {
        text.split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| {
                w.len() > 1
                    && w.starts_with(['c', 't'])
                    && w[1..].bytes().all(|b| b.is_ascii_digit())
            })
            .collect()
    }

    /// `entries` as a JSONC list or object between random numbered comments; each `t` comment
    /// comes with the entries on its line, `usize::MAX` standing for the opening bracket.
    pub(crate) fn commented_entries(
        entries: &[String],
        (open, close): (&str, &str),
        next: &mut impl FnMut(usize) -> usize,
    ) -> (String, Vec<(String, Vec<usize>)>) {
        let mut text = String::from(open);
        let mut n = 0;
        let mut ends = Vec::new();
        let mut line = vec![usize::MAX];
        let mut line_open = false;
        for (i, entry) in entries.iter().enumerate() {
            for _ in 0..next(3) {
                n += 1;
                text.push_str(&match next(3) {
                    0 => format!("\n  // c{n}"),
                    1 => format!("\n  // {}, // c{n}", entries[next(entries.len())]),
                    _ => format!("\n  /* c{n} */"),
                });
                line_open = true;
            }
            if line_open || next(4) != 0 {
                text.push_str("\n  ");
                line.clear();
            } else {
                text.push(' ');
            }
            text.push_str(entry);
            line.push(i);
            if i + 1 < entries.len() || next(3) == 0 {
                text.push(',');
            }
            line_open = next(2) == 0;
            if line_open {
                n += 1;
                text.push_str(&format!(" // t{n}"));
                ends.push((format!("t{n}"), line.clone()));
            }
        }
        for _ in 0..next(3) {
            n += 1;
            text.push_str(&format!("\n  // c{n}"));
        }
        text.push('\n');
        text.push_str(close);
        (text, ends)
    }

    #[test]
    fn unsetting_keeps_every_comment_but_the_one_ending_the_removed_line() {
        let mut state: u64 = 0x3c6e_f372_fe94_f82b;
        let mut next = move |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n.max(1) as u64) as usize
        };
        let members: [(&[&str], &str); 6] = [
            (&["theme"], "\"theme\": \"dark\""),
            (&["ide_integration"], "\"ide_integration\": true"),
            (&["git", "autofetch"], "\"git.autofetch\": true"),
            (&["editor", "tab_size"], "\"editor.tabSize\": 4"),
            (&["editor", "word_wrap"], "\"editor.word_wrap\": true"),
            (
                &["editor", "format_on_save"],
                "\"editor.formatOnSave\": true",
            ),
        ];
        for _ in 0..3000 {
            let mut order: Vec<usize> = (0..members.len()).collect();
            for i in (1..order.len()).rev() {
                order.swap(i, next(i + 1));
            }
            order.truncate(1 + next(members.len()));
            let entries: Vec<String> = order.iter().map(|&m| members[m].1.into()).collect();
            let (text, ends) = commented_entries(&entries, ("{", "}"), &mut next);
            let gone = order[next(order.len())];
            let out = unset_value(&text, members[gone].0).unwrap();
            let (before, _) = parse(&text).unwrap();
            let (after, problems) = parse(&out).unwrap();
            assert!(problems.is_empty(), "{problems:?} in\n{out}");
            for (keys, _) in members {
                let setting = schema::find(keys).unwrap();
                let want = match keys == members[gone].0 {
                    true => None,
                    false => setting.value_in(&before),
                };
                assert_eq!(
                    setting.value_in(&after),
                    want,
                    "{keys:?} in\n{text}\nbecame\n{out}"
                );
            }
            let at = order.iter().position(|&m| m == gone).unwrap();
            let mut want = comment_marks(&text);
            want.retain(|m| !ends.iter().any(|(t, line)| t == m && *line == [at]));
            assert_eq!(
                comment_marks(&out),
                want,
                "{:?} unset in\n{text}\nbecame\n{out}",
                members[gone].0
            );
        }
    }

    #[test]
    fn unsets_land_where_parse_reads_across_many_shapes() {
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n.max(1) as u64) as usize
        };
        for _ in 0..3000 {
            let text = random_settings(&mut next);
            let (mut want, _) = parse(&text).unwrap();
            let keys: &[&str] = match next(7) {
                0 => {
                    want.editor.word_wrap = None;
                    &["editor", "word_wrap"]
                }
                1 => {
                    want.editor.tab_size = None;
                    &["editor", "tab_size"]
                }
                2 => {
                    want.editor.format_on_save = None;
                    &["editor", "format_on_save"]
                }
                3 => {
                    want.editor.autosave_delay_ms = None;
                    &["editor", "autosave_delay_ms"]
                }
                4 => {
                    want.theme = None;
                    &["theme"]
                }
                5 => {
                    want.autofetch = None;
                    &["git", "autofetch"]
                }
                _ => {
                    want.ide_integration = None;
                    &["ide_integration"]
                }
            };
            let out = unset_value(&text, keys).unwrap();
            let (after, problems) = parse(&out).unwrap();
            assert!(problems.is_empty(), "{problems:?} in\n{out}");
            assert_eq!(after, want, "{keys:?} unset in\n{text}\nbecame\n{out}");
        }
    }

    #[test]
    fn problems_point_at_the_key_they_are_about() {
        let text = "{\n  \"theme\": \"purple\",\n  \"editor\": {\n    \"tab_size\": 2,\n    \"bogus\": 1\n  },\n  \"nope\": 1\n}";
        let at: Vec<(&str, bool)> = problems_at(text, false)
            .iter()
            .map(|(r, _, fatal)| (&text[r.clone()], *fatal))
            .collect();
        assert_eq!(
            at,
            [
                ("\"theme\"", false),
                ("\"bogus\"", false),
                ("\"nope\"", false)
            ]
        );
        let nested = r#"{"window": {"zoom_level": 9}, "git": {}}"#;
        let at: Vec<&str> = problems_at(nested, false)
            .iter()
            .map(|(r, _, _)| &nested[r.clone()])
            .collect();
        assert_eq!(at, ["\"zoom_level\"", "\"git\""]);
        let project = problems_at(
            r#"{"editor": {"font_size": 14, "tab_size": 2}, "theme": "dark"}"#,
            true,
        );
        assert_eq!(project.len(), 2, "{project:?}");
        assert!(project[0].1.contains("only in the global"), "{project:?}");
        let broken = "{\n  \"theme\": \"dark\"\n  \"x\": 1\n}";
        let found = problems_at(broken, false);
        assert_eq!(found.len(), 1);
        assert!(found[0].2);
        assert_eq!(&broken[found[0].0.clone()], "\"x\": 1");
        assert!(problems_at(TEMPLATE, false).is_empty());
    }

    #[test]
    fn array_entries_are_found_around_comments() {
        let text = "// head\n[\n  {\"a\": \"]\"}, // one\n  /* two */ [1, 2],\n  3\n]\n";
        let layout = array_layout(text).unwrap().unwrap();
        let items: Vec<&str> = layout.items.iter().map(|r| &text[r.clone()]).collect();
        assert_eq!(items, ["{\"a\": \"]\"}", "[1, 2]", "3"]);
        assert_eq!(&text[layout.close..layout.close + 1], "]");
        assert!(array_layout("// only\n").unwrap().is_none());
        assert!(array_layout("{}").is_err());
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
    fn language_defaults_beat_global_settings_and_lose_to_the_language_block() {
        let cases = [None, Some(true), Some(false)];
        for global in cases {
            for block in cases {
                let mut s = Settings::default();
                s.editor.word_wrap = global;
                s.editor.trim_trailing_whitespace = global;
                s.editor.format_on_save = global;
                for id in ["markdown", "go"] {
                    let over = s.languages.entry(id.into()).or_default();
                    over.word_wrap = block;
                    over.trim_trailing_whitespace = block;
                    over.format_on_save = block;
                }
                let md = s.editor_for(Some(Lang::Markdown));
                let go = s.editor_for(Some(Lang::Go));
                let rust = s.editor_for(Some(Lang::Rust));
                let case = format!("global {global:?}, block {block:?}");
                assert_eq!(md.word_wrap, block.or(Some(true)), "{case}");
                assert_eq!(md.trim_trailing_whitespace, block.or(Some(false)), "{case}");
                assert_eq!(md.format_on_save, block.or(global), "{case}");
                assert_eq!(go.format_on_save, block.or(Some(true)), "{case}");
                assert_eq!(go.word_wrap, block.or(global), "{case}");
                assert_eq!(rust.format_on_save, global, "{case}");
                assert_eq!(
                    s.language_word_wrap(Some(Lang::Markdown)),
                    block.or(Some(true)),
                    "{case}"
                );
                assert_eq!(s.language_word_wrap(Some(Lang::Go)), block, "{case}");
                assert_eq!(s.editor_for(None), s.editor, "{case}");
            }
        }
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
    fn code_actions_on_save_lightbulb_wrap_and_linked_editing_read_vs_codes_spellings() {
        let (s, problems) = parse(
            r#"{"editor.codeActionsOnSave": {"source.organizeImports": "explicit",
                                             "source.fixAll": "never"},
                "editor.lightbulb.enabled": "onCode",
                "editor.wordWrap": "on",
                "editor.linkedEditing": true,
                "[typescript]": {"editor.codeActionsOnSave": ["source.fixAll"]}}"#,
        )
        .unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(s.editor.organize_imports_on_save, Some(true));
        assert_eq!(s.editor.fix_all_on_save, Some(false));
        assert_eq!(s.editor.lightbulb, Some(Lightbulb::All));
        assert_eq!(s.editor.word_wrap, Some(true));
        assert_eq!(s.editor.linked_editing, Some(true));
        let ts = s.editor_for(Some(Lang::TypeScript));
        assert_eq!(
            (ts.organize_imports_on_save, ts.fix_all_on_save),
            (Some(false), Some(true)),
            "source.fixAll covers ESLint's fix-all"
        );
        let (s, problems) = parse(
            r#"{"editor": {"lightbulb": "quickfix", "codeActionsOnSave": {"source.organizeImports": true}}}"#,
        )
        .unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(s.editor.lightbulb, Some(Lightbulb::QuickFixes));
        assert_eq!(s.editor.organize_imports_on_save, Some(true));
        let (_, problems) =
            parse(r#"{"editor.lightbulb.enabled": "sometimes", "editor.codeActionsOnSave": 1}"#)
                .unwrap();
        assert_eq!(problems.len(), 2, "{problems:?}");
        let go = Settings::default().editor_for(Some(Lang::Go));
        assert_eq!(
            go.organize_imports_on_save,
            Some(true),
            "as VS Code's Go setup"
        );
        assert_eq!(Settings::default().editor.lightbulb, None);
    }

    #[test]
    fn project_settings_beat_the_users_and_language_blocks_beat_both_as_in_vs_code() {
        let (user, _) = parse(
            r#"{"editor": {"tab_size": 8, "format_on_save": false},
                "[go]": {"word_wrap": true},
                "lsp": {"gopls": {"staticcheck": true, "hints": {"parameterNames": true}}}}"#,
        )
        .unwrap();
        let (project, problems) = parse_project(
            r#"{"editor": {"tab_size": 2, "word_wrap": false, "format_on_save": true},
                "[go]": {"format_on_save": false},
                "lsp": {"gopls": {"hints": {"assignVariableTypes": true}}},
                "theme": "dark", "window.zoom_level": 2,
                "claude": {"prices": {"claude-x": {"input": 1, "output": 2}}}}"#,
        )
        .unwrap();
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert_eq!(project.theme, None);
        assert!(project.claude_prices.is_empty());
        let both = user.overlaid(&project);
        assert_eq!(both.editor.tab_size, Some(2));
        let go = both.editor_for(Some(Lang::Go));
        assert_eq!(
            go.word_wrap,
            Some(true),
            "the user's [go] beats the project's editor"
        );
        assert_eq!(
            go.format_on_save,
            Some(false),
            "the project's [go] beats both"
        );
        assert_eq!(both.editor_for(Some(Lang::Rust)).format_on_save, Some(true));
        assert_eq!(
            both.lsp["gopls"],
            json!({"staticcheck": true, "hints": {"parameterNames": true, "assignVariableTypes": true}})
        );
        assert!(project.changes_programs());
        let untrusted = user.overlaid(&project.without_programs());
        assert_eq!(untrusted.lsp, user.lsp, "server settings wait for trust");
        assert_eq!(untrusted.editor.tab_size, Some(2), "editor settings do not");
    }

    #[test]
    fn a_vscode_settings_file_gives_athena_the_keys_it_knows_and_nothing_else() {
        let root = Path::new("/p/app");
        let s = parse_vscode(
            r#"{
              // VS Code's own file, with an extension's keys
              "editor.formatOnSave": true,
              "editor.wordWrap": "off",
              "editor.fontSize": 20,
              "editor.defaultFormatter": "esbenp.prettier-vscode",
              "files.insertFinalNewline": true,
              "[typescriptreact]": {"editor.tabSize": 2, "editor.defaultFormatter": "x"},
              "editor.codeActionsOnSave": {"source.organizeImports": "explicit"},
              "gopls": {"ui.semanticTokens": true},
              "go.toolsEnvVars": {"GOTOOLCHAIN": "auto"},
              "typescript.tsdk": "node_modules/typescript/lib",
              "typescript.preferences.importModuleSpecifier": "relative",
              "prettier.semi": false,
              "editor.tabSize": "wide",
            }"#,
            root,
        );
        assert_eq!(s.editor.format_on_save, Some(true));
        assert_eq!(s.editor.word_wrap, Some(false));
        assert_eq!(s.editor.font_size, None, "app-wide");
        assert_eq!(s.editor.insert_final_newline, Some(true));
        assert_eq!(
            s.editor.tab_size, None,
            "a value Athena cannot use is skipped"
        );
        assert_eq!(s.languages["typescriptreact"].tab_size, Some(2));
        assert_eq!(s.editor.organize_imports_on_save, Some(true));
        assert_eq!(
            s.lsp["gopls"],
            json!({"ui.semanticTokens": true, "env": {"GOTOOLCHAIN": "auto"}})
        );
        let ts = &s.lsp["typescript-language-server"];
        assert_eq!(ts["tsserver"]["path"], "/p/app/node_modules/typescript/lib");
        assert_eq!(
            ts["typescript"]["preferences"]["importModuleSpecifier"],
            "relative"
        );
        assert!(s.changes_programs());
        assert_eq!(parse_vscode("not json", root), Settings::default());
        assert!(!parse_vscode(r#"{"editor.tabSize": 2}"#, root).changes_programs());
    }

    #[test]
    fn semantic_highlighting_asks_gopls_for_tokens_unless_turned_off() {
        let (s, problems) = parse(r#"{"editor.semanticHighlighting.enabled": false}"#).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(s.editor.semantic_highlighting, Some(false));
        assert_eq!(s.server_config("gopls"), Value::Null);
        let (s, _) =
            parse(r#"{"editor": {"semanticHighlighting.enabled": "configuredByTheme"}}"#).unwrap();
        assert_eq!(s.editor.semantic_highlighting, Some(true));
        assert_eq!(
            Settings::default().server_config("gopls"),
            json!({"semanticTokens": true})
        );
        let (mut s, _) = parse(r#"{"lsp": {"gopls": {"semanticTokens": false}}}"#).unwrap();
        assert_eq!(
            s.server_config("gopls"),
            json!({"semanticTokens": false}),
            "the user's own"
        );
        s.languages.insert(
            "go".into(),
            EditorSettings {
                semantic_highlighting: Some(false),
                ..EditorSettings::default()
            },
        );
        assert_eq!(
            s.editor_for(Some(Lang::Go)).semantic_highlighting,
            Some(false)
        );
        assert_eq!(
            Settings::default().server_config("typescript-language-server"),
            Value::Null
        );
    }

    #[test]
    fn code_lens_reads_vs_codes_key_and_a_language_block_beats_it() {
        let (s, problems) =
            parse(r#"{"editor.codeLens": false, "[go]": {"editor.codeLens": true}}"#).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(s.editor.code_lens, Some(false));
        assert_eq!(s.editor_for(Some(Lang::Go)).code_lens, Some(true));
        assert_eq!(s.editor_for(Some(Lang::Rust)).code_lens, Some(false));
        let vscode = parse_vscode(
            r#"{"editor.codeLens": false, "typescript.referencesCodeLens.enabled": true}"#,
            Path::new("/p"),
        );
        assert_eq!(vscode.editor.code_lens, Some(false));
        assert_eq!(
            vscode.lsp["typescript-language-server"]["typescript"]["referencesCodeLens"]["enabled"],
            json!(true),
            "typescript-language-server reads its lens settings from there"
        );
    }

    #[test]
    fn inlay_hints_add_curated_server_settings_unless_the_user_chose_some() {
        let (mut s, _) = parse(r#"{"lsp": {"gopls": {"staticcheck": true}}}"#).unwrap();
        assert_eq!(
            s.server_config("gopls"),
            json!({"staticcheck": true, "semanticTokens": true})
        );
        s.editor.inlay_hints = Some(true);
        let go = s.server_config("gopls");
        assert_eq!(go["staticcheck"], json!(true));
        assert_eq!(go["hints"]["parameterNames"], json!(true));
        s.lsp
            .insert("gopls".into(), json!({"hints": {"constantValues": true}}));
        assert_eq!(
            s.server_config("gopls"),
            json!({"hints": {"constantValues": true}, "semanticTokens": true})
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
