use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use athena_lsp::{Diagnostic, Position, Severity};
use athena_ui::{ActiveTheme, Tooltip};
use gpui::{AnyElement, Context, FontWeight, Hsla, div, prelude::*, px, uniform_list};

use super::Shell;
use super::drawer::DrawerTab;
use super::lsp::document_key;

const ROW_HEIGHT: f32 = 24.;

/// One diagnostic as the Problems tab and F8 see it.
#[derive(Clone)]
struct Problem {
    severity: Severity,
    at: Position,
    message: String,
    source: Option<String>,
}

enum Row {
    File {
        path: PathBuf,
        count: usize,
        collapsed: bool,
    },
    Problem {
        path: PathBuf,
        problem: Problem,
    },
}

#[derive(Default)]
pub(super) struct ProblemsState {
    collapsed: HashSet<PathBuf>,
    /// The problem last opened from the list or with F8.
    opened: Option<(PathBuf, Position)>,
}

/// Errors, then warnings, then information, each in file order, as VS Code lists them.
fn problems(list: &[Diagnostic]) -> Vec<Problem> {
    let mut out: Vec<Problem> = list
        .iter()
        .filter(|d| d.severity != Severity::Hint)
        .map(|d| Problem {
            severity: d.severity,
            at: d.range.start,
            message: d.message.lines().next().unwrap_or_default().to_string(),
            source: d.source.clone(),
        })
        .collect();
    out.sort_by_key(|p| (p.severity, p.at));
    out
}

/// The next problem after `from` in (file, position) order, wrapping round; `forward: false`
/// walks backwards.
fn step(
    places: &[(PathBuf, Position)],
    from: Option<(&Path, Position)>,
    forward: bool,
) -> Option<(PathBuf, Position)> {
    let key = |p: &(PathBuf, Position)| (p.0.clone(), p.1);
    let found = match (from, forward) {
        (None, true) => places.first(),
        (None, false) => places.last(),
        (Some((path, at)), true) => places
            .iter()
            .find(|p| key(p) > (path.to_path_buf(), at))
            .or(places.first()),
        (Some((path, at)), false) => places
            .iter()
            .rev()
            .find(|p| key(p) < (path.to_path_buf(), at))
            .or(places.last()),
    };
    found.cloned()
}

fn severity_color(severity: Severity, t: &athena_ui::Theme) -> Hsla {
    match severity {
        Severity::Error => t.color.danger,
        Severity::Warning => t.color.warning,
        Severity::Information | Severity::Hint => t.color.content_muted,
    }
}

impl Shell {
    /// Files of the active project with their problems, by path; paths are the project's own
    /// spelling, so opening one never duplicates a tab opened through a symlink.
    fn project_problems(&self) -> Vec<(PathBuf, Vec<Problem>)> {
        let Some(root) = self.active_root() else {
            return Vec::new();
        };
        let canonical = document_key(&root);
        let mut files: Vec<(PathBuf, Vec<Problem>)> = self
            .lsp_diagnostics_of(&root)
            .into_iter()
            .map(|(doc, list)| {
                let path = match doc.strip_prefix(&canonical) {
                    Ok(rest) => root.join(rest),
                    Err(_) => doc.to_path_buf(),
                };
                (path, problems(list))
            })
            .filter(|(_, list)| !list.is_empty())
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }

    /// Error, warning and information counts in the active project.
    fn problem_counts(&self) -> (usize, usize, usize) {
        let mut counts = (0, 0, 0);
        for (_, list) in self.project_problems() {
            for p in list {
                match p.severity {
                    Severity::Error => counts.0 += 1,
                    Severity::Warning => counts.1 += 1,
                    _ => counts.2 += 1,
                }
            }
        }
        counts
    }

    /// F8 / Shift+F8: the next or previous problem across the project's files.
    pub(super) fn go_to_problem(&mut self, forward: bool, cx: &mut Context<Self>) {
        let mut places: Vec<(PathBuf, Position)> = self
            .project_problems()
            .into_iter()
            .flat_map(|(path, list)| list.into_iter().map(move |p| (path.clone(), p.at)))
            .collect();
        places.sort();
        places.dedup();
        if places.is_empty() {
            return self.transient_notice("No problems", "This project has no problems.", cx);
        }
        let current = self.focused_editor().and_then(|e| {
            let e = e.read(cx);
            let (line, character) = e.cursor_utf16()?;
            Some((e.path().to_path_buf(), Position { line, character }))
        });
        let from = current.as_ref().map(|(p, at)| (p.as_path(), *at));
        let Some((path, at)) = step(&places, from, forward) else {
            return;
        };
        self.problems.opened = Some((path.clone(), at));
        self.lsp.jump = Some((path, at));
        cx.notify();
    }

