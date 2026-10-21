use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use athena_ui::motion::{self, Closing, Opening};
use athena_ui::{ActiveTheme, Ellipsis, Size, Space, TextInput};
use athena_workspace::{Axis, SideMode, UiState};
use gpui::{
    Animation, AnyElement, ClickEvent, Context, Entity, FontWeight, MouseButton, MouseDownEvent,
    ScrollStrategy, Subscription, UniformListScrollHandle, Window, div, prelude::*, px,
    uniform_list,
};

use super::Shell;
use super::dnd::{TabGhost, TreeDrag};
use super::item::ItemView;
use super::panes::{DIVIDER_HIT, Drag, clamp_tree_width, resize_handle};

const ROW_HEIGHT: f32 = 24.;
const INDENT: f32 = 12.;

#[derive(Clone, PartialEq)]
struct DirEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    /// Matched by .gitignore, or inside a folder that is; drawn dimmed.
    ignored: bool,
}

#[derive(Clone)]
struct Row {
    depth: usize,
    entry: DirEntry,
    expanded: bool,
    /// Drawn as the inline name field of [`FileTree::editing`].
    edit: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EditKind {
    NewFile,
    NewFolder,
    Rename,
}

/// The inline name field: `target` is the folder a new entry goes in, or the entry being renamed.
pub(super) struct Edit {
    /// The project whose tree shows the field.
    pub root: PathBuf,
    pub target: PathBuf,
    pub kind: EditKind,
    pub input: Entity<TextInput>,
    /// A rename waits on language servers; another Enter or blur must not submit it again.
    pub pending: bool,
    pub _subscriptions: Vec<Subscription>,
}

/// The entry an inline edit is for, and what it does.
type EditKey = (PathBuf, EditKind);

/// Expanded folders per project and a cache of folder listings.
#[derive(Default)]
pub(super) struct FileTree {
    expanded: HashMap<PathBuf, HashSet<PathBuf>>,
    listings: HashMap<PathBuf, Vec<DirEntry>>,
    /// Listings to read again; each keeps showing until its new one arrives.
    stale: HashSet<PathBuf>,
    /// Folders a render found missing or stale, for the next background read.
    wanted: HashSet<PathBuf>,
    reading: HashSet<PathBuf>,
    /// The last rows built, for the root and inline edit they were built with.
    shown: Option<(PathBuf, Option<EditKey>, Rc<Vec<Row>>)>,
    pub editing: Option<Edit>,
    scroll: UniformListScrollHandle,
    /// A path to bring into view at the next render.
    reveal: Option<PathBuf>,
}

impl FileTree {
    /// Has every listing read again, e.g. after the window regains focus.
    pub fn invalidate(&mut self) {
        self.stale.extend(self.listings.keys().cloned());
        self.stale.extend(self.reading.iter().cloned());
    }

    /// Has the listings a change to `path` can make stale read again: its folder's, and its own.
    pub fn forget(&mut self, path: &Path) {
        for dir in [Some(path), path.parent()].into_iter().flatten() {
            if self.listings.contains_key(dir) || self.reading.contains(dir) {
                self.stale.insert(dir.to_path_buf());
            }
        }
    }

    /// Opens every folder from `root` down to `path`'s parent and scrolls `path` into view.
    pub fn reveal(&mut self, root: &Path, path: &Path) {
        if let Some(parent) = path.parent() {
            self.expand(root, parent);
        }
        self.scroll_to(path);
    }

    /// Opens `dir` and every folder above it inside `root`.
    pub fn expand(&mut self, root: &Path, dir: &Path) {
        let set = self.expanded.entry(root.to_path_buf()).or_default();
        for d in dir.ancestors() {
            if d == root || !d.starts_with(root) {
                break;
            }
            set.insert(d.to_path_buf());
        }
        self.shown = None;
    }

    pub fn scroll_to(&mut self, path: &Path) {
        self.reveal = Some(path.to_path_buf());
    }

    /// What is known of `dir`, asking for a read when that is nothing or stale.
    fn listing(&mut self, dir: &Path) -> &[DirEntry] {
        let known = self.listings.get(dir);
        if (known.is_none() || self.stale.contains(dir)) && !self.reading.contains(dir) {
            self.wanted.insert(dir.to_path_buf());
        }
        self.listings.get(dir).map_or(&[], Vec::as_slice)
    }

