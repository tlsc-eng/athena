use std::collections::HashMap;
use std::path::{Path, PathBuf};

use athena_editor::{EditorEvent, EditorView, ImageView, is_image_path};
use athena_preview::{DocEvent, DocView, PreviewEvent, PreviewView, is_document_path};
use athena_term::{ClaudeState, TerminalEvent, TerminalView};
use athena_ui::{ActiveTheme, Button, ButtonKind, empty_state, motion};
use athena_workspace::{
    Axis, Direction, Divider, Item, ItemId, ItemKind, Layout, Node, NodePath, Pane, PaneId, Rect,
};
use gpui::{
    Animation, AnyElement, Bounds, Context, CursorStyle, DragMoveEvent, ElementId, Entity,
    FontWeight, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, PromptLevel,
    SharedString, Window, canvas, div, linear_color_stop, linear_gradient, point, prelude::*, px,
    relative, size,
};

use super::Shell;
use super::dnd::{DropZone, TabDrag, TabGhost, strip_drop_index, zone_for};
use super::item::{ItemView, file_label};
use crate::actions::NewTerminal;

const TAB_STRIP_HEIGHT: f32 = 32.;
const FRAME: std::time::Duration = std::time::Duration::from_millis(16);
/// How far a resize handle reaches past each side of the line it drags.
pub(super) const DIVIDER_HIT: f32 = 3.;

const TREE_MIN: f32 = 160.;
const TREE_MAX: f32 = 480.;
const DRAWER_MIN: f32 = 120.;
/// Window height the drawer must leave for the title bar and panes.
const DRAWER_ROOM: f32 = 200.;

/// Per pane, how often its shown tab changed and when it last did, so only that pane's content fades.
#[derive(Default)]
pub(super) struct ContentSwitches(HashMap<(PathBuf, PaneId), (u64, motion::Opening)>);

impl ContentSwitches {
    fn note(&mut self, root: &Path, pane: PaneId) {
        let entry = self
            .0
            .entry((root.to_path_buf(), pane))
            .or_insert((0, motion::Opening::now()));
        entry.0 += 1;
        entry.1 = motion::Opening::now();
    }

    /// The fade key for the pane's content, and whether its fade should still play.
    fn fade(&self, root: &Path, pane: PaneId, duration: std::time::Duration) -> (u64, bool) {
        match self.0.get(&(root.to_path_buf(), pane)) {
            Some((count, opening)) => (*count, opening.running(duration)),
            None => (0, false),
        }
    }

    fn retain(&mut self, keep: impl Fn(&(PathBuf, PaneId)) -> bool) {
        self.0.retain(|key, _| keep(key));
    }
}

/// A split easing to 50 % after a divider double-click.
pub(super) struct RatioAnim {
    root: PathBuf,
    path: NodePath,
    from: f32,
    opening: motion::Opening,
    generation: u64,
}

impl RatioAnim {
    /// The ratio to draw for the split at `path` in `root` heading for `to`, if this is that split.
    fn ratio(
        &self,
        root: &Path,
        path: &NodePath,
        to: f32,
        duration: std::time::Duration,
    ) -> Option<f32> {
        if self.root != root || self.path != *path {
            return None;
        }
        let d = (self.opening.since.elapsed().as_secs_f32() / duration.as_secs_f32()).min(1.);
        Some(self.from + (to - self.from) * motion::ease_standard()(d))
    }
}

/// An in-progress resize: a pane divider in the active project, the tree's edge or the drawer's.
pub(super) enum Drag {
    Divider {
        path: NodePath,
        axis: Axis,
        split: Bounds<Pixels>,
    },
    Tree {
        start_x: Pixels,
        start_w: f32,
    },
    Drawer {
        start_y: Pixels,
        start_h: f32,
    },
}

pub(super) fn clamp_tree_width(w: f32) -> f32 {
    w.clamp(TREE_MIN, TREE_MAX)
}

pub(super) fn clamp_drawer_height(h: f32, window_h: f32) -> f32 {
    h.min(window_h - DRAWER_ROOM).max(DRAWER_MIN)
}

/// A 7 px drag strip that shows a 1 px accent line on hover, for dividers and panel edges.
pub(super) fn resize_handle(
    id: impl Into<ElementId>,
    axis: Axis,
    accent: Hsla,
) -> gpui::Stateful<gpui::Div> {
    let id = id.into();
    let group = SharedString::from(format!("handle-{id}"));
    let horizontal = axis == Axis::Horizontal;
    div()
        .id(id)
        .group(group.clone())
        .flex()
        .items_center()
        .justify_center()
        .cursor(if horizontal {
            CursorStyle::ResizeLeftRight
        } else {
            CursorStyle::ResizeUpDown
        })
        .child(
            div()
                .when(horizontal, |el| el.w(px(1.)).h_full())
                .when(!horizontal, |el| el.h(px(1.)).w_full())
                .invisible()
                .bg(accent)
                .group_hover(group, |s| s.visible()),
        )
}

impl Shell {
    pub(super) fn active_root(&self) -> Option<PathBuf> {
        Some(self.workspace.active_project()?.root.clone())
    }

    fn active_layout(&mut self) -> Option<&mut Layout> {
        let i = self.workspace.active?;
        self.workspace.projects.get_mut(i)?.layout.as_mut()
    }

    fn pane_area(&self) -> Rect {
        let b = *self.pane_area.borrow();
        Rect {
            x: b.origin.x.into(),
            y: b.origin.y.into(),
            w: b.size.width.into(),
            h: b.size.height.into(),
        }
    }

