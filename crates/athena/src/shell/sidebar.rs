use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use athena_term::ClaudeState;
use athena_ui::{ActiveTheme, Theme, Tooltip, motion};
use athena_workspace::{SidebarRow, Workspace};
use gpui::{
    Animation, AnyElement, Context, ExternalPaths, FontWeight, MouseButton, SharedString, Window,
    div, prelude::*, px,
};

use super::Shell;
use super::dnd::{ProjectDrag, TabGhost};
use super::palette::Mode;
use super::panes::ClaudeMark;
use super::worktrees::RailGroups;
use crate::actions::AddProject;

const RAIL_WIDTH: f32 = 48.;
const SIDEBAR_WIDTH: f32 = 220.;
const RAIL_ITEM: f32 = 32.;
const RAIL_GAP: f32 = 8.;
const ROW: f32 = 28.;
const ROW_GAP: f32 = 2.;
const TOP: f32 = 8.;
const INDENT: f32 = 12.;

/// A sidebar folder being dragged into another folder or out to the top level.
#[derive(Clone, Debug)]
pub(super) struct FolderDrag {
    pub path: Vec<String>,
    pub label: SharedString,
}

/// What the folder pickers act on.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Subject {
    Project(PathBuf),
    Folder(Vec<String>),
}

/// A folder picker row's effect.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum FolderPick {
    Move(Subject, Vec<String>),
    Rename(Vec<String>, String),
}

pub(super) fn folder_label(path: &[String]) -> String {
    path.join(" / ")
}

/// A typed folder path, `/` separating nested folders.
fn typed_folder(query: &str) -> Vec<String> {
    query
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Rows for Move to Folder: a new folder named by the query, the top level and every other folder.
pub(super) fn move_entries(
    workspace: &Workspace,
    subject: &Subject,
    query: &str,
) -> Vec<(String, Option<String>, FolderPick)> {
    let current: Vec<String> = match subject {
        Subject::Project(root) => workspace
            .projects
            .iter()
            .find(|p| p.root == *root)
            .map(|p| p.folder.clone())
            .unwrap_or_default(),
        Subject::Folder(path) => path[..path.len().saturating_sub(1)].to_vec(),
    };
    let fits = |target: &[String]| match subject {
        Subject::Folder(path) => !target.starts_with(path),
        Subject::Project(_) => true,
    };
    let folders = workspace.folders();
    let mut out = Vec::new();
    let typed = typed_folder(query);
    if !typed.is_empty() && !folders.contains(&typed) && fits(&typed) {
        out.push((
            format!("New folder “{}”", folder_label(&typed)),
            Some("type / to nest one folder in another".into()),
            FolderPick::Move(subject.clone(), typed),
        ));
    }
    if !current.is_empty() {
        out.push((
            "Top level".into(),
            Some("outside every folder".into()),
            FolderPick::Move(subject.clone(), Vec::new()),
        ));
    }
    for folder in folders {
        if folder != current && fits(&folder) {
            out.push((
                folder_label(&folder),
                None,
                FolderPick::Move(subject.clone(), folder),
            ));
        }
    }
    out
}

pub(super) fn rename_entries(
    folder: &[String],
    query: &str,
) -> Vec<(String, Option<String>, FolderPick)> {
    let name = query.trim();
    if name.is_empty() || name.contains('/') {
        return Vec::new();
    }
    vec![(
        format!(
            "Rename “{}” to “{name}”",
            folder.last().map_or("", String::as_str)
        ),
        None,
        FolderPick::Rename(folder.to_vec(), name.to_string()),
    )]
}

/// The row the active-project bar sits beside: the project's, or the closed folder hiding it.
fn row_of(rows: &[SidebarRow], workspace: &Workspace, index: usize) -> Option<usize> {
    let folder = &workspace.projects.get(index)?.folder;
    rows.iter().position(|row| match row {
        SidebarRow::Project { index: i, .. } => *i == index,
        SidebarRow::Folder {
            path, collapsed, ..
        } => *collapsed && folder.starts_with(path),
    })
}

/// The line span of each open folder with its depth, from its heading to its last shown line.
fn open_folder_spans(rows: &[SidebarRow], lines: &[Option<usize>]) -> Vec<(usize, Range<usize>)> {
    rows.iter()
        .enumerate()
        .filter_map(|(r, row)| {
            let SidebarRow::Folder {
                depth,
                collapsed: false,
                ..
            } = row
            else {
                return None;
            };
            let end = rows[r + 1..]
                .iter()
                .position(|row| row.depth() <= *depth)
                .map_or(rows.len(), |n| r + 1 + n);
            let first = lines[r]?;
            let last = lines[r..end].iter().rev().find_map(|l| *l)?;
            Some((*depth, first..last + 1))
        })
        .collect()
}

/// The top-level blocks starting at `starts` that get a divider above them: those beside a block
/// of more than one line, so lone projects stay undivided.
fn divider_lines(starts: &[usize], len: usize) -> Vec<usize> {
    let size = |k: usize| starts.get(k + 1).copied().unwrap_or(len) - starts[k];
    (1..starts.len())
        .filter(|&k| size(k - 1) > 1 || size(k) > 1)
        .map(|k| starts[k])
        .collect()
}

/// A filled square for Claude's state, or a hollow one for a Claude session with nothing to say.
fn claude_dot(t: &Theme, mark: ClaudeMark) -> Option<gpui::Div> {
    let dot = div().size(px(6.)).flex_none();
    match mark.state {
        Some(ClaudeState::NeedsInput) => Some(dot.bg(t.color.warning)),
        Some(ClaudeState::Running) => Some(dot.bg(t.color.success)),
        Some(ClaudeState::Finished) => Some(dot.bg(t.color.content_muted)),
        None => mark
            .open
            .then(|| dot.border_1().border_color(t.color.content_muted)),
    }
}

/// A cup drawn from borders, as an emoji would break the rail's flat colours.
fn awake_mark(t: &Theme, mark: ClaudeMark) -> Option<gpui::Div> {
    let color = t.color.warning;
    mark.awake.then(|| {
        div()
            .relative()
            .flex_none()
            .w(px(9.))
            .h(px(6.))
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .w(px(6.))
                    .h(px(6.))
                    .border_l_1()
                    .border_r_1()
                    .border_b_1()
                    .rounded_b(px(2.))
                    .border_color(color),
            )
            .child(
                div()
                    .absolute()
                    .left(px(5.))
                    .top(px(1.))
                    .w(px(3.))
                    .h(px(3.))
                    .border_t_1()
                    .border_r_1()
                    .border_b_1()
                    .rounded_r(px(1.))
                    .border_color(color),
            )
    })
}

