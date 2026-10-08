use std::path::{Path, PathBuf};

use athena_ui::{ContextMenu, InputEvent, MenuItem, TextInput};
use athena_workspace::{Axis, ItemId, PaneId};
use gpui::{
    App, ClipboardItem, Context, DismissEvent, Focusable, Pixels, Point, PromptLevel, SharedString,
    Window, prelude::*,
};

use super::Shell;
use super::fileops;
use super::item::ItemView;
use super::tree::{Edit, EditKind};

/// A menu row that runs `f` on the shell, if it is still around.
fn shell_item(
    label: impl Into<SharedString>,
    cx: &Context<Shell>,
    f: impl Fn(&mut Shell, &mut Window, &mut Context<Shell>) + 'static,
) -> MenuItem {
    let this = cx.entity().downgrade();
    MenuItem::new(label, move |window, cx| {
        this.update(cx, |this, cx| f(this, window, cx)).ok();
    })
}

fn copy_item(label: &'static str, text: String) -> MenuItem {
    MenuItem::new(label, move |_, cx: &mut App| {
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))
    })
}

fn reveal_in_finder(path: PathBuf, cx: &mut App) {
    cx.background_executor()
        .spawn(async move {
            if let Err(err) = std::process::Command::new("/usr/bin/open")
                .arg("-R")
                .arg(&path)
                .status()
            {
                tracing::warn!("could not reveal {} in Finder: {err}", path.display());
            }
        })
        .detach();
}

/// Rows every file or folder menu offers: Finder and the two path forms.
fn path_items(root: &Path, path: &Path) -> Vec<MenuItem> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let shown = path.to_path_buf();
    let mut items = vec![
        MenuItem::new("Reveal in Finder", move |_, cx| {
            reveal_in_finder(shown.clone(), cx)
        }),
        copy_item("Copy Path", path.display().to_string()),
    ];
    if path != root {
        items.push(copy_item(
            "Copy Relative Path",
            relative.display().to_string(),
        ));
    }
    items
}

impl Shell {
    pub(super) fn open_context_menu(
        &mut self,
        position: Point<Pixels>,
        items: Vec<MenuItem>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu = ContextMenu::build(position, items, window, cx);
        let subscription = cx.subscribe_in(&menu, window, |this, menu, _: &DismissEvent, _, cx| {
            if this.context_menu.as_ref().is_some_and(|(m, _)| m == menu) {
                this.context_menu = None;
                cx.notify();
            }
        });
        self.context_menu = Some((menu, subscription));
        cx.notify();
    }

    /// Right-click in the file tree: on an entry, or on the empty space below (the project root).
    pub(super) fn open_tree_menu(
        &mut self,
        target: Option<(PathBuf, bool)>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root() else {
            return;
        };
        let (path, is_dir) = target.clone().unwrap_or((root.clone(), true));
        let folder = match is_dir {
            true => path.clone(),
            false => path.parent().unwrap_or(&root).to_path_buf(),
        };
        let mut items = vec![
            shell_item("New File…", cx, {
                let folder = folder.clone();
                move |this, w, cx| this.begin_tree_edit(folder.clone(), EditKind::NewFile, w, cx)
            }),
            shell_item("New Folder…", cx, {
                let folder = folder.clone();
                move |this, w, cx| this.begin_tree_edit(folder.clone(), EditKind::NewFolder, w, cx)
            }),
        ];
        if target.is_some() {
            items.push(shell_item("Rename…", cx, {
                let path = path.clone();
                move |this, w, cx| this.begin_tree_edit(path.clone(), EditKind::Rename, w, cx)
            }));
            items.push(shell_item("Delete", cx, {
                let path = path.clone();
                move |this, w, cx| this.confirm_trash(path.clone(), w, cx)
            }));
        }
        items.push(MenuItem::separator());
        items.extend(path_items(&root, &path));
        if !is_dir {
            items.push(MenuItem::separator());
            items.push(shell_item("Open to the Side", cx, move |this, w, cx| {
                this.open_file_beside(path.clone(), w, cx)
            }));
        }
        self.open_context_menu(position, items, window, cx);
    }

