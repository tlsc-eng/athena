mod panes;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use athena_term::TerminalView;
use athena_ui::{ActiveTheme, Button, ButtonKind, Lockup, Tooltip, empty_state, motion};
use athena_workspace::{Axis, Direction, ItemId, PaneId, WindowMode, WindowState, Workspace};
use gpui::{
    Animation, AnyElement, Bounds, Context, Entity, FocusHandle, FontWeight, IntoElement,
    MouseButton, PathPromptOptions, Pixels, Render, Subscription, Task, Window, WindowBounds, div,
    prelude::*, px,
};

use crate::actions::{
    AddProject, CloseProject, CloseTab, FocusPaneDown, FocusPaneLeft, FocusPaneRight, FocusPaneUp,
    Minimize, NewTerminal, NextProject, NextTab, PrevProject, PrevTab, SelectProject, SelectTab,
    SplitDown, SplitRight, ToggleFullScreen, TogglePaneZoom, Zoom,
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
    items: HashMap<(PathBuf, ItemId), Entity<TerminalView>>,
    pane_area: Rc<RefCell<Bounds<Pixels>>>,
    drag: Option<panes::Drag>,
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
            cx.observe_window_activation(window, |_, window, cx| {
                if window.is_window_active() {
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
        Self {
            rail_from: workspace.active.unwrap_or(0),
            workspace,
            path,
            focus,
            items: HashMap::new(),
            pane_area: Rc::default(),
            drag: None,
            zoomed: None,
            entering: None,
            leaving: None,
            tab_switches: 0,
            save_task: None,
            switch_count: 0,
            focus_pending: true,
            _subscriptions: subscriptions,
        }
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
            eprintln!("athena: {err:#}");
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
            this.update(cx, |this, cx| {
                let previous = this.workspace.active;
                let index = this.workspace.add_project(root);
                this.workspace.active = previous;
                this.switch_to(index, cx);
            })
            .ok();
        })
        .detach();
    }

    fn close_project(&mut self, _: &CloseProject, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.workspace.active {
            let root = self.workspace.projects[index].root.clone();
            self.drop_project_items(&root, cx);
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
                    .child(monogram)
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
            Some(_) => self.render_panes(window, cx),
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
            .on_action(|_: &Minimize, window, _| window.minimize_window())
            .on_action(|_: &Zoom, window, _| window.zoom_window())
            .on_action(|_: &ToggleFullScreen, window, _| window.toggle_fullscreen())
            .on_action(cx.listener(|this, _: &NewTerminal, w, cx| this.new_terminal(w, cx)))
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
            .on_action(cx.listener(|this, _: &NextTab, w, cx| this.cycle_tab(1, w, cx)))
            .on_action(cx.listener(|this, _: &PrevTab, w, cx| this.cycle_tab(-1, w, cx)))
            .on_action(
                cx.listener(|this, a: &SelectTab, w, cx| this.activate_tab_in_focused(a.0, w, cx)),
            )
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
            .child(self.render_title_bar(cx))
            .child(body)
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