/// The tooltip lines for what Claude is doing, the most urgent session first, and for sleep.
fn claude_lines(mark: ClaudeMark) -> Option<String> {
    let state = match mark.state {
        Some(ClaudeState::NeedsInput) => "Claude needs input",
        Some(ClaudeState::Running) => "Claude is running",
        Some(ClaudeState::Finished) => "Claude is done",
        None if mark.open => "Claude Code is open",
        None => return None,
    };
    let mut lines = match mark.sessions {
        0 | 1 => state.to_string(),
        n => format!("{state} ({n} sessions)"),
    };
    if mark.awake {
        lines.push_str("\nKeeping the Mac awake");
    }
    Some(lines)
}

/// Scrolls the rail the least that shows the line from `top` down `height`; false before the
/// rail has been laid out.
fn reveal_line(scroll: &gpui::ScrollHandle, top: f32, height: f32) -> bool {
    let view = f32::from(scroll.bounds().size.height);
    if view <= 0. {
        return false;
    }
    let shown = -f32::from(scroll.offset().y);
    let y = if top < shown {
        top
    } else if top + height > shown + view {
        top + height - view
    } else {
        return true;
    };
    let max = f32::from(scroll.max_offset().height);
    scroll.set_offset(gpui::point(px(0.), px(-y.clamp(0., max))));
    true
}

fn fold_tip(folded: bool, worktrees: usize) -> String {
    match (folded, worktrees) {
        (true, 1) => "Show 1 worktree".into(),
        (true, n) => format!("Show {n} worktrees"),
        (false, _) => "Hide worktrees".into(),
    }
}

fn initials(name: &str) -> String {
    name.chars().take(2).collect::<String>().to_uppercase()
}

