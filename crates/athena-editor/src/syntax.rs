use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;

use ropey::Rope;
use streaming_iterator::StreamingIterator;
use tree_sitter::{InputEdit, Language, Node, Parser, Query, QueryCursor, TextProvider, Tree};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Go,
    TypeScript,
    Tsx,
    JavaScript,
    Yaml,
    Json,
    Toml,
    Shell,
    Rust,
    Python,
    Css,
    Html,
    Markdown,
    Swift,
    Dockerfile,
    DotEnv,
}

/// Highlight classes; the theme gives each a colour, and headings a heavier weight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    Keyword,
    Function,
    Type,
    String,
    StringSpecial,
    Number,
    Constant,
    Comment,
    Punctuation,
    PunctuationSpecial,
    Property,
    Variable,
    VariableBuiltin,
    Operator,
    Attribute,
    Tag,
    Namespace,
    Escape,
    Embedded,
    Label,
    Heading,
    Link,
    Error,
}

/// Highlights one line without a parse tree; ranges are byte offsets within the line.
pub type LineScanner = fn(&str) -> Vec<(Range<usize>, Token)>;

enum Engine {
    Grammar {
        language: fn() -> Language,
        query: fn() -> String,
    },
    Lines(LineScanner),
}

struct LangSpec {
    extensions: &'static [&'static str],
    /// Exact file names; a trailing `*` matches any suffix.
    filenames: &'static [&'static str],
    engine: Engine,
    comment: Option<&'static str>,
    /// Which of several patterns capturing one node wins; the bundled queries disagree.
    first_pattern_wins: bool,
}

const JS: &str = tree_sitter_javascript::HIGHLIGHT_QUERY;
const JSX: &str = tree_sitter_javascript::JSX_HIGHLIGHT_QUERY;
const TS: &str = tree_sitter_typescript::HIGHLIGHTS_QUERY;

/// JSON's query leaves punctuation plain.
const JSON_EXTRA: &str = r#"
["{" "}" "[" "]"] @punctuation.bracket
[":" ","] @punctuation.delimiter
"#;

/// TOML's query captures the whole pair as a property and leaves the key a type.
const TOML_EXTRA: &str = r#"
(pair (bare_key) @property)
(pair (quoted_key) @property)
(pair (dotted_key (bare_key) @property))
"#;

const fn grammar(language: fn() -> Language, query: fn() -> String) -> Engine {
    Engine::Grammar { language, query }
}

