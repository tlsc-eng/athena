use athena_workspace::ThemeChoice;
use serde_json::{Map, Value, json};

use super::{Lightbulb, Settings};

/// Where a setting is listed in the Settings tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Editor,
    Workbench,
    Explorer,
    Git,
    Claude,
    LanguageServers,
}

impl Group {
    pub fn title(self) -> &'static str {
        match self {
            Self::Editor => "Editor",
            Self::Workbench => "Workbench",
            Self::Explorer => "Explorer",
            Self::Git => "Git",
            Self::Claude => "Claude",
            Self::LanguageServers => "Language Servers",
        }
    }
}

/// What a setting holds, which picks its control and its JSON Schema.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Toggle {
        default: bool,
    },
    Whole {
        min: i64,
        max: Option<i64>,
        default: i64,
    },
    Number {
        min: f64,
        max: f64,
        default: f64,
    },
    /// One of `options`, each a value and how the dropdown shows it.
    Choice {
        options: &'static [(&'static str, &'static str)],
        default: &'static str,
    },
    /// Structured JSON, edited in settings.json; `schema` describes it and `example` is valid.
    Json {
        schema: &'static str,
        example: &'static str,
    },
}

pub struct Setting {
    /// Where it sits in settings.json, as the writer takes it.
    pub keys: &'static [&'static str],
    /// VS Code's spellings, dotted from the top of the file, which Athena reads too.
    pub aliases: &'static [&'static str],
    pub title: &'static str,
    pub description: &'static str,
    pub group: Group,
    pub kind: Kind,
    /// Applies from the user's settings.json only; a project's file may not set it.
    pub user_only: bool,
    /// Other values Athena accepts, as a JSON Schema, such as VS Code's spellings.
    pub also: Option<&'static str>,
}

impl Setting {
    /// The dotted name, as search and the JSON file show it.
    pub fn id(&self) -> String {
        self.keys.join(".")
    }

    pub fn default_value(&self) -> Value {
        match self.kind {
            Kind::Toggle { default } => json!(default),
            Kind::Whole { default, .. } => json!(default),
            Kind::Number { default, .. } => whole_or_fraction(default),
            Kind::Choice { default, .. } => json!(default),
            Kind::Json { .. } => Value::Null,
        }
    }

