use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::SyncSender;

use athena_editor::EditorView;
use athena_lsp::{Location, Position, Symbol, symbol_kind_label};
use athena_proto::{
    AppMsg, AppReply, BufferInfo, EditorInfo, LocationInfo, PaneId, SourcePosition, SymbolInfo,
    TestFailureInfo, TestResultsInfo,
};
use athena_testing::{Outcome, Report};
use athena_workspace::{ItemKind, resolve_in_roots};
use gpui::{Context, Entity};

use super::Shell;
use super::item::ItemView;
use super::lsp::document_key;

const MAX_LOCATIONS: usize = 200;
const MAX_SYMBOLS: usize = 2000;
/// Well under the 1 MiB frame, leaving room for the JSON around it.
pub(super) const MAX_BUFFER: u32 = 512 * 1024;
const MAX_FAILURES: usize = 50;
const MAX_OUTPUT: usize = 4000;
const SNIPPET: usize = 200;

/// The UTF-16 column of `symbol` on `line`, or of the 1-based `column` given.
fn character(line: &str, column: Option<u32>, symbol: Option<&str>) -> Result<u32, String> {
    if let Some(symbol) = symbol.filter(|s| !s.is_empty()) {
        let at = line
            .find(symbol)
            .ok_or_else(|| format!("`{symbol}` is not on that line"))?;
        return Ok(line[..at].encode_utf16().count() as u32);
    }
    match column {
        Some(c) if c >= 1 => Ok(c - 1),
        _ => Err("give the symbol's text or its 1-based column".into()),
    }
}

/// Text cut to at most `max` bytes on a character boundary.
fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A symbol as the tool shows it, with names cut so a list of them fits one frame.
fn symbol_info(s: Symbol) -> SymbolInfo {
    SymbolInfo {
        kind: symbol_kind_label(s.kind).to_string(),
        name: cut(&s.name, SNIPPET).to_string(),
        container: s.container.map(|c| cut(&c, SNIPPET).to_string()),
        line: s.scope.start.line + 1,
        end_line: s.scope.end.line + 1,
    }
}

/// Locations as the tool shows them, with the source line where the file is shared with Claude.
fn located(found: Vec<Location>, roots: &[PathBuf]) -> Vec<LocationInfo> {
    let mut files: HashMap<PathBuf, Option<Vec<String>>> = HashMap::new();
    found
        .into_iter()
        .take(MAX_LOCATIONS)
        .map(|l| {
            let lines = files.entry(l.path.clone()).or_insert_with(|| {
                resolve_in_roots(&l.path, roots).ok()?;
                let text = std::fs::read_to_string(&l.path).ok()?;
                Some(text.lines().map(str::to_string).collect())
            });
            let text = lines
                .as_ref()
                .and_then(|ls| ls.get(l.range.start.line as usize))
                .map(|s| cut(s.trim(), SNIPPET).to_string());
            LocationInfo {
                path: l.path,
                line: l.range.start.line + 1,
                column: l.range.start.character + 1,
                end_line: l.range.end.line + 1,
                end_column: l.range.end.character + 1,
                text,
            }
        })
        .collect()
}

fn results(project: &Path, running: bool, report: Option<&Report>) -> TestResultsInfo {
    let empty = Report::default();
    let report = report.unwrap_or(&empty);
    let failures = report
        .failed()
        .into_iter()
        .flat_map(|(suite, tests)| {
            let name = suite.name.clone();
            let broken = tests.is_empty().then(|| TestFailureInfo {
                suite: name.clone(),
                test: String::new(),
                output: cut(&suite.output, MAX_OUTPUT).to_string(),
            });
            let failed: Vec<TestFailureInfo> = tests
                .into_iter()
                .map(|t| TestFailureInfo {
                    suite: name.clone(),
                    test: t.name(suite.framework),
                    output: cut(&t.output, MAX_OUTPUT).to_string(),
                })
                .collect();
            broken.into_iter().chain(failed)
        })
        .take(MAX_FAILURES)
        .collect();
    let count = |o| report.count(o) as u32;
    TestResultsInfo {
        project: project.to_path_buf(),
        running,
        passed: count(Outcome::Passed),
        failed: count(Outcome::Failed) + report.broken() as u32,
        skipped: count(Outcome::Skipped),
        failures,
    }
}

impl Shell {
    fn open_roots(&self) -> Vec<PathBuf> {
        self.workspace
            .projects
            .iter()
            .map(|p| p.root.clone())
            .collect()
    }

    /// The project a tool works in: the caller's terminal's, else the active one.
    fn caller_root(&self, caller: Option<PaneId>) -> Option<PathBuf> {
        caller
            .and_then(|s| self.session_root(s))
            .or_else(|| self.active_root())
    }

