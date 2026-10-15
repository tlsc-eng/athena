use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use athena_term::ClaudeState;
use athena_ui::{ActiveTheme, Theme, Tooltip, motion};
use athena_workspace::{FolderColor, FolderIcon, SidebarRow, Workspace, default_look};
use gpui::{
    Animation, AnyElement, Context, ExternalPaths, FontWeight, Hsla, MouseButton, SharedString,
    Window, div, prelude::*, px, svg,
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
/// Height of the spacer that, with the row gap, sets blocks apart from what follows them.
const BLOCK_SPACER: f32 = 2.;
/// Alphas of a folder's colour: its open backing, that backing's edge, and its closed tile.
const TRAY_TINT: f32 = 0.12;
const TRAY_EDGE: f32 = 0.3;
const TILE_TINT: f32 = 0.18;
const FOLDER_ICON: f32 = 18.;
/// The last share of a folder's closing over which its closed tile fades in, and its tray out.
const TILE_FADE: f32 = 0.25;
/// How far above its place a line starts as it eases in.
const SETTLE: f32 = 6.;

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
    /// A folder's icon, or the default again with `None`.
    Icon(Vec<String>, Option<FolderIcon>),
    Color(Vec<String>, Option<FolderColor>),
}

/// A part of the sidebar that eases open and shut.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Fold {
    Folder(Vec<String>),
    /// The linked worktrees of the repository at this root.
    Worktrees(PathBuf),
}

/// An ease toward open or shut, from how open the part was drawn when it was toggled.
#[derive(Clone, Copy, Debug)]
pub(super) struct Tween {
    open: bool,
    from: f32,
    since: Instant,
}

impl Tween {
    /// The ease from `from` toward `open`; None when motion is reduced or nothing would move.
    fn start(reduced: bool, open: bool, from: f32) -> Option<Self> {
        let tween = Self {
            open,
            from,
            since: Instant::now(),
        };
        (!reduced && tween.from != tween.target()).then_some(tween)
    }

    fn target(&self) -> f32 {
        if self.open { 1. } else { 0. }
    }

    /// Covers only the distance left, so a reversal takes no longer than the way it came.
    fn duration(&self, base: Duration) -> Duration {
        base.mul_f32((self.target() - self.from).abs())
    }

    /// How open it is drawn at `now`, `shut` easing the way closed.
    fn at(&self, now: Instant, base: Duration, shut: impl Fn(f32) -> f32) -> f32 {
        let duration = self.duration(base).as_secs_f32();
        let elapsed = now.saturating_duration_since(self.since).as_secs_f32();
        let x = if duration > 0. {
            (elapsed / duration).min(1.)
        } else {
            1.
        };
        let eased = if self.open {
            motion::ease_standard()(x)
        } else {
            shut(x)
        };
        self.from + (self.target() - self.from) * eased
    }

    fn running(&self, now: Instant, base: Duration) -> bool {
        now.saturating_duration_since(self.since) < self.duration(base)
    }
}

pub(super) fn folder_color(t: &Theme, color: FolderColor) -> Hsla {
    let ansi = &t.terminal.ansi;
    match color {
        FolderColor::Red => ansi[1],
        FolderColor::Green => ansi[2],
        FolderColor::Yellow => ansi[3],
        FolderColor::Blue => ansi[4],
        FolderColor::Magenta => ansi[5],
        FolderColor::Cyan => ansi[6],
        FolderColor::Accent => t.color.accent,
        FolderColor::Muted => t.color.content_muted,
    }
}

fn folder_icon_path(icon: FolderIcon) -> &'static str {
    match icon {
        FolderIcon::Cup => "folders/cup.svg",
        FolderIcon::Briefcase => "folders/briefcase.svg",
        FolderIcon::House => "folders/house.svg",
        FolderIcon::Flask => "folders/flask.svg",
        FolderIcon::Rocket => "folders/rocket.svg",
        FolderIcon::Book => "folders/book.svg",
        FolderIcon::Star => "folders/star.svg",
        FolderIcon::Leaf => "folders/leaf.svg",
        FolderIcon::Gear => "folders/gear.svg",
        FolderIcon::Globe => "folders/globe.svg",
        FolderIcon::Heart => "folders/heart.svg",
        FolderIcon::Code => "folders/code.svg",
    }
}

pub(super) fn folder_icon(t: &Theme, icon: FolderIcon, color: Hsla, size: f32) -> gpui::Svg {
    svg()
        .path(folder_icon_path(icon))
        .size(t.ui(size))
        .flex_none()
        .text_color(color)
}