impl Shell {
    pub(super) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        let ui = &mut self.workspace.ui;
        ui.sidebar_expanded = !ui.sidebar_expanded;
        self.schedule_save(cx);
        cx.notify();
    }

    pub(super) fn toggle_folder(&mut self, path: &[String], cx: &mut Context<Self>) {
        self.workspace.toggle_folder(path);
        self.schedule_save(cx);
        cx.notify();
    }

    pub(super) fn toggle_worktrees(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(project) = self.workspace.projects.get_mut(index) {
            project.worktrees_collapsed = !project.worktrees_collapsed;
            self.schedule_save(cx);
            cx.notify();
        }
    }

    /// Opens or closes the folder holding the active project.
    pub(super) fn toggle_active_folder(&mut self, cx: &mut Context<Self>) {
        if let Some(folder) = self
            .workspace
            .active_project()
            .map(|p| p.folder.clone())
            .filter(|f| !f.is_empty())
        {
            self.toggle_folder(&folder, cx);
        }
    }

    /// Shows or hides the worktrees of the active project's repository.
    pub(super) fn toggle_active_worktrees(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        let groups = self.rail_groups();
        let wide = self.workspace.ui.sidebar_expanded;
        if let Some((_, group)) = groups.group_of(&root)
            && self.worktree_count(group.start, &groups, wide) > 0
        {
            self.toggle_worktrees(group.start, cx);
        }
    }

    pub(super) fn collapse_all_folders(&mut self, cx: &mut Context<Self>) {
        self.workspace.collapsed = self.workspace.folders();
        self.schedule_save(cx);
        cx.notify();
    }

    /// The linked worktrees of the repository at `index`, the unopened ones too when `wide` shows
    /// them; 0 for a linked worktree.
    pub(super) fn worktree_count(&self, index: usize, groups: &RailGroups, wide: bool) -> usize {
        let root = &self.workspace.projects[index].root;
        match groups.group_of(root) {
            Some((from, group)) if from == group.start => {
                let unopened = if wide {
                    self.unopened_worktrees(root).len()
                } else {
                    0
                };
                group.len() - 1 + unopened
            }
            _ => 0,
        }
    }

    /// The projects the sidebar shows a row for, top to bottom, which ⌥⌘1…9 count.
    pub(super) fn shown_projects(&self) -> Vec<usize> {
        let groups = self.rail_groups();
        self.workspace
            .sidebar_rows()
            .into_iter()
            .filter_map(|row| match row {
                SidebarRow::Project { index, .. } => Some(index),
                SidebarRow::Folder { .. } => None,
            })
            .filter(|&index| !self.worktree_hidden(index, &groups))
            .collect()
    }

    /// A linked worktree the sidebar leaves out, its repository collapsed and it not active.
    fn worktree_hidden(&self, index: usize, groups: &RailGroups) -> bool {
        let root = &self.workspace.projects[index].root;
        groups.group_of(root).is_some_and(|(from, group)| {
            from != group.start
                && self.workspace.projects[group.start].worktrees_collapsed
                && Some(index) != self.workspace.active
        })
    }

    /// Opens the folder picker for the active project.
    pub(super) fn move_project_to_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(root) = self.workspace.active_project().map(|p| p.root.clone()) {
            self.pick_folder_for(Subject::Project(root), window, cx);
        }
    }

    pub(super) fn pick_folder_for(
        &mut self,
        subject: Subject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_subject = Some(subject);
        self.open_palette(Mode::MoveToFolder, window, cx);
    }

    fn rename_folder_prompt(
        &mut self,
        path: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = path.last().cloned().unwrap_or_default();
        self.sidebar_subject = Some(Subject::Folder(path));
        self.open_palette_with(Mode::RenameFolder, &name, window, cx);
    }

    pub(super) fn run_folder_pick(&mut self, pick: FolderPick, cx: &mut Context<Self>) {
        match pick {
            FolderPick::Move(Subject::Project(root), folder) => {
                if let Some(block) = self.rail_groups().block(&root) {
                    self.workspace.move_to_folder(block, &folder);
                }
            }
            FolderPick::Move(Subject::Folder(path), parent) => {
                if !self.workspace.move_folder(&path, &parent) {
                    self.transient_notice(
                        "Folder not moved",
                        format!(
                            "“{}” already holds a folder of that name.",
                            folder_label(&parent)
                        ),
                        cx,
                    );
                }
            }
            FolderPick::Rename(path, name) => {
                if !self.workspace.rename_folder(&path, &name) {
                    self.transient_notice(
                        "Folder not renamed",
                        format!("Another folder beside it is already called “{name}”."),
                        cx,
                    );
                }
            }
        }
        self.folders_changed(cx);
    }

    /// Regroups the sidebar after a move.
    pub(super) fn folders_changed(&mut self, cx: &mut Context<Self>) {
        self.group_worktrees();
        self.rail_from = self.workspace.active.unwrap_or(0);
        self.schedule_save(cx);
        cx.notify();
    }

    /// A project dropped just before the one at `before`, into its folder, or last at the top level.
    pub(super) fn drop_project(
        &mut self,
        drag: &ProjectDrag,
        before: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let Some((block, at, keep_folder)) = self.rail_groups().beside(&drag.root, before) else {
            return;
        };
        match before {
            _ if keep_folder => {
                self.workspace.move_projects(block, at);
            }
            Some(_) => self.workspace.move_beside(block, at),
            None => self.workspace.move_to_folder(block, &[]),
        }
        self.folders_changed(cx);
    }

    fn drop_project_in_folder(
        &mut self,
        drag: &ProjectDrag,
        folder: &[String],
        cx: &mut Context<Self>,
    ) {
        if let Some(block) = self.rail_groups().block(&drag.root) {
            self.workspace.move_to_folder(block, folder);
            self.folders_changed(cx);
        }
    }

    fn drop_folder(&mut self, drag: &FolderDrag, parent: &[String], cx: &mut Context<Self>) {
        if self.workspace.move_folder(&drag.path, parent) {
            self.folders_changed(cx);
        }
    }

    /// A closed folder's projects in the narrow rail, to switch to without opening it.
    fn folder_project_items(
        &self,
        path: &[String],
        cx: &mut Context<Self>,
    ) -> Vec<athena_ui::MenuItem> {
        let groups = self.rail_groups();
        let mut items = Vec::new();
        for i in self.workspace.folder_range(path) {
            let project = &self.workspace.projects[i];
            let mut label = project.name();
            if groups
                .group_of(&project.root)
                .is_some_and(|(from, group)| from != group.start)
            {
                label = format!("↳ {label}");
            }
            let sub = &project.folder[path.len()..];
            if !sub.is_empty() {
                label = format!("{} / {label}", folder_label(sub));
            }
            let item =
                super::menus::shell_item(label, cx, move |this, _, cx| this.switch(i, false, cx));
            items.push(if Some(i) == self.workspace.active {
                item.hint("current")
            } else {
                item
            });
        }
        items.push(athena_ui::MenuItem::separator());
        items
    }

    /// Right-click on a sidebar folder.
    fn open_folder_menu(
        &mut self,
        path: Vec<String>,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let collapsed = self.workspace.collapsed.contains(&path);
        let mut items = if collapsed && !self.workspace.ui.sidebar_expanded {
            self.folder_project_items(&path, cx)
        } else {
            Vec::new()
        };
        items.extend([
            super::menus::shell_item(if collapsed { "Expand" } else { "Collapse" }, cx, {
                let path = path.clone();
                move |this, _, cx| this.toggle_folder(&path, cx)
            }),
            athena_ui::MenuItem::separator(),
            super::menus::shell_item("Rename Folder…", cx, {
                let path = path.clone();
                move |this, w, cx| this.rename_folder_prompt(path.clone(), w, cx)
            }),
            super::menus::shell_item("Move Folder…", cx, {
                let path = path.clone();
                move |this, w, cx| this.pick_folder_for(Subject::Folder(path.clone()), w, cx)
            }),
            super::menus::shell_item("Remove Folder (Keep Projects)", cx, move |this, _, cx| {
                this.workspace.delete_folder(&path);
                this.folders_changed(cx);
            }),
        ]);
        self.open_context_menu(position, items, window, cx);
    }

    pub(super) fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let wide = self.workspace.ui.sidebar_expanded;
        let (row_h, gap) = if wide {
            (ROW, ROW_GAP)
        } else {
            (RAIL_ITEM, RAIL_GAP)
        };
        let rows = self.workspace.sidebar_rows();
        let monograms = self.workspace.monograms();
        let groups = Rc::new(self.rail_groups());
        let mut items: Vec<AnyElement> = Vec::with_capacity(rows.len());
        // Each row's line, None when hidden; unopened worktrees add lines of their own when wide.
        let mut line_of_row = Vec::with_capacity(rows.len());
        // Lines where a top-level folder, repository with its worktrees, or lone project begins.
        let mut block_starts = Vec::new();
        for row in &rows {
            match row {
                SidebarRow::Folder {
                    path,
                    depth,
                    collapsed,
                    projects,
                } => {
                    line_of_row.push(Some(items.len()));
                    if *depth == 0 {
                        block_starts.push(items.len());
                    }
                    items.push(self.render_folder_row(
                        path, *depth, *collapsed, *projects, &monograms, &groups, cx,
                    ))
                }
                SidebarRow::Project { index, depth } => {
                    if self.worktree_hidden(*index, &groups) {
                        line_of_row.push(None);
                        continue;
                    }
                    line_of_row.push(Some(items.len()));
                    let linked = groups
                        .group_of(&self.workspace.projects[*index].root)
                        .is_some_and(|(from, group)| from != group.start);
                    if *depth == 0 && !linked {
                        block_starts.push(items.len());
                    }
                    items.push(self.render_project_row(
                        *index,
                        *depth,
                        monograms[*index].clone(),
                        &groups,
                        cx,
                    ));
                    if wide {
                        items.extend(self.render_unopened(*index, *depth, &groups, cx));
                    }
                }
            }
        }

        let line =
            |index: usize| row_of(&rows, &self.workspace, index).and_then(|r| line_of_row[r]);
        let active_row = self.workspace.active.and_then(line);
        if self.rail_revealed != self.switch_count
            && let Some(row) = active_row
        {
            let s = t.ui_scale;
            let top = s * row as f32 * (row_h + gap);
            if reveal_line(&self.rail_scroll, top, s * row_h) {
                self.rail_revealed = self.switch_count;
            }
        }
        let from_row = line(self.rail_from).or(active_row);
        let indicator = active_row.map(|to| {
            let s = t.ui_scale;
            let y = |r: usize| s * (r as f32 * (row_h + gap) + (row_h - 20.) / 2.);
            let (from_y, to_y) = (y(from_row.unwrap_or(to)), y(to));
            let bar = div()
                .absolute()
                .left_0()
                .top(px(to_y))
                .w(px(2.))
                .h(t.ui(20.))
                .bg(t.color.accent);
            motion::animate_if(
                t.motion.reduced || from_y == to_y,
                bar,
                ("rail-indicator", self.switch_count),
                Animation::new(t.motion.base).with_easing(motion::ease_standard()),
                move |el, d| el.top(px(from_y + (to_y - from_y) * d)),
            )
        });

        // Overlays placed by line, like the indicator, so the rail's lines keep one height.
        let pitch = row_h + gap;
        let s = t.ui_scale;
        let mid = RAIL_WIDTH / 2.;
        let trays: Vec<AnyElement> = if wide {
            Vec::new()
        } else {
            open_folder_spans(&rows, &line_of_row)
                .into_iter()
                .map(|(depth, span)| {
                    let pad = if depth == 0 { 4. } else { 2. };
                    div()
                        .absolute()
                        .top(px(s * (span.start as f32 * pitch - pad)))
                        .h(px(s * (span.len() as f32 * pitch - gap + 2. * pad)))
                        .left(px(s * (mid - RAIL_ITEM / 2. - pad)))
                        .w(px(s * (RAIL_ITEM + 2. * pad)))
                        .rounded(t.shape.radius_control)
                        .bg(t.color.surface_sunken)
                        .into_any_element()
                })
                .collect()
        };
        let dividers: Vec<AnyElement> = if wide {
            Vec::new()
        } else {
            divider_lines(&block_starts, items.len())
                .into_iter()
                .map(|line| {
                    div()
                        .absolute()
                        .top(px(s * (line as f32 * pitch - gap / 2.)))
                        .left(px(s * (mid - 10.)))
                        .w(px(s * 20.))
                        .h(px(1.))
                        .bg(t.color.border)
                        .into_any_element()
                })
                .collect()
        };

        let drop_tint = t.color.surface_accent;
        let add = sidebar_button(&t, "rail-add", "+", wide.then_some("Open project"))
            .tooltip(|_, cx| Tooltip::view("Open project  ⌘O", cx))
            .drag_over::<ProjectDrag>({
                let groups = groups.clone();
                move |s, drag, _, _| match groups.beside(&drag.root, None) {
                    Some(_) => s.bg(drop_tint),
                    None => s,
                }
            })
            .drag_over::<FolderDrag>(move |s, _, _, _| s.bg(drop_tint))
            .on_drop(
                cx.listener(|this, drag: &ProjectDrag, _, cx| this.drop_project(drag, None, cx)),
            )
            .on_drop(cx.listener(|this, drag: &FolderDrag, _, cx| this.drop_folder(drag, &[], cx)))
            .on_click(cx.listener(|this, _, window, cx| this.add_project(&AddProject, window, cx)));
        let toggle = sidebar_button(&t, "rail-toggle", if wide { "⇤" } else { "⇥" }, None)
            .tooltip(move |_, cx| {
                let label = if wide {
                    "Collapse sidebar  ⌥⌘B"
                } else {
                    "Expand sidebar  ⌥⌘B"
                };
                Tooltip::view(label, cx)
            })
            .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)));
        let footer = if wide {
            div()
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .px(t.ui(6.))
                .child(add)
                .child(toggle)
        } else {
            div()
                .flex_none()
                .flex()
                .flex_col()
                .items_center()
                .gap(t.ui(RAIL_GAP))
                .child(add)
                .child(toggle)
        };

        div()
            .w(t.ui(if wide { SIDEBAR_WIDTH } else { RAIL_WIDTH }))
            .flex_none()
            .drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(drop_tint))
            .flex()
            .flex_col()
            .justify_between()
            .py(t.ui(TOP))
            .bg(t.color.surface)
            .border_r_1()
            .border_color(t.color.border)
            .child(
                div()
                    .id("sidebar-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.rail_scroll)
                    .child(
                        div()
                            .relative()
                            .flex_none()
                            .flex()
                            .flex_col()
                            .when(!wide, |el| el.items_center())
                            .when(wide, |el| el.px(t.ui(6.)))
                            .gap(t.ui(gap))
                            .children(trays)
                            .children(dividers)
                            .children(items)
                            .children(indicator),
                    ),
            )
            .child(footer)
    }

    /// After the last project of a repository's group, its worktrees that are not open, dimmed.
    fn render_unopened(
        &self,
        index: usize,
        depth: usize,
        groups: &Rc<RailGroups>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let root = &self.workspace.projects[index].root;
        let Some((_, group)) = groups.group_of(root) else {
            return Vec::new();
        };
        let main = &self.workspace.projects[group.start];
        if group.end != index + 1 || main.worktrees_collapsed {
            return Vec::new();
        }
        let main = &main.root;
        let t = cx.theme().clone();
        self.unopened_worktrees(main)
            .into_iter()
            .map(|w| {
                let path = w.path.clone();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let tip = format!("{} — not open; click to open it", path.display());
                div()
                    .id(SharedString::from(format!(
                        "rail-unopened-{}",
                        path.display()
                    )))
                    .h(t.ui(ROW))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(t.ui(6.))
                    .pl(t.ui(6. + INDENT * (depth + 1) as f32))
                    .pr(t.ui(6.))
                    .rounded(t.shape.radius_control)
                    .text_size(t.typography.caption)
                    .text_color(t.color.content_muted)
                    .opacity(0.6)
                    .cursor_pointer()
                    .hover(|s| s.opacity(1.).bg(t.color.surface_hover))
                    .tooltip(move |_, cx| Tooltip::view(tip.clone(), cx))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.open_worktree(path.clone(), cx)),
                    )
                    .child("↳")
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .children(
                        w.branch
                            .clone()
                            .map(|b| div().flex_none().max_w(t.ui(90.)).truncate().child(b)),
                    )
                    .into_any_element()
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_folder_row(
        &self,
        path: &[String],
        depth: usize,
        collapsed: bool,
        projects: usize,
        monograms: &[String],
        groups: &Rc<RailGroups>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let wide = self.workspace.ui.sidebar_expanded;
        let name = path.last().cloned().unwrap_or_default();
        let inside = self.workspace.folder_range(path);
        let mark = if collapsed {
            let roots: Vec<&Path> = self.workspace.projects[inside.clone()]
                .iter()
                .map(|p| p.root.as_path())
                .collect();
            self.rail_claude(&roots)
        } else {
            ClaudeMark::default()
        };
        let claude = claude_dot(&t, mark);
        let awake = awake_mark(&t, mark);
        let drop_tint = t.color.surface_accent;
        let drag = FolderDrag {
            path: path.to_vec(),
            label: name.clone().into(),
        };
        let mut tip = format!(
            "{} — {projects} project{}",
            folder_label(path),
            if projects == 1 { "" } else { "s" }
        );
        if let Some(line) = claude_lines(mark) {
            tip = format!("{tip}\n{line}");
        }
        let fits = groups.clone();
        let into = path.to_vec();
        let el = div()
            .id(SharedString::from(format!(
                "rail-folder-{}",
                path.join("/")
            )))
            .relative()
            .flex()
            .items_center()
            .rounded(t.shape.radius_control)
            .cursor_pointer()
            .text_color(t.color.content_muted)
            .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
            .tooltip(move |_, cx| Tooltip::view(tip.clone(), cx))
            .on_click(cx.listener({
                let path = path.to_vec();
                move |this, _, _, cx| this.toggle_folder(&path, cx)
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    let path = path.to_vec();
                    move |this, e: &gpui::MouseDownEvent, window, cx| {
                        this.open_folder_menu(path.clone(), e.position, window, cx)
                    }
                }),
            )
            .on_drag(drag, |drag: &FolderDrag, _, _, cx| {
                let label = drag.label.clone();
                cx.new(|_| TabGhost { label })
            })
            .drag_over::<ProjectDrag>(move |s, drag, _, _| match fits.block(&drag.root) {
                Some(_) => s.bg(drop_tint),
                None => s,
            })
            .drag_over::<FolderDrag>({
                let into = into.clone();
                move |s, drag, _, _| {
                    if into.starts_with(&drag.path) {
                        s
                    } else {
                        s.bg(drop_tint)
                    }
                }
            })
            .on_drop(cx.listener({
                let into = into.clone();
                move |this, drag: &ProjectDrag, _, cx| this.drop_project_in_folder(drag, &into, cx)
            }))
            .on_drop(cx.listener(move |this, drag: &FolderDrag, _, cx| {
                if !into.starts_with(&drag.path) {
                    this.drop_folder(drag, &into, cx)
                }
            }));
        if wide {
            el.h(t.ui(ROW))
                .w_full()
                .pl(t.ui(6. + INDENT * depth as f32))
                .pr(t.ui(6.))
                .gap(t.ui(6.))
                .text_size(t.typography.caption)
                .font_weight(FontWeight::SEMIBOLD)
                .child(if collapsed { "▸" } else { "▾" })
                .child(div().flex_1().min_w_0().truncate().child(name))
                .children(awake)
                .children(claude)
                .when(collapsed, |el| el.child(projects.to_string()))
                .into_any_element()
        } else if collapsed {
            // Its first projects in miniature, so a closed folder never passes for a project.
            let cells = monograms[inside].iter().take(4).map(|m| {
                div()
                    .size(t.ui(11.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(2.))
                    .bg(t.color.surface_hover)
                    .text_size(t.ui(8.))
                    .child(m.chars().next().map(String::from).unwrap_or_default())
            });
            el.size(t.ui(RAIL_ITEM))
                .flex_wrap()
                .content_center()
                .justify_center()
                .gap(t.ui(2.))
                .p(t.ui(3.))
                .border_1()
                .border_color(t.color.border)
                .children(cells)
                .children(claude.map(|dot| dot.absolute().right(px(2.)).bottom(px(2.))))
                .children(awake.map(|cup| cup.absolute().left(px(2.)).top(px(2.))))
                .into_any_element()
        } else {
            el.size(t.ui(RAIL_ITEM))
                .justify_center()
                .border_1()
                .border_color(t.color.border)
                .text_size(t.typography.caption)
                .child(initials(&name))
                .into_any_element()
        }
    }

    fn render_project_row(
        &self,
        i: usize,
        depth: usize,
        monogram: String,
        groups: &Rc<RailGroups>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let wide = self.workspace.ui.sidebar_expanded;
        let gap = if wide { ROW_GAP } else { RAIL_GAP };
        let project = &self.workspace.projects[i];
        let selected = Some(i) == self.workspace.active;
        let main = self
            .worktree_main(&project.root)
            .filter(|m| self.workspace.projects.iter().any(|p| p.root == *m));
        let caption = self.git_caption(&project.root);
        let mut tip = match &main {
            Some(main) => format!(
                "{} — worktree of {}",
                project.root.display(),
                main.display()
            ),
            None => project.root.display().to_string(),
        };
        if let Some(caption) = &caption {
            tip = format!("{tip}\n{caption}");
        }
        let group = format!("rail-item-{i}");
        let drag = ProjectDrag {
            root: project.root.clone(),
            label: project.name().into(),
        };
        let fits = groups.clone();
        let insertion = div()
            .absolute()
            .left_0()
            .top(-t.ui(gap / 2.) - px(1.))
            .when(wide, |el| el.w_full())
            .when(!wide, |el| el.w(t.ui(RAIL_ITEM)))
            .h(px(2.))
            .bg(t.color.accent)
            .invisible()
            .group_drag_over::<ProjectDrag>(group.clone(), |s| s.visible())
            .can_drop(move |drag, _, _| {
                drag.downcast_ref::<ProjectDrag>()
                    .is_some_and(|d| fits.beside(&d.root, Some(i)).is_some())
            })
            // The drop listener gives the bar the hitbox its drag-over style is gated on.
            .on_drop(cx.listener(move |this, drag: &ProjectDrag, _, cx| {
                this.drop_project(drag, Some(i), cx)
            }));
        let worktrees = self.worktree_count(i, groups, wide);
        let folded = worktrees > 0 && project.worktrees_collapsed;
        // A collapsed repository's row speaks for the worktrees it hides.
        let mut claude_roots = vec![project.root.as_path()];
        if folded && let Some((_, group)) = groups.group_of(&project.root) {
            claude_roots.extend(
                (group.start + 1..group.end)
                    .filter(|&j| Some(j) != self.workspace.active)
                    .map(|j| self.workspace.projects[j].root.as_path()),
            );
        }
        let mark = self.rail_claude(&claude_roots);
        if let Some(line) = claude_lines(mark) {
            tip = format!("{tip}\n{line}");
        }
        let claude = claude_dot(&t, mark);
        let awake = awake_mark(&t, mark);
        let el = div()
            .id(("rail-item", i))
            .group(group)
            .relative()
            .flex()
            .items_center()
            .rounded(t.shape.radius_control)
            .text_size(t.typography.caption)
            .cursor_pointer()
            .text_color(if selected {
                t.color.accent
            } else {
                t.color.content_muted
            })
            .when(!selected, |el| {
                el.hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
            })
            .tooltip(move |_, cx| Tooltip::view(tip.clone(), cx))
            .on_click(cx.listener(move |this, _, _, cx| this.switch_to(i, cx)))
            .on_drag(drag, |drag: &ProjectDrag, _, _, cx| {
                let label = drag.label.clone();
                cx.new(|_| TabGhost { label })
            })
            .on_drop(cx.listener(move |this, drag: &ProjectDrag, _, cx| {
                this.drop_project(drag, Some(i), cx)
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &gpui::MouseDownEvent, window, cx| {
                    this.open_rail_menu(i, e.position, window, cx)
                }),
            )
            .child(insertion);
        if wide {
            let indent = depth + usize::from(main.is_some());
            let branch = caption.map(|c| {
                div()
                    .flex_none()
                    .max_w(t.ui(90.))
                    .truncate()
                    .text_color(t.color.content_muted)
                    .child(c)
            });
            let fold = (worktrees > 0).then(|| {
                div()
                    .id(("rail-fold", i))
                    .flex_none()
                    .px(t.ui(4.))
                    .rounded(t.shape.radius_control)
                    .text_color(t.color.content_muted)
                    .hover(|s| s.bg(t.color.surface).text_color(t.color.content))
                    .tooltip(move |_, cx| Tooltip::view(fold_tip(folded, worktrees), cx))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_worktrees(i, cx)
                    }))
                    .child(if folded {
                        format!("▸ {worktrees}")
                    } else {
                        "▾".into()
                    })
            });
            el.h(t.ui(ROW))
                .w_full()
                .pl(t.ui(6. + INDENT * indent as f32))
                .pr(t.ui(6.))
                .gap(t.ui(6.))
                .when(selected, |el| el.bg(t.color.surface_hover))
                .when(main.is_some(), |el| el.child("↳"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .child(project.name()),
                )
                .children(branch)
                .children(awake)
                .children(claude)
                .children(fold)
                .into_any_element()
        } else {
            el.size(t.ui(RAIL_ITEM))
                .justify_center()
                .font_weight(FontWeight::MEDIUM)
                .child(monogram)
                .children(main.map(|_| {
                    // Joins a worktree to the repository above it, as one group.
                    div()
                        .absolute()
                        .top(-t.ui(RAIL_GAP))
                        .left(t.ui(RAIL_ITEM / 2.) - px(0.5))
                        .w(px(1.))
                        .h(t.ui(RAIL_GAP))
                        .bg(t.color.border_strong)
                }))
                .children(claude.map(|dot| dot.absolute().right(px(2.)).bottom(px(2.))))
                .children(awake.map(|cup| cup.absolute().left(px(2.)).top(px(2.))))
                .when(folded, |el| {
                    el.child(
                        div()
                            .id(("rail-fold", i))
                            .absolute()
                            .right(px(1.))
                            .top(px(0.))
                            .px(px(1.))
                            .rounded(px(2.))
                            .text_size(px(9.))
                            .hover(|s| s.bg(t.color.surface).text_color(t.color.content))
                            .tooltip(move |_, cx| Tooltip::view(fold_tip(true, worktrees), cx))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.toggle_worktrees(i, cx)
                            }))
                            .child(worktrees.to_string()),
                    )
                })
                .into_any_element()
        }
    }
}