    fn rows(&mut self, root: &Path) -> Rc<Vec<Row>> {
        let edit = self
            .editing
            .as_ref()
            .filter(|e| e.root == root)
            .map(|e| (e.target.clone(), e.kind));
        if let Some((r, e, rows)) = &self.shown
            && r == root
            && *e == edit
        {
            let expanded = self.expanded.get(root);
            let shown = |d: &&PathBuf| *d == root || expanded.is_some_and(|e| e.contains(*d));
            for dir in self.stale.iter().filter(shown) {
                if self.reading.contains(dir) {
                    continue;
                }
                self.wanted.insert(dir.clone());
            }
            return rows.clone();
        }
        let expanded = self.expanded.remove(root).unwrap_or_default();
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
                    let mut child = child.clone();
                    // A walk started inside an ignored folder cannot see the rule that matched it.
                    child.ignored |= entry.ignored;
                    stack.push((depth + 1, child));
                }
            }
            rows.push(Row {
                depth,
                entry,
                expanded: open,
                edit: false,
            });
        }
        self.expanded.insert(root.to_path_buf(), expanded);
        if let Some((target, kind)) = &edit {
            splice_edit(&mut rows, root, target, *kind);
        }
        let rows = Rc::new(rows);
        self.shown = Some((root.to_path_buf(), edit, rows.clone()));
        rows
    }

    fn toggle(&mut self, root: &Path, dir: &Path) {
        let set = self.expanded.entry(root.to_path_buf()).or_default();
        if !set.remove(dir) {
            set.insert(dir.to_path_buf());
            if self.listings.contains_key(dir) {
                self.stale.insert(dir.to_path_buf());
            }
        }
        self.shown = None;
    }
}

/// Marks the row being renamed, or inserts a blank row first inside the folder getting a new entry.
fn splice_edit(rows: &mut Vec<Row>, root: &Path, target: &Path, kind: EditKind) {
    if kind == EditKind::Rename {
        if let Some(row) = rows.iter_mut().find(|r| r.entry.path == target) {
            row.edit = true;
        }
        return;
    }
    let (at, depth) = match rows.iter().position(|r| r.entry.path == target) {
        Some(i) if target != root => (i + 1, rows[i].depth + 1),
        _ => (0, 0),
    };
    rows.insert(
        at,
        Row {
            depth,
            entry: DirEntry {
                name: String::new(),
                path: target.to_path_buf(),
                is_dir: kind == EditKind::NewFolder,
                ignored: false,
            },
            expanded: false,
            edit: true,
        },
    );
}

