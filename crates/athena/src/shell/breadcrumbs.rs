use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use athena_editor::EditorView;
use athena_lsp::{Position, Symbol};
use athena_ui::{ActiveTheme, MenuItem};
use athena_workspace::{ItemKind, Pane};
use gpui::{
    AnyElement, Context, Entity, MouseButton, MouseDownEvent, Pixels, Point, Task, Window, div,
    prelude::*, px,
};

use super::Shell;
use super::item::ItemView;
use super::lsp::document_key;
use super::menus::shell_item;

/// Edits pause this long before a file's symbols are asked for again.
const SYMBOLS_DELAY: Duration = Duration::from_millis(400);
/// A dropdown lists at most this many entries, as the menu does not scroll.
const MAX_ENTRIES: usize = 40;
const ROW_HEIGHT: f32 = 22.;

#[derive(Default)]
pub(super) struct BreadcrumbState {
    /// Each document's symbols and the buffer version they were found for.
    symbols: HashMap<PathBuf, (u64, Rc<Vec<Symbol>>)>,
    /// The documents whose symbols are being asked for, and for which version.
    asking: HashMap<PathBuf, (u64, Task<()>)>,
    /// The symbols last shown for each document, so a cursor move redraws only when they change.
    shown: HashMap<PathBuf, Vec<usize>>,
}

/// Indexes of the symbols holding `at`, outermost first.
fn symbol_chain(symbols: &[Symbol], at: Position) -> Vec<usize> {
    let holds = |s: &Symbol| s.scope.start <= at && at <= s.scope.end;
    let Some(deepest) = symbols.iter().rposition(holds) else {
        return Vec::new();
    };
    let mut chain = vec![deepest];
    while let Some(parent) = symbols[*chain.last().unwrap_or(&deepest)].parent {
        chain.push(parent);
    }
    if chain.len() == 1 {
        // A flat list names no parents, so every symbol holding the position counts.
        return (0..symbols.len()).filter(|&i| holds(&symbols[i])).collect();
    }
    chain.reverse();
    chain
}

/// A folder's entries for a dropdown: folders first, then files, by name, hidden ones left out.
fn folder_entries(dir: &Path) -> Vec<(PathBuf, bool)> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<(PathBuf, bool)> = read
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| (e.path(), e.file_type().is_ok_and(|t| t.is_dir())))
        .collect();
    entries.sort_by(|a, b| (!a.1, &a.0).cmp(&(!b.1, &b.0)));
    entries
}

impl Shell {
    fn active_editor_of(&self, root: &Path, pane: &Pane) -> Option<Entity<EditorView>> {
        let item = pane.active_item()?;
        if !matches!(item.kind, ItemKind::Editor { .. }) {
            return None;
        }
        match self.items.get(&(root.to_path_buf(), item.id))? {
            ItemView::Editor(e) => Some(e.clone()),
            _ => None,
        }
    }