    /// The value `s` has for this setting, as the control shows it; `None` when it is not set.
    pub fn value_in(&self, s: &Settings) -> Option<Value> {
        let e = &s.editor;
        let flag = |b: Option<bool>| b.map(Value::from);
        match self.keys {
            ["editor", "format_on_save"] => flag(e.format_on_save),
            ["editor", "trim_trailing_whitespace"] => flag(e.trim_trailing_whitespace),
            ["editor", "insert_final_newline"] => flag(e.insert_final_newline),
            ["editor", "word_wrap"] => flag(e.word_wrap),
            ["editor", "font_size"] => e.font_size.map(|f| whole_or_fraction(f as f64)),
            ["editor", "tab_size"] => e.tab_size.map(Value::from),
            ["editor", "autosave_delay_ms"] => e.autosave_delay_ms.map(Value::from),
            ["editor", "inlay_hints"] => flag(e.inlay_hints),
            ["editor", "bracket_pair_colorization"] => flag(e.bracket_pair_colorization),
            ["editor", "lightbulb"] => e.lightbulb.map(|l| {
                json!(match l {
                    Lightbulb::Off => "off",
                    Lightbulb::QuickFixes => "quickfix",
                    Lightbulb::All => "all",
                })
            }),
            ["editor", "linked_editing"] => flag(e.linked_editing),
            ["editor", "minimap"] => flag(e.minimap),
            ["editor", "semantic_highlighting"] => flag(e.semantic_highlighting),
            ["editor", "code_lens"] => flag(e.code_lens),
            ["editor", "code_actions_on_save"] => {
                let on = |b: Option<bool>| match b? {
                    true => Some(json!("explicit")),
                    false => Some(json!("never")),
                };
                let kinds: Map<String, Value> = [
                    ("source.organizeImports", on(e.organize_imports_on_save)),
                    ("source.fixAll.eslint", on(e.fix_all_on_save)),
                ]
                .into_iter()
                .filter_map(|(k, v)| Some((k.to_string(), v?)))
                .collect();
                (!kinds.is_empty()).then_some(Value::Object(kinds))
            }
            ["theme"] => s.theme.map(|t| {
                json!(match t {
                    ThemeChoice::System => "system",
                    ThemeChoice::Light => "light",
                    ThemeChoice::Dark => "dark",
                })
            }),
            ["ide_integration"] => flag(s.ide_integration),
            ["git", "autofetch"] => flag(s.autofetch),
            ["explorer", "confirmDragAndDrop"] => flag(s.confirm_drag_and_drop),
            ["window", "zoom_level"] => s.zoom_level.map(Value::from),
            ["eslint", "fixOnSave"] => flag(s.eslint_fix_on_save),
            ["claude", "prices"] => (!s.claude_prices.is_empty()).then(|| {
                let prices: Map<String, Value> = s
                    .claude_prices
                    .iter()
                    .map(|(model, p)| {
                        let price = json!({
                            "input": p.input,
                            "output": p.output,
                            "cache_write": p.cache_write_5m,
                            "cache_write_1h": p.cache_write_1h,
                            "cache_read": p.cache_read,
                        });
                        (model.clone(), price)
                    })
                    .collect();
                Value::Object(prices)
            }),
            ["lsp"] => (!s.lsp.is_empty()).then(|| {
                Value::Object(s.lsp.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            }),
            _ => None,
        }
    }

    /// `text` read as a value of this setting, as a number field takes it.
    pub fn parse_input(&self, text: &str) -> Result<Value, String> {
        let text = text.trim();
        match self.kind {
            Kind::Whole { min, max, .. } => {
                let n: i64 = text.parse().map_err(|_| "Enter a whole number.".to_string())?;
                let too_big = max.is_some_and(|max| n > max);
                if n < min || too_big {
                    return Err(match max {
                        Some(max) => format!("Enter a whole number from {min} to {max}."),
                        None => format!("Enter a whole number of at least {min}."),
                    });
                }
                Ok(json!(n))
            }
            Kind::Number { min, max, .. } => {
                let n: f64 = text.parse().map_err(|_| "Enter a number.".to_string())?;
                if !(min..=max).contains(&n) {
                    return Err(format!("Enter a number from {min} to {max}."));
                }
                Ok(whole_or_fraction(n))
            }
            _ => Err("This setting is not typed in.".into()),
        }
    }

    fn value_schema(&self) -> Value {
        let mut schema = match self.kind {
            Kind::Toggle { default } => json!({"type": "boolean", "default": default}),
            Kind::Whole { min, max, default } => {
                let mut s = json!({"type": "integer", "minimum": min, "default": default});
                if let Some(max) = max {
                    s["maximum"] = json!(max);
                }
                s
            }
            Kind::Number { min, max, default } => {
                json!({"type": "number", "minimum": min, "maximum": max, "default": default})
            }
            Kind::Choice { options, default } => json!({
                "type": "string",
                "enum": options.iter().map(|(v, _)| *v).collect::<Vec<_>>(),
                "enumDescriptions": options.iter().map(|(_, l)| *l).collect::<Vec<_>>(),
                "default": default,
            }),
            Kind::Json { schema, .. } => serde_json::from_str(schema).expect("valid schema"),
        };
        if let Some(also) = self.also {
            let also: Value = serde_json::from_str(also).expect("valid schema");
            schema = json!({"anyOf": [schema, also]});
        }
        let mut description = self.description.to_string();
        if self.user_only {
            description.push_str(" Applies from the user's settings.json only.");
        }
        schema["description"] = json!(description);
        schema
    }
}

/// A number written as JSON shows it: `13`, not `13.0`.
fn whole_or_fraction(n: f64) -> Value {
    match n.fract() == 0. {
        true => json!(n as i64),
        false => json!(n),
    }
}

const TOGGLE_OFF: Kind = Kind::Toggle { default: false };
const TOGGLE_ON: Kind = Kind::Toggle { default: true };

/// Every setting settings.json takes, in the order the Settings tab lists them.
pub static SETTINGS: &[Setting] = &[
    Setting {
        keys: &["editor", "format_on_save"],
        aliases: &["editor.formatOnSave"],
        title: "Format On Save",
        description: "⌘S formats the file through its language server before saving. Go files \
                      format on save unless their language block turns it off.",
        group: Group::Editor,
        kind: TOGGLE_OFF,
        user_only: false,
        also: None,
    },
    Setting {
        keys: &["editor", "trim_trailing_whitespace"],
        aliases: &["files.trimTrailingWhitespace"],
        title: "Trim Trailing Whitespace",
        description: "Saving removes spaces and tabs at the ends of lines. Markdown keeps them, \
                      as they can mean a line break.",
        group: Group::Editor,
        kind: TOGGLE_OFF,
        user_only: false,
        also: None,
    },
    Setting {
        keys: &["editor", "insert_final_newline"],
        aliases: &["files.insertFinalNewline"],
        title: "Insert Final Newline",
        description: "Saving ends the file with a line break.",
        group: Group::Editor,
        kind: TOGGLE_OFF,
        user_only: false,
        also: None,
    },
    Setting {
        keys: &["editor", "word_wrap"],
        aliases: &["editor.wordWrap"],
        title: "Word Wrap",
        description: "Long lines wrap at the editor's edge, for tabs that have not chosen with ⌥Z.",
        group: Group::Editor,
        kind: TOGGLE_OFF,
        user_only: false,
        also: Some(r#"{"enum": ["on", "off", "wordWrapColumn", "bounded"]}"#),
    },
    Setting {
        keys: &["editor", "font_size"],
        aliases: &["editor.fontSize"],
        title: "Font Size",
        description: "Editor and terminal text size in points; ⌘= and ⌘- change it too.",
        group: Group::Editor,
        kind: Kind::Number {
            min: 6.,
            max: 40.,
            default: 13.,
        },
        user_only: true,
        also: None,
    },
    Setting {
        keys: &["editor", "tab_size"],
        aliases: &["editor.tabSize"],
        title: "Tab Size",
        description: "Spaces per indent, for files whose indentation cannot be detected.",
        group: Group::Editor,
        kind: Kind::Whole {
            min: 1,
            max: Some(16),
            default: 4,
        },
        user_only: false,
        also: None,
    },
    Setting {
        keys: &["editor", "autosave_delay_ms"],
        aliases: &["files.autoSaveDelay"],
        title: "Auto Save Delay",
        description: "Milliseconds after the last edit before the file saves itself; 0 turns \
                      auto save off.",
        group: Group::Editor,
        kind: Kind::Whole {
            min: 0,
            max: None,
            default: 1000,
        },
        user_only: false,
        also: None,
    },
    Setting {
        keys: &["editor", "inlay_hints"],
        aliases: &[],
        title: "Inlay Hints",
        description: "Shows parameter names and inferred types in the code, with a curated set \
                      turned on for each language server; off hides the hints servers send.",
        group: Group::Editor,
        kind: TOGGLE_OFF,
        user_only: false,
        also: None,
    },
    Setting {
        keys: &["editor", "bracket_pair_colorization"],
        aliases: &["editor.bracketPairColorization.enabled"],
        title: "Bracket Pair Colorization",
        description: "Colours brackets by how deeply they nest.",
        group: Group::Editor,
        kind: TOGGLE_ON,
        user_only: false,
        also: None,
    },
    Setting {
        keys: &["editor", "lightbulb"],
        aliases: &["editor.lightbulb.enabled"],
        title: "Lightbulb",
        description: "Which code actions put a lightbulb beside the cursor's line; ⌘. lists \
                      them all either way.",
        group: Group::Editor,
        kind: Kind::Choice {
            options: &[
                ("quickfix", "Quick fixes"),
                ("all", "Quick fixes and refactorings"),
                ("off", "Off"),
            ],
            default: "quickfix",
        },
        user_only: false,
        also: Some(r#"{"enum": ["onCode", "on", true, false]}"#),
    },
    Setting {
        keys: &["editor", "linked_editing"],
        aliases: &["editor.linkedEditing"],
        title: "Linked Editing",
        description: "Typing in an HTML or JSX tag name renames its matching tag.",
        group: Group::Editor,
        kind: TOGGLE_OFF,
        user_only: false,
        also: None,
    },
    Setting {
        keys: &["editor", "minimap"],
        aliases: &["editor.minimap.enabled"],
        title: "Minimap",
        description: "Shows an overview of the file at the editor's right edge.",
        group: Group::Editor,
        kind: TOGGLE_ON,
        user_only: false,
        also: Some(r#"{"type": "object", "properties": {"enabled": {"type": "boolean"}}}"#),
    },
    Setting {
        keys: &["editor", "semantic_highlighting"],
        aliases: &["editor.semanticHighlighting.enabled"],
        title: "Semantic Highlighting",
        description: "Colours names as the language server classifies them.",
        group: Group::Editor,
        kind: TOGGLE_ON,
        user_only: false,
        also: Some(r#"{"enum": ["configuredByTheme"]}"#),
    },
    Setting {
        keys: &["editor", "code_lens"],
        aliases: &["editor.codeLens"],
        title: "Code Lens",
        description: "Shows the commands language servers offer above lines, such as reference \
                      counts and \"run go generate\".",
        group: Group::Editor,
        kind: TOGGLE_ON,
        user_only: false,
        also: None,
    },
    Setting {
        keys: &["editor", "code_actions_on_save"],
        aliases: &["editor.codeActionsOnSave"],
        title: "Code Actions On Save",
        description: "Code actions ⌘S runs first: \"source.organizeImports\" and \
                      \"source.fixAll.eslint\", set to \"explicit\" or \"never\".",
        group: Group::Editor,
        kind: Kind::Json {
            schema: r#"{"anyOf": [
                {"type": "object", "additionalProperties": {"enum": [true, false, "explicit", "always", "never"]}},
                {"type": "array", "items": {"type": "string"}}
            ]}"#,
            example: r#"{"source.organizeImports": "explicit"}"#,
        },
        user_only: false,
        also: None,
    },
    Setting {
        keys: &["theme"],
        aliases: &[],
        title: "Color Theme",
        description: "Light, dark, or following macOS's appearance.",
        group: Group::Workbench,
        kind: Kind::Choice {
            options: &[
                ("system", "Follow system appearance"),
                ("light", "Light"),
                ("dark", "Dark"),
            ],
            default: "system",
        },
        user_only: true,
        also: None,
    },
    Setting {
        keys: &["window", "zoom_level"],
        aliases: &[],
        title: "Zoom Level",
        description: "Interface text and spacing in 10% steps from the default; ⌘⌥= and ⌘⌥- \
                      change it too.",
        group: Group::Workbench,
        kind: Kind::Whole {
            min: -3,
            max: Some(5),
            default: 0,
        },
        user_only: true,
        also: None,
    },
    Setting {
        keys: &["explorer", "confirmDragAndDrop"],
        aliases: &[],
        title: "Confirm Drag And Drop",
        description: "Asks before a file dragged in the tree moves.",
        group: Group::Explorer,
        kind: TOGGLE_ON,
        user_only: true,
        also: None,
    },
    Setting {
        keys: &["git", "autofetch"],
        aliases: &[],
        title: "Autofetch",
        description: "Fetches the active project's remotes every three minutes.",
        group: Group::Git,
        kind: TOGGLE_OFF,
        user_only: true,
        also: None,
    },
    Setting {
        keys: &["ide_integration"],
        aliases: &[],
        title: "Claude Code IDE Integration",
        description: "Claude Code connects to Athena as its IDE: it sees the open file and \
                      selection, and shows its edits as diffs here.",
        group: Group::Claude,
        kind: TOGGLE_OFF,
        user_only: true,
        also: None,
    },
    Setting {
        keys: &["claude", "prices"],
        aliases: &[],
        title: "Prices",
        description: "USD per million tokens by model id, over the built-in list prices the \
                      Claude tab estimates costs with.",
        group: Group::Claude,
        kind: Kind::Json {
            schema: r#"{"type": "object", "additionalProperties": {
                "type": "object",
                "required": ["input", "output"],
                "properties": {
                    "input": {"type": "number", "minimum": 0},
                    "output": {"type": "number", "minimum": 0},
                    "cache_write": {"type": "number", "minimum": 0},
                    "cache_write_1h": {"type": "number", "minimum": 0},
                    "cache_read": {"type": "number", "minimum": 0}
                }
            }}"#,
            example: r#"{"claude-sonnet-5": {"input": 3, "output": 15}}"#,
        },
        user_only: true,
        also: None,
    },
    Setting {
        keys: &["eslint", "fixOnSave"],
        aliases: &[],
        title: "ESLint: Fix On Save",
        description: "⌘S applies ESLint's fixes first; needs the project's own ESLint server.",
        group: Group::LanguageServers,
        kind: TOGGLE_OFF,
        user_only: false,
        also: None,
    },
    Setting {
        keys: &["lsp"],
        aliases: &[],
        title: "Server Settings",
        description: "Each language server's settings by program name, sent as it starts and \
                      whenever they change. A project's take effect once it is trusted.",
        group: Group::LanguageServers,
        kind: Kind::Json {
            schema: r#"{"type": "object", "additionalProperties": {"type": "object"}}"#,
            example: r#"{"gopls": {"staticcheck": true}}"#,
        },
        user_only: false,
        also: None,
    },
];

/// The setting `keys` name, whichever spelling they use.
pub fn find(keys: &[&str]) -> Option<&'static Setting> {
    let name = super::setting_name(None, &keys.join("."));
    SETTINGS
        .iter()
        .find(|s| super::setting_name(None, &s.id()) == name)
}