/// One folder's children, folders first, with `.git` and macOS clutter hidden; git-ignored
/// entries are marked, or left out when `exclude_ignored`.
fn read_dir(dir: &Path, exclude_ignored: bool) -> Vec<DirEntry> {
    let walk = |honour_ignores: bool| {
        ignore::WalkBuilder::new(dir)
            .max_depth(Some(1))
            .standard_filters(honour_ignores)
            .hidden(false)
            .filter_entry(|e| !matches!(e.file_name().to_str(), Some(".git" | ".DS_Store")))
            .build()
            .flatten()
            .filter(|e| e.depth() == 1)
    };
    let kept: HashSet<PathBuf> = walk(true).map(ignore::DirEntry::into_path).collect();
    let mut entries: Vec<DirEntry> = walk(exclude_ignored)
        .map(|e| DirEntry {
            name: e.file_name().to_string_lossy().into_owned(),
            is_dir: e.file_type().is_some_and(|t| t.is_dir()),
            ignored: !kept.contains(e.path()),
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
    pub(super) fn open_editor_path(&self) -> Option<PathBuf> {
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

    /// Reads the folders the tree asked for off the main thread, then shows them.
    fn read_tree_listings(&mut self, cx: &mut Context<Self>) {
        if self.tree.wanted.is_empty() {
            return;
        }
        let dirs: Vec<PathBuf> = self.tree.wanted.drain().collect();
        let exclude_ignored = self.settings.file.exclude_git_ignore();
        for dir in &dirs {
            self.tree.stale.remove(dir);
            self.tree.reading.insert(dir.clone());
        }
        cx.spawn(async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move {
                    dirs.into_iter()
                        .map(|dir| {
                            let listing = read_dir(&dir, exclude_ignored);
                            (dir, listing)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let tree = &mut this.tree;
                let mut changed = false;
                for (dir, listing) in read {
                    tree.reading.remove(&dir);
                    if tree.listings.get(&dir) != Some(&listing) {
                        tree.listings.insert(dir, listing);
                        changed = true;
                    }
                }
                if changed || tree.reading.is_empty() {
                    tree.shown = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn render_tree(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.workspace.ui.tree_visible && self.tree_closing.is_none() {
            return None;
        }
        let root = self.workspace.active_project()?.root.clone();
        let rows = self.tree.rows(&root);
        self.read_tree_listings(cx);
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
        let pending = !self.tree.reading.is_empty();
        if let Some(path) = self.tree.reveal.take_if(|_| !pending)
            && let Some(ix) = rows.iter().position(|r| r.entry.path == path)
        {
            // A new entry's field sits just below the folder it goes in.
            let below = rows
                .get(ix + 1)
                .is_some_and(|r| r.edit && r.entry.path == path);
            let ix = if below { ix + 1 } else { ix };
            self.tree.scroll.scroll_to_item(ix, ScrollStrategy::Center);
        }
        let edit_input = self.tree.editing.as_ref().map(|e| e.input.clone());
        let drop_root = root.clone();
        let (row_height, scale) = (t.ui(ROW_HEIGHT), t.ui_scale);
        let (pad_md, gap_sm, pad_xs) = (t.space(Space::Md), t.space(Space::Sm), t.space(Space::Xs));
        let (ui_6, ui_10, ui_20) = (t.ui(6.), t.ui(10.), t.ui(20.));
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
                        if row.edit
                            && let Some(input) = edit_input.clone()
                        {
                            return div()
                                .id("tree-edit-row")
                                .w_full()
                                .h(row_height)
                                .pl(px(scale * (12. + INDENT * row.depth as f32)))
                                .pr(pad_md)
                                .flex()
                                .items_center()
                                .gap(gap_sm)
                                .text_size(t.typography.caption)
                                .child(div().w(ui_10).flex_none())
                                .child(athena_ui::file_icon(&row.entry.path, is_dir, cx))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .h(ui_20)
                                        .px(pad_xs)
                                        .flex()
                                        .items_center()
                                        .bg(t.color.surface_sunken)
                                        .border_1()
                                        .border_color(t.color.accent)
                                        .child(input),
                                );
                        }
                        let selected = open.as_ref() == Some(&row.entry.path);
                        let unsaved = dirty.contains(&row.entry.path);
                        let ignored = row.entry.ignored;
                        let git = this.git_status_for(&row.entry.path).filter(|_| !ignored);
                        let drop_dir = match is_dir {
                            true => path.clone(),
                            false => path.parent().unwrap_or(&root).to_path_buf(),
                        };
                        let drag = TreeDrag {
                            path: path.clone(),
                            label: row.entry.name.clone().into(),
                        };
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
                            .h(row_height)
                            .pl(px(scale * (12. + INDENT * row.depth as f32)))
                            .pr(pad_md)
                            .flex()
                            .items_center()
                            .gap(gap_sm)
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
                                el.when(ignored, |el| el.text_color(t.color.content_disabled))
                                    .when_some(git, |el, s| {
                                        el.text_color(super::git_view::status_color(s, &git_theme))
                                    })
                                    .hover(|s| {
                                        let s = s.bg(t.color.surface_hover);
                                        match ignored {
                                            true => s,
                                            false => s.text_color(t.color.content),
                                        }
                                    })
                            })
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener({
                                    let path = path.clone();
                                    move |this, event: &MouseDownEvent, window, cx| {
                                        cx.stop_propagation();
                                        let target = Some((path.clone(), is_dir));
                                        this.open_tree_menu(target, event.position, window, cx);
                                    }
                                }),
                            )
                            .on_drag(drag, |drag: &TreeDrag, _, _, cx| {
                                let label = drag.label.clone();
                                cx.new(|_| TabGhost { label })
                            })
                            .when(is_dir, |el| {
                                let (dir, tint) = (path.clone(), t.color.surface_accent);
                                el.drag_over::<TreeDrag>(move |s, drag, window, _| {
                                    match drag.fits(&dir, window.modifiers().alt) {
                                        true => s.bg(tint),
                                        false => s,
                                    }
                                })
                            })
                            .on_drop(cx.listener(move |this, drag: &TreeDrag, window, cx| {
                                this.drop_tree_entry(drag, drop_dir.clone(), window, cx)
                            }))
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
                                    .w(ui_10)
                                    .flex_none()
                                    .text_color(t.color.content_muted)
                                    .child(marker),
                            )
                            .child(athena_ui::file_icon(&row.entry.path, is_dir, cx))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .ellipsis()
                                    .child(row.entry.name.clone()),
                            )
                            .when(unsaved, |el| {
                                el.child(div().size(ui_6).flex_none().bg(t.color.content_disabled))
                            })
                            .children(super::git_view::status_badge(git, is_dir, &git_theme))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.tree.scroll.clone())
        .flex_1();
        let w = clamp_tree_width(self.workspace.ui.tree_width);
        let switch = self.render_side_switch(cx);
        let body = self.render_side_body(list.into_any_element(), cx);
        let files = self.workspace.ui.side_mode == SideMode::Files;
        let panel = div()
            .w(px(w))
            .h_full()
            .flex()
            .flex_col()
            .bg(t.color.surface)
            .when(files, |el| {
                el.drag_over::<gpui::ExternalPaths>({
                    let tint = t.color.surface_accent;
                    move |s, _, _, _| s.bg(tint)
                })
            })
            .border_r_1()
            .border_color(t.color.border)
            .child(
                div()
                    .h(t.size(Size::TabBar))
                    .flex_none()
                    .px(t.space(Space::Md))
                    .flex()
                    .items_center()
                    .overflow_hidden()
                    .border_b_1()
                    .border_color(t.color.border)
                    .text_size(t.typography.caption)
                    .text_color(t.color.content_muted)
                    .child(switch),
            )
            .child(body)
            .when(files, |el| {
                el.on_drop(cx.listener(move |this, drag: &TreeDrag, window, cx| {
                    this.drop_tree_entry(drag, drop_root.clone(), window, cx)
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        this.open_tree_menu(None, event.position, window, cx)
                    }),
                )
            });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, depth: usize, is_dir: bool) -> Row {
        Row {
            depth,
            entry: DirEntry {
                name: Path::new(path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                path: PathBuf::from(path),
                is_dir,
                ignored: false,
            },
            expanded: is_dir,
            edit: false,
        }
    }

    fn rows() -> Vec<Row> {
        vec![
            row("/p/src", 0, true),
            row("/p/src/main.rs", 1, false),
            row("/p/README.md", 0, false),
        ]
    }

    #[test]
    fn a_new_entry_field_goes_first_inside_its_folder() {
        let mut r = rows();
        splice_edit(
            &mut r,
            Path::new("/p"),
            Path::new("/p/src"),
            EditKind::NewFile,
        );
        assert_eq!(r.len(), 4);
        assert!(r[1].edit && r[1].depth == 1 && !r[1].entry.is_dir);

        let mut r = rows();
        splice_edit(
            &mut r,
            Path::new("/p"),
            Path::new("/p"),
            EditKind::NewFolder,
        );
        assert!(r[0].edit && r[0].depth == 0 && r[0].entry.is_dir);
    }

    #[test]
    fn renaming_turns_the_entry_itself_into_the_field() {
        let mut r = rows();
        splice_edit(
            &mut r,
            Path::new("/p"),
            Path::new("/p/README.md"),
            EditKind::Rename,
        );
        assert_eq!(r.len(), 3);
        assert_eq!(r.iter().filter(|r| r.edit).count(), 1);
        assert!(r[2].edit);
    }

    #[test]
    fn reveal_opens_every_folder_above_the_file_but_not_the_root() {
        let mut tree = FileTree::default();
        let root = Path::new("/p");
        tree.reveal(root, Path::new("/p/a/b/c.rs"));
        let open = &tree.expanded[root];
        assert!(open.contains(Path::new("/p/a")) && open.contains(Path::new("/p/a/b")));
        assert!(!open.contains(root));
        assert_eq!(tree.reveal.as_deref(), Some(Path::new("/p/a/b/c.rs")));
    }

    #[test]
    fn read_dir_marks_git_ignored_entries_or_leaves_them_out() {
        let dir = std::env::temp_dir().join(format!("athena-tree-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join(".gitignore"), ".env\ntarget/\n").unwrap();
        std::fs::write(dir.join(".env"), "").unwrap();
        std::fs::write(dir.join("main.rs"), "").unwrap();
        let shown = |exclude| {
            read_dir(&dir, exclude)
                .into_iter()
                .map(|e| (e.name, e.ignored))
                .collect::<Vec<_>>()
        };
        let all = shown(false);
        let kept = shown(true);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            all,
            [
                ("target".into(), true),
                (".env".into(), true),
                (".gitignore".into(), false),
                ("main.rs".into(), false),
            ]
        );
        assert_eq!(
            kept,
            [(".gitignore".into(), false), ("main.rs".into(), false)]
        );
    }
}