    /// An open editor of a file shared with Claude, with its unsaved text sent to the server.
    fn shared_editor(
        &mut self,
        path: &Path,
        cx: &mut Context<Self>,
    ) -> Result<(PathBuf, Entity<EditorView>), String> {
        let path = resolve_in_roots(path, &self.open_roots())?;
        let doc = document_key(&path);
        let editor = self
            .editors_showing(&doc, cx)
            .into_iter()
            .next()
            .ok_or_else(|| {
                format!(
                    "{} is not open in Athena; open it with open_file first so its language server \
                     loads it",
                    path.display()
                )
            })?;
        self.flush_change(&doc, &editor, cx);
        Ok((doc, editor))
    }

    fn lsp_position(
        &mut self,
        at: &SourcePosition,
        cx: &mut Context<Self>,
    ) -> Result<(PathBuf, Position), String> {
        let (doc, editor) = self.shared_editor(&at.path, cx)?;
        let text = editor.read(cx).text().unwrap_or_default();
        let index = at.line.checked_sub(1).ok_or("lines start at 1")?;
        let line = text
            .lines()
            .nth(index as usize)
            .ok_or_else(|| format!("the file has no line {}", at.line))?;
        let character = character(line, at.column, at.symbol.as_deref())?;
        Ok((
            doc,
            Position {
                line: index,
                character,
            },
        ))
    }

    /// Starts answering a request that waits on a language server; false when `msg` is not one.
    pub(super) fn answer_later(
        &mut self,
        msg: &AppMsg,
        reply: &SyncSender<AppReply>,
        cx: &mut Context<Self>,
    ) -> bool {
        let reply = reply.clone();
        let fail = |e: String| {
            let _ = reply.send(AppReply::Error(e));
        };
        let roots = self.open_roots();
        match msg {
            AppMsg::LspDefinition { at } | AppMsg::LspReferences { at } => {
                let references = matches!(msg, AppMsg::LspReferences { .. });
                let (doc, position) = match self.lsp_position(at, cx) {
                    Ok(found) => found,
                    Err(e) => {
                        fail(e);
                        return true;
                    }
                };
                let Some(client) = self.document_client(&doc) else {
                    fail(no_server(&doc));
                    return true;
                };
                cx.spawn(async move |_, cx| {
                    let found = match references {
                        true => client.references(&doc, position).await,
                        false => client.definition(&doc, position).await,
                    };
                    let answer = match found {
                        Ok(list) => AppReply::Locations(
                            cx.background_executor()
                                .spawn(async move { located(list, &roots) })
                                .await,
                        ),
                        Err(e) => AppReply::Error(e),
                    };
                    let _ = reply.send(answer);
                })
                .detach();
                true
            }
            AppMsg::DocumentSymbols { path } => {
                let doc = match self.shared_editor(path, cx) {
                    Ok((doc, _)) => doc,
                    Err(e) => {
                        fail(e);
                        return true;
                    }
                };
                let Some(client) = self.document_client(&doc) else {
                    fail(no_server(&doc));
                    return true;
                };
                cx.spawn(async move |_, _| {
                    let answer = match client.document_symbols(&doc).await {
                        Ok(symbols) => AppReply::Symbols(
                            symbols
                                .into_iter()
                                .take(MAX_SYMBOLS)
                                .map(symbol_info)
                                .collect(),
                        ),
                        Err(e) => AppReply::Error(e),
                    };
                    let _ = reply.send(answer);
                })
                .detach();
                true
            }
            _ => false,
        }
    }

    /// Answers Claude's editor and test tools that need no waiting.
    pub(super) fn answer_tool(
        &mut self,
        msg: AppMsg,
        caller: Option<PaneId>,
        cx: &mut Context<Self>,
    ) -> AppReply {
        match msg {
            AppMsg::OpenEditors => AppReply::Editors(self.open_editors(cx)),
            AppMsg::ReadBuffer { path, max_bytes } => match self.shared_editor(&path, cx) {
                Ok((_, editor)) => {
                    let e = editor.read(cx);
                    let text = e.text().unwrap_or_default();
                    AppReply::Buffer(BufferInfo {
                        path: e.path().to_path_buf(),
                        total_bytes: text.len() as u64,
                        text: cut(&text, max_bytes.min(MAX_BUFFER) as usize).to_string(),
                        dirty: e.is_dirty(),
                    })
                }
                Err(e) => AppReply::Error(e),
            },
            AppMsg::RunTests { path, name } => {
                let Some(root) = self.caller_root(caller) else {
                    return AppReply::Error("no project is open".into());
                };
                let path = match path.map(|p| resolve_in_roots(&p, std::slice::from_ref(&root))) {
                    Some(Err(e)) => return AppReply::Error(e),
                    Some(Ok(p)) => Some(self.project_spelling(&p)),
                    None => None,
                };
                match self.run_tests_for_claude(root, path, name, cx) {
                    Ok(started) => AppReply::Lines(vec![started]),
                    Err(e) => AppReply::Error(e),
                }
            }
            AppMsg::TestResults => match self.caller_root(caller) {
                Some(root) => {
                    let (running, report) = self.test_report_for_claude(&root);
                    AppReply::Tests(results(&root, running, report))
                }
                None => AppReply::Error("no project is open".into()),
            },
            _ => AppReply::Error("Athena could not answer that request".into()),
        }
    }

