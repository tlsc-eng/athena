use std::path::{Path, PathBuf};

use athena_dap::Variable;
use athena_ui::{ActiveTheme, Button, ButtonKind, Theme, Tooltip, motion};
use gpui::{
    Action, Animation, AnyElement, Context, FontWeight, Hsla, MouseButton, SharedString, Window,
    div, prelude::*, px, svg,
};

use super::Shell;
use super::debug::{LineKind, Phase, variable_key};
use crate::actions;

const ROW: f32 = 22.;
const INDENT: f32 = 12.;

/// A titled part of the Debug tab whose body scrolls on its own.
fn section(
    id: &'static str,
    title: &'static str,
    action: Option<AnyElement>,
    body: Vec<AnyElement>,
    footer: Option<AnyElement>,
    t: &Theme,
) -> gpui::Div {
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(
            div()
                .h(px(24.))
                .flex_none()
                .px(px(10.))
                .flex()
                .items_center()
                .justify_between()
                .text_color(t.color.content_muted)
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.to_uppercase())
                .children(action),
        )
        .child(
            div()
                .id(id)
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .children(body),
        )
        .children(footer)
}

fn muted_row(text: impl Into<SharedString>, t: &Theme) -> AnyElement {
    div()
        .px(px(10.))
        .py(px(3.))
        .text_color(t.color.content_muted)
        .child(text.into())
        .into_any_element()
}

fn relative<'a>(path: &'a Path, root: &Path) -> &'a Path {
    path.strip_prefix(root).unwrap_or(path)
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into(),
    )
}

