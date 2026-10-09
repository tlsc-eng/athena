use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use athena_editor::{EditorEvent, EditorView, RunMark, RunState, RunTestAt, TestSymbol};
use athena_lsp::Position;
use athena_testing::{
    Ended, Framework, GoModule, Job, Outcome, Report, Stop, Suite, TestCase, find_go_module,
    find_js_package, go_job, go_run_pattern, go_subtest_pattern, js_job, js_name_pattern,
};
use athena_ui::{ActiveTheme, ButtonKind, Theme, empty_state};
use gpui::{
    AnyElement, Context, Entity, EntityId, FontWeight, Hsla, Subscription, Task, WeakEntity,
    Window, div, prelude::*, px,
};

use super::Shell;
use super::drawer::DrawerTab;
use crate::actions;

/// `go test` stops a package after ten minutes; a whole run gets a little longer than that.
const RUN_LIMIT: Duration = Duration::from_secs(15 * 60);
/// Typing pauses this long before a test file is searched for tests again.
const DETECT_DELAY: Duration = Duration::from_millis(300);
const OUTPUT_LINES: usize = 200;

/// Which tests a run covers, so their marks show it running.
#[derive(Clone, Debug, PartialEq)]
enum Scope {
    All,
    /// A Go package's folder, and the tests asked for (all when `None`).
    Dir(PathBuf, Option<Vec<Vec<String>>>),
    /// A JavaScript test file, and the tests asked for (all when `None`).
    File(PathBuf, Option<Vec<Vec<String>>>),
}

impl Scope {
    fn covers(&self, path: &Path, titles: &[String]) -> bool {
        let named = |names: &Option<Vec<Vec<String>>>| {
            names.as_ref().is_none_or(|names| {
                names
                    .iter()
                    .any(|n| titles.starts_with(n) || n.starts_with(titles))
            })
        };
        match self {
            Self::All => true,
            Self::Dir(dir, names) => path.parent() == Some(dir.as_path()) && named(names),
            Self::File(file, names) => path == file && named(names),
        }
    }
}

/// A job to start, with the module its Go package paths are read against.
struct Planned {
    job: Job,
    module: Option<GoModule>,
    scope: Scope,
}

struct ActiveRun {
    root: PathBuf,
    stop: Arc<Stop>,
    scopes: Vec<Scope>,
}

impl Drop for ActiveRun {
    fn drop(&mut self) {
        self.stop.kill_now();
    }
}

struct Watched {
    editor: WeakEntity<EditorView>,
    symbols: Vec<TestSymbol>,
    detect: Option<Task<()>>,
    _subscription: Subscription,
}

#[derive(Default)]
pub(super) struct TestsState {
    run: Option<ActiveRun>,
    /// Results by project root, later runs folded into earlier ones.
    reports: HashMap<PathBuf, Report>,
    /// The suite and test whose output is open; no titles for the suite's own output.
    expanded: Option<(String, Vec<String>)>,
    watched: HashMap<EntityId, Watched>,
}

/// How the tests under a mark did: failed if any failed, passed if any passed.
fn symbol_state(titles: &[String], tests: &[TestCase]) -> Option<RunState> {
    let mut found = tests.iter().filter(|t| t.titles.starts_with(titles));
    let first = found.next()?;
    let mut state = outcome_state(first.outcome);
    for t in found {
        state = match (state, outcome_state(t.outcome)) {
            (RunState::Failed, _) | (_, RunState::Failed) => RunState::Failed,
            (RunState::Passed, _) | (_, RunState::Passed) => RunState::Passed,
            (s, _) => s,
        };
    }
    Some(state)
}

fn outcome_state(outcome: Outcome) -> RunState {
    match outcome {
        Outcome::Passed => RunState::Passed,
        Outcome::Failed => RunState::Failed,
        Outcome::Skipped => RunState::Skipped,
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || (a
            .canonicalize()
            .ok()
            .is_some_and(|x| b.canonicalize().ok() == Some(x)))
}

/// The suite in `report` that holds the tests of the file at `path`.
fn suite_for<'a>(report: &'a Report, path: &Path) -> Option<&'a Suite> {
    let go = path.extension().is_some_and(|e| e == "go");
    report.suites.iter().find(|s| match s.framework {
        Framework::Go => go && path.parent().is_some_and(|d| same_file(d, &s.dir)),
        _ => !go && s.file.as_deref().is_some_and(|f| same_file(f, path)),
    })
}

/// The innermost test or group around a zero-based line.
fn symbol_at(symbols: &[TestSymbol], line: usize) -> Option<&TestSymbol> {
    symbols
        .iter()
        .filter(|s| s.line <= line && line <= s.end_line)
        .max_by_key(|s| s.line)
}

fn report_file() -> Option<PathBuf> {
    let runs = athena_proto::data_dir().ok()?.join("runs");
    std::fs::create_dir_all(&runs).ok()?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    Some(runs.join(format!("tests-{stamp}.json")))
}

