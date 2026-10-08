use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use athena_ui::ActiveTheme;
use athena_ui::motion::{self, Closing, Opening};
use athena_workspace::{Axis, UiState};
use gpui::{
    Animation, AnyElement, ClickEvent, Context, FontWeight, MouseButton, MouseDownEvent, Window,
    div, prelude::*, px, uniform_list,
};

use super::Shell;
use super::item::ItemView;
use super::panes::{DIVIDER_HIT, Drag, clamp_tree_width, resize_handle};

const ROW_HEIGHT: f32 = 24.;
const INDENT: f32 = 12.;

#[derive(Clone)]
struct DirEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

#[derive(Clone)]
struct Row {
    depth: usize,
    entry: DirEntry,
    expanded: bool,
}

/// Expanded folders per project and a cache of folder listings.
#[derive(Default)]
pub(super) struct FileTree {
    expanded: HashMap<PathBuf, HashSet<PathBuf>>,
    listings: HashMap<PathBuf, Vec<DirEntry>>,
}

impl FileTree {
    /// Drops cached listings so the next render re-reads the disk (e.g. after the window regains focus).
    pub fn invalidate(&mut self) {
        self.listings.clear();
    }

    fn listing(&mut self, dir: &Path) -> &[DirEntry] {
        self.listings
            .entry(dir.to_path_buf())
            .or_insert_with(|| read_dir(dir))
    }

    fn rows(&mut self, root: &Path) -> Vec<Row> {
        let expanded = self.expanded.get(root).cloned().unwrap_or_default();
        let mut rows = Vec::new();
        let mut stack: Vec<(usize, DirEntry)> = self
            .listing(root)
            .iter()
            .rev()
            .map(|e| (0, e.clone()))
            .collect();
        while let Some((depth, entry)) = stack.pop() {
            let open = entry.is_dir && expanded.contains(&entry.path);
            if open {
                for child in self.listing(&entry.path).iter().rev() {
                    stack.push((depth + 1, child.clone()));
                }
            }
            rows.push(Row {
                depth,
                entry,
                expanded: open,
            });
        }
        rows
    }

    fn toggle(&mut self, root: &Path, dir: &Path) {
        let set = self.expanded.entry(root.to_path_buf()).or_default();
        if !set.remove(dir) {
            set.insert(dir.to_path_buf());
            self.listings.remove(dir);
        }
    }
}