    /// Shows the inline name field for a new entry in folder `target`, or for renaming `target`.
    fn begin_tree_edit(
        &mut self,
        target: PathBuf,
        kind: EditKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root() else {
            return;
        };
        self.tree.editing = None;
        if !self.workspace.ui.tree_visible {
            self.toggle_tree(cx);
        }
        let (placeholder, initial) = match kind {
            EditKind::NewFile => ("File name", String::new()),
            EditKind::NewFolder => ("Folder name", String::new()),
            EditKind::Rename => (
                "New name",
                target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
        };
        let input = cx.new(|cx| {
            let mut input = TextInput::new(placeholder, cx);
            input.set_text(initial, cx);
            input
        });
        let focus = input.focus_handle(cx);
        let subscriptions = vec![
            cx.subscribe_in(
                &input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Submit | InputEvent::SubmitBeside => {
                        this.submit_tree_edit(true, window, cx)
                    }
                    InputEvent::Cancel => this.end_tree_edit(true, window, cx),
                    InputEvent::Changed | InputEvent::Up | InputEvent::Down => {}
                },
            ),
            cx.on_blur(&focus, window, {
                let field = focus.clone();
                move |this, window, cx| {
                    let Some(edit) = this.tree.editing.as_ref() else {
                        return;
                    };
                    let same_project = this.active_root().as_ref() == Some(&edit.root);
                    let outcome = blur_outcome(
                        window.is_window_active(),
                        field.is_focused(window),
                        this.focus.contains_focused(window, cx),
                        same_project,
                    );
                    match outcome {
                        BlurOutcome::Commit => this.submit_tree_edit(false, window, cx),
                        BlurOutcome::Cancel { refocus } => this.end_tree_edit(refocus, window, cx),
                        BlurOutcome::Keep => {}
                    }
                }
            }),
        ];
        if kind != EditKind::Rename {
            self.tree.expand(&root, &target);
        }
        self.tree.scroll_to(&target);
        self.tree.editing = Some(Edit {
            root,
            target,
            kind,
            input,
            _subscriptions: subscriptions,
        });
        window.focus(&focus);
        cx.notify();
    }

    fn end_tree_edit(&mut self, refocus: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.tree.editing.take().is_some() && refocus {
            self.focus_active_item(window, cx);
        }
        cx.notify();
    }

    /// Creates or renames from the inline field; `by_enter` keeps the field open on an error.
    fn submit_tree_edit(&mut self, by_enter: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.tree.editing.as_ref() else {
            return;
        };
        let name = edit.input.read(cx).text().trim().to_string();
        let (target, kind) = (edit.target.clone(), edit.kind);
        let unchanged = name.is_empty()
            || kind == EditKind::Rename && target.file_name() == Some(name.as_ref());
        if unchanged {
            return self.end_tree_edit(by_enter, window, cx);
        }
        let result = match valid_name(&name) {
            Err(err) => Err(err),
            Ok(()) => match kind {
                EditKind::NewFile => {
                    let path = target.join(&name);
                    fileops::create_file(&path).map(|()| path)
                }
                EditKind::NewFolder => {
                    let path = target.join(&name);
                    fileops::create_dir(&path).map(|()| path)
                }
                EditKind::Rename => {
                    let path = target.with_file_name(&name);
                    fileops::rename(&target, &path).map(|()| path)
                }
            },
        };
        let path = match result {
            Ok(path) => path,
            Err(err) => {
                self.transient_notice("Could not do that", format!("{err:#}"), cx);
                if !by_enter {
                    self.end_tree_edit(false, window, cx);
                }
                return;
            }
        };
        self.tree.editing = None;
        self.tree.invalidate();
        self.git_kick(cx);
        match kind {
            EditKind::NewFile => self.open_file(path, window, cx),
            EditKind::NewFolder => {
                if let Some(root) = self.active_root() {
                    self.tree.expand(&root, &path);
                }
                self.tree.scroll_to(&path);
                if by_enter {
                    self.focus_active_item(window, cx);
                }
            }
            EditKind::Rename => {
                self.retarget_items(&target, &path, cx);
                self.tree.scroll_to(&path);
                if by_enter {
                    self.focus_active_item(window, cx);
                }
            }
        }
        cx.notify();
    }