const SPECS: [LangSpec; Lang::COUNT] = [
    LangSpec {
        extensions: &["go"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_go::LANGUAGE.into(),
            || tree_sitter_go::HIGHLIGHTS_QUERY.into(),
        ),
        comment: Some("// "),
        first_pattern_wins: true,
    },
    LangSpec {
        extensions: &["ts", "mts", "cts"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            || format!("{JS}\n{TS}"),
        ),
        comment: Some("// "),
        first_pattern_wins: false,
    },
    LangSpec {
        extensions: &["tsx"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_typescript::LANGUAGE_TSX.into(),
            || format!("{JS}\n{JSX}\n{TS}"),
        ),
        comment: Some("// "),
        first_pattern_wins: false,
    },
    LangSpec {
        extensions: &["js", "mjs", "cjs", "jsx"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_javascript::LANGUAGE.into(),
            || format!("{JS}\n{JSX}"),
        ),
        comment: Some("// "),
        first_pattern_wins: false,
    },
    LangSpec {
        extensions: &["yaml", "yml"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_yaml::LANGUAGE.into(),
            || tree_sitter_yaml::HIGHLIGHTS_QUERY.into(),
        ),
        comment: Some("# "),
        first_pattern_wins: false,
    },
    LangSpec {
        extensions: &["json", "jsonc", "json5"],
        filenames: &[".prettierrc", ".eslintrc", ".babelrc"],
        engine: grammar(
            || tree_sitter_json::LANGUAGE.into(),
            || format!("{}\n{JSON_EXTRA}", tree_sitter_json::HIGHLIGHTS_QUERY),
        ),
        comment: None,
        first_pattern_wins: true,
    },
    LangSpec {
        extensions: &["toml"],
        filenames: &["Cargo.lock", "uv.lock", "poetry.lock"],
        engine: grammar(
            || tree_sitter_toml_ng::LANGUAGE.into(),
            || format!("{}\n{TOML_EXTRA}", tree_sitter_toml_ng::HIGHLIGHTS_QUERY),
        ),
        comment: Some("# "),
        first_pattern_wins: false,
    },
    LangSpec {
        extensions: &["sh", "bash", "zsh"],
        filenames: &[
            ".bashrc",
            ".bash_profile",
            ".bash_aliases",
            ".profile",
            ".zshrc",
            ".zshenv",
            ".zprofile",
            ".zlogin",
            ".envrc",
        ],
        engine: grammar(
            || tree_sitter_bash::LANGUAGE.into(),
            || tree_sitter_bash::HIGHLIGHT_QUERY.into(),
        ),
        comment: Some("# "),
        first_pattern_wins: false,
    },
    LangSpec {
        extensions: &["rs"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_rust::LANGUAGE.into(),
            || tree_sitter_rust::HIGHLIGHTS_QUERY.into(),
        ),
        comment: Some("// "),
        first_pattern_wins: true,
    },
    LangSpec {
        extensions: &["py", "pyi"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_python::LANGUAGE.into(),
            || tree_sitter_python::HIGHLIGHTS_QUERY.into(),
        ),
        comment: Some("# "),
        first_pattern_wins: false,
    },
    LangSpec {
        extensions: &["css"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_css::LANGUAGE.into(),
            || tree_sitter_css::HIGHLIGHTS_QUERY.into(),
        ),
        comment: None,
        first_pattern_wins: true,
    },
    LangSpec {
        extensions: &["html", "htm"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_html::LANGUAGE.into(),
            || tree_sitter_html::HIGHLIGHTS_QUERY.into(),
        ),
        comment: None,
        first_pattern_wins: false,
    },
    LangSpec {
        extensions: &["md", "markdown"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_md::LANGUAGE.into(),
            || tree_sitter_md::HIGHLIGHT_QUERY_BLOCK.into(),
        ),
        comment: None,
        first_pattern_wins: false,
    },
    LangSpec {
        extensions: &["swift"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_swift::LANGUAGE.into(),
            || tree_sitter_swift::HIGHLIGHTS_QUERY.into(),
        ),
        comment: Some("// "),
        first_pattern_wins: false,
    },
    LangSpec {
        extensions: &["dockerfile", "containerfile"],
        filenames: &["Dockerfile*", "Containerfile*"],
        engine: Engine::Lines(crate::lines::dockerfile),
        comment: Some("# "),
        first_pattern_wins: false,
    },
    LangSpec {
        extensions: &["env"],
        filenames: &[".env", ".env.*"],
        engine: Engine::Lines(crate::lines::dotenv),
        comment: Some("# "),
        first_pattern_wins: false,
    },
];

impl Lang {
    const COUNT: usize = 16;

    pub const ALL: [Lang; Self::COUNT] = [
        Self::Go,
        Self::TypeScript,
        Self::Tsx,
        Self::JavaScript,
        Self::Yaml,
        Self::Json,
        Self::Toml,
        Self::Shell,
        Self::Rust,
        Self::Python,
        Self::Css,
        Self::Html,
        Self::Markdown,
        Self::Swift,
        Self::Dockerfile,
        Self::DotEnv,
    ];