impl Shell {
    pub(super) fn render_debug(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.ensure_debug_inputs(cx);
        let t = cx.theme().clone();
        let column = |el: gpui::Div| {
            el.h_full()
                .min_w_0()
                .flex()
                .flex_col()
                .border_r_1()
                .border_color(t.color.border)
        };
        let watch_input = self.debug.watch_input.clone().map(|input| {
            div()
                .h(px(26.))
                .flex_none()
                .mx(px(8.))
                .mb(px(6.))
                .px(px(8.))
                .flex()
                .items_center()
                .bg(t.color.surface)
                .border_1()
                .border_color(t.color.border)
                .rounded(t.shape.radius_control)
                .child(input)
                .into_any_element()
        });
        let remove_all = (!self.project_breakpoints().is_empty()).then(|| {
            row_button("debug-remove-all", "Remove All", &t)
                .tooltip(|_, cx| Tooltip::view("Remove All Breakpoints", cx))
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(actions::RemoveAllBreakpoints), cx)
                })
                .into_any_element()
        });
        div()
            .size_full()
            .flex()
            .text_size(t.typography.caption)
            .child(
                column(div().w(gpui::relative(0.26)))
                    .child(section(
                        "debug-stack",
                        "Call Stack",
                        None,
                        self.render_call_stack(cx),
                        None,
                        &t,
                    ))
                    .child(div().h(px(1.)).flex_none().bg(t.color.border))
                    .child(section(
                        "debug-breakpoints",
                        "Breakpoints",
                        remove_all,
                        self.render_breakpoint_rows(cx),
                        None,
                        &t,
                    )),
            )
            .child(
                column(div().w(gpui::relative(0.34)))
                    .child(section(
                        "debug-variables",
                        "Variables",
                        None,
                        self.render_variables(cx),
                        None,
                        &t,
                    ))
                    .child(div().h(px(1.)).flex_none().bg(t.color.border))
                    .child(section(
                        "debug-watch",
                        "Watch",
                        None,
                        self.render_watches(cx),
                        watch_input,
                        &t,
                    )),
            )
            .child(self.render_console(cx))
            .into_any_element()
    }

    fn render_call_stack(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = cx.theme().clone();
        let Some(session) = self.debug.session.as_ref() else {
            return vec![muted_row(
                "Not debugging. F5 debugs the open Go file's package, or the first Go \
                 configuration in .vscode/launch.json.",
                &t,
            )];
        };
        let status = match &session.phase {
            Phase::Starting => "Building and starting…".to_string(),
            Phase::Running => "Running".to_string(),
            Phase::Stopped(s) => match s.description.as_deref().or(s.text.as_deref()) {
                Some(d) if !d.is_empty() => format!("Paused: {d}"),
                _ => format!("Paused on {}", s.reason),
            },
        };
        let mut rows = vec![
            div()
                .px(px(10.))
                .h(px(ROW))
                .flex()
                .items_center()
                .text_color(match session.phase {
                    Phase::Stopped(_) => t.color.warning,
                    _ => t.color.content_secondary,
                })
                .child(status)
                .into_any_element(),
        ];
        let root = session.root.clone();
        for thread in &session.threads {
            let selected = Some(thread.id) == session.thread;
            let id = thread.id;
            rows.push(
                div()
                    .id(("debug-thread", id as u64))
                    .h(px(ROW))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .hover(|s| s.bg(t.color.surface_hover))
                    .text_color(t.color.content_secondary)
                    .on_click(cx.listener(move |this, _, _, cx| this.select_thread(id, cx)))
                    .child(if selected { "▾" } else { "▸" })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(thread.name.clone()),
                    )
                    .into_any_element(),
            );
            if !selected {
                continue;
            }
            for frame in &session.frames {
                let active = Some(frame.id) == session.frame;
                let subtle = frame.hint.as_deref() == Some("subtle") || frame.path.is_none();
                let place = frame
                    .path
                    .as_ref()
                    .map(|p| format!("{}:{}", file_name(p), frame.line))
                    .unwrap_or_default();
                let frame_id = frame.id;
                let full = frame
                    .path
                    .as_ref()
                    .map(|p| format!("{}:{}", relative(p, &root).display(), frame.line))
                    .unwrap_or_else(|| "No source".into());
                rows.push(
                    div()
                        .id(("debug-frame", frame_id as u64))
                        .h(px(ROW))
                        .pl(px(10. + 2. * INDENT))
                        .pr(px(10.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .cursor_pointer()
                        .when(active, |el| el.bg(t.color.surface_active))
                        .when(!active, |el| el.hover(|s| s.bg(t.color.surface_hover)))
                        .tooltip(move |_, cx| Tooltip::view(full.clone(), cx))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_frame(Some(frame_id), true, cx)
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_color(if subtle {
                                    t.color.content_muted
                                } else {
                                    t.color.content
                                })
                                .when(active, |el| el.font_weight(FontWeight::MEDIUM))
                                .child(frame.name.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_color(t.color.content_muted)
                                .child(place),
                        )
                        .into_any_element(),
                );
            }
        }
        rows
    }

    fn render_breakpoint_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = cx.theme().clone();
        let list = self.project_breakpoints();
        if list.is_empty() {
            return vec![muted_row(
                "Click left of a line number, or press F9, to add a breakpoint.",
                &t,
            )];
        }
        let root = self
            .debug_root()
            .map(|r| super::lsp::document_key(&r))
            .unwrap_or_default();
        list.into_iter()
            .enumerate()
            .map(|(i, (path, b))| {
                let group: SharedString = format!("debug-bp-{i}").into();
                let detail = b
                    .log_message
                    .as_ref()
                    .map(|m| format!("log: {m}"))
                    .or_else(|| b.condition.clone())
                    .or_else(|| b.hit_condition.as_ref().map(|h| format!("hit {h}")))
                    .unwrap_or_default();
                let (open, toggle, remove) = (path.clone(), path.clone(), path.clone());
                let line = b.line;
                let enabled = b.enabled;
                let dir = path
                    .parent()
                    .map(|d| relative(d, &root).display().to_string())
                    .unwrap_or_default();
                div()
                    .id(("debug-bp", i))
                    .group(group.clone())
                    .h(px(ROW))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .hover(|s| s.bg(t.color.surface_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_file_link(open.clone(), Some(line as u32 + 1), None, cx)
                    }))
                    .child(
                        div()
                            .id(("debug-bp-enabled", i))
                            .size(px(12.))
                            .flex_none()
                            .rounded(px(3.))
                            .border_1()
                            .border_color(if enabled {
                                t.color.danger
                            } else {
                                t.color.content_disabled
                            })
                            .when(enabled, |el| el.bg(t.color.danger))
                            .tooltip(move |_, cx| {
                                let text = if enabled { "Disable" } else { "Enable" };
                                Tooltip::view(text, cx)
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.enable_breakpoint(toggle.clone(), line, !enabled, cx)
                            })),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(if enabled {
                                t.color.content
                            } else {
                                t.color.content_muted
                            })
                            .child(file_name(&path)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_color(t.color.content_muted)
                            .child(format!("{dir}  {detail}")),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(t.color.content_secondary)
                            .child((line + 1).to_string()),
                    )
                    .child(
                        div()
                            .id(("debug-bp-remove", i))
                            .flex_none()
                            .px(px(4.))
                            .rounded(t.shape.radius_control)
                            .invisible()
                            .group_hover(group, |s| s.visible())
                            .text_color(t.color.content_muted)
                            .hover(|s| s.bg(t.color.surface_active).text_color(t.color.content))
                            .tooltip(|_, cx| Tooltip::view("Remove Breakpoint", cx))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.toggle_breakpoint(remove.clone(), line, cx)
                            }))
                            .child("×"),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn render_variables(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = cx.theme().clone();
        // A step keeps the last stop's variables until the next stop, so they never flicker.
        let Some(session) = self
            .debug
            .session
            .as_ref()
            .filter(|s| matches!(s.phase, Phase::Stopped(_)) || !s.scopes.is_empty())
        else {
            return vec![muted_row("Variables show while the program is paused.", &t)];
        };
        let mut rows = Vec::new();
        for scope in session.scopes.iter().filter(|s| !s.expensive) {
            rows.push(
                div()
                    .h(px(ROW))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .text_color(t.color.content_secondary)
                    .font_weight(FontWeight::MEDIUM)
                    .child(scope.name.clone())
                    .into_any_element(),
            );
            match session.children.get(&scope.variables_reference) {
                Some(vars) if vars.is_empty() => rows.push(muted_row("No variables", &t)),
                Some(vars) => self.variable_rows(&scope.name, vars, 1, &mut rows, cx),
                None => rows.push(muted_row("Loading…", &t)),
            }
        }
        if session.scopes.is_empty() {
            rows.push(muted_row("Loading…", &t));
        }
        rows
    }

    fn variable_rows(
        &self,
        parent: &str,
        vars: &[Variable],
        depth: usize,
        rows: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        let t = cx.theme().clone();
        let Some(session) = self.debug.session.as_ref() else {
            return;
        };
        for v in vars {
            let key = variable_key(parent, &v.name);
            let expandable = v.variables_reference > 0;
            let open = expandable && self.debug.expanded.contains(&key);
            let reference = v.variables_reference;
            let toggle_key = key.clone();
            let tip = match &v.type_name {
                Some(ty) => format!("{}: {ty}\n{}", v.name, v.value),
                None => format!("{}\n{}", v.name, v.value),
            };
            rows.push(
                div()
                    .id(SharedString::from(format!("debug-var-{key}")))
                    .h(px(ROW))
                    .pl(px(10. + INDENT * (depth - 1) as f32))
                    .pr(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .hover(|s| s.bg(t.color.surface_hover))
                    .when(expandable, |el| {
                        el.cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_variable(toggle_key.clone(), reference, cx)
                            }))
                    })
                    .tooltip(move |_, cx| Tooltip::view(tip.clone(), cx))
                    .child(
                        div()
                            .w(px(10.))
                            .flex_none()
                            .text_color(t.color.content_muted)
                            .child(match (expandable, open) {
                                (false, _) => "",
                                (true, false) => "▸",
                                (true, true) => "▾",
                            }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(t.color.accent)
                            .child(format!("{}:", v.name)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .font_family(t.typography.mono.clone())
                            .text_color(t.color.content)
                            .child(v.value.clone()),
                    )
                    .into_any_element(),
            );
            if !open {
                continue;
            }
            match session.children.get(&reference) {
                Some(children) => self.variable_rows(&key, children, depth + 1, rows, cx),
                None => rows.push(
                    div()
                        .pl(px(10. + INDENT * depth as f32 + 14.))
                        .h(px(ROW))
                        .flex()
                        .items_center()
                        .text_color(t.color.content_muted)
                        .child("Loading…")
                        .into_any_element(),
                ),
            }
        }
    }

    fn render_watches(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = cx.theme().clone();
        let list = self
            .debug_root()
            .and_then(|r| self.debug.watch.get(&r))
            .cloned()
            .unwrap_or_default();
        let values = self.debug.session.as_ref().map(|s| &s.watches);
        list.into_iter()
            .enumerate()
            .map(|(i, expression)| {
                let group: SharedString = format!("debug-watch-{i}").into();
                let (value, color) = match values
                    .and_then(|v| v.iter().find(|(e, _)| *e == expression))
                    .map(|(_, v)| v)
                {
                    Some(Ok(v)) => (v.result.clone(), t.color.content),
                    Some(Err(why)) => (why.clone(), t.color.content_muted),
                    None => ("not available".into(), t.color.content_muted),
                };
                div()
                    .id(("debug-watch", i))
                    .group(group.clone())
                    .h(px(ROW))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .hover(|s| s.bg(t.color.surface_hover))
                    .child(
                        div()
                            .flex_none()
                            .text_color(t.color.accent)
                            .child(format!("{expression}:")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .font_family(t.typography.mono.clone())
                            .text_color(color)
                            .child(value),
                    )
                    .child(
                        div()
                            .id(("debug-watch-remove", i))
                            .flex_none()
                            .px(px(4.))
                            .rounded(t.shape.radius_control)
                            .invisible()
                            .group_hover(group, |s| s.visible())
                            .cursor_pointer()
                            .text_color(t.color.content_muted)
                            .hover(|s| s.bg(t.color.surface_active).text_color(t.color.content))
                            .tooltip(|_, cx| Tooltip::view("Remove Expression", cx))
                            .on_click(cx.listener(move |this, _, _, cx| this.remove_watch(i, cx)))
                            .child("×"),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn render_console(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let color = |kind: LineKind| -> Hsla {
            match kind {
                LineKind::Output => t.color.content,
                LineKind::Error => t.color.danger,
                LineKind::Input => t.color.content_secondary,
                LineKind::Result => t.color.accent,
                LineKind::Info => t.color.content_muted,
            }
        };
        let lines: Vec<AnyElement> = self
            .debug
            .console
            .iter()
            .map(|line| {
                div()
                    .px(px(10.))
                    .text_color(color(line.kind))
                    .child(line.text.clone())
                    .into_any_element()
            })
            .collect();
        if self.debug.console_follow.replace(false) {
            self.debug.console_scroll.scroll_to_bottom();
        }
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(24.))
                    .flex_none()
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .text_color(t.color.content_muted)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("DEBUG CONSOLE"),
            )
            .child(
                div()
                    .id("debug-console")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.debug.console_scroll)
                    .font_family(t.typography.mono.clone())
                    .children(lines),
            )
            .children(self.debug.console_input.clone().map(|input| {
                div()
                    .h(px(26.))
                    .flex_none()
                    .mx(px(8.))
                    .mb(px(6.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .bg(t.color.surface)
                    .border_1()
                    .border_color(t.color.border)
                    .rounded(t.shape.radius_control)
                    .child(div().text_color(t.color.content_muted).child("›"))
                    .child(div().flex_1().min_w_0().child(input))
            }))
            .into_any_element()
    }

    /// The Debug tab's header: what is being debugged, or a Start button.
    pub(super) fn render_debug_action(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = cx.theme().clone();
        match self.debug.session.as_ref() {
            Some(session) => Some(
                div()
                    .text_color(t.color.content_muted)
                    .child(session.launch.name.clone())
                    .into_any_element(),
            ),
            None => Some(
                Button::new("debug-start", "Start Debugging", ButtonKind::Ghost)
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(actions::StartDebugging), cx)
                    })
                    .into_any_element(),
            ),
        }
    }

    /// The step, continue and stop buttons in the title bar while a session runs.
    pub(super) fn render_debug_toolbar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let session = self.debug.session.as_ref()?;
        let t = cx.theme().clone();
        let paused = matches!(session.phase, Phase::Stopped(_));
        let running = session.phase == Phase::Running;
        let first: ToolButton = match paused {
            true => (
                "debug-continue",
                "debug/continue.svg",
                "Continue  F5",
                true,
                Box::new(actions::StartDebugging),
                t.color.success,
            ),
            false => (
                "debug-pause",
                "debug/pause.svg",
                "Pause  F6",
                running,
                Box::new(actions::PauseDebugging),
                t.color.content,
            ),
        };
        let buttons: Vec<ToolButton> = vec![
            first,
            (
                "debug-step-over",
                "debug/step-over.svg",
                "Step Over  F10",
                paused,
                Box::new(actions::StepOver),
                t.color.accent,
            ),
            (
                "debug-step-into",
                "debug/step-into.svg",
                "Step Into  F11",
                paused,
                Box::new(actions::StepInto),
                t.color.accent,
            ),
            (
                "debug-step-out",
                "debug/step-out.svg",
                "Step Out  ⇧F11",
                paused,
                Box::new(actions::StepOut),
                t.color.accent,
            ),
            (
                "debug-restart",
                "debug/restart.svg",
                "Restart  ⇧⌘F5",
                true,
                Box::new(actions::RestartDebugging),
                t.color.success,
            ),
            (
                "debug-stop",
                "debug/stop.svg",
                "Stop  ⇧F5",
                true,
                Box::new(actions::StopDebugging),
                t.color.danger,
            ),
        ];
        let buttons = buttons
            .into_iter()
            .map(|(id, icon, tip, enabled, action, color)| {
                div()
                    .id(id)
                    .w(px(26.))
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(t.shape.radius_control)
                    .when(enabled, |el| {
                        el.cursor_pointer()
                            .hover(|s| s.bg(t.color.surface_hover))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(move |_, window, cx| {
                                window.dispatch_action(action.boxed_clone(), cx)
                            })
                    })
                    .tooltip(move |_, cx| Tooltip::view(tip, cx))
                    .child(svg().path(icon).size(px(14.)).text_color(if enabled {
                        color
                    } else {
                        t.color.content_disabled
                    }))
            });
        let bar = div()
            .ml(px(16.))
            .h(px(26.))
            .px(px(3.))
            .flex()
            .items_center()
            .gap(px(1.))
            .bg(t.color.surface_sunken)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_control)
            .children(buttons);
        Some(
            motion::animate_enter(
                t.motion.reduced,
                session.opened.running(t.motion.base),
                bar,
                "debug-toolbar-open",
                Animation::new(t.motion.base).with_easing(motion::ease_enter()),
                |el, d| el.opacity(d),
            )
            .into_any_element(),
        )
    }

    fn select_thread(&mut self, thread: i64, cx: &mut Context<Self>) {
        let Some(session) = self.debug.session.as_mut() else {
            return;
        };
        if session.thread == Some(thread) || !matches!(session.phase, Phase::Stopped(_)) {
            return;
        }
        session.thread = Some(thread);
        session.frames.clear();
        session.frame = None;
        let generation = session.generation;
        let client = session.client();
        cx.spawn(async move |this, cx| {
            let frames = client.stack_trace(thread, 50).await.unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                let Some(session) = this.debug.session.as_mut() else {
                    return;
                };
                if session.generation != generation || session.thread != Some(thread) {
                    return;
                }
                session.frame = frames
                    .iter()
                    .find(|f| f.path.as_ref().is_some_and(|p: &PathBuf| p.exists()))
                    .or(frames.first())
                    .map(|f| f.id);
                session.frames = frames;
                this.select_frame(None, true, cx);
            });
        })
        .detach();
        cx.notify();
    }
}

/// A title bar debug button: its id, icon, tooltip, whether it applies now, action and colour.
type ToolButton = (
    &'static str,
    &'static str,
    &'static str,
    bool,
    Box<dyn Action>,
    Hsla,
);

fn row_button(id: &'static str, label: &'static str, t: &Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px(px(6.))
        .rounded(t.shape.radius_control)
        .cursor_pointer()
        .font_weight(FontWeight::NORMAL)
        .text_color(t.color.content_muted)
        .hover(|s| s.bg(t.color.surface_active).text_color(t.color.content))
        .child(label)
}
