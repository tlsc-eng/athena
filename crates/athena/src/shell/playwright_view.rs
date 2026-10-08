use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use athena_playwright::{Outcome, Report, TestResult};
use athena_proto::NoticeKind;
use athena_ui::{ActiveTheme, ButtonKind, empty_state};
use athena_workspace::ItemId;
use gpui::{AnyElement, Context, ObjectFit, PromptLevel, Window, div, img, prelude::*, px};

use super::Shell;
use super::item::ItemView;

/// A test run typed into a terminal, waiting for its report.
struct Run {
    root: PathBuf,
    item: ItemId,
    /// The config's folder, where Playwright resolves relative paths.
    dir: PathBuf,
    report: PathBuf,
    /// Set once Node is in the foreground; prompt hooks (git, nodenv) briefly run other programs.
    started: bool,
}

#[derive(Default)]
pub(super) struct PlaywrightState {
    run: Option<Run>,
    reports: HashMap<PathBuf, (PathBuf, Report)>,
    expanded: Option<usize>,
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

impl Shell {
    fn playwright_root(&self) -> Option<(PathBuf, PathBuf)> {
        let root = self.workspace.active_project()?.root.clone();
        let config = athena_playwright::find_config(&root)?;
        Some((root, config))
    }

    /// Runs the project's suite in a new terminal tab, so the user's own Node setup applies.
    pub(super) fn run_playwright(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.playwright.run.is_some() {
            return;
        }
        let Some((root, config)) = self.playwright_root() else {
            return;
        };
        let Ok(runs) = athena_proto::data_dir().map(|d| d.join("runs")) else {
            return;
        };
        let _ = std::fs::create_dir_all(&runs);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let report = runs.join(format!("playwright-{stamp}.json"));
        let dir = config.parent().unwrap_or(&root).to_path_buf();
        let command = format!(
            "cd {} && PLAYWRIGHT_JSON_OUTPUT_NAME={} npx playwright test --reporter=list,json\r",
            shell_quote(&dir.to_string_lossy()),
            shell_quote(&report.to_string_lossy()),
        );
        self.new_terminal(window, cx);
        let Some(item) = self
            .workspace
            .active_project()
            .and_then(|p| p.layout.as_ref())
            .and_then(|l| l.focused_pane())
            .and_then(|p| p.active_item())
            .cloned()
        else {
            return;
        };
        if let Some(ItemView::Terminal(view)) = self.item_view(&root, &item, cx) {
            view.update(cx, |v, _| v.run_on_start(command));
        }
        self.playwright.run = Some(Run {
            root,
            item: item.id,
            dir,
            report,
            started: false,
        });
        self.playwright.expanded = None;
        cx.notify();
    }

    /// Called when a terminal changes; finishes a run once its shell is back at the prompt.
    pub(super) fn check_playwright_run(&mut self, cx: &mut Context<Self>) {
        let Some(run) = &mut self.playwright.run else {
            return;
        };
        let Some(ItemView::Terminal(view)) = self.items.get(&(run.root.clone(), run.item)) else {
            self.playwright.run = None;
            return;
        };
        let Some((name, _)) = view.read(cx).foreground() else {
            return;
        };
        if !athena_term::is_shell(&name) {
            run.started |= name == "node";
            return;
        }
        // A run that fails within one foreground poll never shows Node, but leaves no report either.
        if !run.started && !run.report.exists() {
            return;
        }
        let run = self.playwright.run.take().expect("checked above");
        let kind = match athena_playwright::read_report(&run.report) {
            Ok(report) => {
                let (passed, failed) =
                    (report.count(Outcome::Passed), report.count(Outcome::Failed));
                let flaky = report.count(Outcome::Flaky);
                self.playwright.expanded = report
                    .tests
                    .iter()
                    .position(|t| t.outcome == Outcome::Failed);
                self.playwright
                    .reports
                    .insert(run.root.clone(), (run.dir.clone(), report));
                let title = if failed == 0 {
                    "Playwright passed"
                } else {
                    "Playwright failed"
                };
                let mut body = format!("{passed} passed, {failed} failed");
                if flaky > 0 {
                    body.push_str(&format!(", {flaky} flaky"));
                }
                NoticeKind::Message {
                    title: title.into(),
                    body,
                }
            }
            Err(_) => NoticeKind::Message {
                title: "Playwright run ended without a report".into(),
                body: "See its terminal for the reason.".into(),
            },
        };
        let _ = std::fs::remove_file(&run.report);
        self.local_notice(kind, cx);
        cx.notify();
    }

    /// Interrupts the run with Ctrl-C and stops waiting for its report.
    fn stop_playwright(&mut self, cx: &mut Context<Self>) {
        let Some(run) = self.playwright.run.take() else {
            return;
        };
        if let Some(ItemView::Terminal(view)) = self.items.get(&(run.root, run.item)) {
            view.update(cx, |v, cx| v.type_text(vec![0x03], cx));
        }
        let _ = std::fs::remove_file(&run.report);
        cx.notify();
    }

