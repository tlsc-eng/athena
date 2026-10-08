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

impl Lang {
    pub fn for_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "go" => Some(Self::Go),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "js" | "mjs" | "cjs" | "jsx" => Some(Self::JavaScript),
            _ => None,
        }
    }

    pub fn comment_prefix(self) -> &'static str {
        "// "
    }

    fn language(self) -> Language {
        match self {
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        }
    }

    /// Compiled once per language; TypeScript layers its query on JavaScript's.
    fn query(self) -> &'static Query {
        static QUERIES: [OnceLock<Query>; 4] = [const { OnceLock::new() }; 4];
        QUERIES[self as usize].get_or_init(|| {
            let js = tree_sitter_javascript::HIGHLIGHT_QUERY;
            let jsx = tree_sitter_javascript::JSX_HIGHLIGHT_QUERY;
            let ts = tree_sitter_typescript::HIGHLIGHTS_QUERY;
            let source = match self {
                Self::Go => tree_sitter_go::HIGHLIGHTS_QUERY.to_string(),
                Self::TypeScript => format!("{js}\n{ts}"),
                Self::Tsx => format!("{js}\n{jsx}\n{ts}"),
                Self::JavaScript => format!("{js}\n{jsx}"),
            };
            Query::new(&self.language(), &source).expect("bundled highlight query compiles")
        })
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

/// A parsed file kept current with incremental edits.
pub struct Syntax {
    lang: Lang,
    parser: Parser,
    tree: Option<Tree>,
}

impl Syntax {
    pub fn new(lang: Lang, rope: &Rope) -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&lang.language())
            .expect("grammar matches tree-sitter ABI");
        let mut syntax = Self {
            lang,
            parser,
            tree: None,
        };
        syntax.reparse(rope);
        syntax
    }

    pub fn lang(&self) -> Lang {
        self.lang
    }

    pub fn edit(&mut self, edit: &InputEdit, rope: &Rope) {
        if let Some(tree) = self.tree.as_mut() {
            tree.edit(edit);
        }
        self.reparse(rope);
    }

    fn reparse(&mut self, rope: &Rope) {
        let mut chunks = |byte: usize, _| -> &[u8] {
            if byte >= rope.len_bytes() {
                return &[];
            }
            let (chunk, start, _, _) = rope.chunk_at_byte(byte);
            &chunk.as_bytes()[byte - start..]
        };
        self.tree = self
            .parser
            .parse_with_options(&mut chunks, self.tree.as_ref(), None);
    }

    /// Highlighted byte ranges intersecting `bytes`, innermost capture last.
    pub fn highlights(&self, rope: &Rope, bytes: Range<usize>) -> Vec<(Range<usize>, Token)> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        let query = self.lang.query();
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
        assert_eq!(Lang::for_path(Path::new("README.md")), None);
    }
}