    /// The view for an item, created (and for terminals attached to their session) on first use.
    pub(super) fn item_view(
        &mut self,
        root: &Path,
        item: &Item,
        cx: &mut Context<Self>,
    ) -> Option<ItemView> {
        let key = (root.to_path_buf(), item.id);
        if let Some(view) = self.items.get(&key) {
            return Some(view.clone());
        }
        let view = match &item.kind {
            ItemKind::Terminal { session } => {
                let view = cx.new(|cx| TerminalView::new(root.to_path_buf(), *session, cx));
                let (project_root, item_id) = key.clone();
                cx.subscribe(&view, move |this, _, event: &TerminalEvent, cx| {
                    let session = match event {
                        TerminalEvent::Attached(session) => session,
                        TerminalEvent::ContextMenu { open } => {
                            let key = (project_root.clone(), item_id);
                            return this.note_item_menu(key, *open, cx);
                        }
                        TerminalEvent::Changed => {
                            this.check_playwright_run(cx);
                            this.notify_coalesced(cx);
                            return;
                        }
                        TerminalEvent::OpenFile { path, line, column } => {
                            return this.open_file_link(path.clone(), *line, *column, cx);
                        }
                    };
                    let item = this
                        .workspace
                        .projects
                        .iter_mut()
                        .find(|p| p.root == project_root)
                        .and_then(|p| p.layout.as_mut())
                        .and_then(|l| l.item_mut(item_id));
                    if let Some(item) = item {
                        item.kind = ItemKind::Terminal {
                            session: Some(*session),
                        };
                        this.schedule_save(cx);
                    }
                })
                .detach();
                ItemView::Terminal(view)
            }
            ItemKind::Editor { path } => {
                let view = cx.new(|cx| EditorView::open(path.clone(), cx));
                let delay = self.autosave_delay();
                let format_on_save = self.workspace.format_on_save;
                view.update(cx, |v, cx| {
                    v.set_autosave(delay, cx);
                    v.set_format_on_save(format_on_save);
                });
                let tab = key.clone();
                cx.subscribe(&view, move |this, view, event: &EditorEvent, cx| {
                    match event {
                        EditorEvent::Changed => {}
                        EditorEvent::ContextMenu { open } => {
                            return this.note_item_menu(tab.clone(), *open, cx);
                        }
                        EditorEvent::Opened => {
                            this.lsp_opened(&tab.0, &view, cx);
                            this.git_opened(&view, cx);
                        }
                        EditorEvent::Edited { .. } => {
                            this.lsp_edited(&view, cx);
                            this.follow_docs(&view, cx);
                            view.update(cx, |v, cx| v.schedule_autosave(cx));
                        }
                        EditorEvent::Saved => {
                            this.lsp_saved(&view, cx);
                            this.git_kick(cx);
                            let path = view.read(cx).path().to_path_buf();
                            this.refresh_docs(&path, cx);
                        }
                        EditorEvent::GoToDefinition { line, character } => {
                            let at = athena_lsp::Position {
                                line: *line,
                                character: *character,
                            };
                            this.lsp_definition(&view, at, cx);
                        }
                        EditorEvent::FindReferences { line, character } => {
                            let at = athena_lsp::Position {
                                line: *line,
                                character: *character,
                            };
                            this.lsp_references(&view, at, cx);
                        }
                        EditorEvent::CursorMoved { line } => {
                            return this.git_cursor_moved(&view, *line, cx);
                        }
                        EditorEvent::Hover {
                            request,
                            line,
                            character,
                        } => return this.lsp_hover(&view, *request, (*line, *character), cx),
                        EditorEvent::SignatureHelp {
                            request,
                            line,
                            character,
                        } => return this.lsp_signature(&view, *request, (*line, *character), cx),
                        EditorEvent::Format {
                            request,
                            tab_size,
                            insert_spaces,
                        } => {
                            let options = (*tab_size, *insert_spaces);
                            return this.lsp_format(&view, *request, options, cx);
                        }
                        EditorEvent::Complete {
                            request,
                            line,
                            character,
                            trigger,
                        } => {
                            let at = (*line, *character);
                            return this.lsp_complete(&view, *request, at, trigger.clone(), cx);
                        }
                    }
                    cx.notify();
                })
                .detach();
                self.lsp_opened(root, &view, cx);
                self.git_opened(&view, cx);
                ItemView::Editor(view)
            }
            ItemKind::Image { path } => {
                ItemView::Image(cx.new(|cx| ImageView::open(path.clone(), cx)))
            }
            ItemKind::Rendered { path } => {
                let view = cx.new(|cx| DocView::new(root.to_path_buf(), path.clone(), cx));
                cx.subscribe(&view, |this, _, event: &DocEvent, cx| {
                    let DocEvent::OpenFile(path) = event;
                    this.pending_open = Some(path.clone());
                    cx.notify();
                })
                .detach();
                ItemView::Doc(view)
            }
            ItemKind::Diff { path, base } => self.new_diff_view(root, path, base, cx),
            ItemKind::Preview { url } => {
                let view = cx.new(|cx| PreviewView::new(root.to_path_buf(), url.clone(), cx));
                let (project_root, item_id) = key.clone();
                cx.subscribe(&view, move |this, _, event: &PreviewEvent, cx| {
                    let PreviewEvent::Navigated(url) = event;
                    let item = this
                        .workspace
                        .projects
                        .iter_mut()
                        .find(|p| p.root == project_root)
                        .and_then(|p| p.layout.as_mut())
                        .and_then(|l| l.item_mut(item_id));
                    if let Some(item) = item {
                        item.kind = ItemKind::Preview { url: url.clone() };
                        this.schedule_save(cx);
                    }
                    cx.notify();
                })
                .detach();
                ItemView::Preview(view)
            }
        };
        self.items.insert(key, view.clone());
        Some(view)
    }

