mod appearance;
mod branches;
mod breadcrumbs;
mod bridge;
mod calls;
mod claude;
mod claude_ide;
mod claude_sessions;
mod code_actions;
mod codelens;
mod conflicts;
mod containers_view;
mod debug;
mod debug_view;
mod dnd;
mod drawer;
mod edits;
mod fileops;
mod fuzzy;
mod git_gutter;
mod git_view;
mod github;
mod history;
mod item;
mod lsp;
mod mcp_tools;
mod menus;
mod notices;
mod outline;
mod palette;
mod panes;
mod playwright_view;
mod problems;
mod quit;
mod rename;
mod review;
mod review_comments;
mod search;
mod settings;
mod settings_ui;
mod shortcuts;
mod shortcuts_ui;
mod side_panel;
mod sidebar;
mod snippets;
mod status_bar;
mod tasks;
mod terminal_panel;
mod tests_view;
mod timeline;
mod tree;
mod usage_view;
mod watch;
mod windows;
mod worktrees;
mod zoom;

pub use windows::start;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use athena_ui::{ActiveTheme, Button, ButtonKind, Lockup, empty_state, motion};
use athena_workspace::{
    Axis, Direction, ItemId, PaneId, SideMode, WindowMode, WindowState, Workspace,
};
use gpui::{
    Animation, AnyElement, Bounds, Context, ExternalPaths, FocusHandle, FontWeight, IntoElement,
    MouseButton, NavigationDirection, PathPromptOptions, Pixels, Render, Subscription, Task,
    Window, WindowBounds, div, prelude::*, px,
};

use crate::actions::{
    AddProject, ChangeClaudeCommand, CloseProject, CloseTab, CommandPalette, DisableClaudeHooks,
    DisablePlaywrightMcp, EnablePlaywrightMcp, FocusPaneDown, FocusPaneLeft, FocusPaneRight,
    FocusPaneUp, Minimize, MoveProjectToFolder, NewClaudeSession, NewPreview, NewTerminal,
    NextProject, NextTab, PrevProject, PrevTab, QuickOpen, QuickOpenBeside, Quit, RunPlaywright,
    SaveAs, SelectProject, SelectTab, ShowContainers, ShowNotifications, ShowPlaywright, SplitDown,
    SplitRight, ToggleAutoSave, ToggleDrawer, ToggleFileTree, ToggleFormatOnSave, ToggleFullScreen,
    TogglePaneZoom, TogglePreview, ToggleSidebar, ToggleWordWrapDefault, Zoom,
};
use crate::actions::{
    CollapseAllFolders, NextClaudeAttention, SwitchProject, ToggleProjectFolder, ToggleWorktrees,
};
use crate::actions::{
    FindInProject, FontZoomIn, FontZoomOut, FontZoomReset, GoToSymbol, GoToWorkspaceSymbol,
    NavigateBack, NavigateForward, NextProblem, PrevProblem, RevealInTree, SendToClaude,
    ShowChanges, ShowProblems, SwitchBranch, ToggleBlame, ToggleIdeIntegration,
};

const TITLE_BAR_HEIGHT: f32 = 36.;
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