    /// The symbols of `editor`'s file, asking its language server again once edits pause.
    fn breadcrumb_symbols(
        &mut self,
        editor: &Entity<EditorView>,
        cx: &mut Context<Self>,
    ) -> Option<Rc<Vec<Symbol>>> {
        let (doc, version) = {
            let e = editor.read(cx);
            (document_key(e.path()), e.version()?)
        };
        let cached = self.breadcrumbs.symbols.get(&doc).cloned();
        let fresh = cached.as_ref().is_some_and(|(v, _)| *v == version);
        let asked = self
            .breadcrumbs
            .asking
            .get(&doc)
            .is_some_and(|(v, _)| *v == version);
        if !fresh
            && !asked
            && let Some(client) = self
                .document_client(&doc)
                .filter(|c| c.supports("/documentSymbolProvider"))
        {
            let weak = editor.downgrade();
            let task_doc = doc.clone();
            let task = cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SYMBOLS_DELAY).await;
                let sent = this.update(cx, |this, cx| {
                    if let Some(editor) = weak.upgrade() {
                        this.flush_change(&task_doc, &editor, cx);
                    }
                });
                if sent.is_err() {
                    return;
                }
                let found = client.document_symbols(&task_doc).await;
                let _ = this.update(cx, |this, cx| {
                    this.breadcrumbs.asking.remove(&task_doc);
                    match found {
                        Ok(list) => {
                            this.breadcrumbs
                                .symbols
                                .insert(task_doc, (version, Rc::new(list)));
                            cx.notify();
                        }
                        Err(why) => {
                            tracing::debug!("breadcrumb symbols failed: {why}");
                            // Asked again after the next edit, not on every frame.
                            let none = Rc::new(Vec::new());
                            this.breadcrumbs.symbols.insert(task_doc, (version, none));
                        }
                    }
                });
            });
            self.breadcrumbs.asking.insert(doc, (version, task));
        }
        // Older symbols are shown until the new ones come, so the row does not flicker.
        cached.map(|(_, list)| list)
    }

    /// Redraws the breadcrumbs when the cursor has moved into another symbol.
    pub(super) fn breadcrumbs_cursor_moved(
        &mut self,
        editor: &Entity<EditorView>,
        cx: &mut Context<Self>,
    ) {
        let e = editor.read(cx);
        let doc = document_key(e.path());
        let (Some((_, symbols)), Some((line, character))) =
            (self.breadcrumbs.symbols.get(&doc), e.cursor_utf16())
        else {
            return;
        };
        let chain = symbol_chain(symbols, Position { line, character });
        if self.breadcrumbs.shown.get(&doc) != Some(&chain) {
            self.breadcrumbs.shown.insert(doc, chain);
            cx.notify();
        }
    }

    pub(super) fn breadcrumbs_closed(&mut self, path: &Path) {
        let doc = document_key(path);
        self.breadcrumbs.symbols.remove(&doc);
        self.breadcrumbs.asking.remove(&doc);
        self.breadcrumbs.shown.remove(&doc);
    }

    /// The row under a pane's tabs naming the active editor's file, folder by folder, and the
    /// symbols holding its cursor; each part opens a dropdown of its neighbours.
    pub(super) fn render_breadcrumbs(
        &mut self,
        root: &Path,
        pane: &Pane,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let editor = self.active_editor_of(root, pane)?;
        let symbols = self.breadcrumb_symbols(&editor, cx);
        let t = cx.theme().clone();
        let (path, cursor) = {
            let e = editor.read(cx);
            (e.path().to_path_buf(), e.cursor_utf16())
        };
        let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let parts: Vec<String> = relative
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let chain = match (&symbols, cursor) {
            (Some(list), Some((line, character))) => {
                symbol_chain(list, Position { line, character })
            }
            _ => Vec::new(),
        };
        self.breadcrumbs
            .shown
            .insert(document_key(&path), chain.clone());

        let separator = || {
            div()
                .flex_none()
                .text_color(t.color.content_disabled)
                .child("›")
        };
        let crumb = |id: (&'static str, usize), label: String, current: bool| {
            div()
                .id(id)
                .flex_none()
                .px(px(4.))
                .rounded(t.shape.radius_control)
                .cursor_pointer()
                .whitespace_nowrap()
                .text_color(if current {
                    t.color.content_secondary
                } else {
                    t.color.content_muted
                })
                .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                .child(label)
        };
        let mut row = div()
            .flex_none()
            .h(t.ui(ROW_HEIGHT))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(2.))
            .overflow_hidden()
            .bg(t.color.surface_sunken)
            .border_b_1()
            .border_color(t.color.border)
            .text_size(t.typography.caption);
        let last_part = parts.len().saturating_sub(1);
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                row = row.child(separator());
            }
            // Each part lists the folder that holds it.
            let folder = root.join(parts[..i].iter().collect::<PathBuf>());
            let current = i == last_part && chain.is_empty();
            row = row.child(
                crumb(("crumb-path", i), part.clone(), current).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.open_folder_crumb(folder.clone(), crumb_anchor(e), window, cx);
                    }),
                ),
            );
        }
        if let Some(list) = &symbols {
            for (n, &i) in chain.iter().enumerate() {
                row = row.child(separator());
                let (list, path) = (list.clone(), path.clone());
                let current = n + 1 == chain.len();
                row = row.child(
                    crumb(("crumb-symbol", n), list[i].name.clone(), current).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            let at = crumb_anchor(e);
                            this.open_symbol_crumb(&path, &list, i, at, window, cx);
                        }),
                    ),
                );
            }
        }
        Some(row.into_any_element())
    }

    /// A dropdown of `dir`'s folders and files: a file opens, a folder lists its own.
    fn open_folder_crumb(
        &mut self,
        dir: PathBuf,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let items = folder_entries(&dir)
            .into_iter()
            .take(MAX_ENTRIES)
            .map(|(path, is_dir)| {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match is_dir {
                    true => shell_item(format!("{name}/"), cx, move |this, w, cx| {
                        this.open_folder_crumb(path.clone(), at, w, cx)
                    }),
                    false => shell_item(name, cx, move |this, w, cx| {
                        this.open_file(path.clone(), w, cx)
                    }),
                }
            })
            .collect();
        self.open_context_menu(at, items, window, cx);
    }

    /// A dropdown of the symbols beside `list[index]`, under the same parent; one jumps there.
    fn open_symbol_crumb(
        &mut self,
        path: &Path,
        list: &[Symbol],
        index: usize,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let parent = list[index].parent;
        let items: Vec<MenuItem> = list
            .iter()
            .filter(|s| s.parent == parent)
            .take(MAX_ENTRIES)
            .map(|s| {
                let (path, target) = (path.to_path_buf(), s.range.start);
                shell_item(s.name.clone(), cx, move |this, _, cx| {
                    this.lsp.jump = Some((path.clone(), target));
                    cx.notify();
                })
                .hint(athena_lsp::symbol_kind_label(s.kind))
            })
            .collect();
        self.open_context_menu(at, items, window, cx);
    }
}

