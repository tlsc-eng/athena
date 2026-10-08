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

/// Highlight classes; the theme maps each to one of a few muted colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    Keyword,
    Function,
    Type,
    String,
    Number,
    Comment,
    Punctuation,
    Property,
    Variable,
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
}

const JS: &str = tree_sitter_javascript::HIGHLIGHT_QUERY;
const JSX: &str = tree_sitter_javascript::JSX_HIGHLIGHT_QUERY;
const TS: &str = tree_sitter_typescript::HIGHLIGHTS_QUERY;

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
    },
    LangSpec {
        extensions: &["ts", "mts", "cts"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            || format!("{JS}\n{TS}"),
        ),
        comment: Some("// "),
    },
    LangSpec {
        extensions: &["tsx"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_typescript::LANGUAGE_TSX.into(),
            || format!("{JS}\n{JSX}\n{TS}"),
        ),
        comment: Some("// "),
    },
    LangSpec {
        extensions: &["js", "mjs", "cjs", "jsx"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_javascript::LANGUAGE.into(),
            || format!("{JS}\n{JSX}"),
        ),
        comment: Some("// "),
    },
    LangSpec {
        extensions: &["yaml", "yml"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_yaml::LANGUAGE.into(),
            || tree_sitter_yaml::HIGHLIGHTS_QUERY.into(),
        ),
        comment: Some("# "),
    },
    LangSpec {
        extensions: &["json", "jsonc", "json5"],
        filenames: &[".prettierrc", ".eslintrc", ".babelrc"],
        engine: grammar(
            || tree_sitter_json::LANGUAGE.into(),
            || tree_sitter_json::HIGHLIGHTS_QUERY.into(),
        ),
        comment: None,
    },
    LangSpec {
        extensions: &["toml"],
        filenames: &["Cargo.lock", "uv.lock", "poetry.lock"],
        engine: grammar(
            || tree_sitter_toml_ng::LANGUAGE.into(),
            || tree_sitter_toml_ng::HIGHLIGHTS_QUERY.into(),
        ),
        comment: Some("# "),
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
    },
    LangSpec {
        extensions: &["rs"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_rust::LANGUAGE.into(),
            || tree_sitter_rust::HIGHLIGHTS_QUERY.into(),
        ),
        comment: Some("// "),
    },
    LangSpec {
        extensions: &["py", "pyi"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_python::LANGUAGE.into(),
            || tree_sitter_python::HIGHLIGHTS_QUERY.into(),
        ),
        comment: Some("# "),
    },
    LangSpec {
        extensions: &["css"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_css::LANGUAGE.into(),
            || tree_sitter_css::HIGHLIGHTS_QUERY.into(),
        ),
        comment: None,
    },
    LangSpec {
        extensions: &["html", "htm"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_html::LANGUAGE.into(),
            || tree_sitter_html::HIGHLIGHTS_QUERY.into(),
        ),
        comment: None,
    },
    LangSpec {
        extensions: &["md", "markdown"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_md::LANGUAGE.into(),
            || tree_sitter_md::HIGHLIGHT_QUERY_BLOCK.into(),
        ),
        comment: None,
    },
    LangSpec {
        extensions: &["swift"],
        filenames: &[],
        engine: grammar(
            || tree_sitter_swift::LANGUAGE.into(),
            || tree_sitter_swift::HIGHLIGHTS_QUERY.into(),
        ),
        comment: Some("// "),
    },
    LangSpec {
        extensions: &["dockerfile", "containerfile"],
        filenames: &["Dockerfile*", "Containerfile*"],
        engine: Engine::Lines(crate::lines::dockerfile),
        comment: Some("# "),
    },
    LangSpec {
        extensions: &["env"],
        filenames: &[".env", ".env.*"],
        engine: Engine::Lines(crate::lines::dotenv),
        comment: Some("# "),
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

fn token_for(capture: &str) -> Option<Token> {
    let head = capture.split('.').next().unwrap_or(capture);
    Some(match head {
        "keyword" | "conditional" | "repeat" | "include" | "storageclass" | "exception" => {
            Token::Keyword
        }
        "function" | "method" | "constructor" => Token::Function,
        "type" | "tag" | "attribute" | "namespace" | "module" => Token::Type,
        "string" | "escape" | "character" => Token::String,
        "number" | "float" | "boolean" | "constant" => Token::Number,
        "comment" => Token::Comment,
        "punctuation" | "operator" | "delimiter" => Token::Punctuation,
        "property" | "field" => Token::Property,
        "variable" | "label" | "parameter" => Token::Variable,
        _ => return None,
    })
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
        if let Backend::Tree {
            tree: Some(tree), ..
        } = &mut self.backend
        {
            tree.edit(edit);
        }
        self.reparse(rope);
    }

    fn reparse(&mut self, rope: &Rope) {
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

    /// Highlighted byte ranges intersecting `bytes`, innermost capture last.
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
        let mut out = Vec::new();
        let mut captures = cursor.captures(query, tree.root_node(), RopeText(rope));
        while let Some((m, index)) = captures.next() {
            let capture = m.captures()[*index];
            if let Some(token) = token_for(names[capture.index as usize]) {
                out.push((capture.node.byte_range(), token));
            }
        }
        out
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
