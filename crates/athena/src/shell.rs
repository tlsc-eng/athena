mod bridge;
mod claude;
mod containers_view;
mod drawer;
mod fileops;
mod fuzzy;
mod item;
mod lsp;
mod notices;
mod palette;
mod panes;
mod playwright_view;
mod quit;
mod tree;
mod usage_view;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use athena_proto::AppMsg;
use athena_term::ClaudeState;
use athena_ui::{ActiveTheme, Button, ButtonKind, Lockup, Tooltip, empty_state, motion};
use athena_workspace::{Axis, Direction, ItemId, PaneId, WindowMode, WindowState, Workspace};
use gpui::{
    Animation, AnyElement, Bounds, Context, FocusHandle, FontWeight, IntoElement, MouseButton,
    PathPromptOptions, Pixels, Render, Subscription, Task, Window, WindowBounds, div, prelude::*,
    px,
};

use crate::actions::{
    AddProject, ChangeClaudeCommand, CloseProject, CloseTab, CommandPalette, DisableClaudeHooks,
    DisablePlaywrightMcp, EnableClaudeHooks, EnablePlaywrightMcp, FocusPaneDown, FocusPaneLeft,
    FocusPaneRight, FocusPaneUp, Minimize, NewClaudeSession, NewPreview, NewTerminal, NextProject,
    NextTab, PrevProject, PrevTab, QuickOpen, QuickOpenBeside, Quit, RunPlaywright, SaveAs,
    SelectProject, SelectTab, ShowContainers, ShowPlaywright, SplitDown, SplitRight,
    ToggleAutoSave, ToggleFileTree, ToggleFullScreen, ToggleNotifications, TogglePaneZoom,
    TogglePreview, Zoom,
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
    items: HashMap<(PathBuf, ItemId), item::ItemView>,
    pane_area: Rc<RefCell<Bounds<Pixels>>>,
    drag: Option<panes::Drag>,
    palette: Option<palette::Palette>,
    tree: tree::FileTree,
    tree_visible: bool,
    notifications: Vec<notices::Notification>,
    notices_path: PathBuf,
    next_notice: u64,
    toasts: Vec<notices::Toast>,
    drawer: Option<drawer::DrawerTab>,
    last_drawer_tab: drawer::DrawerTab,
    containers: containers_view::ContainersState,
    playwright: playwright_view::PlaywrightState,
    lsp: lsp::LspState,
    /// Per project, the pane a file opened from a terminal goes to.
    last_editor_pane: HashMap<PathBuf, PaneId>,
    window_title: String,
    /// A file a rendered document's link asked for, opened at the next frame.
    pending_open: Option<PathBuf>,
    _notices: Option<Task<()>>,
    _clicks: Task<()>,
    _app_socket: Task<()>,
    usage: usage_view::UsageState,
    _usage: Option<Task<()>>,
    zoomed: Option<PaneId>,
    entering: Option<PaneId>,
    leaving: Option<PaneId>,
    tab_switches: u64,
    save_task: Option<Task<()>>,
    rail_from: usize,
    switch_count: u64,
    focus_pending: bool,
    _subscriptions: Vec<Subscription>,
}

impl Shell {
    pub fn new(
        workspace: Workspace,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        let subscriptions = vec![
            cx.observe_window_bounds(window, |this, window, cx| {
                this.workspace.window = Some(window_state(window.window_bounds()));
                this.schedule_save(cx);
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.tree.invalidate();
                    this.reload_changed_files(cx);
                    let reduced = motion::system_reduce_motion();
                    if cx.theme().motion.reduced != reduced {
                        cx.global_mut::<athena_ui::Theme>().motion.reduced = reduced;
                    }
                }
            }),
            cx.on_app_quit(|this, _| {
                this.save_now();
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
            items: HashMap::new(),
            pane_area: Rc::default(),
            drag: None,
            palette: None,
            tree: tree::FileTree::default(),
            tree_visible: true,
            next_notice: notifications.iter().map(|n| n.id()).max().unwrap_or(0),
            notifications,
            notices_path,
            toasts: Vec::new(),
            drawer: None,
            last_drawer_tab: drawer::DrawerTab::Notifications,
            containers: containers_view::ContainersState::default(),
            playwright: playwright_view::PlaywrightState::default(),
            lsp: lsp::LspState::default(),
            last_editor_pane: HashMap::new(),
            window_title: String::new(),
            pending_open: None,
            _notices: None,
            _clicks: clicks_task,
            _app_socket: app_socket,
            usage: usage_view::UsageState::default(),
            _usage: None,
            zoomed: None,
            entering: None,
            leaving: None,
            tab_switches: 0,
            save_task: None,
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
        shell.start_usage(window, cx);
        crate::system_notify::set_badge(shell.unread());
        shell
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            this.update(cx, |this, _| this.save_now()).ok();
        }));
    }

    fn save_now(&mut self) {
        self.save_task = None;
        if let Err(err) = athena_workspace::save(&self.path, &self.workspace) {
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

    fn close_project(&mut self, _: &CloseProject, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.workspace.active {
            let root = self.workspace.projects[index].root.clone();
            self.drop_project_items(&root, cx);
            self.lsp_project_closed(&root);
            self.zoomed = None;
            self.focus_pending = true;
            self.workspace.close_project(index);
            self.rail_from = self.workspace.active.unwrap_or(0);
            self.switch_count += 1;
            self.schedule_save(cx);
            cx.notify();
        }
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
                    .children(athena_workspace::git_branch(&p.root).map(|branch| {
                        div()
                            .text_color(t.color.content_muted)
                            .child(format!("· {branch}"))
                    }))
            }))
            .child(div().flex_1())
            .child(self.render_usage_button(cx))
            .child(
                self.drawer_button("containers-button", drawer::DrawerTab::Containers, cx)
                    .child("Containers"),
            )
            .child(
                self.drawer_button("playwright-button", drawer::DrawerTab::Playwright, cx)
                    .child("Playwright"),
            )
            .child(div().mr(px(8.)).child(self.render_notice_button(cx)))
    }

    fn render_notice_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let unread = self.unread();
        let accent = cx.theme().color.accent;
        self.drawer_button("notices-button", drawer::DrawerTab::Notifications, cx)
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

        div()
            .relative()
            .w(px(RAIL_WIDTH))
            .flex_none()
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
                .child(
                    div()
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
                        ),
                )
                .children(self.render_drawer(cx))
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
        self.take_lsp_jump(window, cx);
        self.take_pending_open(window, cx);
        self.sync_window_title(window, cx);
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

        div()
            .track_focus(&self.focus)
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
            .on_action(cx.listener(|this, _: &NextTab, w, cx| this.cycle_tab(1, w, cx)))
            .on_action(cx.listener(|this, _: &PrevTab, w, cx| this.cycle_tab(-1, w, cx)))
            .on_action(
                cx.listener(|this, a: &SelectTab, w, cx| this.activate_tab_in_focused(a.0, w, cx)),
            )
            .capture_any_mouse_down(|_, window, _| athena_preview::restore_key_focus(window))
            .on_mouse_move(cx.listener(Self::drag_move))
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
            .on_action(cx.listener(|this, _: &ToggleFileTree, _, cx| {
                this.tree_visible = !this.tree_visible;
                cx.notify();
            }))
            .relative()
            .child(self.render_title_bar(cx))
            .child(body)
            .children(self.render_usage_popover(cx))
            .children(self.render_toasts(cx))
            .children(self.render_palette(cx))
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
