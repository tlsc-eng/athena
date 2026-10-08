use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use athena_ui::ActiveTheme;
use athena_workspace::ItemKind;
use gpui::{AnyElement, Context, FontWeight, Window, div, prelude::*, px, uniform_list};

use super::Shell;

pub(super) const TREE_WIDTH: f32 = 240.;
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
        match &pane.active_item()?.kind {
            ItemKind::Editor { path } => Some(path.clone()),
            ItemKind::Terminal { .. } => None,
        }
    }

    pub(super) fn render_tree(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.tree_visible {
            return None;
        }
        let root = self.workspace.active_project()?.root.clone();
        let rows = self.tree.rows(&root);
        let open = self.open_editor_path();
        let t = cx.theme().clone();
        let count = rows.len();
        let list = uniform_list(
            "file-tree",
            count,
            cx.processor(move |_this, range: std::ops::Range<usize>, _window, cx| {
                rows[range]
                    .iter()
                    .map(|row| {
                        let path = row.entry.path.clone();
                        let is_dir = row.entry.is_dir;
                        let root = root.clone();
                        let selected = open.as_ref() == Some(&row.entry.path);
                        let marker = match (is_dir, row.expanded) {
                            (true, true) => "▾",
                            (true, false) => "▸",
                            (false, _) => "",
                        };
                        div()
                            .id(gpui::ElementId::Name(
                                row.entry.path.to_string_lossy().into_owned().into(),
                            ))
                            .h(px(ROW_HEIGHT))
                            .pl(px(12. + INDENT * row.depth as f32))
                            .pr(px(8.))
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .cursor_pointer()
                            .text_size(t.typography.caption)
                            .text_color(if selected {
                                t.color.accent
                            } else {
                                t.color.content_secondary
                            })
                            .when(selected, |el| el.font_weight(FontWeight::MEDIUM))
                            .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                            .on_click(cx.listener(move |this, _, window: &mut Window, cx| {
                                if is_dir {
                                    this.tree.toggle(&root, &path);
                                    cx.notify();
                                } else {
                                    this.open_file(path.clone(), window, cx);
                                }
                            }))
                            .child(
                                div()
                                    .w(px(10.))
                                    .flex_none()
                                    .text_color(t.color.content_muted)
                                    .child(marker),
                            )
                            .child(
                                div()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(row.entry.name.clone()),
                            )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1();
        Some(
            div()
                .w(px(TREE_WIDTH))
                .flex_none()
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
                .child(list)
                .into_any_element(),
        )
    }
}
