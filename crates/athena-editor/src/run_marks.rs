//! Tests found in a file, and the gutter marks that run them and show how they did.

use std::path::{Path, PathBuf};

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
            out.push(TestSymbol {
                line: func.start_position().row,
                end_line: func.end_position().row,
                titles: vec![name.to_string()],
                group: false,
            });
        }
    }
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
    let title = match first.kind() {
        "string" => raw.get(1..raw.len().checked_sub(1)?)?,
        "template_string" => {
            let mut tc = first.walk();
            if first
                .named_children(&mut tc)
                .any(|c| c.kind() == "template_substitution")
            {
                return None;
            }
            raw.get(1..raw.len().checked_sub(1)?)?
        }
        _ => return None,
    };
    Some((group, title.to_string()))
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
    fn only_test_files_are_test_files() {
        for yes in ["a_test.go", "x.test.ts", "x.spec.jsx", "src/__tests__/a.ts"] {
            assert!(is_test_file(Path::new(yes)), "{yes}");
        }
        for no in ["a.go", "x.ts", "test.md", "x.test.md", "__tests__/notes.md"] {
            assert!(!is_test_file(Path::new(no)), "{no}");
        }
    }
}
