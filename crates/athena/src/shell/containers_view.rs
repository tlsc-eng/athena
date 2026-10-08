use std::collections::HashMap;

use athena_containers::{Container, Handle, Stats, Update};
use athena_ui::{ActiveTheme, ButtonKind};
use gpui::{
    AnyElement, Context, FontWeight, ListSizingBehavior, ScrollStrategy, Task,
    UniformListScrollHandle, div, prelude::*, px, uniform_list,
};

use super::Shell;

const MAX_LOG_LINES: usize = 2000;

#[derive(Default)]
pub(super) struct ContainersState {
    handle: Option<Handle>,
    list: Vec<Container>,
    stats: HashMap<String, Stats>,
    unreachable: Option<String>,
    loaded: bool,
    /// Container whose logs are open, and what has arrived so far.
    logs_for: Option<String>,
    logs: Vec<String>,
    logs_scroll: UniformListScrollHandle,
    _updates: Option<Task<()>>,
}

fn megabytes(bytes: u64) -> String {
    let mb = bytes as f64 / (1024. * 1024.);
    if mb >= 1024. {
        format!("{:.1} GB", mb / 1024.)
    } else {
        format!("{mb:.0} MB")
    }
}

impl Shell {
    /// Starts watching the engine the first time the tab is shown; polls stats only while visible.
    pub(super) fn containers_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if visible && self.containers.handle.is_none() {
            let (tx, rx) = async_channel::unbounded();
            self.containers.handle = Some(athena_containers::start(tx));
            self.containers._updates = Some(cx.spawn(async move |this, cx| {
                while let Ok(update) = rx.recv().await {
                    if this
                        .update(cx, |this, cx| this.container_update(update, cx))
                        .is_err()
                    {
                        return;
                    }
                }
            }));
        }
        self.sync_container_watch(visible);
    }

    fn sync_container_watch(&mut self, visible: bool) {
        let Some(handle) = &self.containers.handle else {
            return;
        };
        let running = if visible {
            self.containers
                .list
                .iter()
                .filter(|c| c.state == "running")
                .map(|c| c.id.clone())
                .collect()
        } else {
            Vec::new()
        };
        handle.watch_stats(running);
        handle.follow_logs(if visible {
            self.containers.logs_for.clone()
        } else {
            None
        });
    }

    fn container_update(&mut self, update: Update, cx: &mut Context<Self>) {
        let state = &mut self.containers;
        match update {
            Update::Unreachable(tried) => {
                state.unreachable = Some(tried);
                state.list.clear();
                state.loaded = true;
            }
            Update::Containers(list) => {
                state.unreachable = None;
                state.list = list;
                state.loaded = true;
                let visible = self.drawer == Some(super::drawer::DrawerTab::Containers);
                self.sync_container_watch(visible);
            }
            Update::Stats(stats) => state.stats = stats,
            Update::Logs { id, lines } => {
                if state.logs_for.as_deref() == Some(id.as_str()) {
                    state.logs.extend(lines);
                    let overflow = state.logs.len().saturating_sub(MAX_LOG_LINES);
                    state.logs.drain(..overflow);
                    state
                        .logs_scroll
                        .scroll_to_item(state.logs.len().saturating_sub(1), ScrollStrategy::Bottom);
                }
            }
        }
        cx.notify();
    }

    fn open_logs(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        self.containers.logs_for = id.clone();
        self.containers.logs.clear();
        if let Some(handle) = &self.containers.handle {
            handle.follow_logs(id);
        }
        cx.notify();
    }

    pub(super) fn render_containers_action(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.containers.logs_for.as_ref()?;
        Some(
            athena_ui::Button::new("logs-back", "All containers", ButtonKind::Ghost)
                .on_click(cx.listener(|this, _, _, cx| this.open_logs(None, cx)))
                .into_any_element(),
        )
    }

    pub(super) fn render_containers(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let state = &self.containers;
        let message = |text: String| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(t.typography.caption)
                .text_color(t.color.content_muted)
                .child(text)
                .into_any_element()
        };
        if let Some(id) = &state.logs_for {
            let lines = state.logs.clone();
            let name = state
                .list
                .iter()
                .find(|c| &c.id == id)
                .map_or(id[..12.min(id.len())].to_string(), |c| c.name.clone());
            if lines.is_empty() {
                return message(format!("Waiting for output from {name}…"));
            }
            let mono = t.typography.mono.clone();
            let color = t.color.content_secondary;
            return uniform_list("container-logs", lines.len(), move |range, _, _| {
                range
                    .map(|i| {
                        div()
                            .px(px(12.))
                            .h(px(18.))
                            .whitespace_nowrap()
                            .child(lines[i].clone())
                    })
                    .collect::<Vec<_>>()
            })
            .with_sizing_behavior(ListSizingBehavior::Auto)
            .track_scroll(state.logs_scroll.clone())
            .font_family(mono)
            .text_size(t.typography.caption)
            .text_color(color)
            .size_full()
            .into_any_element();
        }
        if let Some(tried) = &state.unreachable {
            return message(format!(
                "No container engine is running (checked {tried}). Start Rancher Desktop and this list fills in."
            ));
        }
        if !state.loaded {
            return message("Looking for a container engine…".into());
        }
        if state.list.is_empty() {
            return message(
                "No containers. Start some with docker compose up and they show here.".into(),
            );
        }

        // The active project's compose stack first: Compose names projects after their folder.
        let current = self.workspace.active_project().map(|p| p.name());
        let mut groups: Vec<(String, Vec<&Container>)> = Vec::new();
        for c in &state.list {
            let group = c.project.clone().unwrap_or_else(|| "Other".into());
            match groups.iter_mut().find(|(g, _)| *g == group) {
                Some((_, items)) => items.push(c),
                None => groups.push((group, vec![c])),
            }
        }
        groups.sort_by_key(|(g, _)| (Some(g) != current.as_ref(), g == "Other", g.clone()));

        let mut rows: Vec<AnyElement> = Vec::new();
        for (group, items) in groups {
            rows.push(
                div()
                    .h(px(28.))
                    .px(px(12.))
                    .flex()
                    .items_end()
                    .pb(px(4.))
                    .text_size(t.typography.caption)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if Some(&group) == current.as_ref() {
                        t.color.accent
                    } else {
                        t.color.content_muted
                    })
                    .child(group)
                    .into_any_element(),
            );
            for c in items {
                let color = match c.state.as_str() {
                    "running" => t.color.success,
                    "restarting" | "paused" => t.color.warning,
                    "dead" => t.color.danger,
                    _ => t.color.content_disabled,
                };
                let stats = state.stats.get(&c.id).filter(|_| c.state == "running");
                let id = c.id.clone();
                rows.push(
                    div()
                        .id(gpui::ElementId::Name(c.id.clone().into()))
                        .h(px(28.))
                        .px(px(12.))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .cursor_pointer()
                        .hover(|s| s.bg(t.color.surface_hover))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.open_logs(Some(id.clone()), cx)),
                        )
                        .child(div().size(px(6.)).flex_none().bg(color))
                        .child(
                            div()
                                .w(px(200.))
                                .flex_none()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(c.service.clone().unwrap_or(c.name.clone())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .font_family(t.typography.mono.clone())
                                .text_size(t.typography.caption)
                                .text_color(t.color.content_muted)
                                .child(c.image.clone()),
                        )
                        .child(
                            div()
                                .w(px(170.))
                                .flex_none()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .font_family(t.typography.mono.clone())
                                .text_size(t.typography.caption)
                                .text_color(t.color.content_secondary)
                                .child(c.ports.join(" ")),
                        )
                        .child(
                            div()
                                .w(px(140.))
                                .flex_none()
                                .text_size(t.typography.caption)
                                .text_color(t.color.content_muted)
                                .child(match stats {
                                    Some(s) => format!(
                                        "{:.1}% · {}",
                                        s.cpu_percent,
                                        megabytes(s.memory_bytes)
                                    ),
                                    None => c.status.clone(),
                                }),
                        )
                        .into_any_element(),
                );
            }
        }
        div()
            .id("containers")
            .size_full()
            .overflow_y_scroll()
            .text_size(t.typography.body)
            .children(rows)
            .into_any_element()
    }
}