    fn spec(self) -> &'static LangSpec {
        &SPECS[self as usize]
    }

    /// File names are checked before extensions: `.env.local` is env, not a `local` file.
    pub fn for_path(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        let by_name = Self::ALL.into_iter().find(|lang| {
            lang.spec()
                .filenames
                .iter()
                .any(|pattern| match pattern.strip_suffix('*') {
                    Some(prefix) => name.starts_with(prefix),
                    None => name == *pattern,
                })
        });
        by_name.or_else(|| {
            let ext = path.extension()?.to_str()?.to_ascii_lowercase();
            Self::ALL
                .into_iter()
                .find(|lang| lang.spec().extensions.contains(&ext.as_str()))
        })
    }

    /// Picks a language from a `#!` line, for scripts without an extension.
    pub fn for_shebang(first_line: &str) -> Option<Self> {
        let rest = first_line.strip_prefix("#!")?;
        let mut words = rest.split_whitespace();
        let mut program = words.next()?.rsplit('/').next()?;
        if program == "env" {
            program = words.find(|w| !w.starts_with('-'))?;
        }
        let program = program.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
        match program {
            "sh" | "bash" | "zsh" | "dash" | "ksh" => Some(Self::Shell),
            "python" => Some(Self::Python),
            "node" | "deno" | "bun" => Some(Self::JavaScript),
            _ => None,
        }
    }

    pub fn comment_prefix(self) -> Option<&'static str> {
        self.spec().comment
    }

    /// Compiled once per language.
    fn query(self) -> Option<&'static Query> {
        static QUERIES: [OnceLock<Option<Query>>; Lang::COUNT] =
            [const { OnceLock::new() }; Lang::COUNT];
        QUERIES[self as usize]
            .get_or_init(|| match &self.spec().engine {
                Engine::Grammar { language, query } => Some(
                    Query::new(&language(), &query()).expect("bundled highlight query compiles"),
                ),
                Engine::Lines(_) => None,
            })
            .as_ref()
    }
}

/// Capture name prefixes and their token; the longest prefix on a `.` boundary wins.
const CAPTURES: &[(&str, Option<Token>)] = &[
    ("keyword", Some(Token::Keyword)),
    ("conditional", Some(Token::Keyword)),
    ("repeat", Some(Token::Keyword)),
    ("include", Some(Token::Keyword)),
    ("storageclass", Some(Token::Keyword)),
    ("exception", Some(Token::Keyword)),
    ("charset", Some(Token::Keyword)),
    ("import", Some(Token::Keyword)),
    ("keyframes", Some(Token::Keyword)),
    ("media", Some(Token::Keyword)),
    ("supports", Some(Token::Keyword)),
    ("function", Some(Token::Function)),
    ("method", Some(Token::Function)),
    ("constructor", Some(Token::Type)),
    ("type", Some(Token::Type)),
    ("namespace", Some(Token::Namespace)),
    ("module", Some(Token::Namespace)),
    ("string", Some(Token::String)),
    ("string.special", Some(Token::StringSpecial)),
    ("string.special.key", Some(Token::Property)),
    ("string.regexp", Some(Token::StringSpecial)),
    ("string.escape", Some(Token::Escape)),
    ("character", Some(Token::String)),
    ("character.special", Some(Token::StringSpecial)),
    ("escape", Some(Token::Escape)),
    ("number", Some(Token::Number)),
    ("float", Some(Token::Number)),
    ("boolean", Some(Token::Constant)),
    ("constant", Some(Token::Constant)),
    ("comment", Some(Token::Comment)),
    ("punctuation", Some(Token::Punctuation)),
    ("punctuation.special", Some(Token::PunctuationSpecial)),
    ("delimiter", Some(Token::Punctuation)),
    ("operator", Some(Token::Operator)),
    ("property", Some(Token::Property)),
    ("field", Some(Token::Property)),
    ("variable", Some(Token::Variable)),
    ("variable.builtin", Some(Token::VariableBuiltin)),
    ("variable.member", Some(Token::Property)),
    ("parameter", Some(Token::Variable)),
    ("attribute", Some(Token::Attribute)),
    ("tag", Some(Token::Tag)),
    ("tag.error", Some(Token::Error)),
    ("embedded", Some(Token::Embedded)),
    ("label", Some(Token::Label)),
    ("text.title", Some(Token::Heading)),
    ("text.uri", Some(Token::Link)),
    ("text.reference", Some(Token::Link)),
    ("text.literal", Some(Token::String)),
    ("none", None),
    ("spell", None),
];