    /// Redraws for terminal label, bell and Claude-state changes at most once a frame.
    fn notify_coalesced(&mut self, cx: &mut Context<Self>) {
        if self.redraw_pending.is_some() {
            return;
        }
        self.redraw_pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FRAME).await;
            let _ = this.update(cx, |this, cx| {
                this.redraw_pending = None;
                cx.notify();
            });
        }));
    }

    pub(super) fn item_label(&self, root: &Path, item: &Item, cx: &Context<Self>) -> String {
        match (self.items.get(&(root.to_path_buf(), item.id)), &item.kind) {
            (Some(view), _) => view.label(cx),
            (None, ItemKind::Terminal { .. }) => "Terminal".into(),
            (None, ItemKind::Editor { path } | ItemKind::Image { path }) => file_label(path),
            (None, ItemKind::Preview { url }) => athena_preview::label_for(url),
            (None, ItemKind::Rendered { path }) => athena_preview::doc_label_for(path),
            (None, ItemKind::Diff { path, base }) => super::review::diff_title(path, base),
        }
    }

    /// A 6 px square before the label: Claude's state, or a bell nobody has seen yet.
    fn item_badge(
        &self,
        root: &Path,
        item: &Item,
        t: &athena_ui::Theme,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let view = self.items.get(&(root.to_path_buf(), item.id))?;
        let (color, word) = match view.claude_state(cx) {
            _ if view.is_stale(cx) => (t.color.content_muted, Some("stale")),
            Some(ClaudeState::Waiting) => (t.color.warning, Some("needs input")),
            Some(ClaudeState::Running) => (t.color.success, Some("running")),
            None if view.has_bell(cx) => (t.color.warning, None),
            None => return None,
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(div().size(px(6.)).bg(color))
                .children(word.map(|w| div().text_color(color).child(w)))
                .into_any_element(),
        )
    }

    /// Highest-priority Claude state across a project's tabs, for the rail.
    pub(super) fn project_claude_state(
        &self,
        root: &Path,
        cx: &Context<Self>,
    ) -> Option<ClaudeState> {
        let states = self
            .items
            .iter()
            .filter(|((r, _), _)| r == root)
            .filter_map(|(_, v)| v.claude_state(cx));
        states.max_by_key(|s| matches!(s, ClaudeState::Waiting))
    }

    /// Moves keyboard focus to the focused pane's active item.
    pub(super) fn focus_active_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        let Some(item) = self
            .workspace
            .active_project()
            .and_then(|p| p.layout.as_ref())
            .and_then(|l| l.focused_pane())
            .and_then(|p| p.active_item())
            .cloned()
        else {
            window.focus(&self.focus);
            return;
        };
        if let Some(view) = self.item_view(&root, &item, cx) {
            window.focus(&view.focus_handle(cx));
        }
    }

    fn focus_pane(&mut self, pane: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(layout) = self.active_layout() else {
            return;
        };
        if layout.focused != pane {
            layout.focused = pane;
            self.schedule_save(cx);
        }
        self.note_editor_pane();
        self.focus_active_item(window, cx);
        cx.notify();
    }

    /// Remembers the focused pane as the project's editor pane while it shows an editor.
    fn note_editor_pane(&mut self) {
        let Some(project) = self.workspace.active_project() else {
            return;
        };
        let Some(pane) = project.layout.as_ref().and_then(|l| l.focused_pane()) else {
            return;
        };
        if pane.active_item().is_some_and(|i| i.kind.file().is_some()) {
            self.last_editor_pane.insert(project.root.clone(), pane.id);
        }
    }

    pub(super) fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_item(ItemKind::Terminal { session: None }, window, cx);
    }

    pub(super) fn new_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = athena_preview::DEFAULT_URL.to_string();
        self.add_item(ItemKind::Preview { url }, window, cx);
    }

    /// Web previews float above gpui, so only those in a visible tab with nothing drawn over them show.
    pub(super) fn sync_previews(&mut self, cx: &mut Context<Self>) {
        if !self
            .items
            .values()
            .any(|v| matches!(v, ItemView::Preview(_) | ItemView::Doc(_)))
        {
            return;
        }
        let mut shown = std::collections::HashSet::new();
        let covered = self.palette.is_some()
            || self.palette_closing.is_some()
            || self.context_menu.is_some()
            || !self.item_menus.is_empty()
            || self.usage_open();
        if let Some(project) = self.workspace.active_project().filter(|_| !covered)
            && let Some(layout) = &project.layout
        {
            let panes = match self.zoomed.and_then(|z| layout.pane(z)) {
                Some(pane) => vec![pane],
                None => layout.panes(),
            };
            for item in panes.into_iter().filter_map(|p| p.active_item()) {
                shown.insert((project.root.clone(), item.id));
            }
        }
        for (key, view) in &self.items {
            let visible = shown.contains(key);
            match view {
                ItemView::Preview(view) => view.update(cx, |v, _| v.set_visible(visible)),
                ItemView::Doc(view) => view.update(cx, |v, _| v.set_visible(visible)),
                _ => {}
            }
        }
    }

    /// An editor's or terminal's own right-click menu opened or closed.
    fn note_item_menu(&mut self, key: (PathBuf, ItemId), open: bool, cx: &mut Context<Self>) {
        let changed = if open {
            self.item_menus.insert(key)
        } else {
            self.item_menus.remove(&key)
        };
        if changed {
            cx.notify();
        }
    }

    /// Opens `kind` as a new tab in the focused pane.
    fn add_item(&mut self, kind: ItemKind, window: &mut Window, cx: &mut Context<Self>) {
        let Some(i) = self.workspace.active else {
            return;
        };
        match self.workspace.projects[i].layout.as_mut() {
            Some(layout) => {
                let focused = layout.focused;
                let born = layout.add_item(focused, kind);
                self.note_tab_born(born);
            }
            None => self.workspace.projects[i].layout = Some(Layout::new(kind)),
        }
        if let Some(focused) = self.active_layout().map(|l| l.focused) {
            self.note_content_switch(focused);
        }
        self.after_layout_change(window, cx);
    }

    fn start_layout(
        &mut self,
        project: usize,
        kind: ItemKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let layout = Layout::new(kind);
        let focused = layout.focused;
        self.workspace.projects[project].layout = Some(layout);
        self.note_content_switch(focused);
        self.after_layout_change(window, cx);
    }

    fn note_content_switch(&mut self, pane: PaneId) {
        if let Some(root) = self.active_root() {
            self.content_switches.note(&root, pane);
        }
    }

    fn note_tab_born(&mut self, item: Option<ItemId>) {
        if let Some(item) = item {
            self.tab_born = Some((item, motion::Opening::now()));
        }
    }

    pub(super) fn split(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        self.zoomed = None;
        let Some(layout) = self.active_layout() else {
            return self.new_terminal(window, cx);
        };
        let focused = layout.focused;
        if let Some(pane) = layout.split(focused, axis, ItemKind::Terminal { session: None }) {
            self.entering = Some(pane);
        }
        self.after_layout_change(window, cx);
    }

    pub(super) fn close_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self
            .workspace
            .active_project()
            .and_then(|p| p.layout.as_ref())
            .and_then(|l| l.focused_pane())
            .cloned()
        else {
            return;
        };
        let Some(item) = pane.active_item().map(|i| i.id) else {
            return;
        };
        self.close_tab(pane.id, item, window, cx);
    }

    /// Closes a tab, asking first whether to save an editor with unsaved changes.
    fn close_tab(
        &mut self,
        pane: PaneId,
        item: ItemId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dirty = self
            .active_root()
            .and_then(|root| self.items.get(&(root, item)).cloned())
            .filter(|view| view.is_dirty(cx));
        if let (Some(ItemView::Editor(editor)), Some(root)) = (dirty, self.active_root()) {
            return self.settle_dirty(editor, window, cx, move |this, close, window, cx| {
                if !close {
                    return;
                }
                // The project may have changed while the prompt was up; ids repeat across projects.
                if this.active_root().as_deref() == Some(root.as_path()) {
                    this.close_pane_item(pane, item, window, cx);
                } else {
                    this.remove_item_from(&root, item, window, cx);
                }
            });
        }
        self.close_pane_item(pane, item, window, cx);
    }

    /// Before an editor with unsaved changes closes: saves it quietly when auto save is on (as VS Code
    /// does), otherwise asks. `then` learns whether the tab may close.
    fn settle_dirty(
        &mut self,
        editor: Entity<EditorView>,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, bool, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if self.autosave_delay().is_some() && editor.update(cx, |e, cx| e.save(cx)) {
            return then(self, true, window, cx);
        }
        let name = editor
            .read(cx)
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Save changes to {name}?"),
            Some("Your changes will be lost if you don't save them."),
            &["Save", "Don't Save", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(choice) = answer.await else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                let close = match choice {
                    0 => editor.update(cx, |e, cx| e.save(cx)),
                    1 => true,
                    _ => false,
                };
                then(this, close, window, cx);
            });
        })
        .detach();
    }

    /// Closes several tabs of the active project in order, stopping if the user cancels a save prompt.
    pub(super) fn close_items(
        &mut self,
        items: Vec<ItemId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(root) = self.active_root() {
            self.close_items_in(root, items, window, cx);
        }
    }

    /// Item ids are only unique within a project, so a run interrupted by a prompt keeps its root.
    fn close_items_in(
        &mut self,
        root: PathBuf,
        mut items: Vec<ItemId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        while !items.is_empty() {
            let item = items.remove(0);
            let dirty = self
                .items
                .get(&(root.clone(), item))
                .cloned()
                .filter(|v| v.is_dirty(cx));
            if let Some(ItemView::Editor(editor)) = dirty {
                let root = root.clone();
                return self.settle_dirty(editor, window, cx, move |this, close, window, cx| {
                    if close {
                        this.remove_item_from(&root, item, window, cx);
                        this.close_items_in(root, items, window, cx);
                    }
                });
            }
            self.remove_item_from(&root, item, window, cx);
        }
    }

    /// Moves a tab into a new pane beside its own; a pane's only file tab is opened a second time instead.
    pub(super) fn split_tab(
        &mut self,
        pane: PaneId,
        item: ItemId,
        axis: Axis,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.zoomed = None;
        let Some(layout) = self.active_layout() else {
            return;
        };
        let alone = layout.pane(pane).is_some_and(|p| p.items.len() == 1);
        let new_pane = if alone {
            let kind = layout
                .pane(pane)
                .and_then(|p| p.items.first())
                .filter(|i| i.kind.file().is_some())
                .map(|i| i.kind.clone());
            kind.and_then(|kind| layout.split(pane, axis, kind))
        } else {
            layout.split_with_item(pane, axis, item, false)
        };
        if new_pane.is_some() {
            self.entering = new_pane;
            self.after_layout_change(window, cx);
        }
    }

    fn close_pane_item(
        &mut self,
        pane: PaneId,
        item: ItemId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root() else {
            return;
        };
        let Some(layout) = self
            .workspace
            .active_project()
            .and_then(|p| p.layout.as_ref())
        else {
            return;
        };
        let Some(pane) = layout.pane(pane).cloned() else {
            return;
        };
        let staying = layout
            .panes()
            .iter()
            .filter(|p| !self.leaving.contains_key(&(root.clone(), p.id)))
            .count();
        if self
            .tab_leaving
            .as_ref()
            .is_some_and(|(r, i, _)| *r == root && *i == item)
        {
            return;
        }
        if self.leaving.contains_key(&(root.clone(), pane.id)) {
            return;
        }
        if pane.items.len() == 1 && staying > 1 && !cx.theme().motion.reduced {
            // Fade the pane out first; its sibling takes the space once it is gone.
            let generation = self.next_generation();
            let key = (root.clone(), pane.id);
            self.leaving
                .insert(key.clone(), motion::Closing::new(generation));
            self.focus_successor_pane(pane.id, window, cx);
            cx.notify();
            let delay = cx.theme().motion.fast;
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(delay).await;
                let _ = this.update_in(cx, |this, window, cx| {
                    if this
                        .leaving
                        .get(&key)
                        .is_some_and(|c| c.generation == generation)
                    {
                        this.leaving.remove(&key);
                    }
                    this.remove_item_from(&root, item, window, cx);
                });
            })
            .detach();
            return;
        }
        if cx.theme().motion.reduced {
            return self.remove_item(item, window, cx);
        }
        // One tab fades at a time; a second close finishes the first at once.
        if let Some((root, item, _)) = self.tab_leaving.take() {
            self.remove_item_from(&root, item, window, cx);
        }
        let generation = self.next_generation();
        self.tab_leaving = Some((root.clone(), item, motion::Closing::new(generation)));
        // The neighbour shows and takes the keyboard now, so a second Cmd+W closes it.
        let stepped = self
            .active_layout()
            .and_then(|l| l.pane_mut(pane.id))
            .is_some_and(|p| step_off(p, item));
        if stepped {
            self.note_content_switch(pane.id);
            self.after_layout_change(window, cx);
        }
        let delay = cx.theme().motion.fast;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this
                    .tab_leaving
                    .as_ref()
                    .is_some_and(|l| l.2.generation == generation)
                {
                    this.tab_leaving = None;
                    this.remove_item_from(&root, item, window, cx);
                }
            });
        })
        .detach();
    }

    /// Moves focus off a pane that is fading out, to the neighbour that will take its space.
    fn focus_successor_pane(&mut self, pane: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        let area = self.pane_area();
        let Some(root) = self.active_root() else {
            return;
        };
        let leaving = &self.leaving;
        let Some(layout) = self
            .workspace
            .active
            .and_then(|i| self.workspace.projects.get_mut(i))
            .and_then(|p| p.layout.as_mut())
        else {
            return;
        };
        if layout.focused != pane {
            return;
        }
        let gone = |p: PaneId| p == pane || leaving.contains_key(&(root.clone(), p));
        if let Some(next) = successor_pane(layout, area, pane, gone) {
            layout.focused = next;
            self.after_layout_change(window, cx);
        }
    }

    fn remove_item(&mut self, item: ItemId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(root) = self.active_root() {
            self.remove_item_from(&root, item, window, cx);
        }
    }

    /// Closes a tab of the project at `root`, which may no longer be the active one after a fade.
    pub(super) fn remove_item_from(
        &mut self,
        root: &Path,
        item: ItemId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.item_menus.remove(&(root.to_path_buf(), item));
        if let Some(view) = self.items.remove(&(root.to_path_buf(), item)) {
            view.close(cx);
            if let ItemView::Editor(editor) = &view {
                let path = editor.read(cx).path().to_path_buf();
                self.lsp_closed(&path, cx);
            }
        }
        let Some(project) = self.workspace.projects.iter_mut().find(|p| p.root == root) else {
            return;
        };
        let shown = project.layout.as_ref().and_then(|l| {
            let (pane, at) = l.find_item(item)?;
            (l.pane(pane)?.active == at).then_some(pane)
        });
        if let Some(layout) = project.layout.as_mut()
            && !close_keeping_active(layout, item)
        {
            project.layout = None;
            // A fresh layout numbers its tabs from the start again.
            self.history.forget_root(root);
        }
        if let Some(pane) = shown {
            self.content_switches.note(root, pane);
        }
        // A fade that ends after a project switch must not touch the now-active project's zoom or focus.
        if self.active_root().as_deref() == Some(root) {
            self.zoomed = None;
            self.after_layout_change(window, cx);
        } else {
            self.schedule_save(cx);
            cx.notify();
        }
    }

    /// Opens `path` as an editor tab: raises an existing tab, else joins the focused pane if it
    /// holds editors, else the editor pane used last, else any editor pane, else splits the focused
    /// pane so terminals stay visible.
    pub(super) fn open_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.open_kind(file_kind(path), window, cx);
    }

    /// Opens a file-backed tab (an editor, viewer or diff) where `open_file` would put an editor.
    pub(super) fn open_kind(
        &mut self,
        kind: ItemKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(i) = self.workspace.active else {
            return;
        };
        self.record_location(cx);
        let last = self
            .last_editor_pane
            .get(&self.workspace.projects[i].root)
            .copied();
        let Some(layout) = self.workspace.projects[i].layout.as_mut() else {
            return self.start_layout(i, kind, window, cx);
        };
        self.zoomed = None;
        let is_editor = |item: &Item| item.kind.file().is_some();
        let existing = layout.panes().into_iter().find_map(|p| {
            p.items
                .iter()
                .position(|item| item.kind == kind)
                .map(|index| (p.id, index))
        });
        if let Some((pane, index)) = existing {
            return self.activate_tab(pane, index, window, cx);
        }
        let focused = layout.focused;
        let target = if layout
            .focused_pane()
            .is_some_and(|p| p.items.iter().any(is_editor))
        {
            Some(focused)
        } else {
            let editor_panes: Vec<PaneId> = layout
                .panes()
                .into_iter()
                .filter(|p| p.items.iter().any(is_editor))
                .map(|p| p.id)
                .collect();
            last.filter(|p| editor_panes.contains(p))
                .or(editor_panes.first().copied())
        };
        match target {
            Some(pane) => {
                layout.focused = pane;
                let born = layout.add_item(pane, kind);
                self.note_tab_born(born);
                self.note_content_switch(pane);
            }
            None => {
                if let Some(pane) = layout.split(focused, Axis::Horizontal, kind) {
                    self.entering = Some(pane);
                }
            }
        }
        self.after_layout_change(window, cx);
    }

    /// Opens `path` in a new pane split off beside the focused one, even if a tab already shows it.
    pub(super) fn open_file_beside(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(i) = self.workspace.active else {
            return;
        };
        self.record_location(cx);
        let kind = file_kind(path);
        let Some(layout) = self.workspace.projects[i].layout.as_mut() else {
            return self.start_layout(i, kind, window, cx);
        };
        self.zoomed = None;
        let focused = layout.focused;
        if let Some(pane) = layout.split(focused, Axis::Horizontal, kind) {
            self.entering = Some(pane);
        }
        self.after_layout_change(window, cx);
    }

    pub(super) fn close_item_in(
        &mut self,
        pane: PaneId,
        item: ItemId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The clicked tab is not raised first, so a background tab fades out without showing.
        if let Some(layout) = self.active_layout() {
            layout.focused = pane;
        }
        self.close_tab(pane, item, window, cx);
    }

    pub(super) fn focus_direction(
        &mut self,
        dir: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let area = self.pane_area();
        let Some(layout) = self.active_layout() else {
            return;
        };
        if let Some(next) = layout.neighbor(area, layout.focused, dir) {
            self.zoomed = None;
            self.focus_pane(next, window, cx);
        }
    }

    pub(super) fn toggle_zoom(&mut self, cx: &mut Context<Self>) {
        let focused = self
            .workspace
            .active_project()
            .and_then(|p| p.layout.as_ref())
            .map(|l| l.focused);
        self.zoomed = match (self.zoomed, focused) {
            (Some(_), _) => None,
            (None, f) => f,
        };
        cx.notify();
    }

    pub(super) fn cycle_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        self.record_location(cx);
        let Some(layout) = self.active_layout() else {
            return;
        };
        let focused = layout.focused;
        let Some(pane) = layout.pane_mut(focused) else {
            return;
        };
        let len = pane.items.len() as isize;
        if len == 0 {
            return;
        }
        pane.active = (pane.active as isize + step).rem_euclid(len) as usize;
        self.note_content_switch(focused);
        self.after_layout_change(window, cx);
    }

    pub(super) fn activate_tab(
        &mut self,
        pane: PaneId,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let moving = self
            .workspace
            .active_project()
            .and_then(|p| p.layout.as_ref())
            .is_some_and(|l| l.focused != pane || l.pane(pane).is_some_and(|p| p.active != index));
        if moving {
            self.record_location(cx);
        }
        let Some(layout) = self.active_layout() else {
            return;
        };
        layout.focused = pane;
        let Some(p) = layout.pane_mut(pane) else {
            return;
        };
        if index >= p.items.len() {
            return;
        }
        if p.active != index {
            p.active = index;
            self.note_content_switch(pane);
        }
        self.after_layout_change(window, cx);
    }

    pub(super) fn activate_tab_in_focused(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(focused) = self.active_layout().map(|l| l.focused) {
            self.activate_tab(focused, index, window, cx);
        }
    }

    fn after_layout_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The eased split's path may now name another split.
        self.ratio_anim = None;
        self.note_editor_pane();
        self.focus_active_item(window, cx);
        self.schedule_save(cx);
        cx.notify();
    }

    pub(super) fn drag_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = &self.drag else { return };
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            self.schedule_save(cx);
            return;
        }
        match drag {
            Drag::Divider { path, axis, split } => {
                let (pos, start, len) = match axis {
                    Axis::Horizontal => (event.position.x, split.origin.x, split.size.width),
                    Axis::Vertical => (event.position.y, split.origin.y, split.size.height),
                };
                let len: f32 = len.into();
                let ratio = f32::from(pos - start) / len;
                let path = path.clone();
                if let Some(layout) = self.active_layout() {
                    layout.set_ratio(&path, ratio, len);
                    cx.notify();
                }
            }
            Drag::Tree { start_x, start_w } => {
                let w = clamp_tree_width(start_w + f32::from(event.position.x - *start_x));
                if w != self.workspace.ui.tree_width {
                    self.workspace.ui.tree_width = w;
                    cx.notify();
                }
            }
            Drag::Drawer { start_y, start_h } => {
                let window_h = f32::from(window.viewport_size().height);
                let h =
                    clamp_drawer_height(start_h - f32::from(event.position.y - *start_y), window_h);
                if h != self.workspace.ui.drawer_height {
                    self.workspace.ui.drawer_height = h;
                    cx.notify();
                }
            }
        }
    }

    pub(super) fn drag_end(&mut self, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            self.schedule_save(cx);
        }
    }

    pub(super) fn render_panes(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let Some(root) = self.active_root() else {
            return div().into_any_element();
        };
        let layout = self
            .workspace
            .active_project()
            .and_then(|p| p.layout.clone());
        let Some(layout) = layout else {
            let action = Button::new("new-terminal", "New terminal", ButtonKind::Secondary)
                .on_click(|_, window, cx| window.dispatch_action(Box::new(NewTerminal), cx));
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(empty_state(
                    "No open panes",
                    "Press ⌘T to open a terminal here.",
                    Some(action),
                    cx,
                ))
                .into_any_element();
        };

        let zoomed = self.zoomed.and_then(|z| layout.pane(z).cloned());
        let zoomed_shown = zoomed.is_some();
        let tree = match zoomed {
            Some(pane) => self.render_pane(&root, &pane, true, cx),
            None => self.render_node(&root, &layout.tree, &mut Vec::new(), layout.focused, cx),
        };
        if std::mem::take(&mut self.focus_pending) {
            self.focus_active_item(window, cx);
        }
        if !cx.has_active_drag() {
            self.drop_hint = None;
        }
        let live = |(r, p): &(PathBuf, PaneId)| *r != root || layout.pane(*p).is_some();
        self.tab_scroll.retain(|key, _| live(key));
        self.content_switches.retain(live);

        let area = self.pane_area.clone();
        let recorder = canvas(
            move |bounds, _, _| {
                let mut stored = area.borrow_mut();
                if *stored != bounds {
                    *stored = bounds;
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        let easing = self.ratio_anim.as_ref().is_some_and(|a| a.root == root);
        if easing {
            window.request_animation_frame();
        }
        let dividers = if self.zoomed.is_some() || easing {
            Vec::new()
        } else {
            layout.layout(self.pane_area()).1
        };
        let handles: Vec<AnyElement> = dividers
            .into_iter()
            .enumerate()
            .map(|(i, d)| self.render_divider_handle(i, d, &t, cx))
            .collect();

        // A frame on top, not a border, so zooming does not resize the pane a second time.
        let zoom_frame = zoomed_shown.then(|| {
            div()
                .absolute()
                .inset_0()
                .border_1()
                .border_color(t.color.accent)
        });
        div()
            .relative()
            .size_full()
            .child(recorder)
            .child(tree)
            .children(zoom_frame)
            .children(handles)
            .into_any_element()
    }

    fn render_node(
        &mut self,
        root: &Path,
        node: &Node,
        path: &mut NodePath,
        focused: PaneId,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match node {
            Node::Leaf(pane) => self.render_pane(root, pane, pane.id == focused, cx),
            Node::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                path.push(false);
                let a = self.render_node(root, first, path, focused, cx);
                path.pop();
                path.push(true);
                let b = self.render_node(root, second, path, focused, cx);
                path.pop();
                let t = cx.theme();
                let (border, fast) = (t.color.border, t.motion.fast);
                let horizontal = *axis == Axis::Horizontal;
                // The one sanctioned size animation over live panes, interpolated here rather than
                // through an animation wrapper, which would re-key both panes and replay their fades.
                let ratio = self
                    .ratio_anim
                    .as_ref()
                    .and_then(|a| a.ratio(root, path, *ratio, fast))
                    .unwrap_or(*ratio);
                let first_box = div()
                    .flex_none()
                    .overflow_hidden()
                    .when(horizontal, |el| el.h_full().w(relative(ratio)))
                    .when(!horizontal, |el| el.w_full().h(relative(ratio)))
                    .child(a);
                let line = div()
                    .flex_none()
                    .bg(border)
                    .when(horizontal, |el| el.w(px(1.)).h_full())
                    .when(!horizontal, |el| el.h(px(1.)).w_full());
                div()
                    .size_full()
                    .flex()
                    .when(!horizontal, |el| el.flex_col())
                    .child(first_box)
                    .child(line)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .overflow_hidden()
                            .child(b),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_pane(
        &mut self,
        root: &Path,
        pane: &Pane,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let pane_id = pane.id;
        let tabs: Vec<AnyElement> = pane
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let active = index == pane.active;
                let item_id = item.id;
                let group = format!("tab-{}", item.id.0);
                let close_group = format!("tab-close-{}", item.id.0);
                let dirty = self
                    .items
                    .get(&(root.to_path_buf(), item.id))
                    .is_some_and(|v| v.is_dirty(cx));
                let leaving = self
                    .tab_leaving
                    .as_ref()
                    .filter(|(r, i, _)| r == root && *i == item.id)
                    .map(|(_, _, closing)| closing.generation);
                let born = self
                    .tab_born
                    .is_some_and(|(i, opening)| i == item.id && opening.running(t.motion.fast));
                let label = self.item_label(root, item, cx);
                let drag = TabDrag {
                    pane: pane_id,
                    item: item_id,
                    label: label.clone().into(),
                };
                let insertion = div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(2.))
                    .bg(t.color.accent)
                    .invisible()
                    .group_drag_over::<TabDrag>(group.clone(), |s| s.visible());
                let tab = div()
                    .id(("tab", item.id.0))
                    .group(group.clone())
                    .relative()
                    .flex_none()
                    .whitespace_nowrap()
                    .h_full()
                    .pl(px(12.))
                    .pr(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .border_r_1()
                    .border_color(t.color.border)
                    .text_size(t.typography.caption)
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .text_color(if active {
                        t.color.content
                    } else {
                        t.color.content_muted
                    })
                    .when_some(
                        item.kind.file().and_then(|p| self.git_status_for(p)),
                        |el, s| el.text_color(super::git_view::status_color(s, &t)),
                    )
                    .when(!active, |el| {
                        el.hover(|s| {
                            s.bg(t.color.surface_hover)
                                .text_color(t.color.content_secondary)
                        })
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if leaving.is_none() {
                            this.activate_tab(pane_id, index, window, cx)
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            if leaving.is_none() {
                                this.open_tab_menu(pane_id, item_id, event.position, window, cx)
                            }
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, _, window, cx| {
                            if leaving.is_none() {
                                this.close_item_in(pane_id, item_id, window, cx)
                            }
                        }),
                    )
                    .children(
                        item.kind
                            .file()
                            .map(|path| athena_ui::file_icon(path, false, cx)),
                    )
                    .on_drag(drag, |drag: &TabDrag, _, _, cx| {
                        let label = drag.label.clone();
                        cx.new(|_| TabGhost { label })
                    })
                    .on_drop(cx.listener(move |this, drag: &TabDrag, window, cx| {
                        this.drop_tab_at(drag, pane_id, Some(index), window, cx)
                    }))
                    .child(insertion)
                    .child(label)
                    .children(self.item_badge(root, item, &t, cx))
                    .child(
                        div()
                            .id(("tab-close", item.id.0))
                            .size(px(16.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(t.shape.radius_control)
                            .relative()
                            .group(close_group.clone())
                            .text_color(t.color.content_muted)
                            .hover(|s| s.bg(t.color.surface_active).text_color(t.color.content))
                            .tooltip(|_, cx| athena_ui::Tooltip::view("Close  ⌘W", cx))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_item_in(pane_id, item_id, window, cx);
                            }))
                            .child(
                                div()
                                    .invisible()
                                    .when(active && !dirty, |el| el.visible())
                                    .when(dirty, |el| {
                                        el.group_hover(close_group.clone(), |s| s.visible())
                                    })
                                    .when(!dirty, |el| el.group_hover(group, |s| s.visible()))
                                    .child("×"),
                            )
                            .when(dirty, |el| {
                                el.child(
                                    div()
                                        .absolute()
                                        .size(px(6.))
                                        .bg(t.color.content_disabled)
                                        .group_hover(close_group, |s| s.invisible()),
                                )
                            }),
                    )
                    .when(active && focused, |el| {
                        el.child(
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .bottom_0()
                                .h(px(1.))
                                .bg(t.color.accent),
                        )
                    });
                // Opacity and offset only; the strip never animates a tab's width.
                match leaving {
                    Some(generation) => motion::animate_exit(
                        t.motion.reduced,
                        tab,
                        ("tab-leave", generation),
                        t.motion.fast,
                        |el, d| el.opacity(1. - d),
                    ),
                    None => motion::animate_enter(
                        t.motion.reduced,
                        born,
                        tab,
                        ("tab-in", item.id.0),
                        Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
                        |el, d| el.opacity(d).top(px(4. * (1. - d))),
                    ),
                }
            })
            .collect();

        let active_id = pane.active_item().map(|i| i.id);
        let (scroll, revealed) = self
            .tab_scroll
            .entry((root.to_path_buf(), pane_id))
            .or_default();
        if *revealed != active_id {
            scroll.scroll_to_item(pane.active);
            *revealed = active_id;
        }
        let scroll = scroll.clone();
        let edges = |s: &gpui::ScrollHandle| {
            let (offset, max) = (-s.offset().x, s.max_offset().width);
            (offset > px(0.5), max - offset > px(0.5))
        };
        // Last frame's scroll position: the strip is laid out after this render.
        let (left, right) = edges(&scroll);
        let check = scroll.clone();
        let fades_stale = canvas(
            move |_, window, _| {
                if edges(&check) != (left, right) {
                    window.request_animation_frame();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_0();
        let fade = |left: bool| {
            let (solid, clear) = (t.color.surface, t.color.surface.opacity(0.));
            let (from, to) = if left { (solid, clear) } else { (clear, solid) };
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .w(px(16.))
                .when(left, |el| el.left_0())
                .when(!left, |el| el.right_0())
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(from, 0.),
                    linear_color_stop(to, 1.),
                ))
        };
        let strip = div()
            .h(px(TAB_STRIP_HEIGHT))
            .flex_none()
            .relative()
            .bg(t.color.surface)
            .border_b_1()
            .border_color(t.color.border)
            .child(
                div()
                    .id(("tabs", pane_id.0))
                    .size_full()
                    .flex()
                    .overflow_x_scroll()
                    .track_scroll(&scroll)
                    .children(tabs)
                    // Last, so tab indices still match the scroll handle's children.
                    .child(
                        div()
                            .id(("tab-strip-end", pane_id.0))
                            .flex_1()
                            .h_full()
                            .drag_over::<TabDrag>(|s, _, _, cx| {
                                s.bg(cx.theme().color.surface_accent)
                            })
                            .on_drop(cx.listener(move |this, drag: &TabDrag, window, cx| {
                                this.drop_tab_at(drag, pane_id, None, window, cx)
                            })),
                    ),
            )
            .when(left, |el| el.child(fade(true)))
            .when(right, |el| el.child(fade(false)))
            .child(fades_stale);

        let active = pane.active_item().cloned();
        let content: AnyElement = match active
            .as_ref()
            .and_then(|item| self.item_view(root, item, cx))
        {
            Some(view) => view.element(),
            None => div().into_any_element(),
        };
        let drop_layer = cx
            .has_active_drag()
            .then(|| self.render_drop_layer(pane_id, &t, cx));
        let (switches, fresh) = self.content_switches.fade(root, pane_id, t.motion.fast);
        // Opacity only: moving or resizing the box would resize the shell mid-animation.
        let content = motion::animate_enter(
            t.motion.reduced,
            fresh,
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(content)
                .children(drop_layer),
            (
                "tab-content",
                active.map_or(0, |i| i.id.0) ^ (switches << 32),
            ),
            Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d),
        );

        let body = div()
            .id(("pane", pane_id.0))
            .size_full()
            .flex()
            .flex_col()
            .bg(t.terminal.background)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| this.focus_pane(pane_id, window, cx)),
            )
            // So a context menu opened in the pane hands focus back to this pane's tab.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| this.focus_pane(pane_id, window, cx)),
            )
            .child(strip)
            .child(content);

        if let Some(closing) = self.leaving.get(&(root.to_path_buf(), pane_id)) {
            return motion::animate_exit(
                t.motion.reduced,
                // A pane on its way out no longer takes clicks, which would focus it again.
                body.capture_any_mouse_down(|_, _, cx| cx.stop_propagation()),
                ("pane-leave", closing.generation),
                t.motion.fast,
                |el, d| el.opacity(1. - d),
            );
        }
        if self.entering == Some(pane_id) {
            self.entering = None;
            self.pane_opening = Some((pane_id, motion::Opening::now()));
        }
        let fresh = self
            .pane_opening
            .is_some_and(|(p, o)| p == pane_id && o.running(t.motion.base));
        motion::animate_enter(
            t.motion.reduced,
            fresh,
            body,
            ("pane-enter", pane_id.0),
            Animation::new(t.motion.base).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d).top(px(4. * (1. - d))),
        )
    }

    /// Covers a pane's content while a tab is dragged, tinting the half (or all) it would land in.
    fn render_drop_layer(
        &self,
        pane: PaneId,
        t: &athena_ui::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let zone = self.drop_hint.filter(|(p, _)| *p == pane).map(|(_, z)| z);
        let tint = zone.map(|zone| {
            let half = relative(0.5);
            let el = div()
                .absolute()
                .bg(t.color.accent.opacity(0.12))
                .border_1()
                .border_color(t.color.accent);
            match zone {
                DropZone::Center => el.inset_0(),
                DropZone::Left => el.left_0().top_0().bottom_0().w(half),
                DropZone::Right => el.right_0().top_0().bottom_0().w(half),
                DropZone::Top => el.left_0().right_0().top_0().h(half),
                DropZone::Bottom => el.left_0().right_0().bottom_0().h(half),
            }
        });
        div()
            .id(("drop-layer", pane.0))
            .absolute()
            .inset_0()
            // Every pane hears every move of a drag, so each claims it only while under the cursor.
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<TabDrag>, _, cx| {
                    let at = event.event.position;
                    let next = if event.bounds.contains(&at) {
                        Some((pane, zone_for(event.bounds, at)))
                    } else if this.drop_hint.is_some_and(|(p, _)| p == pane) {
                        None
                    } else {
                        return;
                    };
                    if this.drop_hint != next {
                        this.drop_hint = next;
                        cx.notify();
                    }
                }),
            )
            .on_drop(cx.listener(move |this, drag: &TabDrag, window, cx| {
                this.drop_tab_on_pane(drag, pane, window, cx)
            }))
            .children(tint)
            .into_any_element()
    }

    /// A tab dropped on the strip: before the tab at `before`, or after the last one.
    fn drop_tab_at(
        &mut self,
        drag: &TabDrag,
        to: PaneId,
        before: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drop_hint = None;
        let Some(layout) = self.active_layout() else {
            return;
        };
        let Some(len) = layout.pane(to).map(|p| p.items.len()) else {
            return;
        };
        let item = drag.item;
        let here = layout
            .find_item(item)
            .filter(|(p, _)| *p == to)
            .map(|(_, at)| at);
        if layout.move_item(item, to, strip_drop_index(here, before, len)) {
            self.zoomed = None;
            self.after_tab_moved(item, drag.pane, None, window, cx);
        }
    }

    /// A tab dropped on a pane's content: its centre joins the pane, an edge splits it.
    fn drop_tab_on_pane(
        &mut self,
        drag: &TabDrag,
        target: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let zone = self
            .drop_hint
            .take()
            .filter(|(p, _)| *p == target)
            .map_or(DropZone::Center, |(_, z)| z);
        let Some(layout) = self.active_layout() else {
            return;
        };
        let split = match zone {
            DropZone::Center => None,
            DropZone::Left => Some((Axis::Horizontal, true)),
            DropZone::Right => Some((Axis::Horizontal, false)),
            DropZone::Top => Some((Axis::Vertical, true)),
            DropZone::Bottom => Some((Axis::Vertical, false)),
        };
        let moved = match split {
            None => {
                let len = layout.pane(target).map_or(0, |p| p.items.len());
                (drag.pane != target && layout.move_item(drag.item, target, len)).then_some(None)
            }
            Some((axis, first)) => layout
                .split_with_item(target, axis, drag.item, first)
                .map(Some),
        };
        if let Some(new_pane) = moved {
            self.zoomed = None;
            self.after_tab_moved(drag.item, drag.pane, new_pane, window, cx);
        }
    }

    fn after_tab_moved(
        &mut self,
        item: ItemId,
        from: PaneId,
        new_pane: Option<PaneId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only a new pane fades in; an existing one keeps showing its live content.
        if new_pane.is_some() {
            self.entering = new_pane;
        }
        self.note_tab_born(Some(item));
        let to = self
            .active_layout()
            .and_then(|l| l.find_item(item))
            .map(|(p, _)| p);
        for pane in to.filter(|_| new_pane.is_none()).into_iter().chain([from]) {
            self.note_content_switch(pane);
        }
        self.after_layout_change(window, cx);
    }

    fn render_divider_handle(
        &self,
        index: usize,
        divider: Divider,
        t: &athena_ui::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let area = *self.pane_area.borrow();
        let r = divider.rect;
        let horizontal = divider.axis == Axis::Horizontal;
        let (x, y, w, h) = if horizontal {
            (r.x - DIVIDER_HIT, r.y, r.w + 2. * DIVIDER_HIT, r.h)
        } else {
            (r.x, r.y - DIVIDER_HIT, r.w, r.h + 2. * DIVIDER_HIT)
        };
        let origin = area.origin;
        let split = Bounds::new(
            point(px(divider.split.x), px(divider.split.y)),
            size(px(divider.split.w), px(divider.split.h)),
        );
        let path = divider.path.clone();
        let axis = divider.axis;
        resize_handle(("divider", index), axis, t.color.accent)
            .absolute()
            .left(px(x) - origin.x)
            .top(px(y) - origin.y)
            .w(px(w))
            .h(px(h))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    if event.click_count == 2 {
                        this.drag = None;
                        this.center_split(path.clone(), split.size, axis, cx);
                        return;
                    }
                    this.drag = Some(Drag::Divider {
                        path: path.clone(),
                        axis,
                        split,
                    });
                }),
            )
            .into_any_element()
    }

    /// Double-clicked divider: splits the space evenly, easing the panes there unless motion is reduced.
    fn center_split(
        &mut self,
        path: NodePath,
        split: gpui::Size<Pixels>,
        axis: Axis,
        cx: &mut Context<Self>,
    ) {
        let len = f32::from(match axis {
            Axis::Horizontal => split.width,
            Axis::Vertical => split.height,
        });
        let Some(layout) = self.active_layout() else {
            return;
        };
        let Some(from) = layout.ratio_at(&path) else {
            return;
        };
        layout.set_ratio(&path, 0.5, len);
        self.schedule_save(cx);
        let reduced = cx.theme().motion.reduced;
        if !reduced && (from - 0.5).abs() > f32::EPSILON {
            let generation = self.next_generation();
            let Some(root) = self.active_root() else {
                return;
            };
            self.ratio_anim = Some(RatioAnim {
                root,
                path,
                from,
                opening: motion::Opening::now(),
                generation,
            });
            let delay = cx.theme().motion.fast;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                let _ = this.update(cx, |this, cx| {
                    if this
                        .ratio_anim
                        .as_ref()
                        .is_some_and(|a| a.generation == generation)
                    {
                        this.ratio_anim = None;
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        cx.notify();
    }

    /// Picks up edits made outside Athena, e.g. after the window regains focus.
    pub(super) fn reload_changed_files(&mut self, cx: &mut Context<Self>) {
        for view in self.items.values() {
            match view {
                ItemView::Editor(editor) => editor.update(cx, |v, cx| v.check_disk(cx)),
                ItemView::Image(image) => image.update(cx, |v, cx| v.reload_if_changed(cx)),
                ItemView::Doc(doc) => doc.update(cx, |v, cx| v.refresh(cx)),
                _ => {}
            }
        }
    }

    pub(super) fn autosave_delay(&self) -> Option<std::time::Duration> {
        let ms = self.workspace.autosave_delay_ms;
        (ms > 0).then(|| std::time::Duration::from_millis(ms))
    }

    /// Turns auto save off, or back on at the default delay, for every open editor.
    pub(super) fn toggle_autosave(&mut self, cx: &mut Context<Self>) {
        self.workspace.autosave_delay_ms = match self.workspace.autosave_delay_ms {
            0 => athena_workspace::DEFAULT_AUTOSAVE_DELAY_MS,
            _ => 0,
        };
        let delay = self.autosave_delay();
        for view in self.items.values() {
            if let ItemView::Editor(editor) = view {
                editor.update(cx, |v, cx| v.set_autosave(delay, cx));
            }
        }
        let (title, body) = match delay {
            Some(d) => (
                "Auto save is on",
                format!("Files save {} ms after you stop typing.", d.as_millis()),
            ),
            None => ("Auto save is off", "Save with ⌘S.".to_string()),
        };
        self.transient_notice(title, body, cx);
        self.schedule_save(cx);
    }

    /// Turns formatting on Cmd+S on or off for every language and every open editor.
    pub(super) fn toggle_format_on_save(&mut self, cx: &mut Context<Self>) {
        let on = !self.workspace.format_on_save.unwrap_or(false);
        self.workspace.format_on_save = Some(on);
        for view in self.items.values() {
            if let ItemView::Editor(editor) = view {
                editor.update(cx, |v, _| v.set_format_on_save(Some(on)));
            }
        }
        let (title, body) = match on {
            true => (
                "Format on save is on",
                "⌘S formats through the language server, then saves.",
            ),
            false => ("Format on save is off", "⌘S saves without formatting."),
        };
        self.transient_notice(title, body.to_string(), cx);
        self.schedule_save(cx);
    }

    /// Cmd+Shift+S: writes the focused editor to a new file and keeps editing it there.
    pub(super) fn save_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
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
        let Some(ItemView::Editor(editor)) = self.item_view(&root, &item, cx) else {
            return;
        };
        let from = editor.read(cx).path().to_path_buf();
        let dir = from.parent().unwrap_or(&root).to_path_buf();
        let name = from.file_name().map(|n| n.to_string_lossy().into_owned());
        let picked = cx.prompt_for_new_path(&dir, name.as_deref());
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(to))) = picked.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.finish_save_as(&root, item.id, &editor, from, to, window, cx)
            });
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_save_as(
        &mut self,
        root: &Path,
        item: ItemId,
        editor: &Entity<EditorView>,
        from: PathBuf,
        to: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = editor.update(cx, |e, cx| e.save_as(to.clone(), cx)) {
            let body = format!("{err:#}");
            self.transient_notice("Could not save the file", body, cx);
            return;
        }
        let item = self
            .workspace
            .projects
            .iter_mut()
            .find(|p| p.root == root)
            .and_then(|p| p.layout.as_mut())
            .and_then(|l| l.item_mut(item));
        if let Some(item) = item {
            item.kind = ItemKind::Editor { path: to };
        }
        self.lsp_closed(&from, cx);
        self.lsp_opened(root, editor, cx);
        self.tree.invalidate();
        self.after_layout_change(window, cx);
    }

    /// Previews of the file an editor changed show its unsaved text.
    fn follow_docs(&self, editor: &Entity<EditorView>, cx: &mut Context<Self>) {
        let path = editor.read(cx).path();
        let docs: Vec<_> = self
            .items
            .values()
            .filter_map(|v| match v {
                ItemView::Doc(doc) if doc.read(cx).path() == path => Some(doc.clone()),
                _ => None,
            })
            .collect();
        if docs.is_empty() {
            return;
        }
        let Some(text) = editor.read(cx).text() else {
            return;
        };
        for doc in docs {
            let text = text.clone();
            doc.update(cx, |d, cx| d.follow_text(text, cx));
        }
    }

    /// Re-renders previews of `path` after its editor saved it.
    fn refresh_docs(&mut self, path: &Path, cx: &mut Context<Self>) {
        for view in self.items.values() {
            if let ItemView::Doc(doc) = view
                && doc.read(cx).path() == path
            {
                doc.update(cx, |v, cx| v.refresh(cx));
            }
        }
    }

    /// Cmd+Shift+V: from a Markdown or Mermaid editor shows its preview beside it; from a preview
    /// goes back to the source.
    pub(super) fn toggle_rendered(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = self
            .workspace
            .active_project()
            .and_then(|p| p.layout.as_ref())
            .and_then(|l| l.focused_pane())
            .and_then(|p| p.active_item())
            .map(|i| i.kind.clone())
        else {
            return;
        };
        let path = match active {
            ItemKind::Rendered { path } => return self.open_file(path, window, cx),
            ItemKind::Editor { path } if is_document_path(&path) => path,
            _ => return,
        };
        let Some(layout) = self.active_layout() else {
            return;
        };
        let kind = ItemKind::Rendered { path };
        let existing = layout.panes().into_iter().find_map(|p| {
            p.items
                .iter()
                .position(|item| item.kind == kind)
                .map(|index| (p.id, index))
        });
        if let Some((pane, index)) = existing {
            return self.activate_tab(pane, index, window, cx);
        }
        let focused = layout.focused;
        let pane = layout.split(focused, Axis::Horizontal, kind);
        self.zoomed = None;
        self.entering = pane;
        self.after_layout_change(window, cx);
    }

    /// Opens a file a rendered document linked to; opening a tab needs the window.
    pub(super) fn take_pending_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.pending_open.take() {
            self.open_file(path, window, cx);
        }
    }

    /// Hangs up every shell in a project that is being closed.
    pub(super) fn drop_project_items(&mut self, root: &Path, cx: &mut Context<Self>) {
        self.tab_scroll.retain(|(r, _), _| r != root);
        self.content_switches.retain(|(r, _)| r != root);
        self.item_menus.retain(|(r, _)| r != root);
        let keys: Vec<_> = self
            .items
            .keys()
            .filter(|(r, _)| r == root)
            .cloned()
            .collect();
        for key in keys {
            if let Some(view) = self.items.remove(&key) {
                view.close(cx);
            }
        }
    }
}