/// Where a crumb's dropdown opens: just below the click, so it hangs from the row.
fn crumb_anchor(e: &MouseDownEvent) -> Point<Pixels> {
    gpui::point(e.position.x - px(8.), e.position.y + px(ROW_HEIGHT / 2.))
}

#[cfg(test)]
mod tests {
    use super::*;
    use athena_lsp::Range;

    fn symbol(name: &str, lines: (u32, u32), parent: Option<usize>) -> Symbol {
        let at = |line| Position { line, character: 0 };
        Symbol {
            name: name.into(),
            kind: 12,
            container: None,
            path: PathBuf::from("/p/a.go"),
            range: Range {
                start: at(lines.0),
                end: at(lines.0),
            },
            scope: Range {
                start: at(lines.0),
                end: at(lines.1),
            },
            parent,
        }
    }

    #[test]
    fn the_chain_runs_from_the_outermost_symbol_holding_the_cursor_inward() {
        let list = vec![
            symbol("Server", (2, 20), None),
            symbol("Start", (4, 10), Some(0)),
            symbol("Stop", (12, 18), Some(0)),
            symbol("main", (22, 30), None),
        ];
        let at = |line| Position { line, character: 3 };
        assert_eq!(symbol_chain(&list, at(5)), [0, 1]);
        assert_eq!(symbol_chain(&list, at(11)), [0]);
        assert_eq!(symbol_chain(&list, at(25)), [3]);
        assert!(symbol_chain(&list, at(0)).is_empty());
        let flat = vec![symbol("pkg", (0, 40), None), symbol("run", (3, 9), None)];
        assert_eq!(
            symbol_chain(&flat, at(4)),
            [0, 1],
            "a flat list nests by range"
        );
    }

    #[test]
    fn folder_dropdowns_list_folders_first_and_skip_hidden_entries() {
        let dir = std::env::temp_dir().join(format!("athena-crumbs-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("a.go"), "").unwrap();
        std::fs::write(dir.join(".env"), "").unwrap();
        let names: Vec<String> = folder_entries(&dir)
            .into_iter()
            .map(|(p, d)| {
                format!(
                    "{}{}",
                    p.file_name().unwrap().to_string_lossy(),
                    if d { "/" } else { "" }
                )
            })
            .collect();
        assert_eq!(names, ["src/", "a.go"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