/// `go test` for the package in `dir`, limited to the tests or subtests `wanted` (all when empty).
fn plan_go(root: &Path, dir: &Path, mut wanted: Vec<Vec<String>>) -> Option<Planned> {
    let module = find_go_module(dir, root)?;
    wanted.sort();
    wanted.dedup();
    // One `-run` cannot name subtests of different tests, so several mean their whole tests.
    if wanted.len() > 1 {
        wanted.iter_mut().for_each(|t| t.truncate(1));
        wanted.dedup();
    }
    let pattern = match wanted.as_slice() {
        [] => None,
        [one] => Some(go_subtest_pattern(one)),
        many => Some(go_run_pattern(
            &many.iter().map(|t| t[0].clone()).collect::<Vec<_>>(),
        )),
    };
    let wanted = (!wanted.is_empty()).then_some(wanted);
    Some(Planned {
        job: go_job(&module, &[module.package_arg(dir)], pattern),
        module: Some(module),
        scope: Scope::Dir(dir.to_path_buf(), wanted),
    })
}

/// Vitest or Jest for the test file, limited to the test or group `titles` names.
fn plan_js(root: &Path, file: &Path, titles: Option<(&[String], bool)>) -> Option<Planned> {
    let (package, framework) = find_js_package(file.parent()?, root)?;
    let pattern = titles.map(|(t, group)| js_name_pattern(t, !group));
    let job = js_job(
        framework,
        &package,
        &[file.to_path_buf()],
        pattern,
        report_file()?,
    );
    Some(Planned {
        job,
        module: None,
        scope: Scope::File(file.to_path_buf(), titles.map(|(t, _)| vec![t.to_vec()])),
    })
}

/// The run for some tests of the file at `path`: those named by `titles`, or all of its tests.
fn plan_file(
    root: &Path,
    path: &Path,
    titles: Option<(&[String], bool)>,
    symbols: &[TestSymbol],
) -> Option<Planned> {
    if path.extension().is_none_or(|e| e != "go") {
        return plan_js(root, path, titles);
    }
    // A Go package's tests are named, not filed; a file's own are the ones it declares.
    let names: Vec<Vec<String>> = match titles {
        Some((titles, _)) if !titles.is_empty() => vec![titles.to_vec()],
        Some(_) => return None,
        None => symbols.iter().map(|s| vec![s.titles[0].clone()]).collect(),
    };
    if names.is_empty() {
        return None;
    }
    plan_go(root, path.parent()?, names)
}

/// Every Go package and the Vitest or Jest suite at the project root.
fn plan_all(root: &Path) -> Vec<Planned> {
    let mut out = Vec::new();
    if let Some(module) = find_go_module(root, root) {
        out.push(Planned {
            job: go_job(&module, &["./...".into()], None),
            module: Some(module),
            scope: Scope::All,
        });
    }
    if let (Some((package, framework)), Some(report)) = (find_js_package(root, root), report_file())
    {
        out.push(Planned {
            job: js_job(framework, &package, &[], None, report),
            module: None,
            scope: Scope::All,
        });
    }
    out
}

/// One run per suite that failed: its failed tests, or the whole suite when it could not run.
fn plan_failed(root: &Path, report: &Report) -> Vec<Planned> {
    report
        .failed()
        .into_iter()
        .filter_map(|(suite, tests)| match (suite.framework, &suite.file) {
            (Framework::Go, _) => {
                let names = tests.iter().map(|t| vec![t.titles[0].clone()]).collect();
                plan_go(root, &suite.dir, names)
            }
            (_, Some(file)) => {
                let mut planned = plan_js(root, file, None)?;
                if !tests.is_empty() {
                    let patterns: Vec<String> = tests
                        .iter()
                        .map(|t| js_name_pattern(&t.titles, true))
                        .collect();
                    planned.job.args.push("-t".into());
                    planned.job.args.push(patterns.join("|"));
                    let wanted = tests.iter().map(|t| t.titles.clone()).collect();
                    planned.scope = Scope::File(file.clone(), Some(wanted));
                }
                Some(planned)
            }
            _ => None,
        })
        .collect()
}

fn summary(report: &Report) -> String {
    let mut parts = Vec::new();
    for (n, word) in [
        (report.count(Outcome::Failed), "failed"),
        (report.broken(), "could not run"),
        (report.count(Outcome::Passed), "passed"),
        (report.count(Outcome::Skipped), "skipped"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {word}"));
        }
    }
    if parts.is_empty() {
        "No tests ran".into()
    } else {
        parts.join(", ")
    }
}

fn outcome_color(outcome: Outcome, t: &Theme) -> Hsla {
    match outcome {
        Outcome::Passed => t.color.success,
        Outcome::Failed => t.color.danger,
        Outcome::Skipped => t.color.content_disabled,
    }
}