fn token_for(capture: &str) -> Option<Token> {
    CAPTURES
        .iter()
        .filter(|(prefix, _)| {
            capture
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
        })
        .max_by_key(|(prefix, _)| prefix.len())
        .and_then(|(_, token)| *token)
}

enum Backend {
    Tree { parser: Parser, tree: Option<Tree> },
    Lines(LineScanner),
}

/// A highlighted file: a parse tree kept current with incremental edits, or a per-line scanner.
pub struct Syntax {
    lang: Lang,
    backend: Backend,
}

impl Syntax {
    pub fn new(lang: Lang, rope: &Rope) -> Self {
        let backend = match &lang.spec().engine {
            Engine::Grammar { language, .. } => {
                let mut parser = Parser::new();
                parser
                    .set_language(&language())
                    .expect("grammar matches tree-sitter ABI");
                Backend::Tree { parser, tree: None }
            }
            Engine::Lines(scan) => Backend::Lines(*scan),
        };
        let mut syntax = Self { lang, backend };
        syntax.reparse(rope);
        syntax
    }

    pub fn lang(&self) -> Lang {
        self.lang
    }

    pub fn edit(&mut self, edit: &InputEdit, rope: &Rope) {
        self.edit_tree(edit);
        self.reparse(rope);
    }

    /// Moves the tree past an edit without parsing; several edits can share one [`Self::reparse`].
    pub fn edit_tree(&mut self, edit: &InputEdit) {
        if let Backend::Tree {
            tree: Some(tree), ..
        } = &mut self.backend
        {
            tree.edit(edit);
        }
    }

    pub fn reparse(&mut self, rope: &Rope) {
        let Backend::Tree { parser, tree } = &mut self.backend else {
            return;
        };
        let mut chunks = |byte: usize, _| -> &[u8] {
            if byte >= rope.len_bytes() {
                return &[];
            }
            let (chunk, start, _, _) = rope.chunk_at_byte(byte);
            &chunk.as_bytes()[byte - start..]
        };
        *tree = parser.parse_with_options(&mut chunks, tree.as_ref(), None);
    }

    /// Highlighted byte ranges intersecting `bytes`, outer before inner so inner ones paint last.
    pub fn highlights(&self, rope: &Rope, bytes: Range<usize>) -> Vec<(Range<usize>, Token)> {
        let tree = match &self.backend {
            Backend::Tree {
                tree: Some(tree), ..
            } => tree,
            Backend::Tree { tree: None, .. } => return Vec::new(),
            Backend::Lines(scan) => return scan_lines(*scan, rope, bytes),
        };
        let Some(query) = self.lang.query() else {
            return Vec::new();
        };
        let names = query.capture_names();
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(bytes);
        let first_wins = self.lang.spec().first_pattern_wins;
        let mut best: HashMap<(usize, usize), (usize, Token)> = HashMap::new();
        let mut captures = cursor.captures(query, tree.root_node(), RopeText(rope));
        while let Some((m, index)) = captures.next() {
            let capture = m.captures()[*index];
            let Some(token) = token_for(names[capture.index as usize]) else {
                continue;
            };
            let range = capture.node.byte_range();
            let entry = best
                .entry((range.start, range.end))
                .or_insert((m.pattern_index, token));
            if (m.pattern_index < entry.0) == first_wins {
                *entry = (m.pattern_index, token);
            }
        }
        let mut out: Vec<_> = best
            .into_iter()
            .map(|((start, end), (_, token))| (start..end, token))
            .collect();
        out.sort_by_key(|(r, _)| (r.start, std::cmp::Reverse(r.end)));
        out
    }
}