pub struct Shell {
    workspace: Workspace,
    /// This shell's own window, which other windows' requests are told apart from.
    window_handle: gpui::AnyWindowHandle,
    focus: FocusHandle,
    drawer_focus: FocusHandle,
    items: HashMap<(PathBuf, ItemId), item::ItemView>,
    pane_area: Rc<RefCell<Bounds<Pixels>>>,
    drag: Option<panes::Drag>,
    /// The pane and zone a dragged tab would land in.
    drop_hint: Option<(PaneId, dnd::DropZone)>,
    palette: Option<palette::Palette>,
    palette_closing: Option<(palette::Palette, motion::Closing)>,
    /// Each project's files as last walked, listed while Go to File walks them again.
    palette_files: HashMap<PathBuf, Rc<Vec<String>>>,
    context_menu: Option<(gpui::Entity<athena_ui::ContextMenu>, Subscription)>,
    /// Tabs whose editor or terminal has its own right-click menu open.
    item_menus: HashSet<(PathBuf, ItemId)>,
    tree: tree::FileTree,
    tree_opening: Option<motion::Opening>,
    tree_closing: Option<motion::Closing>,
    /// The Notifications list, shared with the other windows.
    notices: Rc<RefCell<notices::Log>>,
    toasts: Vec<notices::Toast>,
    /// Where the toasts were last drawn, so web previews under them can step aside.
    toast_area: Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
    drawer: Option<drawer::DrawerTab>,
    /// The drawer state `drawer_changed` last saw, to tell opening from closing.
    drawer_shown: Option<drawer::DrawerTab>,
    drawer_opening: Option<motion::Opening>,
    drawer_closing: Option<(drawer::DrawerTab, motion::Closing)>,
    last_drawer_tab: drawer::DrawerTab,
    containers: containers_view::ContainersState,
    playwright: playwright_view::PlaywrightState,
    tests: tests_view::TestsState,
    debug: debug::DebugState,
    lsp: lsp::LspState,
    settings: settings::SettingsState,
    breadcrumbs: breadcrumbs::BreadcrumbState,
    outline: outline::OutlineState,
    side_panel: side_panel::SidePanelState,
    problems: problems::ProblemsState,
    code_actions: code_actions::CodeActionState,
    git: git_view::GitState,
    review: review::ReviewState,
    search: search::SearchState,
    history: history::History,
    watch: watch::WatchState,
    /// Per project, the pane a file opened from a terminal goes to.
    last_editor_pane: HashMap<PathBuf, PaneId>,
    window_title: String,
    /// A file a rendered document's link asked for, opened at the next frame.
    pending_open: Option<PathBuf>,
    ide: claude_ide::IdeState,
    usage: usage_view::UsageState,
    bell: notices::BellState,
    _usage: Option<Task<()>>,
    zoomed: Option<PaneId>,
    entering: Option<PaneId>,
    pane_opening: Option<(PaneId, motion::Opening)>,
    /// Panes fading out before they close, by project.
    leaving: HashMap<(PathBuf, PaneId), motion::Closing>,
    tab_born: Option<(ItemId, motion::Opening)>,
    /// A tab fading out before it is closed, with its project.
    tab_leaving: Option<(PathBuf, ItemId, motion::Closing)>,
    content_switches: panes::ContentSwitches,
    /// Each pane's tab strip scroll, and the tab it last scrolled into view.
    tab_scroll: HashMap<(PathBuf, PaneId), (gpui::ScrollHandle, Option<ItemId>)>,
    ratio_anim: Option<panes::RatioAnim>,
    /// Keys oneshot animations so reopening something replays them.
    generation: u64,
    save_task: Option<Task<()>>,
    /// Unsaved files were settled by the Save question, so quitting must not keep copies of them.
    quit_settled: bool,
    redraw_pending: Option<Task<()>>,
    /// What each terminal last gave the sidebar about Claude, by view.
    claude_marks: HashMap<gpui::EntityId, panes::ClaudeMark>,
    _awake_poll: Option<Task<()>>,
    rail_from: usize,
    rail_scroll: gpui::ScrollHandle,
    /// The switch the rail last scrolled the active project into view for.
    rail_revealed: u64,
    /// The project or folder the open folder picker moves or renames.
    sidebar_subject: Option<sidebar::Subject>,
    /// Folders and worktree groups easing open or shut.
    sidebar_folds: Vec<(sidebar::Fold, sidebar::Tween)>,
    /// The sidebar easing between the rail and its wide layout.
    sidebar_width: Option<sidebar::Tween>,
    switch_count: u64,
    /// The switch whose fade-in the content is playing, and since when.
    content_switch: (u64, motion::Opening),
    focus_pending: bool,
    _subscriptions: Vec<Subscription>,
}