impl Shell {
    /// Follows a test file's editor: finds its tests as it changes and keeps its marks current.
    pub(super) fn watch_tests(&mut self, editor: &Entity<EditorView>, cx: &mut Context<Self>) {
        let id = editor.entity_id();
        self.tests
            .watched
            .retain(|_, w| w.editor.upgrade().is_some());
        if self.tests.watched.contains_key(&id)
            || !athena_editor::is_test_file(editor.read(cx).path())
        {
            return;
        }
        let subscription = cx.subscribe(editor, |this, editor, event, cx| match event {
            EditorEvent::Opened | EditorEvent::Saved => this.detect_tests(&editor, false, cx),
            EditorEvent::Edited { .. } => this.detect_tests(&editor, true, cx),
            _ => {}
        });
        self.tests.watched.insert(
            id,
            Watched {
                editor: editor.downgrade(),
                symbols: Vec::new(),
                detect: None,
                _subscription: subscription,
            },
        );
        self.detect_tests(editor, false, cx);
    }

    fn detect_tests(&mut self, editor: &Entity<EditorView>, wait: bool, cx: &mut Context<Self>) {
        let id = editor.entity_id();
        let weak = editor.downgrade();
        let task = cx.spawn(async move |this, cx| {
            if wait {
                cx.background_executor().timer(DETECT_DELAY).await;
            }
            let Ok(Some((lang, path, text))) = weak.read_with(cx, |e, _| {
                let lang = e.status()?.lang?;
                Some((lang, e.path().to_path_buf(), e.text()?))
            }) else {
                return;
            };
            let symbols = cx
                .background_executor()
                .spawn(async move { athena_editor::find_tests(lang, &path, &text) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(w) = this.tests.watched.get_mut(&id) {
                    w.symbols = symbols;
                    w.detect = None;
                }
                this.push_run_marks(cx);
            });
        });
        if let Some(w) = self.tests.watched.get_mut(&id) {
            w.detect = Some(task);
        }
    }

    /// Gives every watched editor its marks from the latest results and the run under way.
    fn push_run_marks(&mut self, cx: &mut Context<Self>) {
        let mut updates = Vec::new();
        for w in self.tests.watched.values() {
            let Some(editor) = w.editor.upgrade() else {
                continue;
            };
            let path = editor.read(cx).path().to_path_buf();
            let root = self
                .workspace
                .projects
                .iter()
                .map(|p| p.root.clone())
                .find(|r| path.starts_with(r));
            let tests = root
                .as_ref()
                .and_then(|r| self.tests.reports.get(r))
                .and_then(|report| suite_for(report, &path))
                .map(|s| s.tests.as_slice())
                .unwrap_or_default();
            let run = self
                .tests
                .run
                .as_ref()
                .filter(|r| root.as_ref() == Some(&r.root));
            let marks: Vec<RunMark> = w
                .symbols
                .iter()
                .map(|s| {
                    let running =
                        run.is_some_and(|r| r.scopes.iter().any(|sc| sc.covers(&path, &s.titles)));
                    let state = if running {
                        RunState::Running
                    } else {
                        symbol_state(&s.titles, tests).unwrap_or(RunState::Idle)
                    };
                    RunMark {
                        line: s.line,
                        state,
                    }
                })
                .collect();
            updates.push((editor, marks));
        }
        for (editor, marks) in updates {
            editor.update(cx, |e, cx| e.set_run_marks(marks, cx));
        }
    }

    fn symbols_of(&self, path: &Path, cx: &Context<Self>) -> Vec<TestSymbol> {
        self.tests
            .watched
            .values()
            .find(|w| {
                w.editor
                    .upgrade()
                    .is_some_and(|e| e.read(cx).path() == path)
            })
            .map(|w| w.symbols.clone())
            .unwrap_or_default()
    }

    fn root_of(&self, path: &Path) -> Option<PathBuf> {
        self.workspace
            .projects
            .iter()
            .map(|p| p.root.clone())
            .find(|r| path.starts_with(r))
    }

    /// The gutter's run mark: runs the test or group declared on `line`.
    fn run_test_at(&mut self, path: PathBuf, line: usize, cx: &mut Context<Self>) {
        let symbols = self.symbols_of(&path, cx);
        let (Some(root), Some(symbol)) =
            (self.root_of(&path), symbols.iter().find(|s| s.line == line))
        else {
            return;
        };
        let planned = plan_file(&root, &path, Some((&symbol.titles, symbol.group)), &symbols);
        self.start_tests(root, planned.into_iter().collect(), cx);
    }

