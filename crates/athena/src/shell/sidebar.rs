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
                self.workspace.move_folder(&path, &parent);
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

    /// Regroups the sidebar after a move, keeping the active project in sight.
    pub(super) fn folders_changed(&mut self, cx: &mut Context<Self>) {
        self.group_worktrees();
        if let Some(active) = self.workspace.active {
            self.workspace.reveal(active);
        }
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

    /// Right-click on a sidebar folder.
    fn open_folder_menu(
        &mut self,
        path: Vec<String>,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let collapsed = self.workspace.collapsed.contains(&path);
        let items = vec![
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
        ];
        self.open_context_menu(position, items, window, cx);
    }

    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
        let items: Vec<AnyElement> = rows
            .iter()
            .enumerate()
            .map(|(r, row)| match row {
                SidebarRow::Folder {
                    path,
                    depth,
                    collapsed,
                    projects,
                } => self.render_folder_row(r, path, *depth, *collapsed, *projects, &groups, cx),
                SidebarRow::Project { index, depth } => {
                    self.render_project_row(*index, *depth, monograms[*index].clone(), &groups, cx)
                }
            })
            .collect();

        let active_row = self
            .workspace
            .active
            .and_then(|a| row_of(&rows, &self.workspace, a));
        let from_row = row_of(&rows, &self.workspace, self.rail_from).or(active_row);
        let indicator = active_row.map(|to| {
            let s = t.ui_scale;
            let y = |r: usize| s * (TOP + r as f32 * (row_h + gap) + (row_h - 20.) / 2.);
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
                .flex()
                .items_center()
                .justify_between()
                .px(t.ui(6.))
                .child(add)
                .child(toggle)
        } else {
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(t.ui(RAIL_GAP))
                .child(add)
                .child(toggle)
        };

        div()
            .relative()
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
                    .flex()
                    .flex_col()
                    .when(!wide, |el| el.items_center())
                    .when(wide, |el| el.px(t.ui(6.)))
                    .gap(t.ui(gap))
                    .children(items),
            )
            .child(footer)
            .children(indicator)
    }

    #[allow(clippy::too_many_arguments)]
    fn render_folder_row(
        &self,
        r: usize,
        path: &[String],
        depth: usize,
        collapsed: bool,
        projects: usize,
        groups: &Rc<RailGroups>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let wide = self.workspace.ui.sidebar_expanded;
        let name = path.last().cloned().unwrap_or_default();
        let drop_tint = t.color.surface_accent;
        let drag = FolderDrag {
            path: path.to_vec(),
            label: name.clone().into(),
        };
        let tip = format!(
            "{} — {projects} project{}",
            folder_label(path),
            if projects == 1 { "" } else { "s" }
        );
        let fits = groups.clone();
        let into = path.to_vec();
        let el = div()
            .id(("rail-folder", r))
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
                .when(collapsed, |el| el.child(projects.to_string()))
                .into_any_element()
        } else {
            el.size(t.ui(RAIL_ITEM))
                .justify_center()
                .border_1()
                .border_color(t.color.border)
                .text_size(t.typography.caption)
                .child(initials(&name))
                .when(collapsed, |el| {
                    el.child(
                        div()
                            .absolute()
                            .right(px(2.))
                            .bottom(px(0.))
                            .text_size(px(9.))
                            .child(projects.to_string()),
                    )
                })
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
        let claude = self.project_claude_state(&project.root, cx).map(|state| {
            let color = match state {
                ClaudeState::NeedsInput => t.color.warning,
                ClaudeState::Running => t.color.success,
                ClaudeState::Finished => t.color.content_muted,
            };
            div().size(px(6.)).flex_none().bg(color)
        });
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
                .children(claude)
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