    /// Cmd+Shift+M, or the title bar counter: opens the Problems tab or closes it.
    pub(super) fn toggle_problems(&mut self, cx: &mut Context<Self>) {
        self.toggle_drawer_tab(DrawerTab::Problems, cx);
    }

    /// The count beside the tab name, hidden when there is nothing to fix.
    pub(super) fn render_problems_badge(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (errors, warnings, infos) = self.problem_counts();
        let total = errors + warnings + infos;
        let t = cx.theme();
        (total > 0).then(|| {
            div()
                .ml(px(6.))
                .px(px(6.))
                .rounded(t.shape.radius_control)
                .bg(t.color.surface_active)
                .text_color(t.color.content_muted)
                .child(total.to_string())
                .into_any_element()
        })
    }

    /// Errors and warnings in the title bar, as VS Code's status bar shows them.
    pub(super) fn render_problems_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (errors, warnings, _) = self.problem_counts();
        let t = cx.theme().clone();
        let count = |n: usize, color: Hsla| {
            div()
                .flex()
                .items_center()
                .gap(px(4.))
                .child(div().size(px(6.)).flex_none().bg(if n > 0 {
                    color
                } else {
                    t.color.content_disabled
                }))
                .child(n.to_string())
        };
        let active = self.drawer == Some(DrawerTab::Problems);
        div()
            .id("problems-button")
            .h(px(24.))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(8.))
            .rounded(t.shape.radius_control)
            .cursor_pointer()
            .text_color(if active {
                t.color.content
            } else {
                t.color.content_muted
            })
            .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
            .tooltip(move |_, cx| {
                let text = match (errors, warnings) {
                    (0, 0) => "No problems  ⇧⌘M".to_string(),
                    _ => format!("{errors} errors, {warnings} warnings  ⇧⌘M"),
                };
                Tooltip::view(text, cx)
            })
            .on_click(cx.listener(|this, _, _, cx| this.toggle_problems(cx)))
            .child(count(errors, t.color.danger))
            .child(count(warnings, t.color.warning))
    }

    pub(super) fn render_problems(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let files = self.project_problems();
        if files.is_empty() {
            let text = if self.lsp_running_for_active() {
                "No problems have been detected in the project."
            } else {
                "Problems appear here once a Go or TypeScript file is open."
            };
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(t.typography.caption)
                .text_color(t.color.content_muted)
                .child(text)
                .into_any_element();
        }
        let root = self.active_root();
        let mut rows = Vec::new();
        for (path, list) in files {
            let collapsed = self.problems.collapsed.contains(&path);
            rows.push(Row::File {
                path: path.clone(),
                count: list.len(),
                collapsed,
            });
            if !collapsed {
                rows.extend(list.into_iter().map(|problem| Row::Problem {
                    path: path.clone(),
                    problem,
                }));
            }
        }
        let rows = Rc::new(rows);
        let opened = self.problems.opened.clone();
        uniform_list(
            "problems",
            rows.len(),
            cx.processor(move |_this, range: Range<usize>, _window, cx| {
                range
                    .map(|i| match &rows[i] {
                        Row::File {
                            path,
                            count,
                            collapsed,
                        } => {
                            let rel = root
                                .as_ref()
                                .and_then(|r| path.strip_prefix(r).ok())
                                .unwrap_or(path);
                            let name = rel
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let dir = rel
                                .parent()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default();
                            let toggle = path.clone();
                            div()
                                .id(("problem-file", i))
                                .w_full()
                                .h(px(ROW_HEIGHT))
                                .px(px(12.))
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .text_size(t.typography.caption)
                                .cursor_pointer()
                                .hover(|s| s.bg(t.color.surface_hover))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !this.problems.collapsed.remove(&toggle) {
                                        this.problems.collapsed.insert(toggle.clone());
                                    }
                                    cx.notify();
                                }))
                                .child(
                                    div()
                                        .w(px(10.))
                                        .flex_none()
                                        .text_color(t.color.content_muted)
                                        .child(if *collapsed { "▸" } else { "▾" }),
                                )
                                .child(athena_ui::file_icon(path, false, cx))
                                .child(
                                    div()
                                        .flex_none()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(t.color.content)
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_color(t.color.content_muted)
                                        .child(dir),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .px(px(6.))
                                        .rounded(t.shape.radius_control)
                                        .bg(t.color.surface_active)
                                        .text_color(t.color.content_muted)
                                        .child(count.to_string()),
                                )
                                .into_any_element()
                        }
                        Row::Problem { path, problem } => {
                            let target = (path.clone(), problem.at);
                            let selected = opened.as_ref() == Some(&target);
                            let place = format!(
                                "[Ln {}, Col {}]",
                                problem.at.line + 1,
                                problem.at.character + 1
                            );
                            div()
                                .id(("problem", i))
                                .w_full()
                                .h(px(ROW_HEIGHT))
                                .pl(px(40.))
                                .pr(px(12.))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .text_size(t.typography.caption)
                                .cursor_pointer()
                                .hover(|s| s.bg(t.color.surface_hover))
                                .when(selected, |el| el.bg(t.color.surface_active))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.problems.opened = Some(target.clone());
                                    this.lsp.jump = Some(target.clone());
                                    cx.notify();
                                }))
                                .child(
                                    div()
                                        .size(px(6.))
                                        .flex_none()
                                        .bg(severity_color(problem.severity, &t)),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_color(if selected {
                                            t.color.content
                                        } else {
                                            t.color.content_secondary
                                        })
                                        .child(problem.message.clone()),
                                )
                                .children(problem.source.clone().map(|s| {
                                    div().flex_none().text_color(t.color.content_muted).child(s)
                                }))
                                .child(
                                    div()
                                        .flex_none()
                                        .text_color(t.color.content_muted)
                                        .child(place),
                                )
                                .into_any_element()
                        }
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .size_full()
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn stepping_goes_through_files_in_order_and_wraps() {
        let a = PathBuf::from("/p/a.go");
        let b = PathBuf::from("/p/b.go");
        let places = vec![
            (a.clone(), at(2, 0)),
            (a.clone(), at(9, 4)),
            (b.clone(), at(1, 1)),
        ];
        assert_eq!(step(&places, None, true), Some((a.clone(), at(2, 0))));
        assert_eq!(step(&places, None, false), Some((b.clone(), at(1, 1))));
        assert_eq!(
            step(&places, Some((&a, at(2, 0))), true),
            Some((a.clone(), at(9, 4))),
            "the problem under the cursor is not revisited"
        );
        assert_eq!(
            step(&places, Some((&a, at(12, 0))), true),
            Some((b.clone(), at(1, 1)))
        );
        assert_eq!(
            step(&places, Some((&b, at(5, 0))), true),
            Some((a.clone(), at(2, 0))),
            "past the last problem it wraps"
        );
        assert_eq!(
            step(&places, Some((&a, at(2, 0))), false),
            Some((b.clone(), at(1, 1)))
        );
        let other = PathBuf::from("/p/c.go");
        assert_eq!(
            step(&places, Some((&other, at(0, 0))), false),
            Some((b, at(1, 1)))
        );
    }

    #[test]
    fn problems_list_errors_first_and_leave_out_hints() {
        let d = |severity, line| Diagnostic {
            range: athena_lsp::Range {
                start: at(line, 0),
                end: at(line, 1),
            },
            severity,
            message: format!("line {line}\nmore detail"),
            source: None,
            raw: serde_json::Value::Null,
        };
        let list = problems(&[
            d(Severity::Warning, 1),
            d(Severity::Hint, 2),
            d(Severity::Error, 7),
            d(Severity::Error, 3),
        ]);
        let order: Vec<(Severity, u32)> = list.iter().map(|p| (p.severity, p.at.line)).collect();
        assert_eq!(
            order,
            [
                (Severity::Error, 3),
                (Severity::Error, 7),
                (Severity::Warning, 1)
            ]
        );
        assert_eq!(list[0].message, "line 3", "only the first line is listed");
    }
}