    fn run_test_at_cursor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.focused_editor() else {
            return;
        };
        let (path, line) = {
            let e = editor.read(cx);
            (e.path().to_path_buf(), e.cursor_line())
        };
        let symbols = self.symbols_of(&path, cx);
        match symbol_at(&symbols, line) {
            Some(symbol) => self.run_test_at(path, symbol.line, cx),
            None => self.transient_notice(
                "No test at the cursor",
                "Put the cursor in a Go test function or an it/test/describe block.",
                cx,
            ),
        }
    }

    fn run_tests_in_file(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.focused_editor() else {
            return;
        };
        let path = editor.read(cx).path().to_path_buf();
        let symbols = self.symbols_of(&path, cx);
        let planned = self
            .root_of(&path)
            .and_then(|root| Some((plan_file(&root, &path, None, &symbols)?, root)));
        match planned {
            Some((planned, root)) => self.start_tests(root, vec![planned], cx),
            None => self.transient_notice(
                "No tests in this file",
                "Athena runs Go tests and Vitest or Jest test files.",
                cx,
            ),
        }
    }

    fn run_all_tests(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        let planned = plan_all(&root);
        if planned.is_empty() {
            return self.transient_notice(
                "No tests found",
                "Athena runs go test where go.mod is, and Vitest or Jest from package.json, at \
                 the project root.",
                cx,
            );
        }
        self.start_tests(root, planned, cx);
    }

    fn rerun_failed_tests(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        let planned = self
            .tests
            .reports
            .get(&root)
            .map(|r| plan_failed(&root, r))
            .unwrap_or_default();
        if planned.is_empty() {
            return self.transient_notice("No failed tests", "The last run had no failures.", cx);
        }
        self.start_tests(root, planned, cx);
    }

    /// Runs one suite's test (or the whole suite for `None`) again from the Tests tab.
    fn rerun_case(&mut self, suite: &Suite, titles: Option<Vec<String>>, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        let planned = match (suite.framework, &suite.file) {
            (Framework::Go, _) => plan_go(&root, &suite.dir, titles.into_iter().collect()),
            (_, Some(file)) => plan_js(&root, file, titles.as_deref().map(|t| (t, false))),
            _ => None,
        };
        self.start_tests(root, planned.into_iter().collect(), cx);
    }

    fn stop_tests(&mut self, cx: &mut Context<Self>) {
        if let Some(run) = &self.tests.run {
            run.stop.request();
        }
        cx.notify();
    }

    /// The app is going, so test processes must not outlive it.
    pub(super) fn tests_quit(&mut self) {
        self.tests.run = None;
    }

    /// Runs the jobs one after another in the background, folding each one's results in.
    fn start_tests(&mut self, root: PathBuf, planned: Vec<Planned>, cx: &mut Context<Self>) {
        if planned.is_empty() {
            return;
        }
        if self.tests.run.is_some() {
            return self.transient_notice(
                "Tests are already running",
                "Stop them from the Tests tab first.",
                cx,
            );
        }
        let stop = Arc::new(Stop::default());
        self.tests.run = Some(ActiveRun {
            root: root.clone(),
            stop: stop.clone(),
            scopes: planned.iter().map(|p| p.scope.clone()).collect(),
        });
        self.push_run_marks(cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let mut errors: Vec<String> = Vec::new();
            let mut ran = Report::default();
            let mut cancelled = false;
            for p in planned {
                let stop = stop.clone();
                let done = cx
                    .background_executor()
                    .spawn(async move {
                        let finished = athena_testing::run(&p.job, RUN_LIMIT, &stop)?;
                        // Jest held open by a leftover handle times out after writing its report.
                        let report = match finished.ended {
                            Ended::Cancelled => Ok(Report::default()),
                            _ => athena_testing::report(&p.job, &finished, p.module.as_ref()),
                        };
                        if let Some(file) = &p.job.report {
                            let _ = std::fs::remove_file(file);
                        }
                        anyhow::Ok((finished.ended, report, p.job.framework))
                    })
                    .await;
                match done {
                    Ok((Ended::Cancelled, ..)) => {
                        cancelled = true;
                        break;
                    }
                    Ok((ended, report, framework)) => {
                        if ended == Ended::TimedOut {
                            errors.push(format!(
                                "{} did not finish within {} minutes.",
                                framework.label(),
                                RUN_LIMIT.as_secs() / 60
                            ));
                        }
                        let report = match report {
                            Ok(report) => report,
                            Err(_) if ended == Ended::TimedOut => continue,
                            Err(err) => {
                                errors.push(format!("{}: {err:#}", framework.label()));
                                continue;
                            }
                        };
                        ran.merge(report.clone());
                        let root = root.clone();
                        let _ = this.update(cx, |this, cx| {
                            this.tests.reports.entry(root).or_default().merge(report);
                            this.push_run_marks(cx);
                            cx.notify();
                        });
                    }
                    Err(err) => errors.push(format!("{err:#}")),
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.tests_finished(ran, errors, cancelled, cx)
            });
        })
        .detach();
    }

    fn tests_finished(
        &mut self,
        ran: Report,
        errors: Vec<String>,
        cancelled: bool,
        cx: &mut Context<Self>,
    ) {
        self.tests.run = None;
        self.push_run_marks(cx);
        let failed = ran.count(Outcome::Failed) + ran.broken() > 0;
        if failed {
            let first = ran.failed().into_iter().next().map(|(suite, tests)| {
                let titles = tests.first().map(|t| t.titles.clone()).unwrap_or_default();
                (suite.name.clone(), titles)
            });
            self.tests.expanded = first;
        }
        if cancelled {
            self.transient_notice("Test run stopped", summary(&ran), cx);
        } else if let Some(error) = errors.first().filter(|_| ran.suites.is_empty()) {
            self.transient_notice("Tests could not run", error.clone(), cx);
            self.show_drawer_tab(DrawerTab::Tests, cx);
        } else {
            let title = if failed || !errors.is_empty() {
                "Tests failed"
            } else {
                "Tests passed"
            };
            let mut body = summary(&ran);
            if let Some(error) = errors.first() {
                body.push_str(&format!(". {error}"));
            }
            self.transient_notice(title, body, cx);
            if failed {
                self.show_drawer_tab(DrawerTab::Tests, cx);
            }
        }
        cx.notify();
    }

    /// Opens a test's source: at the reporter's line, or where a Go test function starts.
    fn go_to_test(&mut self, suite: &Suite, test: &TestCase, cx: &mut Context<Self>) {
        if let Some(file) = suite.file.clone() {
            let line = test.line.unwrap_or(1).saturating_sub(1);
            self.lsp.jump = Some((file, Position { line, character: 0 }));
            return cx.notify();
        }
        let (dir, name) = (suite.dir.clone(), test.titles[0].clone());
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move { find_go_func(&dir, &name) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some((file, line)) = found {
                    this.lsp.jump = Some((file, Position { line, character: 0 }));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn render_tests_action(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = cx.theme().clone();
        let root = self.active_root()?;
        let report = self.tests.reports.get(&root);
        let running = self.tests.run.as_ref().is_some_and(|r| r.root == root);
        let any_failed = report.is_some_and(|r| !r.failed().is_empty());
        let button = |id: &'static str, label: &'static str, kind: ButtonKind| {
            athena_ui::Button::new(id, label, kind)
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .children(report.map(|r| {
                    div().text_color(t.color.content_muted).child(if running {
                        "Running…".to_string()
                    } else {
                        summary(r)
                    })
                }))
                .when(running, |el| {
                    el.child(
                        button("tests-stop", "Stop", ButtonKind::Secondary)
                            .on_click(cx.listener(|this, _, _, cx| this.stop_tests(cx))),
                    )
                })
                .when(!running && any_failed, |el| {
                    el.child(
                        button("tests-rerun", "Re-run Failed", ButtonKind::Secondary)
                            .on_click(cx.listener(|this, _, _, cx| this.rerun_failed_tests(cx))),
                    )
                })
                .when(!running, |el| {
                    el.child(
                        button("tests-run-all", "Run All", ButtonKind::Primary)
                            .on_click(cx.listener(|this, _, _, cx| this.run_all_tests(cx))),
                    )
                })
                .into_any_element(),
        )
    }

    pub(super) fn render_tests(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let centered = |el: gpui::Div| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(el)
                .into_any_element()
        };
        let Some(root) = self.active_root() else {
            return centered(div());
        };
        let Some(report) = self
            .tests
            .reports
            .get(&root)
            .filter(|r| !r.suites.is_empty())
        else {
            let running = self.tests.run.as_ref().is_some_and(|r| r.root == root);
            return centered(if running {
                div()
                    .text_size(t.typography.caption)
                    .text_color(t.color.content_muted)
                    .child("Running…")
            } else {
                empty_state(
                    "No test results",
                    "Run All, or click ▶ beside a test in a Go or Vitest/Jest test file.",
                    None,
                    cx,
                )
            });
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        for (si, suite) in report.suites.iter().enumerate() {
            rows.push(self.render_suite_row(si, suite, &root, cx));
            let open_suite =
                self.tests.expanded.as_ref() == Some(&(suite.name.clone(), Vec::new()));
            if open_suite && !suite.output.trim().is_empty() {
                rows.push(self.render_output(&suite.output, &suite.dir, ("suite-out", si), cx));
            }
            for (ti, test) in suite.tests.iter().enumerate() {
                let id = si * 100_000 + ti;
                rows.push(self.render_test_row(id, suite, test, cx));
                let open = self.tests.expanded.as_ref()
                    == Some(&(suite.name.clone(), test.titles.clone()));
                if open && !test.output.trim().is_empty() {
                    rows.push(self.render_output(&test.output, &suite.dir, ("test-out", id), cx));
                }
            }
        }
        div()
            .id("tests")
            .size_full()
            .overflow_y_scroll()
            .text_size(t.typography.caption)
            .children(rows)
            .into_any_element()
    }

    fn render_suite_row(
        &self,
        index: usize,
        suite: &Suite,
        root: &Path,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let status = suite.status();
        let label = match &suite.file {
            Some(file) => file
                .strip_prefix(root)
                .unwrap_or(file)
                .display()
                .to_string(),
            None => suite.name.clone(),
        };
        let broken = suite.outcome == Outcome::Failed
            && !suite.tests.iter().any(|x| x.outcome == Outcome::Failed);
        let detail = if broken {
            "could not run".to_string()
        } else {
            let passed = suite
                .tests
                .iter()
                .filter(|x| x.outcome == Outcome::Passed)
                .count();
            format!("{passed}/{} passed", suite.tests.len())
        };
        let name = suite.name.clone();
        let rerun = suite.clone();
        let group = format!("tests-suite-{index}");
        div()
            .id(("tests-suite", index))
            .group(group.clone())
            .h(px(26.))
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(8.))
            .cursor_pointer()
            .hover(|s| s.bg(t.color.surface_hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                let key = (name.clone(), Vec::new());
                this.tests.expanded = (this.tests.expanded.as_ref() != Some(&key)).then_some(key);
                cx.notify();
            }))
            .child(div().size(px(6.)).flex_none().bg(outcome_color(status, &t)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.color.content)
                    .child(label),
            )
            .child(row_link(
                ("tests-suite-run", index),
                "Run",
                group,
                &t,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.rerun_case(&rerun, None, cx)
                }),
            ))
            .child(
                div()
                    .flex_none()
                    .text_color(if broken {
                        t.color.danger
                    } else {
                        t.color.content_muted
                    })
                    .child(detail),
            )
            .into_any_element()
    }

    fn render_test_row(
        &self,
        id: usize,
        suite: &Suite,
        test: &TestCase,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let depth = test.titles.len() as f32;
        let key = (suite.name.clone(), test.titles.clone());
        let (open_suite, open_test) = (suite.clone(), test.clone());
        let (run_suite, run_titles) = (suite.clone(), test.titles.clone());
        let group = format!("tests-row-{id}");
        div()
            .id(("tests-case", id))
            .group(group.clone())
            .h(px(24.))
            .pl(px(12. + 14. * depth))
            .pr(px(12.))
            .flex()
            .items_center()
            .gap(px(8.))
            .cursor_pointer()
            .hover(|s| s.bg(t.color.surface_hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.tests.expanded =
                    (this.tests.expanded.as_ref() != Some(&key)).then(|| key.clone());
                cx.notify();
            }))
            .child(
                div()
                    .size(px(6.))
                    .flex_none()
                    .bg(outcome_color(test.outcome, &t)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(t.color.content)
                    .child(test.titles.last().cloned().unwrap_or_default()),
            )
            .child(row_link(
                ("tests-open", id),
                "Go to Test",
                group.clone(),
                &t,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.go_to_test(&open_suite, &open_test, cx)
                }),
            ))
            .child(row_link(
                ("tests-run", id),
                "Run",
                group,
                &t,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.rerun_case(&run_suite, Some(run_titles.clone()), cx)
                }),
            ))
            .child(
                div()
                    .w(px(56.))
                    .flex_none()
                    .text_color(t.color.content_muted)
                    .child(format!("{} ms", test.duration_ms)),
            )
            .into_any_element()
    }

    /// Output lines with their `file:line` places clickable.
    fn render_output(
        &self,
        output: &str,
        dir: &Path,
        id: (&'static str, usize),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let lines: Vec<AnyElement> = output
            .lines()
            .take(OUTPUT_LINES)
            .enumerate()
            .map(|(n, line)| {
                let mut parts: Vec<AnyElement> = Vec::new();
                let mut at = 0;
                for (k, link) in athena_testing::links(line).into_iter().enumerate() {
                    parts.push(line[at..link.span.start].to_string().into_any_element());
                    let target = athena_testing::resolve(dir, &link.path);
                    let at_pos = Position {
                        line: link.line.saturating_sub(1),
                        character: link.column.unwrap_or(1).saturating_sub(1),
                    };
                    parts.push(
                        div()
                            .id(("tests-link", n * 64 + k))
                            .cursor_pointer()
                            .text_color(t.color.accent)
                            .hover(|s| s.underline())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.lsp.jump = Some((target.clone(), at_pos));
                                cx.notify();
                            }))
                            .child(line[link.span.clone()].to_string())
                            .into_any_element(),
                    );
                    at = link.span.end;
                }
                parts.push(line[at..].to_string().into_any_element());
                div()
                    .flex()
                    .whitespace_nowrap()
                    .children(parts)
                    .into_any_element()
            })
            .collect();
        div()
            .id(id)
            .mx(px(12.))
            .mb(px(8.))
            .px(px(10.))
            .py(px(6.))
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .font_family(t.typography.mono.clone())
            .text_color(t.color.content_secondary)
            .overflow_x_scroll()
            .children(lines)
            .into_any_element()
    }
}