    /// Points tabs at a renamed file, or at files inside a renamed folder.
    fn retarget_items(&mut self, from: &Path, to: &Path, cx: &mut Context<Self>) {
        let mut moved = Vec::new();
        for project in &mut self.workspace.projects {
            let Some(layout) = project.layout.as_mut() else {
                continue;
            };
            let ids: Vec<ItemId> = layout.items().map(|i| i.id).collect();
            for id in ids {
                let Some(item) = layout.item_mut(id) else {
                    continue;
                };
                let Some(old) = item.kind.file().cloned() else {
                    continue;
                };
                let Ok(rest) = old.strip_prefix(from) else {
                    continue;
                };
                let new = if rest.as_os_str().is_empty() {
                    to.to_path_buf()
                } else {
                    to.join(rest)
                };
                item.kind = super::panes::file_kind_like(&item.kind, new.clone());
                moved.push((project.root.clone(), id, old, new));
            }
        }
        for (root, id, old, new) in moved {
            let key = (root.clone(), id);
            match self.items.get(&key).cloned() {
                // The editor follows the file, keeping its cursor and any unsaved edits.
                Some(ItemView::Editor(editor)) => {
                    editor.update(cx, |e, cx| e.set_path(new, cx));
                    self.lsp_closed(&old, cx);
                    self.lsp_opened(&root, &editor, cx);
                }
                Some(view) => {
                    self.items.remove(&key);
                    view.close(cx);
                }
                None => {}
            }
        }
        self.schedule_save(cx);
    }

    fn confirm_trash(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let detail = match path.is_dir() {
            true => "The folder and everything in it go to the Trash, where you can restore them.",
            false => "You can restore it from the Trash.",
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Move “{name}” to the Trash?"),
            Some(detail),
            &["Move to Trash", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(0) = answer.await {
                let _ = this.update_in(cx, |this, window, cx| this.trash_path(&path, window, cx));
            }
        })
        .detach();
    }

    fn trash_path(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(err) = fileops::trash(path) {
            self.transient_notice("Could not move to the Trash", format!("{err:#}"), cx);
            return;
        }
        let showing: Vec<(PathBuf, ItemId)> = self
            .workspace
            .projects
            .iter()
            .flat_map(|p| {
                p.layout
                    .iter()
                    .flat_map(|l| l.items())
                    .filter(|i| i.kind.file().is_some_and(|f| f.starts_with(path)))
                    .map(|i| (p.root.clone(), i.id))
            })
            .collect();
        for (root, item) in showing {
            // As in VS Code, unsaved edits stay open; saving them writes the file back.
            let dirty = self
                .items
                .get(&(root.clone(), item))
                .is_some_and(|v| v.is_dirty(cx));
            if dirty {
                continue;
            }
            self.remove_item_from(&root, item, window, cx);
        }
        self.tree.invalidate();
        self.git_kick(cx);
        cx.notify();
    }

