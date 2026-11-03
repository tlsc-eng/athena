//! Tests found in a file, and the gutter marks that run them and show how they did.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{Action, Context, Pixels, Point, Window, px};
use tree_sitter::{Node, Parser};

use crate::Lang;
use crate::element::GUTTER_PAD;
use crate::view::EditorView;

/// Runs the test whose mark was clicked, on a zero-based line of `path`.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = editor, no_json)]
pub struct RunTestAt {
    pub path: PathBuf,
    pub line: usize,
}

/// A test or group of tests a runner can be asked for by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestSymbol {
    /// Zero-based lines the declaration spans.
    pub line: usize,
    pub end_line: usize,
    /// A Go test's name, or the enclosing `describe` titles then the test's own.
    pub titles: Vec<String>,
    /// A `describe` block, which runs every test inside it.
    pub group: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Running,
    Passed,
    Failed,
    Skipped,
}

/// Zero-based lines a coverage run measured, and whether each one ran.
pub type Coverage = std::collections::BTreeMap<usize, bool>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunMark {
    pub line: usize,
    pub state: RunState,
}

/// Whether `path` is a file test runners look in: `_test.go`, `*.test.ts`, `*.spec.js`,
/// or anything under `__tests__`.
pub fn is_test_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    if name.ends_with("_test.go") {
        return true;
    }
    let script = [".ts", ".tsx", ".js", ".jsx", ".mts", ".mjs", ".cts", ".cjs"]
        .iter()
        .any(|ext| name.ends_with(ext));
    script
        && (name.contains(".test.")
            || name.contains(".spec.")
            || path.components().any(|c| c.as_os_str() == "__tests__"))
}

/// The tests declared in a test file's `text`: Go's `func TestX(t *testing.T)` and
/// `func FuzzX(f *testing.F)`, or JavaScript's `describe` / `it` / `test` calls.
pub fn find_tests(lang: Lang, path: &Path, text: &str) -> Vec<TestSymbol> {
    if !is_test_file(path) {
        return Vec::new();
    }
    let language: tree_sitter::Language = match lang {
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        _ => return Vec::new(),
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if lang == Lang::Go {
        go_tests(tree.root_node(), text, &mut out);
    } else {
        js_tests(tree.root_node(), text, &mut Vec::new(), &mut out);
    }
    out
}

fn text_of<'a>(node: Node, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or_default()
}

/// `TestXxx` where Xxx does not start with a lower-case letter, as `go test` requires.
fn go_test_name(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|rest| !rest.starts_with(|c: char| c.is_lowercase()))
}

fn go_tests(root: Node, src: &str, out: &mut Vec<TestSymbol>) {
    let mut cursor = root.walk();
    for func in root.named_children(&mut cursor) {
        if func.kind() != "function_declaration" {
            continue;
        }
        let (Some(name), Some(params)) = (
            func.child_by_field_name("name"),
            func.child_by_field_name("parameters"),
        ) else {
            continue;
        };
        let name = text_of(name, src);
        let mut pc = params.walk();
        let types: Vec<String> = params
            .named_children(&mut pc)
            .filter_map(|p| p.child_by_field_name("type"))
            .map(|t| text_of(t, src).split_whitespace().collect())
            .collect();
        let runnable = match types.as_slice() {
            [t] if t == "*testing.T" => go_test_name(name, "Test"),
            [t] if t == "*testing.F" => go_test_name(name, "Fuzz"),
            _ => false,
        };
        if runnable {
            let titles = vec![name.to_string()];
            out.push(TestSymbol {
                line: func.start_position().row,
                end_line: func.end_position().row,
                titles: titles.clone(),
                group: false,
            });
            if let (Some(t), Some(body)) =
                (go_testing_t(params, src), func.child_by_field_name("body"))
            {
                go_subtests(body, src, t, &titles, out);
            }
        }
    }
}

/// The name of a parameter list's only parameter when it is a `*testing.T`.
fn go_testing_t<'a>(params: Node, src: &'a str) -> Option<&'a str> {
    let mut pc = params.walk();
    let mut list = params.named_children(&mut pc);
    let (Some(param), None) = (list.next(), list.next()) else {
        return None;
    };
    let ty: String = text_of(param.child_by_field_name("type")?, src)
        .split_whitespace()
        .collect();
    if ty != "*testing.T" {
        return None;
    }
    Some(text_of(param.child_by_field_name("name")?, src))
}