/// A folder's icon on a square tinted with its colour, as a closed folder shows in the rail.
pub(super) fn folder_tile(t: &Theme, icon: FolderIcon, color: Hsla, size: f32) -> gpui::Div {
    div()
        .size(t.ui(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(t.shape.radius_control)
        .border_1()
        .border_color(color.opacity(TRAY_EDGE))
        .bg(color.opacity(TILE_TINT))
        .child(folder_icon(t, icon, color, size * 0.6))
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

/// Rows for Change Icon: every icon, the current one marked, and the default again when a
/// choice was saved.
pub(super) fn icon_entries(
    workspace: &Workspace,
    folder: &[String],
) -> Vec<(String, Option<String>, FolderPick)> {
    let (current, _) = workspace.folder_look(folder);
    let mut out: Vec<_> = FolderIcon::ALL
        .into_iter()
        .map(|icon| {
            (
                icon.label().to_string(),
                (icon == current).then(|| "current".to_string()),
                FolderPick::Icon(folder.to_vec(), Some(icon)),
            )
        })
        .collect();
    if workspace
        .folder_style(folder)
        .is_some_and(|s| s.icon.is_some())
    {
        let (default, _) = default_look(folder.last().map_or("", String::as_str));
        out.push((
            "Use default".into(),
            Some(default.label().to_string()),
            FolderPick::Icon(folder.to_vec(), None),
        ));
    }
    out
}

/// Rows for Change Colour, as `icon_entries` lists icons.
pub(super) fn color_entries(
    workspace: &Workspace,
    folder: &[String],
) -> Vec<(String, Option<String>, FolderPick)> {
    let (_, current) = workspace.folder_look(folder);
    let mut out: Vec<_> = FolderColor::ALL
        .into_iter()
        .map(|color| {
            (
                color.label().to_string(),
                (color == current).then(|| "current".to_string()),
                FolderPick::Color(folder.to_vec(), Some(color)),
            )
        })
        .collect();
    if workspace
        .folder_style(folder)
        .is_some_and(|s| s.color.is_some())
    {
        let (_, default) = default_look(folder.last().map_or("", String::as_str));
        out.push((
            "Use default".into(),
            Some(default.label().to_string()),
            FolderPick::Color(folder.to_vec(), None),
        ));
    }
    out
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

/// The row and line span of each open folder, from its heading to the line before whatever
/// follows it, so the unopened worktrees under its last project count.
fn open_folder_spans(
    rows: &[SidebarRow],
    lines: &[Option<usize>],
    total: usize,
) -> Vec<(usize, Range<usize>)> {
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
            let after = lines[end..].iter().find_map(|l| *l).unwrap_or(total);
            Some((r, first..after))
        })
        .collect()
}

/// How much block spacer each of `total` lines gets above it, given where each open folder's
/// block ends and how open it is drawn; blocks ending together share one spacer.
fn block_breaks(ends: &[(usize, f32)], total: usize) -> Vec<f32> {
    let mut spaced = vec![0f32; total];
    for &(end, weight) in ends {
        if let Some(space) = spaced.get_mut(end) {
            *space = space.max(weight);
        }
    }
    spaced
}

/// A sidebar line's row height and the gap below it, in the rail or the wide layout.
fn line_size(wide: bool) -> (f32, f32) {
    if wide {
        (ROW, ROW_GAP)
    } else {
        (RAIL_ITEM, RAIL_GAP)
    }
}

/// Where each line's top is drawn, `shown` being how much of each line is drawn and `spaced` how
/// much block spacer sits above it; `line_box` and the spacers lay the column out by the same sums.
fn line_tops(shown: &[f32], spaced: &[f32], pitch: f32, extra: f32) -> Vec<f32> {
    let mut y = 0.;
    shown
        .iter()
        .zip(spaced)
        .map(|(shown, space)| {
            y += space * extra;
            let top = y;
            y += shown * pitch;
            top
        })
        .collect()
}

/// Where the active bar sits beside `line`: a line easing in or out carries it from the heading
/// above, so the two move as one.
fn bar_top_of(shown: &[f32], tops: &[f32], line: usize) -> f32 {
    if shown[line] >= 1. {
        return tops[line];
    }
    let anchor = (0..line).rev().find(|&l| shown[l] >= 1.).unwrap_or(line);
    tops[anchor] + (tops[line] - tops[anchor]) * shown[line]
}

/// How open the folder at `path` is drawn: its fold's progress, else whether it is collapsed.
fn folder_open(folds: &[(Fold, f32)], collapsed: &[Vec<String>], path: &[String]) -> f32 {
    folds
        .iter()
        .find_map(|(fold, open)| matches!(fold, Fold::Folder(f) if f == path).then_some(*open))
        .unwrap_or(if collapsed.iter().any(|c| c == path) {
            0.
        } else {
            1.
        })
}

/// How much of a line inside `folder` is drawn: the openness of every folder around it.
fn inside(folds: &[(Fold, f32)], collapsed: &[Vec<String>], folder: &[String]) -> f32 {
    (1..=folder.len())
        .map(|n| folder_open(folds, collapsed, &folder[..n]))
        .product()
}

/// How far a folder drawn `open` shows its closed tile, which fades in only at the end of a close.
fn tile_shown(open: f32) -> f32 {
    (1. - open / TILE_FADE).clamp(0., 1.)
}

/// A line's box, `shown` of a pitch tall with the gap inside it, so the column can only stack lines
/// where `line_tops` puts them. Part-drawn, its row is clipped and faded, settling into place.
fn line_box(t: &Theme, item: AnyElement, shown: f32, wide: bool) -> AnyElement {
    let (row_h, gap) = line_size(wide);
    let line = div()
        .flex_none()
        .when(!wide, |el| el.w(t.ui(RAIL_ITEM)))
        .h(t.ui(shown * (row_h + gap)));
    if shown >= 1. {
        return line.child(item).into_any_element();
    }
    line.child(
        div()
            .h(t.ui(shown * row_h))
            .overflow_hidden()
            .opacity(shown)
            .child(
                div()
                    .relative()
                    .top(-t.ui(SETTLE * (1. - shown)))
                    .child(item),
            ),
    )
    .into_any_element()
}

/// The top-level blocks starting at `starts` that get a divider above them, and how strongly:
/// those beside a block of more than one line, so lone projects stay undivided, fading as a block
/// eases down to one. `shown` is how much of each line is drawn.
fn divider_lines(starts: &[usize], shown: &[f32]) -> Vec<(usize, f32)> {
    let size = |k: usize| -> f32 {
        let end = starts.get(k + 1).copied().unwrap_or(shown.len());
        shown[starts[k]..end].iter().sum()
    };
    (1..starts.len())
        .filter_map(|k| {
            let strength = (size(k - 1).max(size(k)) - 1.).clamp(0., 1.);
            (strength > 0.).then_some((starts[k], strength))
        })
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
    // Layout clamps to the content as it is now; folders a switch opened may have grown it.
    scroll.set_offset(gpui::point(px(0.), px(-y.max(0.))));
    true
}

fn fold_tip(folded: bool, worktrees: usize) -> String {
    match (folded, worktrees) {
        (true, 1) => "Show 1 worktree".into(),
        (true, n) => format!("Show {n} worktrees"),
        (false, _) => "Hide worktrees".into(),
    }
}

impl Shell {
    pub(super) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        let motion = &cx.theme().motion;
        let (reduced, base) = (motion.reduced, motion.base);
        let from = self.sidebar_spread(Instant::now(), base);
        let ui = &mut self.workspace.ui;
        ui.sidebar_expanded = !ui.sidebar_expanded;
        self.sidebar_width = Tween::start(reduced, ui.sidebar_expanded, from);
        self.schedule_save(cx);
        cx.notify();
    }

    pub(super) fn toggle_folder(&mut self, path: &[String], cx: &mut Context<Self>) {
        let fold = Fold::Folder(path.to_vec());
        let from = self.fold_drawn(&fold, cx.theme().motion.base);
        self.workspace.toggle_folder(path);
        self.start_fold(fold, from, cx);
        self.schedule_save(cx);
        cx.notify();
    }

    pub(super) fn toggle_worktrees(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(root) = self.workspace.projects.get(index).map(|p| p.root.clone()) else {
            return;
        };
        let fold = Fold::Worktrees(root);
        let from = self.fold_drawn(&fold, cx.theme().motion.base);
        let project = &mut self.workspace.projects[index];
        project.worktrees_collapsed = !project.worktrees_collapsed;
        self.start_fold(fold, from, cx);
        self.schedule_save(cx);
        cx.notify();
    }

    /// Opens the folders around the project at `index`, easing each one it opens from where it
    /// is drawn.
    pub(super) fn reveal_project(&mut self, index: usize, cx: &Context<Self>) {
        let base = cx.theme().motion.base;
        let Some(folder) = self.workspace.projects.get(index).map(|p| p.folder.clone()) else {
            return;
        };
        let opening: Vec<(Fold, f32)> = self
            .workspace
            .collapsed
            .iter()
            .filter(|path| folder.starts_with(path))
            .map(|path| {
                let fold = Fold::Folder(path.clone());
                let from = self.fold_drawn(&fold, base);
                (fold, from)
            })
            .collect();
        self.workspace.reveal(index);
        for (fold, from) in opening {
            self.start_fold(fold, from, cx);
        }
    }

    /// How far the sidebar is drawn toward its wide layout at `now`, 1 being wide.
    fn sidebar_spread(&self, now: Instant, base: Duration) -> f32 {
        let wide = self.workspace.ui.sidebar_expanded;
        self.sidebar_width
            .filter(|tween| tween.open == wide)
            .map_or(if wide { 1. } else { 0. }, |tween| {
                tween.at(now, base, motion::ease_exit())
            })
    }

    /// Whether `fold` is open in the workspace; None once it is gone.
    fn fold_open(&self, fold: &Fold) -> Option<bool> {
        match fold {
            Fold::Folder(path) => Some(!self.workspace.collapsed.contains(path)),
            Fold::Worktrees(root) => self
                .workspace
                .projects
                .iter()
                .find(|p| p.root == *root)
                .map(|p| !p.worktrees_collapsed),
        }
    }

    /// How open each fold still easing toward the workspace's state is drawn at `now`; one the
    /// workspace has moved past, as when a switch opens a folder that was closing, drops out.
    fn fold_progress(&self, now: Instant, base: Duration) -> Vec<(Fold, f32)> {
        self.sidebar_folds
            .iter()
            .filter(|(fold, tween)| self.fold_open(fold) == Some(tween.open))
            .map(|(fold, tween)| (fold.clone(), tween.at(now, base, motion::ease_exit())))
            .collect()
    }

    /// How open `fold` is drawn now.
    fn fold_drawn(&self, fold: &Fold, base: Duration) -> f32 {
        self.fold_progress(Instant::now(), base)
            .into_iter()
            .find(|(f, _)| f == fold)
            .map_or(
                if self.fold_open(fold) == Some(true) {
                    1.
                } else {
                    0.
                },
                |(_, open)| open,
            )
    }

    /// Eases `fold` from `from` toward the state the workspace now gives it.
    fn start_fold(&mut self, fold: Fold, from: f32, cx: &Context<Self>) {
        let reduced = cx.theme().motion.reduced;
        self.sidebar_folds.retain(|(f, _)| *f != fold);
        if let Some(open) = self.fold_open(&fold)
            && let Some(tween) = Tween::start(reduced, open, from)
        {
            self.sidebar_folds.push((fold, tween));
        }
    }

    /// Drops finished sidebar eases and keeps frames coming while any still run.
    pub(super) fn sync_sidebar_motion(&mut self, window: &mut Window, cx: &Context<Self>) {
        let base = cx.theme().motion.base;
        let now = Instant::now();
        self.sidebar_folds
            .retain(|(_, tween)| tween.running(now, base));
        self.sidebar_width = self.sidebar_width.filter(|tween| tween.running(now, base));
        if !self.sidebar_folds.is_empty() || self.sidebar_width.is_some() {
            window.request_animation_frame();
        }
    }

    /// The room the sidebar takes from the content: its final width from the toggle on, so the
    /// panes resize once rather than every frame.
    pub(super) fn render_sidebar_slot(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let wide = self.workspace.ui.sidebar_expanded;
        // Widening, the sidebar slides into this room over a band that matches the panes.
        div()
            .flex_none()
            .w(t.ui(if wide { SIDEBAR_WIDTH } else { RAIL_WIDTH }))
            .bg(t.color.surface_sunken)
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
        let base = cx.theme().motion.base;
        let folders = self.workspace.folders();
        let closing: Vec<(Fold, f32)> = folders
            .iter()
            .filter(|path| !self.workspace.collapsed.contains(path))
            .map(|path| {
                let fold = Fold::Folder(path.clone());
                let from = self.fold_drawn(&fold, base);
                (fold, from)
            })
            .collect();
        self.workspace.collapsed = folders;
        for (fold, from) in closing {
            self.start_fold(fold, from, cx);
        }
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

    fn pick_folder_style(
        &mut self,
        path: Vec<String>,
        mode: Mode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_subject = Some(Subject::Folder(path));
        self.open_palette(mode, window, cx);
    }

    /// The picture a Change Icon or Change Colour row shows beside its label: the icon in the
    /// folder's colour, or the folder's icon on a tile in the colour offered.
    pub(super) fn folder_pick_preview(&self, pick: &FolderPick, t: &Theme) -> Option<AnyElement> {
        match pick {
            FolderPick::Icon(path, icon) => {
                let (_, color) = self.workspace.folder_look(path);
                let default = default_look(path.last().map_or("", String::as_str)).0;
                let color = folder_color(t, color);
                Some(folder_icon(t, icon.unwrap_or(default), color, 16.).into_any_element())
            }
            FolderPick::Color(path, color) => {
                let (icon, _) = self.workspace.folder_look(path);
                let default = default_look(path.last().map_or("", String::as_str)).1;
                let color = folder_color(t, color.unwrap_or(default));
                Some(folder_tile(t, icon, color, 20.).into_any_element())
            }
            _ => None,
        }
    }

    pub(super) fn run_folder_pick(&mut self, pick: FolderPick, cx: &mut Context<Self>) {
        match pick {
            FolderPick::Icon(path, icon) => {
                self.workspace.set_folder_icon(&path, icon);
                self.schedule_save(cx);
                return cx.notify();
            }
            FolderPick::Color(path, color) => {
                self.workspace.set_folder_color(&path, color);
                self.schedule_save(cx);
                return cx.notify();
            }
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
            super::menus::shell_item("Change Icon…", cx, {
                let path = path.clone();
                move |this, w, cx| this.pick_folder_style(path.clone(), Mode::FolderIcon, w, cx)
            }),
            super::menus::shell_item("Change Colour…", cx, {
                let path = path.clone();
                move |this, w, cx| this.pick_folder_style(path.clone(), Mode::FolderColor, w, cx)
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

    pub(super) fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = cx.theme().clone();
        let now = Instant::now();
        let spread = self.sidebar_spread(now, t.motion.base);
        // The layouts swap halfway, each fading out before the other fades in.
        let wide = spread > 0.5;
        let fade = ((spread - 0.5).abs() * 2.).min(1.);
        let (row_h, gap) = line_size(wide);
        let folds = self.fold_progress(now, t.motion.base);
        let collapsed = &self.workspace.collapsed;
        let closing: Vec<Vec<String>> = folds
            .iter()
            .filter_map(|(fold, _)| match fold {
                Fold::Folder(path) if collapsed.contains(path) => Some(path.clone()),
                _ => None,
            })
            .collect();
        let rows = self.workspace.sidebar_rows_opening(&closing);
        let monograms = self.workspace.monograms();
        let groups = Rc::new(self.rail_groups());
        let mut items: Vec<AnyElement> = Vec::with_capacity(rows.len());
        // Each row's line, None when hidden; unopened worktrees add lines of their own when wide.
        let mut line_of_row = Vec::with_capacity(rows.len());
        // How much of each line is drawn, below 1 while a fold eases it in or out.
        let mut shown: Vec<f32> = Vec::with_capacity(rows.len());
        // How much of each line is drawn once every fold is done.
        let mut settled: Vec<f32> = Vec::with_capacity(rows.len());
        // Lines where a top-level folder, repository with its worktrees, or lone project begins.
        let mut block_starts = Vec::new();
        for row in &rows {
            match row {
                SidebarRow::Folder {
                    path,
                    depth,
                    projects,
                    ..
                } => {
                    line_of_row.push(Some(items.len()));
                    if *depth == 0 {
                        block_starts.push(items.len());
                    }
                    shown.push(inside(&folds, collapsed, &path[..*depth]));
                    settled.push(inside(&[], collapsed, &path[..*depth]));
                    let closed = tile_shown(folder_open(&folds, collapsed, path));
                    items.push(self.render_folder_row(
                        path,
                        *depth,
                        collapsed.contains(path),
                        closed,
                        *projects,
                        wide,
                        &groups,
                        cx,
                    ))
                }
                SidebarRow::Project { index, depth } => {
                    let project = &self.workspace.projects[*index];
                    let group = groups.group_of(&project.root);
                    let linked = group
                        .as_ref()
                        .is_some_and(|(from, group)| *from != group.start);
                    let folding = group.as_ref().and_then(|(_, group)| {
                        let main = &self.workspace.projects[group.start].root;
                        folds.iter().find_map(|(fold, open)| {
                            matches!(fold, Fold::Worktrees(r) if r == main).then_some(*open)
                        })
                    });
                    if self.worktree_hidden(*index, &groups) && folding.is_none() {
                        line_of_row.push(None);
                        continue;
                    }
                    line_of_row.push(Some(items.len()));
                    if *depth == 0 && !linked {
                        block_starts.push(items.len());
                    }
                    let within = inside(&folds, collapsed, &project.folder);
                    let worktrees = within * folding.unwrap_or(1.);
                    let settled_within = inside(&[], collapsed, &project.folder);
                    let folded = group.as_ref().is_some_and(|(_, group)| {
                        self.workspace.projects[group.start].worktrees_collapsed
                    });
                    let settled_worktrees = if folded { 0. } else { settled_within };
                    // The active worktree stays shown whatever its repository's fold does.
                    if linked && Some(*index) != self.workspace.active {
                        shown.push(worktrees);
                        settled.push(settled_worktrees);
                    } else {
                        shown.push(within);
                        settled.push(settled_within);
                    }
                    items.push(self.render_project_row(
                        *index,
                        *depth,
                        monograms[*index].clone(),
                        wide,
                        &groups,
                        cx,
                    ));
                    if wide {
                        let unopened =
                            self.render_unopened(*index, *depth, folding.is_some(), &groups, cx);
                        shown.extend(std::iter::repeat_n(worktrees, unopened.len()));
                        settled.extend(std::iter::repeat_n(settled_worktrees, unopened.len()));
                        items.extend(unopened);
                    }
                }
            }
        }

        let pitch = row_h + gap;
        let extra = BLOCK_SPACER + gap;
        let spans = open_folder_spans(&rows, &line_of_row, items.len());
        let open_at = |folds: &[(Fold, f32)], r: usize| match &rows[r] {
            SidebarRow::Folder { path, .. } => folder_open(folds, collapsed, path),
            SidebarRow::Project { .. } => 1.,
        };
        let tops_for = |folds: &[(Fold, f32)], shown: &[f32]| {
            let ends: Vec<(usize, f32)> = spans
                .iter()
                .map(|(r, span)| (span.end, shown[span.start] * open_at(folds, *r)))
                .collect();
            let spaced = block_breaks(&ends, shown.len());
            let tops = line_tops(shown, &spaced, pitch, extra);
            (spaced, tops)
        };
        let (spaced, tops) = tops_for(&folds, &shown);
        let bar_top = |line: usize| bar_top_of(&shown, &tops, line);
        let line =
            |index: usize| row_of(&rows, &self.workspace, index).and_then(|r| line_of_row[r]);
        let active_row = self.workspace.active.and_then(line);
        let from_row = line(self.rail_from).or(active_row);
        if self.rail_revealed != self.switch_count
            && let Some(row) = active_row
        {
            let s = t.ui_scale;
            // Where the row will rest, so a fold still easing cannot leave it scrolled part-way.
            let (_, settled_tops) = tops_for(&[], &settled);
            let top = s * bar_top_of(&settled, &settled_tops, row);
            if reveal_line(&self.rail_scroll, top, s * row_h) {
                self.rail_revealed = self.switch_count;
            } else {
                let shell = cx.entity_id();
                cx.defer(move |cx| cx.notify(shell));
            }
        }
        let indicator = active_row.map(|to| {
            let s = t.ui_scale;
            let y = |r: usize| s * (bar_top(r) + (row_h - 20.) / 2.);
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
        let s = t.ui_scale;
        let mid = RAIL_WIDTH / 2.;
        let trays: Vec<AnyElement> = spans
            .iter()
            .filter_map(|(r, span)| {
                let SidebarRow::Folder { path, depth, .. } = &rows[*r] else {
                    return None;
                };
                let color = folder_color(&t, self.workspace.folder_look(path).1);
                let pad = match (wide, depth) {
                    (false, 0) => 4.,
                    (false, 1) => 2.,
                    (false, _) => 1.,
                    (true, 0) => 2.,
                    (true, _) => 1.,
                };
                let (left, width) = if wide {
                    let inset = INDENT * *depth as f32;
                    (6. + inset, SIDEBAR_WIDTH - 12. - inset)
                } else {
                    (mid - RAIL_ITEM / 2. - pad, RAIL_ITEM + 2. * pad)
                };
                let last = span.end - 1;
                let top = tops[span.start] - pad;
                let bottom = tops[last] + shown[last] * row_h + pad;
                // Gives way to the closed tile, and goes with the lines of a folder around it.
                let strength = shown[span.start] * (1. - tile_shown(open_at(&folds, *r)));
                Some(
                    div()
                        .absolute()
                        .top(px(s * top))
                        .h(px(s * (bottom - top)))
                        .left(px(s * left))
                        .w(px(s * width))
                        .rounded(t.shape.radius_control)
                        .border_1()
                        .border_color(color.opacity(TRAY_EDGE * strength))
                        .bg(color.opacity(TRAY_TINT * strength))
                        .into_any_element(),
                )
            })
            .collect();
        let dividers: Vec<AnyElement> = if wide {
            Vec::new()
        } else {
            divider_lines(&block_starts, &shown)
                .into_iter()
                .map(|(line, strength)| {
                    let above = spaced[line] * extra;
                    div()
                        .absolute()
                        .top(px(s * (tops[line] - (gap + above) / 2.)))
                        .left(px(s * (mid - 10.)))
                        .w(px(s * 20.))
                        .h(px(1.))
                        .bg(t.color.border.opacity(strength))
                        .into_any_element()
                })
                .collect()
        };
        let rows_and_spacers: Vec<AnyElement> = items
            .into_iter()
            .enumerate()
            .flat_map(|(line, item)| {
                let space = spaced[line];
                let spacer = (space > 0.)
                    .then(|| div().h(t.ui(extra * space)).flex_none().into_any_element());
                spacer
                    .into_iter()
                    .chain([line_box(&t, item, shown[line], wide)])
            })
            .collect();

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

        // The layout keeps its own width inside a clip that eases, so nothing wraps on the way.
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .w(t.ui(RAIL_WIDTH + (SIDEBAR_WIDTH - RAIL_WIDTH) * spread))
            // Only while it narrows over the panes: occluding hides it from the shell's drop and mouse-up.
            .when(self.sidebar_width.is_some_and(|tween| !tween.open), |el| {
                el.occlude()
            })
            .overflow_hidden()
            .drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(drop_tint))
            .bg(t.color.surface)
            .border_r_1()
            .border_color(t.color.border)
            .child(
                div()
                    .w(t.ui(if wide { SIDEBAR_WIDTH } else { RAIL_WIDTH }) - px(1.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .justify_between()
                    .py(t.ui(TOP))
                    .opacity(fade)
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
                                    .children(trays)
                                    .children(dividers)
                                    .children(rows_and_spacers)
                                    .children(indicator),
                            ),
                    )
                    .child(footer),
            )
    }

    /// After the last project of a repository's group, its worktrees that are not open, dimmed;
    /// `folding` keeps them while the group eases shut.
    fn render_unopened(
        &self,
        index: usize,
        depth: usize,
        folding: bool,
        groups: &Rc<RailGroups>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let root = &self.workspace.projects[index].root;
        let Some((_, group)) = groups.group_of(root) else {
            return Vec::new();
        };
        let main = &self.workspace.projects[group.start];
        if group.end != index + 1 || (main.worktrees_collapsed && !folding) {
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
                    .ml(t.ui(INDENT * depth.saturating_sub(1) as f32))
                    .flex()
                    .items_center()
                    .gap(t.ui(6.))
                    .pl(t.ui(6. + INDENT * (depth.min(1) + 1) as f32))
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

    /// A folder heading, its closed tile and marks drawn `closed` of the way in.
    #[allow(clippy::too_many_arguments)]
    fn render_folder_row(
        &self,
        path: &[String],
        depth: usize,
        collapsed: bool,
        closed: f32,
        projects: usize,
        wide: bool,
        groups: &Rc<RailGroups>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let name = path.last().cloned().unwrap_or_default();
        let (icon, color) = self.workspace.folder_look(path);
        let color = folder_color(&t, color);
        let inside = self.workspace.folder_range(path);
        let mark = if closed > 0. {
            let roots: Vec<&Path> = self.workspace.projects[inside.clone()]
                .iter()
                .map(|p| p.root.as_path())
                .collect();
            self.rail_claude(&roots)
        } else {
            ClaudeMark::default()
        };
        let claude = claude_dot(&t, mark).map(|dot| dot.opacity(closed));
        let awake = awake_mark(&t, mark).map(|cup| cup.opacity(closed));
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
                .ml(t.ui(INDENT * depth as f32))
                .pl(t.ui(6.))
                .pr(t.ui(6.))
                .gap(t.ui(6.))
                .text_size(t.typography.caption)
                .font_weight(FontWeight::SEMIBOLD)
                .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                .child(folder_icon(&t, icon, color, 14.))
                .child(div().flex_1().min_w_0().truncate().child(name))
                .children(awake)
                .children(claude)
                .child(if collapsed { "▸" } else { "▾" })
                .when(collapsed, |el| el.child(projects.to_string()))
                .into_any_element()
        } else {
            el.size(t.ui(RAIL_ITEM))
                .justify_center()
                .when(closed > 0., |el| {
                    el.border_1()
                        .border_color(color.opacity(TRAY_EDGE * closed))
                        .bg(color.opacity(TILE_TINT * closed))
                })
                .hover(|s| {
                    s.bg(if collapsed {
                        color.opacity(TILE_TINT * 1.6)
                    } else {
                        t.color.surface_hover
                    })
                })
                .child(folder_icon(&t, icon, color, FOLDER_ICON))
                .children(claude.map(|dot| dot.absolute().right(px(2.)).bottom(px(2.))))
                .children(awake.map(|cup| cup.absolute().left(px(2.)).top(px(2.))))
                .into_any_element()
        }
    }

    fn render_project_row(
        &self,
        i: usize,
        depth: usize,
        monogram: String,
        wide: bool,
        groups: &Rc<RailGroups>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
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
            // The row starts at its folder's backing, then indents inside it.
            let indent = depth.min(1) + usize::from(main.is_some());
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
                .ml(t.ui(INDENT * depth.saturating_sub(1) as f32))
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
        assert_eq!(open_folder_spans(&rows, &lines, 7), [(0, 0..5), (2, 2..4)]);
    }

    #[test]
    fn a_span_takes_in_the_lines_unopened_worktrees_add_after_its_last_project() {
        let rows = [folder("work", false), project(0, 1), project(1, 0)];
        // Two unopened worktrees sit on lines 2 and 3, so the next project is on line 4.
        let lines = [Some(0), Some(1), Some(4)];
        assert_eq!(open_folder_spans(&rows, &lines, 5), [(0, 0..4)]);
        let last = [folder("work", false), project(0, 1)];
        assert_eq!(
            open_folder_spans(&last, &[Some(0), Some(1)], 4),
            [(0, 0..4)]
        );
    }

    #[test]
    fn blocks_are_set_apart_from_what_follows_them_and_lines_shift_to_match() {
        // work holds a nested folder that ends with it, then a project follows both.
        let spaced = block_breaks(&[(5, 1.), (5, 1.), (7, 1.)], 9);
        assert_eq!(spaced, [0., 0., 0., 0., 0., 1., 0., 1., 0.]);
        assert_eq!(
            block_breaks(&[(3, 1.)], 3),
            [0.; 3],
            "nothing follows the last block"
        );
        let tops = line_tops(&[1.; 9], &spaced, 40., 10.);
        assert_eq!(tops[4], 160.);
        assert_eq!(tops[5], 210.);
        assert_eq!(tops[7], 300.);
    }

    #[test]
    fn lines_keep_one_pitch_in_each_layout_while_others_ease() {
        // A line's box is its row and the gap below it, so the rail steps 40 and the wide list 30.
        let (row, gap) = line_size(false);
        assert_eq!(row + gap, RAIL_ITEM + RAIL_GAP);
        let rail = line_tops(&[1., 1., 0.5, 1., 1.], &[0.; 5], row + gap, 0.);
        assert_eq!(rail, [0., 40., 80., 100., 140.]);
        let (row, gap) = line_size(true);
        assert_eq!(row + gap, ROW + ROW_GAP);
        let wide = line_tops(&[1., 1., 0.5, 1., 1.], &[0.; 5], row + gap, 0.);
        assert_eq!(wide, [0., 30., 60., 75., 105.]);
    }

    #[test]
    fn lines_easing_in_or_out_take_their_share_of_the_space_and_overlays_follow() {
        // A folder at line 0 half shut: its two lines are half drawn, as is the spacer after it.
        let spaced = block_breaks(&[(3, 0.5)], 4);
        let tops = line_tops(&[1., 0.5, 0.5, 1.], &spaced, 40., 10.);
        assert_eq!(tops, [0., 40., 60., 85.]);
        let shut = line_tops(&[1., 0., 0., 1.], &block_breaks(&[(3, 0.)], 4), 40., 10.);
        assert_eq!(
            shut[3], 40.,
            "the next line sits where it will once the fold is done"
        );
        assert_eq!(tile_shown(1.), 0.);
        assert_eq!(tile_shown(0.5), 0.);
        assert_eq!(tile_shown(0.), 1.);
    }

    #[test]
    fn the_bar_rides_a_line_easing_out_and_rests_on_its_heading() {
        let tops = line_tops(&[1., 0.5, 0.5, 1.], &[0.; 4], 40., 10.);
        assert_eq!(bar_top_of(&[1., 0.5, 0.5, 1.], &tops, 2), 30.);
        assert_eq!(bar_top_of(&[1., 0.5, 0.5, 1.], &tops, 3), tops[3]);
        // Settled, the folder is shut: its last line rests on the heading, the next line below it.
        let settled = [1., 0., 0., 1.];
        let tops = line_tops(&settled, &[0.; 4], 40., 10.);
        assert_eq!(bar_top_of(&settled, &tops, 2), 0.);
        assert_eq!(bar_top_of(&settled, &tops, 3), 40.);
    }

    #[test]
    fn a_reversal_eases_back_from_where_the_fold_is_drawn() {
        let base = Duration::from_millis(200);
        let opening = Tween::start(false, true, 0.).unwrap();
        let halfway = opening.since + base / 2;
        let drawn = opening.at(halfway, base, motion::ease_exit());
        assert!(drawn > 0. && drawn < 1.);
        let closing = Tween {
            since: halfway,
            ..Tween::start(false, false, drawn).unwrap()
        };
        assert!((closing.at(halfway, base, motion::ease_exit()) - drawn).abs() < 1e-4);
        assert!(closing.running(halfway, base));
        let done = halfway + closing.duration(base);
        assert!(closing.duration(base) < base);
        assert!(closing.at(done, base, motion::ease_exit()).abs() < 1e-4);
        assert!(!closing.running(done, base));
    }

    #[test]
    fn reduced_motion_or_nothing_to_move_lands_at_once() {
        assert!(Tween::start(true, true, 0.).is_none());
        assert!(Tween::start(false, false, 0.).is_none());
        assert!(Tween::start(false, false, 1.).is_some());
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
    fn every_folder_icon_is_bundled() {
        use gpui::AssetSource;
        for icon in FolderIcon::ALL {
            let bytes = athena_ui::Assets.load(folder_icon_path(icon)).unwrap();
            assert!(bytes.is_some(), "{icon:?}");
        }
    }

    #[test]
    fn the_style_pickers_mark_the_current_choice_and_offer_the_default_only_once_saved() {
        let work = vec!["work".to_string()];
        let mut w = Workspace::default();
        let (icon, color) = default_look("work");
        let rows = icon_entries(&w, &work);
        assert_eq!(rows.len(), FolderIcon::ALL.len());
        let marked: Vec<_> = rows.iter().filter(|r| r.1.is_some()).collect();
        assert_eq!(marked.len(), 1);
        assert_eq!(marked[0].2, FolderPick::Icon(work.clone(), Some(icon)));

        w.set_folder_color(&work, Some(FolderColor::Cyan));
        assert_eq!(icon_entries(&w, &work).len(), FolderIcon::ALL.len());
        let rows = color_entries(&w, &work);
        assert_eq!(rows.len(), FolderColor::ALL.len() + 1);
        let last = rows.last().unwrap();
        assert_eq!(last.0, "Use default");
        assert_eq!(last.1.as_deref(), Some(color.label()));
        assert_eq!(last.2, FolderPick::Color(work.clone(), None));
        let cyan = rows.iter().find(|r| r.0 == "Cyan").unwrap();
        assert_eq!(cyan.1.as_deref(), Some("current"));
    }

    #[test]
    fn dividers_only_border_blocks_of_more_than_one_line() {
        assert_eq!(
            divider_lines(&[0, 1, 2, 5, 6], &[1.; 7]),
            [(2, 1.), (5, 1.)]
        );
        assert!(divider_lines(&[0, 1, 2], &[1.; 3]).is_empty());
        assert!(divider_lines(&[], &[]).is_empty());
        // The block at line 2 easing down to its heading fades the dividers beside it.
        let closing = [1., 1., 1., 0.25, 0.25, 1., 1.];
        assert_eq!(
            divider_lines(&[0, 1, 2, 5, 6], &closing),
            [(2, 0.5), (5, 0.5)]
        );
    }
}