    /// Right-click on a tab.
    pub(super) fn open_tab_menu(
        &mut self,
        pane: PaneId,
        item: ItemId,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root() else {
            return;
        };
        let Some(p) = self
            .workspace
            .active_project()
            .and_then(|p| p.layout.as_ref())
            .and_then(|l| l.pane(pane))
        else {
            return;
        };
        let ids: Vec<ItemId> = p.items.iter().map(|i| i.id).collect();
        let Some(index) = ids.iter().position(|&i| i == item) else {
            return;
        };
        let file = p.items[index].kind.file().cloned();
        let others: Vec<ItemId> = ids.iter().copied().filter(|&i| i != item).collect();
        let right = ids[index + 1..].to_vec();
        let can_split = ids.len() > 1 || file.is_some();
        let mut items = vec![
            shell_item("Close", cx, move |this, w, cx| {
                this.close_item_in(pane, item, w, cx)
            })
            .hint("⌘W"),
            shell_item("Close Others", cx, {
                let others = others.clone();
                move |this, w, cx| this.close_items(others.clone(), w, cx)
            })
            .disabled(others.is_empty()),
            shell_item("Close to the Right", cx, {
                let right = right.clone();
                move |this, w, cx| this.close_items(right.clone(), w, cx)
            })
            .disabled(right.is_empty()),
            shell_item("Close All", cx, move |this, w, cx| {
                this.close_items(ids.clone(), w, cx)
            }),
            MenuItem::separator(),
        ];
        if let Some(file) = file {
            items.extend(path_items(&root, &file));
            items.push(shell_item("Reveal in File Tree", cx, move |this, _, cx| {
                this.reveal_in_tree(&file, cx)
            }));
            items.push(MenuItem::separator());
        }
        items.push(
            shell_item("Split Right", cx, move |this, w, cx| {
                this.split_tab(pane, item, Axis::Horizontal, w, cx)
            })
            .disabled(!can_split),
        );
        items.push(
            shell_item("Split Down", cx, move |this, w, cx| {
                this.split_tab(pane, item, Axis::Vertical, w, cx)
            })
            .disabled(!can_split),
        );
        self.open_context_menu(position, items, window, cx);
    }

    /// Opens the tree at `path` and scrolls it into view.
    pub(super) fn reveal_in_tree(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        if !path.starts_with(&root) {
            return;
        }
        if !self.workspace.ui.tree_visible {
            self.toggle_tree(cx);
        }
        self.tree.reveal(&root, path);
        cx.notify();
    }
}

#[derive(Debug, PartialEq, Eq)]
enum BlurOutcome {
    Commit,
    Cancel { refocus: bool },
    Keep,
}

/// On losing focus, clicking elsewhere in the project keeps what was typed (as in VS Code), a field
/// that vanished (scrolled away, tree hidden) or a project switch drops it, and an app switch waits.
fn blur_outcome(
    window_active: bool,
    still_focused: bool,
    moved_to_drawn: bool,
    same_project: bool,
) -> BlurOutcome {
    if !window_active {
        BlurOutcome::Keep
    } else if still_focused {
        BlurOutcome::Cancel { refocus: true }
    } else if moved_to_drawn && same_project {
        BlurOutcome::Commit
    } else {
        BlurOutcome::Cancel {
            refocus: !moved_to_drawn,
        }
    }
}

/// Refuses names the file system would read as a path rather than one entry.
fn valid_name(name: &str) -> anyhow::Result<()> {
    if name.contains('/') || name == "." || name == ".." || name.contains('\0') {
        anyhow::bail!("“{name}” is not a valid file or folder name");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tree_edit_commits_only_when_focus_moves_elsewhere_in_its_project() {
        assert_eq!(blur_outcome(true, false, true, true), BlurOutcome::Commit);
        // Scrolled off or hidden: the field is gone but still holds focus.
        assert_eq!(
            blur_outcome(true, true, false, true),
            BlurOutcome::Cancel { refocus: true }
        );
        // A project switch moved focus to the other project's tab.
        assert_eq!(
            blur_outcome(true, false, true, false),
            BlurOutcome::Cancel { refocus: false }
        );
        assert_eq!(
            blur_outcome(true, false, false, true),
            BlurOutcome::Cancel { refocus: true }
        );
        assert_eq!(blur_outcome(false, true, false, true), BlurOutcome::Keep);
    }

    #[test]
    fn names_with_a_slash_or_dots_alone_are_refused() {
        assert!(valid_name("main.rs").is_ok());
        assert!(valid_name(".env").is_ok());
        assert!(valid_name("a/b").is_err());
        assert!(valid_name("..").is_err());
        assert!(valid_name(".").is_err());
    }
}
