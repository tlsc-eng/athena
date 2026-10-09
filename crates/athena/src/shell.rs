mod appearance;
mod branches;
mod breadcrumbs;
mod bridge;
mod claude;
mod claude_ide;
mod code_actions;
mod conflicts;
mod containers_view;
mod dnd;
mod drawer;
mod edits;
mod fileops;
mod fuzzy;
mod git_view;
mod history;
mod item;
mod lsp;
mod menus;
mod notices;
mod palette;
mod panes;
mod playwright_view;
mod problems;
mod quit;
mod rename;
mod review;
mod search;
mod settings;
mod shortcuts;
mod status_bar;
mod tasks;
mod terminal_panel;
mod tests_view;
mod tree;
mod usage_view;
mod watch;
mod zoom;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use athena_proto::AppMsg;
use athena_term::ClaudeState;
use athena_ui::{ActiveTheme, Button, ButtonKind, Lockup, Tooltip, empty_state, motion};
use athena_workspace::{Axis, Direction, ItemId, PaneId, WindowMode, WindowState, Workspace};
use gpui::{
    Animation, AnyElement, Bounds, Context, ExternalPaths, FocusHandle, FontWeight, IntoElement,
    MouseButton, NavigationDirection, PathPromptOptions, Pixels, Render, Subscription, Task,
    Window, WindowBounds, div, prelude::*, px,
};

use crate::actions::{
    AddProject, ChangeClaudeCommand, CloseProject, CloseTab, CommandPalette, DisableClaudeHooks,
    DisablePlaywrightMcp, EnableClaudeHooks, EnablePlaywrightMcp, FocusPaneDown, FocusPaneLeft,
    FocusPaneRight, FocusPaneUp, Minimize, NewClaudeSession, NewPreview, NewTerminal, NextProject,
    NextTab, PrevProject, PrevTab, QuickOpen, QuickOpenBeside, Quit, RunPlaywright, SaveAs,
    SelectProject, SelectTab, ShowContainers, ShowPlaywright, SplitDown, SplitRight,
    ToggleAutoSave, ToggleFileTree, ToggleFormatOnSave, ToggleFullScreen, ToggleNotifications,
    TogglePaneZoom, TogglePreview, ToggleWordWrapDefault, Zoom,
};
use crate::actions::{
    FindInProject, FontZoomIn, FontZoomOut, FontZoomReset, GoToSymbol, GoToWorkspaceSymbol,
    NavigateBack, NavigateForward, NextProblem, PrevProblem, RevealInTree, SendToClaude,
    ShowChanges, ShowProblems, SwitchBranch, ToggleBlame, ToggleIdeIntegration,
};

const TITLE_BAR_HEIGHT: f32 = 36.;
const RAIL_WIDTH: f32 = 48.;
const RAIL_ITEM: f32 = 32.;
const RAIL_GAP: f32 = 8.;
const RAIL_TOP: f32 = 8.;
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

pub struct Shell {
    workspace: Workspace,
    path: PathBuf,
    focus: FocusHandle,
    drawer_focus: FocusHandle,
    items: HashMap<(PathBuf, ItemId), item::ItemView>,
    pane_area: Rc<RefCell<Bounds<Pixels>>>,
    drag: Option<panes::Drag>,
    /// The pane and zone a dragged tab would land in.
    drop_hint: Option<(PaneId, dnd::DropZone)>,
    palette: Option<palette::Palette>,
    palette_closing: Option<(palette::Palette, motion::Closing)>,
    context_menu: Option<(gpui::Entity<athena_ui::ContextMenu>, Subscription)>,
    /// Tabs whose editor or terminal has its own right-click menu open.
    item_menus: HashSet<(PathBuf, ItemId)>,
    tree: tree::FileTree,
    tree_opening: Option<motion::Opening>,
    tree_closing: Option<motion::Closing>,
    notifications: Vec<notices::Notification>,
    notices_path: PathBuf,
    next_notice: u64,
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
    lsp: lsp::LspState,
    settings: settings::SettingsState,
    breadcrumbs: breadcrumbs::BreadcrumbState,
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
    _notices: Option<Task<()>>,
    _clicks: Task<()>,
    _app_socket: Task<()>,
    ide: claude_ide::IdeState,
    usage: usage_view::UsageState,
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
    rail_from: usize,
    switch_count: u64,
    focus_pending: bool,
    _subscriptions: Vec<Subscription>,
}