impl Shell {
    pub fn new(mut workspace: Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        let settings = settings::SettingsState::load(&mut workspace);
        cx.global_mut::<athena_ui::Theme>()
            .set_code_zoom(workspace.ui.font_zoom);
        athena_ui::set_appearance(appearance::resolve(workspace.theme, window), cx);
        let subscriptions = vec![
            cx.observe_window_appearance(window, |this, window, cx| {
                athena_ui::set_appearance(appearance::resolve(this.workspace.theme, window), cx);
            }),
            cx.observe_window_bounds(window, |this, window, cx| {
                this.workspace.window = Some(window_state(window.window_bounds()));
                this.schedule_save(cx);
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    windows::set_focused(window.window_handle(), cx);
                    // Claude Code hears this window's selection now, even if it has not moved.
                    this.ide_selection_stale();
                    this.tree.invalidate();
                    crate::system_notify::recheck();
                    this.reload_changed_files(cx);
                    this.git_kick(cx);
                    let reduced = motion::system_reduce_motion();
                    if cx.theme().motion.reduced != reduced {
                        cx.global_mut::<athena_ui::Theme>().motion.reduced = reduced;
                        cx.refresh_windows();
                    }
                }
            }),
            cx.on_app_quit(|this, cx| {
                this.flush_unsaved(cx);
                this.save_now(cx);
                windows::quit_ide(cx);
                this.tests_quit();
                this.debug_quit();
                async {}
            }),
        ];
        let roots: Vec<PathBuf> = workspace.projects.iter().map(|p| p.root.clone()).collect();
        let debug = debug::DebugState::load(&windows::workspace_path(cx), &roots);
        let last_drawer_tab = drawer::DrawerTab::restored(workspace.ui.drawer_tab);
        let mut shell = Self {
            rail_from: workspace.active.unwrap_or(0),
            rail_scroll: gpui::ScrollHandle::new(),
            rail_revealed: u64::MAX,
            sidebar_subject: None,
            sidebar_folds: Vec::new(),
            sidebar_width: None,
            workspace,
            window_handle: window.window_handle(),
            focus,
            drawer_focus: cx.focus_handle(),
            items: HashMap::new(),
            pane_area: Rc::default(),
            drag: None,
            drop_hint: None,
            palette: None,
            palette_closing: None,
            palette_files: HashMap::new(),
            context_menu: None,
            item_menus: HashSet::new(),
            tree: tree::FileTree::default(),
            tree_opening: None,
            tree_closing: None,
            notices: cx.global::<windows::Windows>().notices.clone(),
            toasts: Vec::new(),
            toast_area: Rc::default(),
            drawer: None,
            drawer_shown: None,
            drawer_opening: None,
            drawer_closing: None,
            last_drawer_tab,
            containers: containers_view::ContainersState::default(),
            playwright: playwright_view::PlaywrightState::default(),
            tests: tests_view::TestsState::default(),
            debug,
            lsp: lsp::LspState::default(),
            settings,
            breadcrumbs: breadcrumbs::BreadcrumbState::default(),
            outline: outline::OutlineState::default(),
            side_panel: side_panel::SidePanelState::default(),
            problems: problems::ProblemsState::default(),
            code_actions: code_actions::CodeActionState::default(),
            git: git_view::GitState::default(),
            review: review::ReviewState::default(),
            search: search::SearchState::default(),
            history: history::History::default(),
            watch: Self::start_watching(window, cx),
            last_editor_pane: HashMap::new(),
            window_title: String::new(),
            pending_open: None,
            ide: claude_ide::IdeState::default(),
            usage: usage_view::UsageState::default(),
            bell: notices::BellState::new(cx),
            _usage: None,
            zoomed: None,
            entering: None,
            pane_opening: None,
            leaving: HashMap::new(),
            tab_born: None,
            tab_leaving: None,
            content_switches: panes::ContentSwitches::default(),
            tab_scroll: HashMap::new(),
            ratio_anim: None,
            generation: 0,
            save_task: None,
            quit_settled: false,
            redraw_pending: None,
            claude_marks: HashMap::new(),
            _awake_poll: None,
            switch_count: 0,
            content_switch: (0, motion::Opening::now()),
            focus_pending: true,
            _subscriptions: subscriptions,
        };
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            this.update(cx, |this, cx| this.close_window(window, cx))
                .is_err()
        });
        shell.restore_terminal_panel();
        shell.announce_recovery(cx);
        shell.start_usage(window, cx);
        shell.poll_awake(cx);
        shell.start_git(window, cx);
        shell.start_ide(window, cx);
        shell.start_keymap(window, cx);
        shell.start_settings(window, cx);
        shell.sync_claude_hooks(cx);
        crate::system_notify::set_badge(shell.unread());
        shell
    }

    fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        windows::publish(
            Some(self.window_handle),
            windows::AppFields::of(&self.workspace),
            cx,
        );
        // Claude Code sees a project open or close now, not when the debounced save lands.
        cx.defer(windows::refresh_ide_folders);
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            this.update(cx, |this, cx| this.save_now(cx)).ok();
        }));
    }

    fn save_now(&mut self, cx: &mut Context<Self>) {
        self.save_task = None;
        self.capture_view_states(cx);
        let persisted = self
            .settings
            .persisted(self.persisted_workspace())
            .into_owned();
        windows::store(self.window_handle, persisted, cx);
    }

    fn switch_to(&mut self, index: usize, cx: &mut Context<Self>) {
        self.switch(index, true, cx);
    }

    /// `reveal` opens the sidebar folders around the project.
    fn switch(&mut self, index: usize, reveal: bool, cx: &mut Context<Self>) {
        if Some(index) == self.workspace.active || index >= self.workspace.projects.len() {
            return;
        }
        self.rail_from = self.workspace.active.unwrap_or(index);
        self.switch_count += 1;
        self.workspace.activate(index);
        if reveal {
            self.reveal_project(index, cx);
        }
        self.zoomed = None;
        self.focus_pending = true;
        self.schedule_save(cx);
        self.git_kick(cx);
        cx.notify();
    }

    fn add_project(&mut self, _: &AddProject, _: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open Project".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(root) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |this, cx| this.open_folder(root, cx)).ok();
        })
        .detach();
    }

    /// Opens a folder as a project, or switches to it if it is already open, in this window or
    /// another, as VS Code does.
    pub fn open_folder(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if let Some(holder) = windows::holder_of(&root, self.window_handle, cx) {
            return windows::focus_project(holder, root, cx);
        }
        let previous = self.workspace.active;
        let index = match windows::take_parked(&root, cx) {
            Some((project, bindings)) => {
                self.review.sessions.absorb(bindings);
                let index = self.workspace.adopt_project(project);
                self.sync_review_columns(cx);
                index
            }
            None => self.workspace.add_project(root),
        };
        let root = self.workspace.projects[index].root.clone();
        self.debug.reload_project(&root);
        self.workspace.active = previous;
        self.group_worktrees();
        self.refresh_worktree_lists(cx);
        if let Some(index) = self.workspace.projects.iter().position(|p| p.root == root) {
            self.switch_to(index, cx);
        }
    }

    /// Files and folders dropped from Finder: folders open as projects, files as tabs.
    fn open_dropped(&mut self, paths: &[PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        for path in paths {
            if path.is_dir() {
                self.open_folder(path.clone(), cx);
                continue;
            }
            if self.workspace.active.is_none()
                && let Some(parent) = path.parent()
            {
                self.open_folder(parent.to_path_buf(), cx);
            }
            self.open_file(path.clone(), window, cx);
        }
    }

    fn close_project(&mut self, _: &CloseProject, window: &mut Window, cx: &mut Context<Self>) {
        self.close_active_project(window, cx);
    }

    /// Steps through every project, leaving collapsed folders closed.
    fn cycle(&mut self, step: isize, cx: &mut Context<Self>) {
        let len = self.workspace.projects.len() as isize;
        if let Some(active) = self.workspace.active {
            self.switch((active as isize + step).rem_euclid(len) as usize, false, cx);
        }
    }

    /// "<active tab> — <project>" for the Window menu, Mission Control and Cmd+`.
    fn sync_window_title(&mut self, window: &mut Window, cx: &Context<Self>) {
        let title = match self.workspace.active_project() {
            None => "Athena".to_string(),
            Some(project) => {
                let tab = project
                    .layout
                    .as_ref()
                    .and_then(|l| l.focused_pane())
                    .and_then(|p| p.active_item())
                    .map(
                        |item| match self.items.get(&(project.root.clone(), item.id)) {
                            // Claude animates its title as it works; each retitle resets the cursor.
                            Some(item::ItemView::Terminal(v)) if v.read(cx).is_claude() => {
                                "Claude".into()
                            }
                            _ => self.item_label(&project.root, item, cx),
                        },
                    );
                let dirty = if self.items.values().any(|v| v.is_dirty(cx)) {
                    "• "
                } else {
                    ""
                };
                match tab {
                    Some(tab) => format!("{dirty}{tab} — {}", project.name()),
                    None => format!("{dirty}{}", project.name()),
                }
            }
        };
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let project = self.workspace.active_project();
        div()
            .id("title-bar")
            .h(t.ui(TITLE_BAR_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .pl(px(88.))
            .bg(t.color.surface)
            .border_b_1()
            .border_color(t.color.border)
            .text_size(t.typography.caption)
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    window.titlebar_double_click();
                }
            })
            .children(project.map(|p| {
                div()
                    .flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(t.color.content)
                            .child(p.name()),
                    )
                    .children(self.cached_branch(&p.root).map(|branch| {
                        div()
                            .text_color(t.color.content_muted)
                            .child(format!("· {branch}"))
                    }))
            }))
            .children(self.render_debug_toolbar(cx))
            .children(self.render_attention_chip(cx))
            .child(div().flex_1())
            .child(self.render_bell(cx))
    }

    fn render_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        if self.content_switch.0 != self.switch_count {
            self.content_switch = (self.switch_count, motion::Opening::now());
        }
        let body = match self.workspace.active {
            Some(_) => div()
                .size_full()
                .flex()
                .flex_col()
                .relative()
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .children(self.render_tree(cx))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .child(self.render_panes(window, cx)),
                        )
                        .children(self.render_tree_handle(cx)),
                )
                .children(self.render_drawer(window, cx))
                .children(self.render_drawer_handle(window, cx))
                .into_any_element(),
            None => div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(24.))
                .child(Lockup::new(t.typography.display))
                .child(empty_state(
                    "No projects open",
                    "Each project keeps its own terminals, editors and Claude sessions.",
                    Some(
                        Button::new("open-project", "Open project", ButtonKind::Secondary)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(AddProject), cx)
                            }),
                    ),
                    cx,
                ))
                .into_any_element(),
        };
        let pane = div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(t.color.surface_sunken)
            .child(body);
        motion::animate_if(
            t.motion.reduced,
            pane,
            ("project-content", self.switch_count),
            Animation::new(t.motion.base).with_easing(motion::ease_standard()),
            |el, d| el.opacity(d),
        )
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_window_zoom(cx);
        self.sync_previews(cx);
        self.sync_watchers();
        if self.lsp.jump.is_some() {
            self.record_location(cx);
        }
        self.take_lsp_jump(window, cx);
        self.take_pending_open(window, cx);
        self.sync_window_title(window, cx);
        crate::actions::sync_recent_menu(&self.workspace.recent, cx);
        self.sync_sidebar_motion(window, cx);
        let t = cx.theme().clone();
        let sidebar = self.render_sidebar(cx);
        // The sidebar draws over its slot, so it paints above the content as it narrows.
        let body = div()
            .relative()
            .flex_1()
            .min_h_0()
            .flex()
            .child(self.render_sidebar_slot(cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.render_content(window, cx)),
            )
            .child(sidebar);
        let body = motion::animate_if(
            t.motion.reduced,
            body,
            "launch",
            Animation::new(t.motion.base).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d).top(px(4. * (1. - d))),
        );

        let root = git_view::bind_git_actions(div(), cx);
        let root = conflicts::bind_conflict_actions(tasks::bind_run_actions(root, cx), cx);
        let root = tests_view::bind_test_actions(root, cx);
        let root = snippets::bind_snippet_actions(root, cx);
        let root = debug::bind_debug_actions(root, cx);
        let root = claude_sessions::bind_claude_actions(root, cx);
        let root = review_comments::bind_comment_actions(root, cx);
        let root = github::bind_github_actions(root, cx);
        root.track_focus(&self.focus)
            .key_context("Shell")
            .on_action(cx.listener(Self::add_project))
            .on_action(cx.listener(Self::close_project))
            .on_action(cx.listener(|this, _: &PrevProject, _, cx| this.cycle(-1, cx)))
            .on_action(cx.listener(|this, _: &NextProject, _, cx| this.cycle(1, cx)))
            .on_action(cx.listener(|this, a: &SelectProject, _, cx| {
                if let Some(&index) = this.shown_projects().get(a.0) {
                    this.switch_to(index, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &Quit, w, cx| this.quit(w, cx)))
            .on_action(|_: &Minimize, window, _| window.minimize_window())
            .on_action(|_: &Zoom, window, _| window.zoom_window())
            .on_action(|_: &ToggleFullScreen, window, _| window.toggle_fullscreen())
            .on_action(cx.listener(|this, _: &NewTerminal, w, cx| this.new_terminal(w, cx)))
            .on_action(cx.listener(|this, _: &NewPreview, w, cx| this.new_preview(w, cx)))
            .on_action(cx.listener(|this, _: &CloseTab, w, cx| this.close_active_tab(w, cx)))
            .on_action(
                cx.listener(|this, _: &SplitRight, w, cx| this.split(Axis::Horizontal, w, cx)),
            )
            .on_action(cx.listener(|this, _: &SplitDown, w, cx| this.split(Axis::Vertical, w, cx)))
            .on_action(cx.listener(|this, _: &FocusPaneLeft, w, cx| {
                this.focus_direction(Direction::Left, w, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusPaneRight, w, cx| {
                this.focus_direction(Direction::Right, w, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusPaneUp, w, cx| {
                this.focus_direction(Direction::Up, w, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusPaneDown, w, cx| {
                this.focus_direction(Direction::Down, w, cx)
            }))
            .on_action(cx.listener(|this, _: &TogglePaneZoom, _, cx| this.toggle_zoom(cx)))
            .on_action(cx.listener(|this, _: &TogglePreview, w, cx| this.toggle_rendered(w, cx)))
            .on_action(cx.listener(|this, _: &SaveAs, w, cx| this.save_as(w, cx)))
            .on_action(cx.listener(|this, _: &ToggleAutoSave, _, cx| this.toggle_autosave(cx)))
            .on_action(
                cx.listener(|this, _: &ToggleFormatOnSave, _, cx| this.toggle_format_on_save(cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleWordWrapDefault, _, cx| {
                this.toggle_word_wrap_default(cx)
            }))
            .on_action(cx.listener(|this, _: &NextTab, w, cx| this.cycle_tab(1, w, cx)))
            .on_action(cx.listener(|this, _: &PrevTab, w, cx| this.cycle_tab(-1, w, cx)))
            .on_action(
                cx.listener(|this, a: &SelectTab, w, cx| this.activate_tab_in_focused(a.0, w, cx)),
            )
            .capture_any_mouse_down(|_, window, _| athena_preview::restore_key_focus(window))
            .on_mouse_move(cx.listener(Self::drag_move))
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _, window, cx| this.navigate(false, window, cx)),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|this, _, window, cx| this.navigate(true, window, cx)),
            )
            .on_action(cx.listener(|this, _: &RevealInTree, _, cx| {
                if let Some(path) = this.open_editor_path() {
                    this.reveal_in_tree(&path, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NavigateBack, w, cx| this.navigate(false, w, cx)))
            .on_action(cx.listener(|this, _: &NavigateForward, w, cx| this.navigate(true, w, cx)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.open_dropped(paths.paths(), window, cx)
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.drag_end(cx)),
            )
            .size_full()
            .flex()
            .flex_col()
            .bg(t.color.surface)
            .font_family(t.typography.ui.clone())
            .text_size(t.typography.body)
            .text_color(t.color.content)
            .on_action(cx.listener(|this, _: &QuickOpen, w, cx| {
                this.open_palette(palette::Mode::Files, w, cx)
            }))
            .on_action(cx.listener(|this, _: &QuickOpenBeside, w, cx| {
                this.open_palette(palette::Mode::FilesBeside, w, cx)
            }))
            .on_action(cx.listener(|this, _: &CommandPalette, w, cx| {
                this.open_palette(palette::Mode::Commands, w, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleDrawer, _, cx| this.toggle_drawer(cx)))
            .on_action(cx.listener(|this, _: &ShowNotifications, w, cx| this.open_bell(w, cx)))
            .on_action(
                cx.listener(|this, _: &crate::actions::ToggleTerminalPanel, w, cx| {
                    this.toggle_terminal_panel(w, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::NewPanelTerminal, w, cx| {
                    this.new_panel_terminal(w, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::MoveTerminalToPanel, w, cx| {
                    this.move_terminal(true, w, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::MoveTerminalToEditor, w, cx| {
                    this.move_terminal(false, w, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ShowContainers, _, cx| {
                this.toggle_drawer_tab(drawer::DrawerTab::Containers, cx)
            }))
            .on_action(
                cx.listener(|this, _: &NewClaudeSession, w, cx| this.new_claude_session(w, cx)),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::ContinueClaudeSession, w, cx| {
                    this.continue_claude_session(w, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ChangeClaudeCommand, w, cx| {
                    this.change_claude_command(w, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &DisableClaudeHooks, w, cx| {
                this.remove_project_claude_hooks(w, cx)
            }))
            .on_action(cx.listener(|this, _: &ShowPlaywright, _, cx| {
                this.toggle_drawer_tab(drawer::DrawerTab::Playwright, cx)
            }))
            .on_action(cx.listener(|this, _: &RunPlaywright, w, cx| this.run_playwright(w, cx)))
            .on_action(cx.listener(|this, _: &EnablePlaywrightMcp, w, cx| {
                this.set_playwright_mcp(true, w, cx)
            }))
            .on_action(cx.listener(|this, _: &DisablePlaywrightMcp, w, cx| {
                this.set_playwright_mcp(false, w, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleFileTree, _, cx| this.toggle_tree(cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &MoveProjectToFolder, window, cx| {
                this.move_project_to_folder(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ToggleProjectFolder, _, cx| this.toggle_active_folder(cx)),
            )
            .on_action(
                cx.listener(|this, _: &ToggleWorktrees, _, cx| this.toggle_active_worktrees(cx)),
            )
            .on_action(
                cx.listener(|this, _: &CollapseAllFolders, _, cx| this.collapse_all_folders(cx)),
            )
            .on_action(cx.listener(|this, _: &SwitchProject, window, cx| {
                this.open_palette(palette::Mode::Projects, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NextClaudeAttention, window, cx| {
                this.next_claude_attention(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &crate::actions::ShowSessions, w, cx| {
                    this.show_side_mode(SideMode::Sessions, w, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &crate::actions::ShowFiles, w, cx| {
                this.show_side_mode(SideMode::Files, w, cx)
            }))
            .on_action(
                cx.listener(|this, _: &crate::actions::ShowExplorer, w, cx| {
                    this.show_side_mode(SideMode::Files, w, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &crate::actions::ShowOutline, w, cx| {
                this.show_side_mode(SideMode::Outline, w, cx)
            }))
            .on_action(cx.listener(|this, _: &crate::actions::ShowGit, w, cx| {
                this.show_side_mode(SideMode::Git, w, cx)
            }))
            .on_action(cx.listener(|this, _: &FindInProject, w, cx| this.find_in_project(w, cx)))
            .on_action(cx.listener(|this, _: &ShowChanges, _, cx| this.toggle_changes(cx)))
            .on_action(cx.listener(|this, _: &ToggleBlame, _, cx| this.toggle_blame(cx)))
            .on_action(cx.listener(|this, _: &FontZoomIn, _, cx| this.zoom_font(Some(1), cx)))
            .on_action(cx.listener(|this, _: &FontZoomOut, _, cx| this.zoom_font(Some(-1), cx)))
            .on_action(cx.listener(|this, _: &FontZoomReset, _, cx| this.zoom_font(None, cx)))
            .on_action(
                cx.listener(|this, _: &crate::actions::WindowZoomIn, _, cx| {
                    this.zoom_window(Some(1), cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::WindowZoomOut, _, cx| {
                    this.zoom_window(Some(-1), cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::WindowZoomReset, _, cx| {
                    this.zoom_window(None, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &SwitchBranch, window, cx| this.open_branches(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ShowProblems, _, cx| this.toggle_problems(cx)))
            .on_action(cx.listener(|this, _: &NextProblem, _, cx| this.go_to_problem(true, cx)))
            .on_action(cx.listener(|this, _: &PrevProblem, _, cx| this.go_to_problem(false, cx)))
            .on_action(
                cx.listener(|this, _: &athena_editor::GoToImplementation, _, cx| {
                    this.lsp_implementation(false, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &athena_editor::GoToTypeDefinition, _, cx| {
                    this.lsp_implementation(true, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &athena_editor::ShowCallHierarchy, _, cx| {
                    this.show_call_hierarchy(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &athena_editor::ShowTypeHierarchy, _, cx| {
                    this.show_type_hierarchy(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &athena_editor::ShowCodeActions, w, cx| {
                    this.show_code_actions(w, cx)
                }),
            )
            .on_action(
                cx.listener(|this, a: &crate::actions::ApplyCodeAction, _, cx| {
                    this.apply_code_action(a.0, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &athena_editor::RenameSymbol, w, cx| {
                this.lsp_rename_start(w, cx)
            }))
            .on_action(
                cx.listener(|this, a: &athena_editor::ConfirmRename, _, cx| {
                    this.lsp_rename_confirm(a.name.clone(), cx)
                }),
            )
            .on_action(cx.listener(|this, _: &GoToSymbol, w, cx| {
                this.open_palette_with(palette::Mode::Files, "@", w, cx)
            }))
            .on_action(cx.listener(|this, _: &GoToWorkspaceSymbol, w, cx| {
                this.open_palette_with(palette::Mode::Files, "#", w, cx)
            }))
            .on_action(cx.listener(|this, _: &lsp::AllowProjectLinters, _, cx| {
                this.change_linter_trust(true, cx)
            }))
            .on_action(cx.listener(|this, _: &lsp::DisallowProjectLinters, _, cx| {
                this.change_linter_trust(false, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleIdeIntegration, w, cx| {
                this.toggle_ide_integration(w, cx)
            }))
            .on_action(cx.listener(|this, _: &SendToClaude, w, cx| this.send_to_claude(w, cx)))
            .on_action(cx.listener(|this, _: &crate::actions::OpenRecent, w, cx| {
                this.open_palette(palette::Mode::Recent, w, cx)
            }))
            .on_action(
                cx.listener(|this, a: &crate::actions::OpenRecentProject, _, cx| {
                    this.open_folder(a.0.clone(), cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::ThemeFollowSystem, w, cx| {
                    this.set_theme_choice(athena_workspace::ThemeChoice::System, w, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &crate::actions::ThemeLight, w, cx| {
                this.set_theme_choice(athena_workspace::ThemeChoice::Light, w, cx)
            }))
            .on_action(cx.listener(|this, _: &crate::actions::ThemeDark, w, cx| {
                this.set_theme_choice(athena_workspace::ThemeChoice::Dark, w, cx)
            }))
            .on_action(
                cx.listener(|this, _: &crate::actions::OpenKeyboardShortcuts, w, cx| {
                    this.open_shortcuts_ui(w, cx)
                }),
            )
            .on_action(cx.listener(
                |this, _: &crate::actions::OpenKeyboardShortcutsJson, w, cx| {
                    this.open_keymap_file(w, cx)
                },
            ))
            .on_action(
                cx.listener(|this, _: &crate::actions::OpenSettings, w, cx| {
                    this.open_settings_ui(w, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::OpenSettingsJson, w, cx| {
                    this.open_settings_file(w, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::ToggleInlayHints, _, cx| {
                    this.toggle_inlay_hints(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::ToggleMinimap, _, cx| {
                    this.toggle_minimap(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::MoveProjectToNewWindow, _, cx| {
                    this.move_active_to_new_window(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::OpenProjectInNewWindow, _, cx| {
                    this.open_in_new_window(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::CloseWindow, w, cx| {
                    this.close_window(w, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::MergeAllWindows, _, cx| {
                    let into = this.window_handle;
                    cx.defer(move |cx| windows::merge_into(into, cx));
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::ClearRecent, window, cx| {
                    this.clear_recent(window, cx)
                }),
            )
            .relative()
            .child(self.render_title_bar(cx))
            .child(body)
            .child(self.render_status_bar(cx))
            .children(self.render_usage_popover(cx))
            .children(self.render_bell_popover(window, cx))
            .children(self.render_toasts(cx))
            .children(self.render_palette(cx))
            .children(self.context_menu.as_ref().map(|(menu, _)| menu.clone()))
    }
}

fn window_state(bounds: WindowBounds) -> WindowState {
    let (mode, b) = match bounds {
        WindowBounds::Windowed(b) => (WindowMode::Windowed, b),
        WindowBounds::Maximized(b) => (WindowMode::Maximized, b),
        WindowBounds::Fullscreen(b) => (WindowMode::Fullscreen, b),
    };
    WindowState {
        x: b.origin.x.into(),
        y: b.origin.y.into(),
        width: b.size.width.into(),
        height: b.size.height.into(),
        mode,
    }
}