    fn open_editors(&self, cx: &Context<Self>) -> Vec<EditorInfo> {
        let focused = self.workspace.active_project().and_then(|p| {
            let item = p.layout.as_ref()?.focused_pane()?.active_item()?;
            Some((p.root.clone(), item.id))
        });
        let mut out = Vec::new();
        for project in &self.workspace.projects {
            for item in project.items() {
                let ItemKind::Editor { path } = &item.kind else {
                    continue;
                };
                let key = (project.root.clone(), item.id);
                let dirty = match self.items.get(&key) {
                    Some(ItemView::Editor(e)) => e.read(cx).is_dirty(),
                    _ => false,
                };
                out.push(EditorInfo {
                    path: path.clone(),
                    project: project.root.clone(),
                    dirty,
                    active: focused.as_ref() == Some(&key),
                });
            }
        }
        out
    }
}

fn no_server(doc: &Path) -> String {
    format!(
        "no language server has {} open; Athena runs gopls and typescript-language-server",
        doc.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use athena_lsp::Range;

    #[test]
    fn a_symbol_or_a_column_becomes_a_utf16_character() {
        let line = "\tlet café = ünïcode(x);";
        assert_eq!(character(line, None, Some("ünïcode")), Ok(12));
        assert_eq!(character("a😀b", None, Some("b")), Ok(3));
        assert_eq!(character(line, Some(5), None), Ok(4));
        assert_eq!(character(line, Some(5), Some("café")), Ok(5));
        assert!(character(line, None, Some("missing")).is_err());
        assert!(character(line, None, None).is_err());
        assert!(character(line, Some(0), None).is_err());
    }

    #[test]
    fn buffers_are_cut_on_a_character_boundary() {
        assert_eq!(cut("héllo", 2), "h");
        assert_eq!(cut("héllo", 3), "hé");
        assert_eq!(cut("abc", 10), "abc");
    }

    #[test]
    fn locations_are_1_based_capped_and_quote_only_shared_files() {
        let dir = std::env::temp_dir().join(format!("athena-mcp-loc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        std::fs::write(dir.join("a.go"), "package a\n\n  func A() {}\n").unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        let at = |file: &str, line: u32| Location {
            path: dir.join(file),
            range: Range {
                start: Position { line, character: 2 },
                end: Position { line, character: 8 },
            },
        };
        let mut found = vec![at("a.go", 2), at(".env", 0)];
        found.extend((0..300).map(|_| at("a.go", 0)));
        let list = located(found, std::slice::from_ref(&dir));
        assert_eq!(list.len(), MAX_LOCATIONS);
        assert_eq!(
            (list[0].line, list[0].column, list[0].end_column),
            (3, 3, 9)
        );
        assert_eq!(list[0].text.as_deref(), Some("func A() {}"));
        assert_eq!(list[1].text, None, "secrets are never quoted");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn symbol_names_are_cut_to_a_snippet() {
        let span = Range {
            start: Position {
                line: 4,
                character: 0,
            },
            end: Position {
                line: 9,
                character: 1,
            },
        };
        let info = symbol_info(Symbol {
            name: "n".repeat(5000),
            kind: 12,
            container: Some("c".repeat(5000)),
            path: PathBuf::from("/a.ts"),
            range: span,
            scope: span,
            parent: None,
        });
        assert_eq!(info.name.len(), SNIPPET);
        assert_eq!(info.container.map(|c| c.len()), Some(SNIPPET));
        assert_eq!((info.line, info.end_line), (5, 10));
    }

    #[test]
    fn results_list_failed_tests_and_suites_that_could_not_run() {
        use athena_testing::{Framework, Suite, TestCase};
        let case = |name: &str, outcome| TestCase {
            titles: vec![name.into()],
            line: None,
            outcome,
            duration_ms: 1,
            output: "x".repeat(MAX_OUTPUT + 10),
        };
        let suite = |name: &str, outcome, tests| Suite {
            framework: Framework::Go,
            name: name.into(),
            dir: PathBuf::from("/p"),
            file: None,
            outcome,
            output: "does not compile".into(),
            tests,
        };
        let report = Report {
            suites: vec![
                suite(
                    "example.com/a",
                    Outcome::Failed,
                    vec![
                        case("TestOK", Outcome::Passed),
                        case("TestBad", Outcome::Failed),
                    ],
                ),
                suite("example.com/b", Outcome::Failed, vec![]),
            ],
        };
        let info = results(Path::new("/p"), true, Some(&report));
        assert!(info.running);
        assert_eq!((info.passed, info.failed, info.skipped), (1, 2, 0));
        assert_eq!(info.failures.len(), 2);
        assert_eq!(info.failures[0].test, "TestBad");
        assert_eq!(info.failures[0].output.len(), MAX_OUTPUT);
        assert_eq!(
            (
                info.failures[1].test.as_str(),
                info.failures[1].output.as_str()
            ),
            ("", "does not compile")
        );
        assert_eq!(results(Path::new("/p"), false, None).failures, vec![]);
    }
}
