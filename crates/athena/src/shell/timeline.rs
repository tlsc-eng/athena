use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::SystemTime;

use athena_ui::{ActiveTheme, MenuItem, Tooltip};
use athena_workspace::DiffBase;
use athena_workspace::git::{self, LogEntry};
use gpui::{
    AnyElement, ClipboardItem, Context, MouseButton, MouseDownEvent, Task, Window, div, prelude::*,
    px, uniform_list,
};

use super::Shell;
use super::drawer::DrawerTab;
use super::git_view::under_root;
use super::item::file_label;
use super::menus::shell_item;
use super::review::short_rev;

const ROW_HEIGHT: f32 = 24.;

/// The file history the Timeline tab shows: the file it was read for and its commits.
#[derive(Default)]
pub(super) struct TimelineState {
    /// The project root and the file the list belongs to.
    file: Option<(PathBuf, PathBuf)>,
    entries: Rc<Vec<LogEntry>>,
    error: Option<String>,
    loading: Option<Task<()>>,
}

/// The diff a timeline entry opens: what that commit did to the file.
pub(super) fn commit_base(entry: &LogEntry) -> DiffBase {
    DiffBase::Commit {
        rev: entry.sha.clone(),
        old: entry.old_path.clone(),
        new: entry.path.clone(),
    }
}