/// Shows the tab next to `item` (as closing it would) if `item` is the one showing.
fn step_off(pane: &mut Pane, item: ItemId) -> bool {
    let Some(at) = pane.items.iter().position(|i| i.id == item) else {
        return false;
    };
    if pane.active != at || pane.items.len() < 2 {
        return false;
    }
    pane.active = if at + 1 < pane.items.len() {
        at + 1
    } else {
        at - 1
    };
    true
}

/// Closes `item` while its pane keeps showing the tab it showed before, unless that was `item`.
fn close_keeping_active(layout: &mut Layout, item: ItemId) -> bool {
    let shown = layout
        .find_item(item)
        .and_then(|(p, _)| layout.pane(p)?.active_item())
        .map(|i| i.id)
        .filter(|&id| id != item);
    if !layout.close_item(item) {
        return false;
    }
    if let Some(shown) = shown
        && let Some((pane, at)) = layout.find_item(shown)
        && let Some(pane) = layout.pane_mut(pane)
    {
        pane.active = at;
    }
    true
}

/// The pane that takes over from a closing one: its neighbour in the order `close_pane` tries,
/// else any pane that is not `gone`.
fn successor_pane(
    layout: &Layout,
    area: Rect,
    pane: PaneId,
    gone: impl Fn(PaneId) -> bool,
) -> Option<PaneId> {
    [
        Direction::Left,
        Direction::Up,
        Direction::Right,
        Direction::Down,
    ]
    .into_iter()
    .filter_map(|dir| layout.neighbor(area, pane, dir))
    .chain(layout.panes().into_iter().map(|p| p.id))
    .find(|&p| !gone(p))
}