/// `t.Run("name", func(t *testing.T) { … })` calls with a literal name, nested ones included.
fn go_subtests(node: Node, src: &str, t: &str, parents: &[String], out: &mut Vec<TestSymbol>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let Some((name, func, inner_t)) = go_run_call(child, src, t) else {
            go_subtests(child, src, t, parents, out);
            continue;
        };
        let mut titles = parents.to_vec();
        // `go test` names a subtest by its rewritten name, and a `/` in it nests another level.
        titles.extend(go_subtest_name(&name).split('/').map(String::from));
        out.push(TestSymbol {
            line: child.start_position().row,
            end_line: child.end_position().row,
            titles: titles.clone(),
            group: false,
        });
        if let Some(body) = func.child_by_field_name("body") {
            go_subtests(body, src, inner_t, &titles, out);
        }
    }
}

/// A `t.Run` call on the test's own `t`: the subtest's name, its function and that one's `t`.
fn go_run_call<'a>(call: Node<'a>, src: &'a str, t: &str) -> Option<(String, Node<'a>, &'a str)> {
    if call.kind() != "call_expression" {
        return None;
    }
    let callee = call.child_by_field_name("function")?;
    let receiver = callee.child_by_field_name("operand")?;
    if callee.kind() != "selector_expression"
        || receiver.kind() != "identifier"
        || text_of(receiver, src) != t
        || text_of(callee.child_by_field_name("field")?, src) != "Run"
    {
        return None;
    }
    let args = call.child_by_field_name("arguments")?;
    let mut ac = args.walk();
    let mut list = args.named_children(&mut ac);
    let (Some(name), Some(func), None) = (list.next(), list.next(), list.next()) else {
        return None;
    };
    if func.kind() != "func_literal" {
        return None;
    }
    let inner_t = go_testing_t(func.child_by_field_name("parameters")?, src)?;
    let raw = text_of(name, src);
    let body = raw.get(1..raw.len().checked_sub(1)?)?;
    let name = match name.kind() {
        "interpreted_string_literal" => go_unescape(body)?,
        // Go drops carriage returns from raw strings.
        "raw_string_literal" => body.replace('\r', ""),
        _ => return None,
    };
    Some((name, func, inner_t))
}

/// A Go interpreted string literal's body as its value; `None` for an escape that is not UTF-8.
fn go_unescape(body: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(body.len());
    let mut chars = body.chars();
    let hex = |chars: &mut std::str::Chars, n: usize| {
        let digits: String = chars.take(n).collect();
        (digits.len() == n).then(|| u32::from_str_radix(&digits, 16).ok())?
    };
    while let Some(c) = chars.next() {
        if c != '\\' {
            bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
            continue;
        }
        let unicode = |n: u32, bytes: &mut Vec<u8>| -> Option<()> {
            bytes.extend_from_slice(char::from_u32(n)?.encode_utf8(&mut [0; 4]).as_bytes());
            Some(())
        };
        match chars.next()? {
            'a' => bytes.push(0x07),
            'b' => bytes.push(0x08),
            'f' => bytes.push(0x0c),
            'n' => bytes.push(b'\n'),
            'r' => bytes.push(b'\r'),
            't' => bytes.push(b'\t'),
            'v' => bytes.push(0x0b),
            'x' => bytes.push(u8::try_from(hex(&mut chars, 2)?).ok()?),
            'u' => unicode(hex(&mut chars, 4)?, &mut bytes)?,
            'U' => unicode(hex(&mut chars, 8)?, &mut bytes)?,
            d @ '0'..='7' => {
                let rest: String = chars.by_ref().take(2).collect();
                bytes.push(u8::from_str_radix(&format!("{d}{rest}"), 8).ok()?);
            }
            other @ ('\\' | '"' | '\'') => bytes.push(other as u8),
            _ => return None,
        }
    }
    String::from_utf8(bytes).ok()
}