/// The JSON Schema of settings.json, generated from [`SETTINGS`] for a JSON language server.
pub fn json_schema() -> Value {
    let mut top = Map::new();
    let mut blocks: Map<String, Value> = Map::new();
    let mut editor = Map::new();
    for s in SETTINGS {
        let schema = s.value_schema();
        top.insert(s.id(), schema.clone());
        for alias in s.aliases {
            top.insert(alias.to_string(), schema.clone());
        }
        if let [block, rest @ ..] = s.keys
            && !rest.is_empty()
        {
            let inner = blocks
                .entry(block.to_string())
                .or_insert_with(|| json!({"type": "object", "properties": {}}));
            inner["properties"][rest.join(".")] = schema.clone();
            if *block == "editor" {
                editor.insert(rest.join("."), schema.clone());
                for alias in s.aliases {
                    let bare = alias.split_once('.').map_or(*alias, |(_, b)| b);
                    inner["properties"][bare] = schema.clone();
                    editor.insert(bare.to_string(), schema.clone());
                }
            }
        }
    }
    top.extend(blocks);
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "Athena settings",
        "type": "object",
        "allowComments": true,
        "allowTrailingCommas": true,
        "properties": top,
        "patternProperties": {
            r"^\[.+\]$": {
                "type": "object",
                "description": "Editor settings for one language, by VS Code's language id.",
                "properties": editor,
            }
        },
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use athena_editor::Lang;
    use athena_workspace::Workspace;

    use super::super::{TEMPLATE, parse, parse_project, setting_name};
    use super::*;

    fn example(s: &Setting) -> Value {
        match s.kind {
            Kind::Json { example, .. } => serde_json::from_str(example).unwrap(),
            _ => s.default_value(),
        }
    }

    fn with(s: &Setting, value: &Value) -> String {
        let nested = s
            .keys
            .iter()
            .rev()
            .fold(value.clone(), |inner, key| json!({ *key: inner }));
        nested.to_string()
    }

    /// The template with its example settings uncommented, as a reader would.
    fn uncommented() -> String {
        TEMPLATE
            .lines()
            .map(|l| match l.trim_start().strip_prefix("// ") {
                Some(rest) if l.starts_with("  ") => format!("  {rest}\n"),
                _ => format!("{l}\n"),
            })
            .collect()
    }

    /// The canonical names of the settings a settings.json sets.
    fn names_in(text: &str) -> BTreeSet<String> {
        let json = crate::keymap::strip_jsonc(text);
        let Value::Object(root) = serde_json::from_str(&json).unwrap() else {
            panic!("not an object");
        };
        let mut out = BTreeSet::new();
        for (key, value) in &root {
            if key.starts_with('[') {
                continue;
            }
            let nested = SETTINGS.iter().any(|s| s.keys.len() > 1 && s.keys[0] == key);
            match value.as_object() {
                Some(inner) if nested => {
                    out.extend(inner.keys().map(|k| setting_name(Some(key), k)));
                }
                _ => {
                    out.insert(setting_name(None, key));
                }
            }
        }
        out
    }

    #[test]
    fn every_setting_is_in_the_template_and_every_template_key_is_a_setting() {
        let documented = names_in(&uncommented());
        let known: BTreeSet<String> = SETTINGS
            .iter()
            .map(|s| setting_name(None, &s.id()))
            .collect();
        assert_eq!(documented, known);
    }

    #[test]
    fn every_setting_and_alias_parses_and_reads_back() {
        for s in SETTINGS {
            let value = example(s);
            let (parsed, problems) = parse(&with(s, &value)).unwrap();
            assert!(problems.is_empty(), "{}: {problems:?}", s.id());
            assert!(s.value_in(&parsed).is_some(), "{} reads back", s.id());
            if !matches!(s.kind, Kind::Json { .. }) {
                assert_eq!(s.value_in(&parsed), Some(value.clone()), "{}", s.id());
            }
            for alias in s.aliases {
                let text = json!({ *alias: value }).to_string();
                let (parsed, problems) = parse(&text).unwrap();
                assert!(problems.is_empty(), "{alias}: {problems:?}");
                assert!(s.value_in(&parsed).is_some(), "{alias}");
                assert_eq!(find(&[alias]).map(Setting::id), Some(s.id()), "{alias}");
            }
            assert_eq!(s.value_in(&Settings::default()), None, "{}", s.id());
        }
    }

    #[test]
    fn choices_and_ranges_match_what_parse_accepts() {
        for s in SETTINGS {
            match s.kind {
                Kind::Choice { options, default } => {
                    assert!(options.iter().any(|(v, _)| *v == default), "{}", s.id());
                    for (value, _) in options {
                        let (_, problems) = parse(&with(s, &json!(value))).unwrap();
                        assert!(problems.is_empty(), "{} = {value}", s.id());
                    }
                    let (_, problems) = parse(&with(s, &json!("nonsense"))).unwrap();
                    assert_eq!(problems.len(), 1, "{}", s.id());
                }
                Kind::Whole { min, max, .. } => {
                    let below = parse(&with(s, &json!(min - 1))).unwrap().1;
                    assert_eq!(below.len(), 1, "{} below {min}", s.id());
                    assert!(s.parse_input(&(min - 1).to_string()).is_err());
                    assert_eq!(s.parse_input(&min.to_string()), Ok(json!(min)));
                    if let Some(max) = max {
                        assert_eq!(parse(&with(s, &json!(max + 1))).unwrap().1.len(), 1);
                        assert!(s.parse_input(&(max + 1).to_string()).is_err());
                        assert!(parse(&with(s, &json!(max))).unwrap().1.is_empty());
                    }
                }
                Kind::Number { min, max, .. } => {
                    assert_eq!(parse(&with(s, &json!(min - 1.))).unwrap().1.len(), 1);
                    assert_eq!(parse(&with(s, &json!(max + 1.))).unwrap().1.len(), 1);
                    assert_eq!(s.parse_input("13.5"), Ok(json!(13.5)));
                    assert_eq!(s.parse_input(" 14 "), Ok(json!(14)));
                    assert!(s.parse_input("big").is_err());
                }
                Kind::Toggle { .. } | Kind::Json { .. } => {}
            }
        }
    }

    #[test]
    fn user_only_settings_are_the_ones_a_project_file_refuses() {
        for s in SETTINGS {
            let (_, problems) = parse_project(&with(s, &example(s))).unwrap();
            assert_eq!(problems.len(), usize::from(s.user_only), "{}", s.id());
        }
    }

    /// What Athena does with `s`, through the same fallbacks the shell applies.
    fn behaviour(s: &Settings) -> Vec<String> {
        let prefs = s.over(Workspace::default().preferences());
        let e = s.editor_for(Some(Lang::Rust));
        let font = e.font_size.unwrap_or(athena_ui::CODE_SIZE);
        vec![
            format!("format {}", e.format_on_save.or(prefs.format_on_save) == Some(true)),
            format!("trim {}", e.trim_trailing_whitespace == Some(true)),
            format!("final newline {}", e.insert_final_newline == Some(true)),
            format!("wrap {}", e.word_wrap.unwrap_or(prefs.word_wrap)),
            format!("font {font}"),
            format!("tab {}", e.tab_size.unwrap_or(4)),
            format!("autosave {}", prefs.autosave_delay_ms),
            format!("inlay curated {}", s.server_config("gopls").get("hints").is_some()),
            format!("brackets {}", e.bracket_pair_colorization != Some(false)),
            format!("lightbulb {:?}", e.lightbulb.unwrap_or(Lightbulb::QuickFixes)),
            format!("linked {}", e.linked_editing == Some(true)),
            format!("minimap {}", e.minimap != Some(false)),
            format!("semantic {}", e.semantic_highlighting != Some(false)),
            format!("lens {}", e.code_lens != Some(false)),
            format!("theme {:?}", prefs.theme),
            format!("zoom {}", s.zoom_level.unwrap_or(0)),
            format!("drag {}", s.confirm_drag_and_drop()),
            format!("fetch {}", s.git_autofetch()),
            format!("ide {}", prefs.ide_integration),
            format!("eslint {}", s.eslint_fix_on_save()),
        ]
    }

    #[test]
    fn each_default_is_what_athena_does_without_the_setting() {
        let unset = behaviour(&Settings::default());
        for s in SETTINGS {
            if matches!(s.kind, Kind::Json { .. }) {
                continue;
            }
            let (set, problems) = parse(&with(s, &s.default_value())).unwrap();
            assert!(problems.is_empty(), "{}: {problems:?}", s.id());
            assert_eq!(behaviour(&set), unset, "{} = its default", s.id());
        }
    }

    #[test]
    fn the_json_schema_describes_every_spelling_and_language_blocks() {
        let schema = json_schema();
        let props = &schema["properties"];
        for s in SETTINGS {
            assert!(props.get(s.id()).is_some(), "{}", s.id());
            for alias in s.aliases {
                assert!(props.get(*alias).is_some(), "{alias}");
            }
        }
        assert_eq!(props["editor"]["properties"]["tab_size"]["maximum"], 16);
        assert_eq!(props["editor"]["properties"]["tabSize"]["type"], "integer");
        assert_eq!(props["theme"]["enum"], json!(["system", "light", "dark"]));
        assert!(props["editor.word_wrap"]["anyOf"].is_array());
        let lang = &schema["patternProperties"][r"^\[.+\]$"]["properties"];
        assert_eq!(lang["format_on_save"]["type"], "boolean");
        assert!(
            props["theme"]["description"]
                .as_str()
                .unwrap()
                .ends_with("user's settings.json only.")
        );
    }
}