fn sidebar_button(
    t: &Theme,
    id: &'static str,
    glyph: &'static str,
    label: Option<&'static str>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(t.ui(RAIL_ITEM))
        .when(label.is_none(), |el| el.w(t.ui(RAIL_ITEM)))
        .px(t.ui(if label.is_some() { 8. } else { 0. }))
        .flex()
        .items_center()
        .justify_center()
        .gap(t.ui(6.))
        .rounded(t.shape.radius_control)
        .text_size(t.typography.body)
        .text_color(t.color.content_muted)
        .cursor_pointer()
        .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
        .child(glyph)
        .children(label.map(|l| div().text_size(t.typography.caption).child(l)))
}

impl RailGroups {
    /// The repository at `root` and its worktrees; None for a linked worktree, which stays put.
    pub fn block(&self, root: &Path) -> Option<Range<usize>> {
        let (from, group) = self.group_of(root)?;
        (from == group.start).then_some(group)
    }

    /// What moves when `root` is dropped before `before` (or last), where to, and whether it keeps
    /// its folder; None when a worktree would leave its repository.
    pub fn beside(
        &self,
        root: &Path,
        before: Option<usize>,
    ) -> Option<(Range<usize>, usize, bool)> {
        let (from, group) = self.group_of(root)?;
        if from != group.start {
            return self
                .landing(root, before)
                .map(|(block, at)| (block, at, true));
        }
        let at = before.unwrap_or(self.len());
        (self.starts_group(at) && at != group.start).then_some((group, at, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(path: &str, collapsed: bool) -> SidebarRow {
        let path: Vec<String> = path.split('/').map(String::from).collect();
        SidebarRow::Folder {
            depth: path.len() - 1,
            path,
            collapsed,
            projects: 1,
        }
    }

    fn project(index: usize, depth: usize) -> SidebarRow {
        SidebarRow::Project { index, depth }
    }

    #[test]
    fn an_open_folder_spans_its_shown_lines_and_a_closed_one_none() {
        let rows = [
            folder("work", false),
            project(0, 1),
            folder("work/clients", false),
            project(1, 2),
            project(2, 2),
            project(3, 1),
            project(4, 0),
            folder("hobby", true),
        ];
        // The last worktree in work/clients is hidden.
        let lines = [
            Some(0),
            Some(1),
            Some(2),
            Some(3),
            None,
            Some(4),
            Some(5),
            Some(6),
        ];
        assert_eq!(open_folder_spans(&rows, &lines), [(0, 0..5), (1, 2..4)]);
    }

    #[test]
    fn the_tooltip_names_the_most_urgent_state_and_how_many_sessions() {
        let mark = |state, sessions, awake| ClaudeMark {
            state,
            open: true,
            awake,
            sessions,
        };
        assert_eq!(
            claude_lines(mark(Some(ClaudeState::NeedsInput), 3, true)).as_deref(),
            Some("Claude needs input (3 sessions)\nKeeping the Mac awake")
        );
        assert_eq!(
            claude_lines(mark(None, 1, false)).as_deref(),
            Some("Claude Code is open")
        );
        assert_eq!(claude_lines(ClaudeMark::default()), None);
    }

    #[test]
    fn dividers_only_border_blocks_of_more_than_one_line() {
        assert_eq!(divider_lines(&[0, 1, 2, 5, 6], 7), [2, 5]);
        assert!(divider_lines(&[0, 1, 2], 3).is_empty());
        assert!(divider_lines(&[], 0).is_empty());
    }
}