/// The name `go test` gives a subtest: spaces become `_` and control characters are escaped,
/// as the testing package's `rewrite` does.
fn go_subtest_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            c if c.is_whitespace() => out.push('_'),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            c if c.is_control() && (c as u32) < 0x80 => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c if c.is_control() || unprintable(c) => match c as u32 {
                n @ ..0x10000 => out.push_str(&format!("\\u{n:04x}")),
                n => out.push_str(&format!("\\U{n:08x}")),
            },
            c => out.push(c),
        }
    }
    out
}

/// Format and private-use characters, which Go's `strconv.IsPrint` refuses beside controls and
/// spaces; unassigned code points are left as they are.
fn unprintable(c: char) -> bool {
    const RANGES: &[(u32, u32)] = &[
        (0xad, 0xad),
        (0x600, 0x605),
        (0x61c, 0x61c),
        (0x6dd, 0x6dd),
        (0x70f, 0x70f),
        (0x890, 0x891),
        (0x8e2, 0x8e2),
        (0x180e, 0x180e),
        (0x200b, 0x200f),
        (0x202a, 0x202e),
        (0x2060, 0x2064),
        (0x2066, 0x206f),
        (0xe000, 0xf8ff),
        (0xfeff, 0xfeff),
        (0xfff9, 0xfffb),
        (0x110bd, 0x110bd),
        (0x110cd, 0x110cd),
        (0x13430, 0x1343f),
        (0x1bca0, 0x1bca3),
        (0x1d173, 0x1d17a),
        (0xe0001, 0xe0001),
        (0xe0020, 0xe007f),
        (0xf0000, 0x10ffff),
    ];
    let n = c as u32;
    RANGES.iter().any(|&(lo, hi)| (lo..=hi).contains(&n))
}

/// `describe("x", …)`, `it.only("y", …)` and the like: whether it groups, and its title.
fn js_call(call: Node, src: &str) -> Option<(bool, String)> {
    let callee = call.child_by_field_name("function")?;
    let base = match callee.kind() {
        "identifier" => text_of(callee, src),
        "member_expression" => {
            let object = callee.child_by_field_name("object")?;
            let property = text_of(callee.child_by_field_name("property")?, src);
            let known = ["only", "skip", "concurrent", "sequential", "fails"];
            if object.kind() != "identifier" || !known.contains(&property) {
                return None;
            }
            text_of(object, src)
        }
        _ => return None,
    };
    let group = match base {
        "describe" | "suite" => true,
        "it" | "test" => false,
        _ => return None,
    };
    let args = call.child_by_field_name("arguments")?;
    let mut ac = args.walk();
    let first = args.named_children(&mut ac).next()?;
    let raw = text_of(first, src);
    match first.kind() {
        "string" => {}
        "template_string" => {
            let mut tc = first.walk();
            if first
                .named_children(&mut tc)
                .any(|c| c.kind() == "template_substitution")
            {
                return None;
            }
        }
        _ => return None,
    }
    // The runner matches `-t` against the title's value, so `'it\'s'` must become `it's`.
    let title = js_unescape(raw.get(1..raw.len().checked_sub(1)?)?)?;
    Some((group, title))
}

/// A JavaScript string literal's body as its value; `None` for a malformed escape.
fn js_unescape(body: &str) -> Option<String> {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let hex = |chars: &mut std::iter::Peekable<std::str::Chars>, n: usize| {
            let digits: String = chars.take(n).collect();
            (digits.len() == n).then(|| u32::from_str_radix(&digits, 16).ok())?
        };
        match chars.next()? {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            '0' if !chars.peek().is_some_and(char::is_ascii_digit) => out.push('\0'),
            'x' => out.push(char::from_u32(hex(&mut chars, 2)?)?),
            'u' if chars.peek() == Some(&'{') => {
                chars.next();
                let digits: String = chars.by_ref().take_while(|c| *c != '}').collect();
                out.push(char::from_u32(u32::from_str_radix(&digits, 16).ok()?)?);
            }
            'u' => out.push(char::from_u32(hex(&mut chars, 4)?)?),
            '\r' => {
                chars.next_if_eq(&'\n');
            }
            '\n' | '\u{2028}' | '\u{2029}' => {}
            other => out.push(other),
        }
    }
    Some(out)
}