impl Syntax {
    /// Byte of the bracket paired with the one at `byte`, when the parse tree pairs them.
    pub fn bracket_partner(&self, byte: usize) -> Option<usize> {
        let Backend::Tree {
            tree: Some(tree), ..
        } = &self.backend
        else {
            return None;
        };
        let node = tree.root_node().descendant_for_byte_range(byte, byte + 1)?;
        let (open, close) = bracket_pair(node.kind())?;
        if node.is_named() || node.start_byte() != byte {
            return None;
        }
        let forward = node.kind() == open;
        let want = if forward { close } else { open };
        let mut sibling = node;
        loop {
            sibling = if forward {
                sibling.next_sibling()?
            } else {
                sibling.prev_sibling()?
            };
            if !sibling.is_named() && sibling.kind() == want {
                return Some(sibling.start_byte());
            }
        }
    }
}

/// The open and close strings of the bracket pair `kind` belongs to.
pub fn bracket_pair(kind: &str) -> Option<(&'static str, &'static str)> {
    match kind {
        "(" | ")" => Some(("(", ")")),
        "[" | "]" => Some(("[", "]")),
        "{" | "}" => Some(("{", "}")),
        _ => None,
    }
}

fn scan_lines(scan: LineScanner, rope: &Rope, bytes: Range<usize>) -> Vec<(Range<usize>, Token)> {
    let first = rope.byte_to_line(bytes.start.min(rope.len_bytes()));
    let last = rope.byte_to_line(bytes.end.min(rope.len_bytes()));
    let mut out = Vec::new();
    for line in first..=last.min(rope.len_lines().saturating_sub(1)) {
        let start = rope.line_to_byte(line);
        let text = rope.line(line).to_string();
        let text = text.trim_end_matches(['\n', '\r']);
        out.extend(
            scan(text)
                .into_iter()
                .map(|(r, t)| (start + r.start..start + r.end, t)),
        );
    }
    out
}

struct RopeText<'a>(&'a Rope);