/// The diff Compare with Current opens: the file as the commit left it against the file on disk.
pub(super) fn revision_base(entry: &LogEntry) -> DiffBase {
    DiffBase::Revision {
        rev: entry.sha.clone(),
        at: entry.path.clone(),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

impl Shell {
    /// The file the active tab shows, with its project root.
    fn timeline_target(&self) -> Option<(PathBuf, PathBuf)> {
        let project = self.workspace.active_project()?;
        let item = project.layout.as_ref()?.focused_pane()?.active_item()?;
        let file = item.kind.file()?.clone();
        Some((project.root.clone(), file))
    }

    /// "Open Timeline": the active file's history in the drawer.
    pub(super) fn open_timeline(&mut self, cx: &mut Context<Self>) {
        self.show_drawer_tab(DrawerTab::Timeline, cx);
        match self.timeline_target() {
            Some(target) => self.load_timeline(target, cx),
            None => {
                self.transient_notice("Open a file first", "The timeline lists its commits.", cx)
            }
        }
    }

    fn load_timeline(&mut self, (root, path): (PathBuf, PathBuf), cx: &mut Context<Self>) {
        let state = &mut self.review.timeline;
        state.file = Some((root.clone(), path.clone()));
        state.error = None;
        let git_path = under_root(&root, &path);
        state.loading = Some(cx.spawn(async move |this, cx| {
            let log = cx
                .background_executor()
                .spawn(async move { git::file_log(&root, &git_path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let state = &mut this.review.timeline;
                state.loading = None;
                match log {
                    Ok(entries) => state.entries = Rc::new(entries),
                    Err(err) => {
                        tracing::debug!("git log: {err:#}");
                        state.entries = Rc::default();
                        state.error = Some("This file has no git history here.".into());
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn open_timeline_entry(
        &mut self,
        entry: &LogEntry,
        compare_current: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, path)) = self.review.timeline.file.clone() else {
            return;
        };
        let base = match compare_current {
            true => revision_base(entry),
            false => commit_base(entry),
        };
        self.open_diff(path, base, window, cx);
    }

    fn open_timeline_menu(
        &mut self,
        entry: LogEntry,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sha = entry.sha.clone();
        let subject = entry.subject.clone();
        let changes = entry.clone();
        let items = vec![
            shell_item("Open Changes", cx, move |this, w, cx| {
                this.open_timeline_entry(&changes, false, w, cx)
            }),
            shell_item("Compare with Current", cx, move |this, w, cx| {
                this.open_timeline_entry(&entry, true, w, cx)
            }),
            MenuItem::separator(),
            MenuItem::new("Copy Commit ID", move |_, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(sha.clone()))
            }),
            MenuItem::new("Copy Commit Message", move |_, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(subject.clone()))
            }),
        ];
        self.open_context_menu(position, items, window, cx);
    }

    /// The file the list is for, shown beside the drawer tabs.
    pub(super) fn render_timeline_title(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (_, path) = self.review.timeline.file.as_ref()?;
        let n = self.review.timeline.entries.len();
        let count = match (self.review.timeline.loading.is_some(), n) {
            (true, _) => "Reading history…".to_string(),
            (false, 1) => "1 commit".into(),
            (false, n) if n >= git::LOG_LIMIT => format!("Latest {n} commits"),
            (false, n) => format!("{n} commits"),
        };
        Some(
            div()
                .text_color(cx.theme().color.content_muted)
                .child(format!("{} · {count}", file_label(path)))
                .into_any_element(),
        )
    }

    pub(super) fn render_timeline(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let message = |text: &str| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(t.typography.caption)
                .text_color(t.color.content_muted)
                .child(text.to_string())
                .into_any_element()
        };
        if !git::available() {
            return message("Git needs the Xcode Command Line Tools (xcode-select --install).");
        }
        // The list follows the active file, as VS Code's Timeline view does.
        if let Some(target) = self.timeline_target()
            && self.review.timeline.file.as_ref() != Some(&target)
        {
            self.load_timeline(target, cx);
        }
        let state = &self.review.timeline;
        if state.file.is_none() {
            return message("Open a file to see its history.");
        }
        if let Some(error) = &state.error {
            return message(error);
        }
        if state.entries.is_empty() {
            return match state.loading {
                Some(_) => message("Reading history…"),
                None => message("No commits touch this file yet."),
            };
        }
        let entries = state.entries.clone();
        let now = now();
        uniform_list(
            "timeline",
            entries.len(),
            cx.processor(move |_this, range: Range<usize>, _window, cx| {
                range
                    .map(|i| {
                        let e = &entries[i];
                        let hover = format!("timeline-row-{i}");
                        let (open, menu, compare) = (e.clone(), e.clone(), e.clone());
                        let tip = format!("{} · {}\n{}", short_rev(&e.sha), e.author, e.subject);
                        div()
                            .id(("timeline-row", i))
                            .group(hover.clone())
                            .w_full()
                            .h(t.ui(ROW_HEIGHT))
                            .px(px(12.))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .text_size(t.typography.caption)
                            .cursor_pointer()
                            .hover(|s| s.bg(t.color.surface_hover))
                            .tooltip(move |_, cx| Tooltip::view(tip.clone(), cx))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_timeline_entry(&open, false, window, cx)
                            }))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                    cx.stop_propagation();
                                    this.open_timeline_menu(menu.clone(), ev.position, window, cx)
                                }),
                            )
                            .child(
                                div()
                                    .w(px(6.))
                                    .h(px(6.))
                                    .flex_none()
                                    .rounded_full()
                                    .bg(t.color.content_muted),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_color(t.color.content)
                                    .child(e.subject.clone()),
                            )
                            .child(super::git_view::row_button(
                                ("timeline-compare", i),
                                "Compare with Current",
                                hover,
                                &t,
                                cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.open_timeline_entry(&compare, true, window, cx)
                                }),
                            ))
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(t.color.content_muted)
                                    .child(e.author.clone()),
                            )
                            .child(
                                div()
                                    .w(px(96.))
                                    .flex_none()
                                    .flex()
                                    .justify_end()
                                    .text_color(t.color.content_muted)
                                    .child(git::relative_time(now - e.time)),
                            )
                            .into_any_element()
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

    fn entry(old: Option<&str>, path: &str) -> LogEntry {
        LogEntry {
            sha: "0123456789".into(),
            parents: vec!["abcdef0".into()],
            author: "Ann".into(),
            time: 0,
            subject: "Move".into(),
            path: path.into(),
            old_path: old.map(str::to_string),
        }
    }

    #[test]
    fn a_renaming_commit_diffs_the_old_name_against_the_new_one() {
        let moved = entry(Some("src/a.rs"), "src/b.rs");
        assert_eq!(
            commit_base(&moved),
            DiffBase::Commit {
                rev: "0123456789".into(),
                old: Some("src/a.rs".into()),
                new: "src/b.rs".into(),
            }
        );
        assert_eq!(
            revision_base(&moved),
            DiffBase::Revision {
                rev: "0123456789".into(),
                at: "src/b.rs".into(),
            }
        );
        let added = entry(None, "src/a.rs");
        assert!(matches!(
            commit_base(&added),
            DiffBase::Commit { old: None, .. }
        ));
    }
}