fn js_tests(node: Node, src: &str, stack: &mut Vec<String>, out: &mut Vec<TestSymbol>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let found = (child.kind() == "call_expression")
            .then(|| js_call(child, src))
            .flatten();
        let Some((group, title)) = found else {
            js_tests(child, src, stack, out);
            continue;
        };
        let mut titles = stack.clone();
        titles.push(title.clone());
        out.push(TestSymbol {
            line: child.start_position().row,
            end_line: child.end_position().row,
            titles,
            group,
        });
        if group {
            stack.push(title);
            js_tests(child, src, stack, out);
            stack.pop();
        }
    }
}

impl EditorView {
    /// The run marks to show; a test file's are recomputed by whoever owns its results.
    pub fn set_run_marks(&mut self, marks: Vec<RunMark>, cx: &mut Context<Self>) {
        if self.run_marks != marks {
            self.run_marks = marks;
            cx.notify();
        }
    }

    /// Covered and uncovered line tints for the gutter; `None` clears them.
    pub fn set_coverage(&mut self, coverage: Option<Arc<Coverage>>, cx: &mut Context<Self>) {
        let same = match (&self.coverage, &coverage) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (a, b) => a.is_none() && b.is_none(),
        };
        if !same {
            self.coverage = coverage;
            cx.notify();
        }
    }

    pub(crate) fn coverage_at(&self, line: usize) -> Option<bool> {
        self.coverage.as_ref()?.get(&line).copied()
    }

    pub(crate) fn run_mark(&self, line: usize) -> Option<RunState> {
        self.run_marks
            .iter()
            .find(|m| m.line == line)
            .map(|m| m.state)
    }

    /// A click on a line's run mark runs that test, unless the lightbulb has that spot.
    pub(crate) fn click_run_mark(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(layout) = self.layout.as_ref() else {
            return false;
        };
        if position.x < layout.origin.x || position.x >= layout.origin.x + px(GUTTER_PAD) {
            return false;
        }
        let y = position.y - layout.origin.y + px(self.scroll.y);
        let row = (y / layout.line_height).floor().max(0.) as usize;
        let Some(line) = layout
            .row(row)
            .filter(|r| r.chars.start == 0)
            .map(|r| r.line)
        else {
            return false;
        };
        if self.lightbulb == Some(line) || self.run_mark(line).is_none() {
            return false;
        }
        let action = RunTestAt {
            path: self.path().to_path_buf(),
            line,
        };
        window.dispatch_action(Box::new(action), cx);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_tests_and_fuzz_targets_are_found_but_not_helpers() {
        let src = "package p\n\nimport \"testing\"\n\nfunc TestAdd(t *testing.T) {\n}\n\n\
                   func Testhelper(t *testing.T) {}\nfunc TestNoArgs() {}\n\
                   func helper(t *testing.T) {}\nfunc Test_under(t *testing.T) {}\n\
                   func FuzzParse(f *testing.F) {}\nfunc BenchmarkX(b *testing.B) {}\n\
                   func Test(t *testing.T) {}\n";
        let found = find_tests(Lang::Go, Path::new("p/a_test.go"), src);
        let names: Vec<(&str, usize)> = found
            .iter()
            .map(|t| (t.titles[0].as_str(), t.line))
            .collect();
        assert_eq!(
            names,
            [
                ("TestAdd", 4),
                ("Test_under", 10),
                ("FuzzParse", 11),
                ("Test", 13)
            ]
        );
        assert_eq!(found[0].end_line, 5);
        assert!(find_tests(Lang::Go, Path::new("p/a.go"), src).is_empty());
    }

    #[test]
    fn go_subtests_with_literal_names_are_found_under_their_test() {
        let src = "package p\n\nimport \"testing\"\n\n\
                   func TestMath(t *testing.T) {\n\
                   \tt.Run(\"adds two\", func(t *testing.T) {\n\
                   \t\tt.Run(`deep`, func(u *testing.T) {})\n\
                   \t})\n\
                   \tfor _, tc := range cases {\n\
                   \t\tt.Run(tc.name, func(t *testing.T) {})\n\
                   \t\tt.Run(\"in\\tloop\", func(t *testing.T) {})\n\
                   \t}\n\
                   \tother.Run(\"not t\", func(t *testing.T) {})\n\
                   \tt.Run(\"a/b\", func(t *testing.T) {})\n\
                   }\n";
        let found = find_tests(Lang::Go, Path::new("p/m_test.go"), src);
        let titles: Vec<(String, usize)> = found
            .iter()
            .map(|t| (t.titles.join(" > "), t.line))
            .collect();
        let expected = [
            ("TestMath", 4),
            ("TestMath > adds_two", 5),
            ("TestMath > adds_two > deep", 6),
            ("TestMath > in_loop", 10),
            ("TestMath > a > b", 13),
        ];
        let expected: Vec<(String, usize)> =
            expected.iter().map(|(t, l)| (t.to_string(), *l)).collect();
        assert_eq!(titles, expected);
        assert_eq!((found[1].line, found[1].end_line), (5, 7));
    }

    #[test]
    fn go_strings_unescape_and_subtest_names_are_rewritten_as_go_test_does() {
        assert_eq!(
            go_unescape(r#"a\"b\\c\x41\u00e9\101"#).as_deref(),
            Some("a\"b\\cAéA")
        );
        assert_eq!(go_unescape(r"\q"), None);
        assert_eq!(go_unescape(r"\xff"), None);
        assert_eq!(go_subtest_name("two words\there"), "two_words_here");
        assert_eq!(go_subtest_name("bell\u{7}nul\u{0}"), "bell\\anul\\x00");
        // As `go test -v` prints them.
        assert_eq!(go_subtest_name("zero\u{200b}width"), "zero\\u200bwidth");
        assert_eq!(go_subtest_name("tag\u{e0001}"), "tag\\U000e0001");
        assert_eq!(go_subtest_name("café ✓"), "café_✓");
    }

    #[test]
    fn javascript_tests_nest_under_their_describe_blocks() {
        let src = "import { describe, it, expect } from 'vitest';\n\
                   describe('math', () => {\n\
                   \x20 it('adds', () => {});\n\
                   \x20 it.only(\"subtracts\", () => {});\n\
                   \x20 describe(`deep`, () => { test('x', () => {}) });\n\
                   \x20 it(`skip ${name}`, () => {});\n\
                   \x20 it.each([1])('each %s', () => {});\n\
                   });\n\
                   test('top', () => {});\n";
        let found = find_tests(Lang::TypeScript, Path::new("src/math.test.ts"), src);
        let titles: Vec<String> = found.iter().map(|t| t.titles.join(" > ")).collect();
        assert_eq!(
            titles,
            [
                "math",
                "math > adds",
                "math > subtracts",
                "math > deep",
                "math > deep > x",
                "top"
            ]
        );
        assert!(found[0].group && !found[1].group);
        assert_eq!((found[0].line, found[0].end_line), (1, 7));
        assert_eq!(found[5].line, 8);
        let tsx = find_tests(Lang::Tsx, Path::new("src/__tests__/App.tsx"), src);
        assert_eq!(tsx.len(), 6);
    }

    #[test]
    fn javascript_titles_are_unescaped_to_the_runner_s_names() {
        let src = "it('it\\'s', () => {});\n\
                   it(\"tab\\there \\u00e9\\u{1F600} \\x41\", () => {});\n\
                   it(`back\\`tick \\${x}`, () => {});\n";
        let found = find_tests(Lang::JavaScript, Path::new("a.test.js"), src);
        let titles: Vec<&str> = found.iter().map(|t| t.titles[0].as_str()).collect();
        assert_eq!(titles, ["it's", "tab\there é\u{1F600} A", "back`tick ${x}"]);
        assert_eq!(js_unescape("bad \\u12"), None);
    }

    #[test]
    fn only_test_files_are_test_files() {
        for yes in ["a_test.go", "x.test.ts", "x.spec.jsx", "src/__tests__/a.ts"] {
            assert!(is_test_file(Path::new(yes)), "{yes}");
        }
        for no in ["a.go", "x.ts", "test.md", "x.test.md", "__tests__/notes.md"] {
            assert!(!is_test_file(Path::new(no)), "{no}");
        }
    }
}