impl Shell {
    pub fn new(
        mut workspace: Workspace,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
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
                    this.tree.invalidate();
                    this.reload_changed_files(cx);
                    this.git_kick(cx);
                    let reduced = motion::system_reduce_motion();
                    if cx.theme().motion.reduced != reduced {
                        cx.global_mut::<athena_ui::Theme>().motion.reduced = reduced;
                    }
                }
            }),
            cx.on_app_quit(|this, cx| {
                this.flush_unsaved(cx);
                this.save_now(cx);
                this.ide_quit();
                this.tests_quit();
                async {}
            }),
        ];
        let notices_path = path.with_file_name("notifications.json");
        let notifications = notices::load(&notices_path);
        let (clicks, banner_clicks) = async_channel::unbounded::<u64>();
        crate::system_notify::init(clicks);
        let clicks_task = cx.spawn_in(window, async move |this, cx| {
            while let Ok(id) = banner_clicks.recv().await {
                let opened = this.update_in(cx, |this, window, cx| {
                    cx.activate(true);
                    this.open_notification(id, window, cx);
                });
                if opened.is_err() {
                    return;
                }
            }
        });
        let requests = crate::app_socket::listen();
        let app_socket = cx.spawn_in(window, async move |this, cx| {
            while let Ok(request) = requests.recv().await {
                let answered = this.update_in(cx, |this, window, cx| {
                    let caller = this.verify_caller(request.claimed, &request.lineage, cx);
                    if let AppMsg::RunInTerminal {
                        session,
                        text,
                        newline,
                    } = request.msg
                    {
                        this.confirm_run(session, text, newline, caller, request.reply, window, cx);
                        return;
                    }
                    let reply = this.handle_app(request.msg, caller, window, cx);
                    let _ = request.reply.send(reply);
                });
                if answered.is_err() {
                    return;
                }
            }
        });
        let mut shell = Self {
            rail_from: workspace.active.unwrap_or(0),
            workspace,
            path,
            focus,
            drawer_focus: cx.focus_handle(),
            items: HashMap::new(),
            pane_area: Rc::default(),
            drag: None,
            drop_hint: None,
            palette: None,
            palette_closing: None,
            context_menu: None,
            item_menus: HashSet::new(),
            tree: tree::FileTree::default(),
            tree_opening: None,
            tree_closing: None,
            next_notice: notifications.iter().map(|n| n.id()).max().unwrap_or(0),
            notifications,
            notices_path,
            toasts: Vec::new(),
            toast_area: Rc::default(),
            drawer: None,
            drawer_shown: None,
            drawer_opening: None,
            drawer_closing: None,
            last_drawer_tab: drawer::DrawerTab::Notifications,
            containers: containers_view::ContainersState::default(),
            playwright: playwright_view::PlaywrightState::default(),
            tests: tests_view::TestsState::default(),
            lsp: lsp::LspState::default(),
            settings,
            breadcrumbs: breadcrumbs::BreadcrumbState::default(),
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
            _notices: None,
            _clicks: clicks_task,
            _app_socket: app_socket,
            ide: claude_ide::IdeState::default(),
            usage: usage_view::UsageState::default(),
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
            switch_count: 0,
            focus_pending: true,
            _subscriptions: subscriptions,
        };
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            // Closing the only window quits, so unsaved files get the same question as Cmd+Q.
            this.update(cx, |this, cx| this.quit(window, cx)).is_err()
        });
        shell.start_notices(window, cx);
        shell.announce_recovery(cx);
        shell.start_usage(window, cx);
        shell.start_git(window, cx);
        shell.start_ide(window, cx);
        shell.start_keymap(window, cx);
        shell.start_settings(window, cx);
        crate::system_notify::set_badge(shell.unread());
        shell
    }

    fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            this.update(cx, |this, cx| this.save_now(cx)).ok();
        }));
    }

    fn save_now(&mut self, cx: &gpui::App) {
        self.save_task = None;
        self.sync_ide_folders();
        self.capture_view_states(cx);
        let persisted = self.settings.persisted(self.persisted_workspace());
        if let Err(err) = athena_workspace::save(&self.path, &persisted) {
            tracing::error!("could not save the workspace: {err:#}");
        }
        if let Err(err) = notices::save(&self.notices_path, &self.notifications) {
            tracing::error!("could not save notifications: {err:#}");
        }
    }

    fn switch_to(&mut self, index: usize, cx: &mut Context<Self>) {
        if Some(index) == self.workspace.active || index >= self.workspace.projects.len() {
            return;
        }
        self.rail_from = self.workspace.active.unwrap_or(index);
        self.switch_count += 1;
        self.workspace.activate(index);
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

    /// Opens a folder as a project, or switches to it if it is already open.
    pub fn open_folder(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let previous = self.workspace.active;
        let index = self.workspace.add_project(root);
        self.workspace.active = previous;
        self.switch_to(index, cx);
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

    fn cycle(&mut self, step: isize, cx: &mut Context<Self>) {
        let len = self.workspace.projects.len() as isize;
        if let Some(active) = self.workspace.active {
            self.switch_to((active as isize + step).rem_euclid(len) as usize, cx);
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
                    .map(|item| self.item_label(&project.root, item, cx));
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
            .h(px(TITLE_BAR_HEIGHT))
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
            .child(div().flex_1())
            .child(self.render_problems_button(cx))
            .child(self.render_usage_button(cx))
            .child(
                self.drawer_button("containers-button", drawer::DrawerTab::Containers, cx)
                    .tooltip(|_, cx| Tooltip::view("Docker containers for this project", cx))
                    .child("Containers"),
            )
            .child(
                self.drawer_button("playwright-button", drawer::DrawerTab::Playwright, cx)
                    .tooltip(|_, cx| Tooltip::view("Playwright test runs", cx))
                    .child("Playwright"),
            )
            .child(div().mr(px(8.)).child(self.render_notice_button(cx)))
    }

    fn render_notice_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let unread = self.unread();
        let accent = cx.theme().color.accent;
        self.drawer_button("notices-button", drawer::DrawerTab::Notifications, cx)
            .tooltip(|_, cx| Tooltip::view("Notifications  ⌘J", cx))
            .child("Notifications")
            .when(unread > 0, |el| {
                el.child(
                    div()
                        .text_color(accent)
                        .font_weight(FontWeight::MEDIUM)
                        .child(unread.to_string()),
                )
            })
    }

    fn drawer_button(
        &self,
        id: &'static str,
        tab: drawer::DrawerTab,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let t = cx.theme();
        div()
            .id(id)
            .h(px(24.))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(6.))
            .rounded(t.shape.radius_control)
            .cursor_pointer()
            .text_color(if self.drawer == Some(tab) {
                t.color.content
            } else {
                t.color.content_muted
            })
            .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_drawer_tab(tab, cx)))
    }

    fn render_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let active = self.workspace.active;
        let monograms = self.workspace.monograms();
        let items = self
            .workspace
            .projects
            .iter()
            .zip(monograms)
            .enumerate()
            .map(|(i, (project, monogram))| {
                let selected = Some(i) == active;
                let root = project.root.display().to_string();
                div()
                    .id(("rail-item", i))
                    .size(px(RAIL_ITEM))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(t.shape.radius_control)
                    .text_size(t.typography.caption)
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .text_color(if selected {
                        t.color.accent
                    } else {
                        t.color.content_muted
                    })
                    .when(!selected, |el| {
                        el.hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                    })
                    .tooltip(move |_, cx| Tooltip::view(root.clone(), cx))
                    .on_click(cx.listener(move |this, _, _, cx| this.switch_to(i, cx)))
                    .relative()
                    .child(monogram)
                    .children(self.project_claude_state(&project.root, cx).map(|state| {
                        let color = match state {
                            ClaudeState::Waiting => t.color.warning,
                            ClaudeState::Running => t.color.success,
                        };
                        div()
                            .absolute()
                            .right(px(2.))
                            .bottom(px(2.))
                            .size(px(6.))
                            .bg(color)
                    }))
            });

        let indicator = active.map(|to| {
            let y =
                |i: usize| RAIL_TOP + i as f32 * (RAIL_ITEM + RAIL_GAP) + (RAIL_ITEM - 20.) / 2.;
            let (from_y, to_y) = (y(self.rail_from), y(to));
            let bar = div()
                .absolute()
                .left_0()
                .top(px(to_y))
                .w(px(2.))
                .h(px(20.))
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
        div()
            .relative()
            .w(px(RAIL_WIDTH))
            .flex_none()
            .drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(drop_tint))
            .flex()
            .flex_col()
            .items_center()
            .justify_between()
            .py(px(RAIL_TOP))
            .bg(t.color.surface)
            .border_r_1()
            .border_color(t.color.border)
            .child(div().flex().flex_col().gap(px(RAIL_GAP)).children(items))
            .child(
                div()
                    .id("rail-add")
                    .size(px(RAIL_ITEM))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(t.shape.radius_control)
                    .text_size(t.typography.body)
                    .text_color(t.color.content_muted)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                    .tooltip(|_, cx| Tooltip::view("Open project  ⌘O", cx))
                    .on_click(
                        cx.listener(|this, _, window, cx| {
                            this.add_project(&AddProject, window, cx)
                        }),
                    )
                    .child("+"),
            )
            .children(indicator)
    }

    fn render_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
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
        self.sync_previews(cx);
        self.sync_watchers();
        if self.lsp.jump.is_some() {
            self.record_location(cx);
        }
        self.take_lsp_jump(window, cx);
        self.take_pending_open(window, cx);
        self.sync_window_title(window, cx);
        crate::actions::sync_recent_menu(&self.workspace.recent, cx);
        let t = cx.theme().clone();
        let body = div()
            .relative()
            .flex_1()
            .min_h_0()
            .flex()
            .child(self.render_rail(cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.render_content(window, cx)),
            );
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
        root.track_focus(&self.focus)
            .key_context("Shell")
            .on_action(cx.listener(Self::add_project))
            .on_action(cx.listener(Self::close_project))
            .on_action(cx.listener(|this, _: &PrevProject, _, cx| this.cycle(-1, cx)))
            .on_action(cx.listener(|this, _: &NextProject, _, cx| this.cycle(1, cx)))
            .on_action(cx.listener(|this, a: &SelectProject, _, cx| this.switch_to(a.0, cx)))
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
            .on_action(cx.listener(|this, _: &ToggleNotifications, _, cx| this.toggle_drawer(cx)))
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
                cx.listener(|this, _: &ChangeClaudeCommand, w, cx| {
                    this.change_claude_command(w, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &EnableClaudeHooks, w, cx| {
                    this.set_claude_hooks(true, w, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &DisableClaudeHooks, w, cx| {
                this.set_claude_hooks(false, w, cx)
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
            .on_action(cx.listener(|this, _: &FindInProject, w, cx| this.find_in_project(w, cx)))
            .on_action(cx.listener(|this, _: &ShowChanges, _, cx| {
                this.toggle_drawer_tab(drawer::DrawerTab::Changes, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleBlame, _, cx| this.toggle_blame(cx)))
            .on_action(cx.listener(|this, _: &FontZoomIn, _, cx| this.zoom_font(Some(1), cx)))
            .on_action(cx.listener(|this, _: &FontZoomOut, _, cx| this.zoom_font(Some(-1), cx)))
            .on_action(cx.listener(|this, _: &FontZoomReset, _, cx| this.zoom_font(None, cx)))
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
                    this.open_keymap_file(w, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::OpenSettings, w, cx| {
                    this.open_settings_file(w, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::ToggleInlayHints, _, cx| {
                    this.toggle_inlay_hints(cx)
                }),
            )
            .on_action(cx.listener(|this, _: &crate::actions::ClearRecent, _, cx| {
                this.workspace.recent.clear();
                this.schedule_save(cx);
                cx.notify();
            }))
            .relative()
            .child(self.render_title_bar(cx))
            .child(body)
            .child(self.render_status_bar(cx))
            .children(self.render_usage_popover(cx))
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