/// The same kind of tab as `kind`, showing `path` instead.
pub(super) fn file_kind_like(kind: &ItemKind, path: PathBuf) -> ItemKind {
    match kind {
        ItemKind::Image { .. } => ItemKind::Image { path },
        ItemKind::Rendered { .. } => ItemKind::Rendered { path },
        ItemKind::Diff { base, .. } => ItemKind::Diff {
            path,
            base: base.clone(),
        },
        _ => ItemKind::Editor { path },
    }
}

/// Images open in the viewer; everything else in the text editor.
fn file_kind(path: PathBuf) -> ItemKind {
    if is_image_path(&path) {
        ItemKind::Image { path }
    } else {
        ItemKind::Editor { path }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tab_switch_fades_only_its_own_pane() {
        let (a, b) = (Path::new("/a"), Path::new("/b"));
        let long = std::time::Duration::from_secs(60);
        let mut switches = ContentSwitches::default();
        switches.note(a, PaneId(1));
        switches.note(a, PaneId(1));
        assert_eq!(switches.fade(a, PaneId(1), long), (2, true));
        assert_eq!(switches.fade(a, PaneId(2), long), (0, false));
        assert_eq!(switches.fade(b, PaneId(1), long), (0, false));
        assert!(!switches.fade(a, PaneId(1), std::time::Duration::ZERO).1);
        switches.retain(|(r, _)| r != a);
        assert_eq!(switches.fade(a, PaneId(1), long), (0, false));
    }

    fn term() -> ItemKind {
        ItemKind::Terminal { session: None }
    }

    /// One pane holding four tabs, the second one showing.
    fn four_tabs() -> (Layout, PaneId, Vec<ItemId>) {
        let mut layout = Layout::new(term());
        let pane = layout.focused;
        for _ in 0..3 {
            layout.add_item(pane, term());
        }
        let pane_ref = layout.pane_mut(pane).unwrap();
        pane_ref.active = 1;
        let ids = pane_ref.items.iter().map(|i| i.id).collect();
        (layout, pane, ids)
    }

    fn shown(layout: &Layout, pane: PaneId) -> ItemId {
        layout.pane(pane).unwrap().active_item().unwrap().id
    }

    #[test]
    fn closing_the_shown_tab_shows_its_neighbour_at_once_and_keeps_it_after() {
        let (mut layout, pane, ids) = four_tabs();
        assert!(step_off(layout.pane_mut(pane).unwrap(), ids[1]));
        assert_eq!(shown(&layout, pane), ids[2]);
        // The fade ends: the dying tab goes and the neighbour stays shown.
        assert!(close_keeping_active(&mut layout, ids[1]));
        assert_eq!(shown(&layout, pane), ids[2]);
        // A second close during the next fade moves on again.
        assert!(step_off(layout.pane_mut(pane).unwrap(), ids[2]));
        assert_eq!(shown(&layout, pane), ids[3]);
    }

    #[test]
    fn the_last_tab_steps_back_and_a_background_tab_steps_nowhere() {
        let (mut layout, pane, ids) = four_tabs();
        assert!(!step_off(layout.pane_mut(pane).unwrap(), ids[3]));
        layout.pane_mut(pane).unwrap().active = 3;
        assert!(step_off(layout.pane_mut(pane).unwrap(), ids[3]));
        assert_eq!(shown(&layout, pane), ids[2]);
    }

    #[test]
    fn closing_a_tab_left_of_the_shown_one_keeps_showing_it() {
        let (mut layout, pane, ids) = four_tabs();
        layout.pane_mut(pane).unwrap().active = 2;
        assert!(close_keeping_active(&mut layout, ids[0]));
        assert_eq!(shown(&layout, pane), ids[2]);
    }

    #[test]
    fn a_closing_pane_hands_over_to_a_pane_that_is_not_closing_too() {
        let mut layout = Layout::new(term());
        let left = layout.focused;
        let middle = layout.split(left, Axis::Horizontal, term()).unwrap();
        let right = layout.split(middle, Axis::Horizontal, term()).unwrap();
        let area = Rect {
            x: 0.,
            y: 0.,
            w: 900.,
            h: 600.,
        };
        assert_eq!(
            successor_pane(&layout, area, middle, |p| p == middle),
            Some(left)
        );
        let gone = |p: PaneId| p == middle || p == left;
        assert_eq!(successor_pane(&layout, area, middle, gone), Some(right));
        let all = |_: PaneId| true;
        assert_eq!(successor_pane(&layout, area, middle, all), None);
    }

    #[test]
    fn an_easing_split_is_matched_by_project_and_path() {
        let anim = RatioAnim {
            root: PathBuf::from("/a"),
            path: vec![true],
            from: 0.2,
            opening: motion::Opening::now(),
            generation: 1,
        };
        let long = std::time::Duration::from_secs(60);
        let start = anim.ratio(Path::new("/a"), &vec![true], 0.5, long).unwrap();
        assert!((start - 0.2).abs() < 0.05);
        let done = anim.ratio(Path::new("/a"), &vec![true], 0.5, std::time::Duration::ZERO);
        assert_eq!(done, Some(0.5));
        assert_eq!(anim.ratio(Path::new("/b"), &vec![true], 0.5, long), None);
        assert_eq!(anim.ratio(Path::new("/a"), &vec![false], 0.5, long), None);
    }

    #[test]
    fn tree_width_stays_between_its_limits() {
        assert_eq!(clamp_tree_width(240.), 240.);
        assert_eq!(clamp_tree_width(20.), 160.);
        assert_eq!(clamp_tree_width(900.), 480.);
    }

    #[test]
    fn drawer_leaves_room_for_the_panes() {
        assert_eq!(clamp_drawer_height(240., 900.), 240.);
        assert_eq!(clamp_drawer_height(800., 900.), 700.);
        assert_eq!(clamp_drawer_height(40., 900.), 120.);
        assert_eq!(clamp_drawer_height(240., 250.), 120.);
    }
}