/// Where `func name(` starts among the package's test files, zero-based.
fn find_go_func(dir: &Path, name: &str) -> Option<(PathBuf, u32)> {
    let needle = format!("func {name}(");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with("_test.go"))
        .find_map(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            let line = text.lines().position(|l| l.starts_with(&needle))?;
            Some((p, line as u32))
        })
}

/// A small text button shown while its row is hovered.
fn row_link(
    id: (&'static str, usize),
    label: &'static str,
    group: String,
    t: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .px(px(6.))
        .rounded(t.shape.radius_control)
        .invisible()
        .group_hover(group, |s| s.visible())
        .cursor_pointer()
        .text_color(t.color.content_muted)
        .hover(|s| s.bg(t.color.surface_active).text_color(t.color.content))
        .on_click(on_click)
        .child(label)
}

/// Binds the test commands and the gutter's run marks on the shell's root element.
pub(super) fn bind_test_actions(el: gpui::Div, cx: &mut Context<Shell>) -> gpui::Div {
    el.on_action(
        cx.listener(|this, a: &RunTestAt, _, cx| this.run_test_at(a.path.clone(), a.line, cx)),
    )
    .on_action(cx.listener(|this, _: &actions::ShowTests, _, cx| {
        this.toggle_drawer_tab(DrawerTab::Tests, cx)
    }))
    .on_action(cx.listener(|this, _: &actions::RunTestAtCursor, _, cx| this.run_test_at_cursor(cx)))
    .on_action(cx.listener(|this, _: &actions::RunTestsInFile, _, cx| this.run_tests_in_file(cx)))
    .on_action(cx.listener(|this, _: &actions::RunAllTests, _, cx| this.run_all_tests(cx)))
    .on_action(
        cx.listener(|this, _: &actions::RerunFailedTests, _, cx| this.rerun_failed_tests(cx)),
    )
    .on_action(cx.listener(|this, _: &actions::StopTests, _, cx| this.stop_tests(cx)))
}

impl Shell {
    /// Starts tests for Claude's run_tests tool: all of the project's, a Go package's (named by
    /// a folder or any file in it) or a JavaScript test file's, optionally one test by name.
    pub(super) fn run_tests_for_claude(
        &mut self,
        root: PathBuf,
        path: Option<PathBuf>,
        name: Option<String>,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        if self.tests.run.is_some() {
            return Err("Tests are already running; call get_test_results.".into());
        }
        let names: Vec<String> = name.iter().cloned().collect();
        let planned: Vec<Planned> = match &path {
            None if name.is_some() => {
                return Err("Give the file or folder that holds the test.".into());
            }
            None => plan_all(&root),
            Some(dir) if dir.is_dir() => plan_go(&root, dir, names).into_iter().collect(),
            Some(file) if file.extension().is_some_and(|e| e == "go") => file
                .parent()
                .and_then(|dir| plan_go(&root, dir, names))
                .into_iter()
                .collect(),
            Some(file) => plan_js(
                &root,
                file,
                name.as_ref().map(|n| (std::slice::from_ref(n), true)),
            )
            .into_iter()
            .collect(),
        };
        if planned.is_empty() {
            return Err(
                "No tests found there. Athena runs go test where go.mod is, and Vitest \
                        or Jest from package.json."
                    .into(),
            );
        }
        let jobs = planned.len();
        self.start_tests(root, planned, cx);
        Ok(format!(
            "Started {jobs} test run{}; call get_test_results for the outcome.",
            if jobs == 1 { "" } else { "s" }
        ))
    }

    /// Whether tests of the project at `root` are running, and its results so far.
    pub(super) fn test_report_for_claude(&self, root: &Path) -> (bool, Option<&Report>) {
        let running = self.tests.run.as_ref().is_some_and(|r| r.root == root);
        (running, self.tests.reports.get(root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(titles: &[&str], outcome: Outcome) -> TestCase {
        TestCase {
            titles: titles.iter().map(|t| t.to_string()).collect(),
            line: None,
            outcome,
            duration_ms: 0,
            output: String::new(),
        }
    }

    fn strings(titles: &[&str]) -> Vec<String> {
        titles.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn a_mark_takes_the_worst_of_the_tests_under_it() {
        let tests = [
            case(&["TestSub"], Outcome::Failed),
            case(&["TestSub", "one"], Outcome::Passed),
            case(&["TestSub", "two"], Outcome::Failed),
            case(&["TestOK"], Outcome::Passed),
            case(&["math", "skipped"], Outcome::Skipped),
        ];
        assert_eq!(
            symbol_state(&strings(&["TestSub"]), &tests),
            Some(RunState::Failed)
        );
        assert_eq!(
            symbol_state(&strings(&["TestSub", "one"]), &tests),
            Some(RunState::Passed)
        );
        assert_eq!(
            symbol_state(&strings(&["TestOK"]), &tests),
            Some(RunState::Passed)
        );
        assert_eq!(
            symbol_state(&strings(&["math"]), &tests),
            Some(RunState::Skipped)
        );
        assert_eq!(symbol_state(&strings(&["TestNew"]), &tests), None);
    }

    #[test]
    fn a_run_covers_its_package_file_or_named_tests() {
        let file = Path::new("/m/sub/a_test.go");
        let named = Scope::Dir(PathBuf::from("/m/sub"), Some(vec![strings(&["TestA"])]));
        assert!(named.covers(file, &strings(&["TestA"])));
        assert!(!named.covers(file, &strings(&["TestB"])));
        assert!(!named.covers(Path::new("/m/other/a_test.go"), &strings(&["TestA"])));
        let group = Scope::File(
            PathBuf::from("/w/x.test.ts"),
            Some(vec![strings(&["math"])]),
        );
        assert!(group.covers(Path::new("/w/x.test.ts"), &strings(&["math", "adds"])));
        assert!(group.covers(Path::new("/w/x.test.ts"), &strings(&["math"])));
        assert!(!group.covers(Path::new("/w/x.test.ts"), &strings(&["top"])));
        assert!(Scope::All.covers(file, &strings(&["TestZ"])));
    }

    #[test]
    fn the_cursor_picks_the_innermost_test() {
        let symbol = |line, end_line, titles: &[&str], group| TestSymbol {
            line,
            end_line,
            titles: strings(titles),
            group,
        };
        let symbols = [
            symbol(1, 10, &["math"], true),
            symbol(2, 4, &["math", "adds"], false),
            symbol(12, 14, &["top"], false),
        ];
        assert_eq!(symbol_at(&symbols, 3).unwrap().titles, ["math", "adds"]);
        assert_eq!(symbol_at(&symbols, 6).unwrap().titles, ["math"]);
        assert!(symbol_at(&symbols, 11).is_none());
    }

    #[test]
    fn go_runs_name_the_package_and_test_from_the_module_root() {
        let dir = std::env::temp_dir().join(format!("athena-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("svc")).unwrap();
        std::fs::write(dir.join("go.mod"), "module example.com/m\n").unwrap();
        let file = dir.join("svc/a_test.go");
        let symbols = [TestSymbol {
            line: 4,
            end_line: 6,
            titles: strings(&["TestA"]),
            group: false,
        }];
        let one = plan_file(&dir, &file, Some((&symbols[0].titles, false)), &symbols).unwrap();
        assert_eq!(one.job.dir, dir);
        assert_eq!(one.job.args, ["test", "-json", "-run", "^TestA$", "./svc"]);
        assert_eq!(
            one.scope,
            Scope::Dir(dir.join("svc"), Some(vec![strings(&["TestA"])]))
        );
        assert_eq!(plan_all(&dir)[0].job.args, ["test", "-json", "./..."]);
        assert!(plan_file(&dir, &file, None, &[]).is_none());

        let sub = strings(&["TestA", "adds_1+1"]);
        let one_sub = plan_file(&dir, &file, Some((&sub, false)), &symbols).unwrap();
        assert_eq!(
            one_sub.job.args,
            ["test", "-json", "-run", r"^TestA$/^adds_1\+1$", "./svc"]
        );
        assert!(one_sub.scope.covers(&file, &strings(&["TestA"])));
        assert!(one_sub.scope.covers(&file, &sub));
        assert!(!one_sub.scope.covers(&file, &strings(&["TestA", "other"])));
        let mixed = plan_go(&dir, &dir.join("svc"), vec![sub, strings(&["TestB", "x"])]).unwrap();
        assert_eq!(
            mixed.job.args,
            ["test", "-json", "-run", "^(TestA|TestB)$", "./svc"]
        );

        let failed = Report {
            suites: vec![Suite {
                framework: Framework::Go,
                name: "example.com/m/svc".into(),
                dir: dir.join("svc"),
                file: None,
                outcome: Outcome::Failed,
                output: String::new(),
                tests: vec![
                    case(&["TestA"], Outcome::Failed),
                    case(&["TestA", "sub"], Outcome::Failed),
                    case(&["TestB"], Outcome::Passed),
                ],
            }],
        };
        let again = plan_failed(&dir, &failed);
        assert_eq!(again.len(), 1);
        assert_eq!(
            again[0].job.args,
            ["test", "-json", "-run", "^TestA$", "./svc"]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn summaries_lead_with_failures() {
        let r = Report {
            suites: vec![Suite {
                framework: Framework::Vitest,
                name: "/w/a.test.ts".into(),
                dir: PathBuf::from("/w"),
                file: Some(PathBuf::from("/w/a.test.ts")),
                outcome: Outcome::Failed,
                output: String::new(),
                tests: vec![
                    case(&["a"], Outcome::Passed),
                    case(&["b"], Outcome::Failed),
                    case(&["c"], Outcome::Skipped),
                ],
            }],
        };
        assert_eq!(summary(&r), "1 failed, 1 passed, 1 skipped");
        assert_eq!(summary(&Report::default()), "No tests ran");
    }
}