/// One folder's children: .gitignore honoured, `.git` and macOS clutter hidden, folders first.
fn read_dir(dir: &Path) -> Vec<DirEntry> {
    let mut entries: Vec<DirEntry> = ignore::WalkBuilder::new(dir)
        .max_depth(Some(1))
        .hidden(false)
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some(".git" | ".DS_Store")))
        .build()
        .flatten()
        .filter(|e| e.depth() == 1)
        .map(|e| DirEntry {
            name: e.file_name().to_string_lossy().into_owned(),
            is_dir: e.file_type().is_some_and(|t| t.is_dir()),
            path: e.into_path(),
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
}

impl Shell {
    fn open_editor_path(&self) -> Option<PathBuf> {
        let pane = self
            .workspace
            .active_project()?
            .layout
            .as_ref()?
            .focused_pane()?;
        pane.active_item()?.kind.file().cloned()
    }

    /// Cmd+B: shows or hides the tree, sliding it in from or out to the left.
    pub(super) fn toggle_tree(&mut self, cx: &mut Context<Self>) {
        let visible = !self.workspace.ui.tree_visible;
        self.workspace.ui.tree_visible = visible;
        if visible {
            self.tree_closing = None;
            self.tree_opening = Some(Opening::now());
        } else {
            let generation = self.next_generation();
            self.tree_closing = Some(Closing::new(generation));
            let t = cx.theme();
            let delay = motion::exit_delay(t.motion.reduced, t.motion.fast);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                let _ = this.update(cx, |this, cx| {
                    if this
                        .tree_closing
                        .is_some_and(|c| c.generation == generation)
                    {
                        this.tree_closing = None;
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        self.schedule_save(cx);
        cx.notify();
    }

    pub(super) fn render_tree(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.workspace.ui.tree_visible && self.tree_closing.is_none() {
            return None;
        }
        let root = self.workspace.active_project()?.root.clone();
        let rows = self.tree.rows(&root);
        let open = self.open_editor_path();
        let dirty: HashSet<PathBuf> = self
            .items
            .iter()
            .filter(|((r, _), v)| *r == root && v.is_dirty(cx))
            .filter_map(|(_, v)| match v {
                ItemView::Editor(e) => Some(e.read(cx).path().to_path_buf()),
                _ => None,
            })
            .collect();
        let t = cx.theme().clone();
        let git_theme = t.clone();
        let count = rows.len();
        let list = uniform_list(
            "file-tree",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                rows[range]
                    .iter()
                    .map(|row| {
                        let path = row.entry.path.clone();
                        let is_dir = row.entry.is_dir;
                        let root = root.clone();
                        let selected = open.as_ref() == Some(&row.entry.path);
                        let unsaved = dirty.contains(&row.entry.path);
                        let git = this.git_status_for(&row.entry.path);
                        let marker = match (is_dir, row.expanded) {
                            (true, true) => "▾",
                            (true, false) => "▸",
                            (false, _) => "",
                        };
                        div()
                            .id(gpui::ElementId::Name(
                                row.entry.path.to_string_lossy().into_owned().into(),
                            ))
                            .w_full()
                            .h(px(ROW_HEIGHT))
                            .pl(px(12. + INDENT * row.depth as f32))
                            .pr(px(8.))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .cursor_pointer()
                            .text_size(t.typography.caption)
                            .text_color(if selected {
                                t.color.accent
                            } else {
                                t.color.content_secondary
                            })
                            .when(selected, |el| {
                                el.font_weight(FontWeight::MEDIUM)
                                    .bg(t.color.surface_accent)
                            })
                            .when(!selected, |el| {
                                el.when_some(git, |el, s| {
                                    el.text_color(super::git_view::status_color(s, &git_theme))
                                })
                                .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                            })
                            .on_click(cx.listener(
                                move |this, event: &ClickEvent, window: &mut Window, cx| {
                                    if is_dir {
                                        this.tree.toggle(&root, &path);
                                        cx.notify();
                                    } else if event.modifiers().platform {
                                        this.open_file_beside(path.clone(), window, cx);
                                    } else {
                                        this.open_file(path.clone(), window, cx);
                                    }
                                },
                            ))
                            .child(
                                div()
                                    .w(px(10.))
                                    .flex_none()
                                    .text_color(t.color.content_muted)
                                    .child(marker),
                            )
                            .child(athena_ui::file_icon(&row.entry.path, is_dir, cx))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(row.entry.name.clone()),
                            )
                            .when(unsaved, |el| {
                                el.child(
                                    div().size(px(6.)).flex_none().bg(t.color.content_disabled),
                                )
                            })
                            .children(super::git_view::status_badge(git, is_dir, &git_theme))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1();
        let w = clamp_tree_width(self.workspace.ui.tree_width);
        let panel = div()
            .w(px(w))
            .h_full()
            .flex()
            .flex_col()
            .bg(t.color.surface)
            .border_r_1()
            .border_color(t.color.border)
            .child(
                div()
                    .h(px(32.))
                    .flex_none()
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(t.color.border)
                    .text_size(t.typography.caption)
                    .text_color(t.color.content_muted)
                    .child("Files"),
            )
            .child(list);
        // The box keeps its width throughout, so the panes beside it resize once, not per frame.
        let panel = match self.tree_closing {
            Some(closing) => motion::animate_exit(
                t.motion.reduced,
                panel,
                ("tree-close", closing.generation),
                t.motion.fast,
                move |el, d| el.ml(px(-w * d)).opacity(1. - d),
            ),
            None => motion::animate_enter(
                t.motion.reduced,
                self.tree_opening.is_some_and(|o| o.running(t.motion.base)),
                panel,
                "tree-open",
                Animation::new(t.motion.base).with_easing(motion::ease_enter()),
                move |el, d| el.ml(px(-w * (1. - d))).opacity(d),
            ),
        };
        Some(
            div()
                .w(px(w))
                .flex_none()
                .h_full()
                .overflow_hidden()
                .child(panel)
                .into_any_element(),
        )
    }

    /// The drag strip on the tree's right edge; a double-click restores the default width.
    pub(super) fn render_tree_handle(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.workspace.ui.tree_visible {
            return None;
        }
        let w = clamp_tree_width(self.workspace.ui.tree_width);
        Some(
            resize_handle("tree-edge", Axis::Horizontal, cx.theme().color.accent)
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(w - 1. - DIVIDER_HIT))
                .w(px(1. + 2. * DIVIDER_HIT))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        if event.click_count == 2 {
                            this.drag = None;
                            this.workspace.ui.tree_width = UiState::default().tree_width;
                            this.schedule_save(cx);
                            cx.notify();
                            return;
                        }
                        this.drag = Some(Drag::Tree {
                            start_x: event.position.x,
                            start_w: w,
                        });
                    }),
                )
                .into_any_element(),
        )
    }
}