    fn open_trace(dir: &Path, trace: &Path) {
        // Through a login shell so nodenv/Homebrew PATH entries are present, as in a terminal.
        let _ = Command::new("/bin/zsh")
            .args([
                "-lc",
                "cd \"$1\" && exec npx playwright show-trace \"$2\"",
                "athena",
            ])
            .arg(dir)
            .arg(trace)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    /// Writes or removes the Playwright MCP entry in `.mcp.json` after the user confirms.
    pub(super) fn set_playwright_mcp(
        &mut self,
        enable: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.workspace.active_project().map(|p| p.root.clone()) else {
            return;
        };
        let file = athena_playwright::mcp_json_path(&root);
        let detail = if enable {
            format!(
                "Athena will add a \"playwright\" server ({}) to {} so Claude Code can drive a fresh, \
                 isolated browser. Claude asks you to approve the server the first time. The file is \
                 listed in .git/info/exclude so it stays out of commits.",
                athena_playwright::MCP_PACKAGE,
                file.display()
            )
        } else {
            format!(
                "Athena will remove the \"playwright\" server from {}.",
                file.display()
            )
        };
        let button = if enable { "Add" } else { "Remove" };
        let answer = window.prompt(
            PromptLevel::Info,
            "Playwright MCP for Claude",
            Some(&detail),
            &[button, "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let result = athena_playwright::write_mcp_json(&root, enable, true);
            let _ = this.update(cx, |this, cx| {
                let (title, body) = match result {
                    Ok(()) => (
                        if enable {
                            "Playwright MCP added"
                        } else {
                            "Playwright MCP removed"
                        }
                        .to_string(),
                        file.display().to_string(),
                    ),
                    Err(e) => ("Could not update .mcp.json".to_string(), format!("{e:#}")),
                };
                this.local_notice(NoticeKind::Message { title, body }, cx);
            });
        })
        .detach();
    }

    pub(super) fn render_playwright_action(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.playwright_root()?;
        if self.playwright.run.is_some() {
            return Some(
                athena_ui::Button::new("playwright-stop", "Stop", ButtonKind::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.stop_playwright(cx)))
                    .into_any_element(),
            );
        }
        Some(
            athena_ui::Button::new("playwright-run", "Run tests", ButtonKind::Primary)
                .on_click(cx.listener(|this, _, window, cx| this.run_playwright(window, cx)))
                .into_any_element(),
        )
    }

    pub(super) fn render_playwright(&self, cx: &mut Context<Self>) -> AnyElement {
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
        let Some((root, _)) = self.playwright_root() else {
            return centered(empty_state(
                "No Playwright config",
                "Add playwright.config.ts to this project to run its tests here.",
                None,
                cx,
            ));
        };
        let Some((dir, report)) = self.playwright.reports.get(&root) else {
            let text = if self.playwright.run.is_some() {
                "Running…"
            } else {
                "No runs yet. Run tests to see results here."
            };
            return centered(
                div()
                    .text_size(t.typography.caption)
                    .text_color(t.color.content_muted)
                    .child(text),
            );
        };
        let rows: Vec<AnyElement> = report
            .tests
            .iter()
            .enumerate()
            .map(|(i, test)| self.render_test(i, test, dir, cx))
            .collect();
        div()
            .id("playwright")
            .size_full()
            .overflow_y_scroll()
            .text_size(t.typography.caption)
            .children(rows)
            .into_any_element()
    }

    fn render_test(
        &self,
        index: usize,
        test: &TestResult,
        dir: &Path,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let color = match test.outcome {
            Outcome::Passed => t.color.success,
            Outcome::Failed => t.color.danger,
            Outcome::Flaky => t.color.warning,
            Outcome::Skipped => t.color.content_disabled,
        };
        let expanded = self.playwright.expanded == Some(index);
        let row = div()
            .id(("pw-test", index))
            .h(px(28.))
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(10.))
            .cursor_pointer()
            .hover(|s| s.bg(t.color.surface_hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.playwright.expanded = if this.playwright.expanded == Some(index) {
                    None
                } else {
                    Some(index)
                };
                cx.notify();
            }))
            .child(div().size(px(6.)).flex_none().bg(color))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(t.color.content)
                    .child(test.title.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(t.color.content_disabled)
                    .child(test.project.clone()),
            )
            .child(
                div()
                    .w(px(64.))
                    .flex_none()
                    .text_color(t.color.content_muted)
                    .child(format!("{:.1}s", test.duration_ms as f64 / 1000.)),
            );
        if !expanded || (test.error.is_none() && test.attachments.is_empty()) {
            return row.into_any_element();
        }
        let screenshot = test
            .attachments
            .iter()
            .find(|a| a.content_type.starts_with("image/") && a.path.exists());
        let trace = test
            .attachments
            .iter()
            .find(|a| a.name == "trace" && a.path.exists())
            .map(|a| a.path.clone());
        let dir = dir.to_path_buf();
        div()
            .flex()
            .flex_col()
            .child(row)
            .child(
                div()
                    .pl(px(28.))
                    .pr(px(12.))
                    .pb(px(12.))
                    .flex()
                    .gap(px(16.))
                    .children(test.error.clone().map(|e| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family(t.typography.mono.clone())
                            .text_color(t.color.danger)
                            .child(e.lines().take(12).collect::<Vec<_>>().join("\n"))
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .children(screenshot.map(|s| {
                                img(s.path.clone())
                                    .w(px(280.))
                                    .h(px(170.))
                                    .object_fit(ObjectFit::Contain)
                                    .border_1()
                                    .border_color(t.color.border)
                            }))
                            .children(trace.map(|path| {
                                athena_ui::Button::new(
                                    ("pw-trace", index),
                                    "Open trace",
                                    ButtonKind::Secondary,
                                )
                                .on_click(move |_, _, _| Self::open_trace(&dir, &path))
                            })),
                    ),
            )
            .into_any_element()
    }
}