impl<'a> TextProvider<&'a [u8]> for RopeText<'a> {
    type I = std::vec::IntoIter<&'a [u8]>;

    fn text(&mut self, node: Node) -> Self::I {
        let range = node.byte_range();
        let slice = self.0.byte_slice(range);
        slice
            .chunks()
            .map(str::as_bytes)
            .collect::<Vec<_>>()
            .into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(lang: Lang, src: &str) -> Vec<(String, Token)> {
        let rope = Rope::from_str(src);
        let syntax = Syntax::new(lang, &rope);
        syntax
            .highlights(&rope, 0..src.len())
            .into_iter()
            .map(|(r, t)| (src[r].to_string(), t))
            .collect()
    }

    /// The token painted over `needle`'s first byte, as the element applies them.
    fn painted(lang: Lang, src: &str, needle: &str) -> Option<Token> {
        let at = src.find(needle).expect("needle in source");
        let rope = Rope::from_str(src);
        Syntax::new(lang, &rope)
            .highlights(&rope, 0..src.len())
            .into_iter()
            .rfind(|(r, _)| r.contains(&at))
            .map(|(_, t)| t)
    }

    #[test]
    fn json_keys_are_properties() {
        let src = "{\"name\": \"athena\", \"n\": 1, \"ok\": true}\n";
        assert_eq!(painted(Lang::Json, src, "\"name\""), Some(Token::Property));
        assert_eq!(painted(Lang::Json, src, "\"athena\""), Some(Token::String));
        assert_eq!(painted(Lang::Json, src, "1"), Some(Token::Number));
        assert_eq!(painted(Lang::Json, src, "true"), Some(Token::Constant));
        assert_eq!(painted(Lang::Json, src, "{"), Some(Token::Punctuation));
    }

    #[test]
    fn yaml_keys_and_values_differ() {
        let src = "services:\n  web:\n    image: nginx # proxy\n    ports: [80]\n";
        assert_eq!(painted(Lang::Yaml, src, "image"), Some(Token::Property));
        assert_eq!(painted(Lang::Yaml, src, "nginx"), Some(Token::String));
        assert_eq!(painted(Lang::Yaml, src, "80"), Some(Token::Number));
        assert_eq!(painted(Lang::Yaml, src, "# proxy"), Some(Token::Comment));
    }

    #[test]
    fn toml_keys_are_properties_and_tables_types() {
        let src = "[package]\nname = \"athena\"\nedition.workspace = true\n";
        assert_eq!(painted(Lang::Toml, src, "package"), Some(Token::Type));
        assert_eq!(painted(Lang::Toml, src, "name"), Some(Token::Property));
        assert_eq!(painted(Lang::Toml, src, "workspace"), Some(Token::Property));
        assert_eq!(painted(Lang::Toml, src, "\"athena\""), Some(Token::String));
    }

    #[test]
    fn markdown_headings_are_headings() {
        let src = "# Title\n\n- item\n\n```go\nx := 1\n```\n";
        assert_eq!(painted(Lang::Markdown, src, "Title"), Some(Token::Heading));
        assert_eq!(
            painted(Lang::Markdown, src, "#"),
            Some(Token::PunctuationSpecial)
        );
        assert_eq!(
            painted(Lang::Markdown, src, "-"),
            Some(Token::PunctuationSpecial)
        );
        assert_eq!(painted(Lang::Markdown, src, "x :="), Some(Token::String));
    }

    #[test]
    fn later_patterns_win_on_the_same_node() {
        let src = "function f() {}\nf(x);\n";
        assert_eq!(painted(Lang::TypeScript, src, "f("), Some(Token::Function));
        assert_eq!(painted(Lang::TypeScript, src, "x)"), Some(Token::Variable));
        let src = "def f(a):\n    return None\n";
        assert_eq!(painted(Lang::Python, src, "f("), Some(Token::Function));
        assert_eq!(painted(Lang::Python, src, "None"), Some(Token::Constant));
        let src = "let s = self.name\nfunc go() {}\n";
        assert_eq!(
            painted(Lang::Swift, src, "self"),
            Some(Token::VariableBuiltin)
        );
        assert_eq!(painted(Lang::Swift, src, "func"), Some(Token::Keyword));
    }

    #[test]
    fn earlier_patterns_win_where_the_query_expects_it() {
        let src = "package main\n\nfunc main() { fmt.Println(len(x)) }\n";
        assert_eq!(painted(Lang::Go, src, "main() "), Some(Token::Function));
        assert_eq!(painted(Lang::Go, src, "Println"), Some(Token::Function));
        assert_eq!(painted(Lang::Go, src, "x)"), Some(Token::Variable));
        let src = "fn main() { let v = Vec::new(); Some(v); self.x; }\n";
        assert_eq!(painted(Lang::Rust, src, "main"), Some(Token::Function));
        assert_eq!(painted(Lang::Rust, src, "let"), Some(Token::Keyword));
        assert_eq!(painted(Lang::Rust, src, "new"), Some(Token::Function));
        assert_eq!(painted(Lang::Rust, src, "Some"), Some(Token::Type));
        assert_eq!(
            painted(Lang::Rust, src, "self"),
            Some(Token::VariableBuiltin)
        );
        let src = ":root { --gap: 4px; color: var(--gap); }\n";
        assert_eq!(painted(Lang::Css, src, "--gap:"), Some(Token::Variable));
        assert_eq!(painted(Lang::Css, src, "color"), Some(Token::Property));
        assert_eq!(painted(Lang::Css, src, "var"), Some(Token::Function));
    }

    #[test]
    fn capture_names_use_the_longest_prefix() {
        assert_eq!(token_for("string.special.key"), Some(Token::Property));
        assert_eq!(token_for("string.special"), Some(Token::StringSpecial));
        assert_eq!(token_for("string"), Some(Token::String));
        assert_eq!(token_for("keyword.function"), Some(Token::Keyword));
        assert_eq!(token_for("variable.builtin"), Some(Token::VariableBuiltin));
        assert_eq!(
            token_for("punctuation.special"),
            Some(Token::PunctuationSpecial)
        );
        assert_eq!(token_for("text.title"), Some(Token::Heading));
        assert_eq!(token_for("none"), None);
        assert_eq!(token_for("stringy"), None);
    }

    #[test]
    fn go_highlights() {
        let t = tokens(
            Lang::Go,
            "package main\n\nfunc main() { s := \"hi\" // c\n}\n",
        );
        assert!(t.contains(&("func".into(), Token::Keyword)));
        assert!(t.contains(&("main".into(), Token::Function)));
        assert!(t.contains(&("\"hi\"".into(), Token::String)));
        assert!(t.contains(&("// c".into(), Token::Comment)));
    }

    #[test]
    fn typescript_uses_javascript_base_query() {
        let t = tokens(Lang::TypeScript, "const n: number = 1;\nfunction f() {}\n");
        assert!(t.contains(&("const".into(), Token::Keyword)));
        assert!(t.contains(&("number".into(), Token::Type)));
        assert!(t.contains(&("1".into(), Token::Number)));
        assert!(t.contains(&("f".into(), Token::Function)));
    }

    #[test]
    fn languages_by_extension() {
        assert_eq!(Lang::for_path(Path::new("a/b.tsx")), Some(Lang::Tsx));
        assert_eq!(Lang::for_path(Path::new("main.go")), Some(Lang::Go));
        assert_eq!(Lang::for_path(Path::new("README.md")), Some(Lang::Markdown));
        assert_eq!(Lang::for_path(Path::new("x.yml")), Some(Lang::Yaml));
        assert_eq!(Lang::for_path(Path::new("a/.env")), Some(Lang::DotEnv));
        assert_eq!(Lang::for_path(Path::new(".env.local")), Some(Lang::DotEnv));
        assert_eq!(
            Lang::for_path(Path::new("Dockerfile")),
            Some(Lang::Dockerfile)
        );
        assert_eq!(
            Lang::for_path(Path::new("Dockerfile.dev")),
            Some(Lang::Dockerfile)
        );
        assert_eq!(
            Lang::for_path(Path::new("api.Dockerfile")),
            Some(Lang::Dockerfile)
        );
        assert_eq!(Lang::for_path(Path::new(".zshrc")), Some(Lang::Shell));
        assert_eq!(Lang::for_path(Path::new("Cargo.lock")), Some(Lang::Toml));
        assert_eq!(Lang::for_path(Path::new("notes.txt")), None);
    }

    #[test]
    fn shebang_picks_shell() {
        assert_eq!(Lang::for_shebang("#!/bin/sh"), Some(Lang::Shell));
        assert_eq!(
            Lang::for_shebang("#!/usr/bin/env -S bash -e"),
            Some(Lang::Shell)
        );
        assert_eq!(
            Lang::for_shebang("#!/usr/bin/env python3"),
            Some(Lang::Python)
        );
        assert_eq!(
            Lang::for_shebang("#!/usr/bin/env node"),
            Some(Lang::JavaScript)
        );
        assert_eq!(Lang::for_shebang("echo hi"), None);
        let b = crate::Buffer::new("#!/bin/zsh\necho hi\n", None);
        assert_eq!(b.lang(), Some(Lang::Shell));
    }

    fn sample(lang: Lang) -> &'static str {
        match lang {
            Lang::Go => "package main\n",
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript => "const x = 1;\n",
            Lang::Yaml => "key: value\n",
            Lang::Json => "{\"a\": 1}\n",
            Lang::Toml => "a = 1\n",
            Lang::Shell => "echo hi\n",
            Lang::Rust => "fn main() {}\n",
            Lang::Python => "def f(): pass\n",
            Lang::Css => "a { color: red; }\n",
            Lang::Html => "<p>hi</p>\n",
            Lang::Markdown => "# Title\n",
            Lang::Swift => "let x = 1\n",
            Lang::Dockerfile => "FROM alpine\n",
            Lang::DotEnv => "A=1\n",
        }
    }

    #[test]
    fn every_language_compiles_and_highlights() {
        for lang in Lang::ALL {
            assert!(
                !tokens(lang, sample(lang)).is_empty(),
                "{lang:?} highlights nothing"
            );
        }
    }
}
