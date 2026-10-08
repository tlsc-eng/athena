use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use athena_proto::{ClientMsg, Notice, NoticeKind, PaneId as Session, ServerMsg};
use athena_term::ClaudeState;
use athena_ui::{ActiveTheme, motion};
use athena_workspace::{ItemId, ItemKind};
use gpui::{Animation, AnyElement, Context, FontWeight, Hsla, Task, Window, div, prelude::*, px};
use serde::{Deserialize, Serialize};

use super::Shell;
use super::item::ItemView;

const KEEP: usize = 200;
const TOAST_FOR: Duration = Duration::from_secs(5);
const MAX_TOASTS: usize = 3;
const RECONNECT_AFTER: Duration = Duration::from_secs(2);

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(super) struct Notification {
    id: u64,
    project: Option<PathBuf>,
    item: Option<ItemId>,
    kind: NoticeKind,
    at: u64,
    read: bool,
}

impl Notification {
    pub fn id(&self) -> u64 {
        self.id
    }
}

pub(super) struct Toast {
    id: u64,
    _dismiss: Task<()>,
}

/// Loads saved notifications; a missing or unreadable file starts empty.
pub(super) fn load(path: &Path) -> Vec<Notification> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Saves without command text, which can hold secrets typed on the command line.
pub(super) fn save(path: &Path, list: &[Notification]) -> anyhow::Result<()> {
    let scrubbed: Vec<Notification> = list
        .iter()
        .cloned()
        .map(|mut n| {
            if let NoticeKind::CommandFinished { command, .. } = &mut n.kind {
                *command = None;
            }
            n
        })
        .collect();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&scrubbed)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn ago(at: u64) -> String {
    let secs = now_ms().saturating_sub(at) / 1000;
    match secs {
        0..60 => "now".into(),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

fn duration(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m {}s", s / 60, s % 60)
    }
}

/// Title and body for a notice, as shown in toasts, the drawer and macOS banners.
pub(super) fn describe(kind: &NoticeKind, project: Option<&str>) -> (String, String) {
    let place = project.unwrap_or("Athena");
    match kind {
        NoticeKind::CommandFinished {
            exit_code,
            elapsed_ms,
            command,
        } => {
            let title = if *exit_code == 0 {
                "Command finished"
            } else {
                "Command failed"
            };
            let what = command.clone().unwrap_or_else(|| "Command".into());
            (
                title.into(),
                format!(
                    "{what} · exit {exit_code} · {} · {place}",
                    duration(*elapsed_ms)
                ),
            )
        }
        NoticeKind::ClaudeStopped => (
            "Claude finished".into(),
            format!("Ready for you in {place}"),
        ),
        NoticeKind::ClaudeNeedsInput { message } => {
            let body = if message.is_empty() {
                format!("Waiting in {place}")
            } else {
                message.clone()
            };
            ("Claude needs input".into(), body)
        }
        NoticeKind::ClaudeRunning => ("Claude is working".into(), place.into()),
        NoticeKind::Message { title, body } => (title.clone(), body.clone()),
    }
}

impl Shell {
    pub(super) fn start_notices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._notices = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let connected = cx
                    .background_executor()
                    .spawn(async move {
                        let (conn, reader) = athena_term::open_connection()?;
                        conn.send(&ClientMsg::Subscribe)?;
                        anyhow::Ok((conn, reader))
                    })
                    .await;
                if let Ok((_conn, reader)) = connected {
                    let messages = athena_term::read_messages(reader);
                    while let Ok(msg) = messages.recv().await {
                        if let ServerMsg::Notice(notice) = msg
                            && this
                                .update_in(cx, |this, window, cx| {
                                    this.on_notice(notice, window, cx)
                                })
                                .is_err()
                        {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(RECONNECT_AFTER).await;
            }
        }));
    }

    /// Project, tab and view showing a daemon session, if any project holds it.
    fn find_session(&self, session: Session) -> Option<(PathBuf, ItemId)> {
        self.workspace.projects.iter().find_map(|p| {
            let item = p.layout.as_ref()?.items().find(|i| {
                i.kind
                    == ItemKind::Terminal {
                        session: Some(session),
                    }
            })?;
            Some((p.root.clone(), item.id))
        })
    }

    /// True when the user is looking at that tab right now.
    fn is_watching(&self, target: &Option<(PathBuf, ItemId)>, window: &Window) -> bool {
        let Some((root, item)) = target else {
            return false;
        };
        window.is_window_active()
            && self.workspace.active_project().is_some_and(|p| {
                &p.root == root
                    && p.layout
                        .as_ref()
                        .and_then(|l| l.focused_pane())
                        .and_then(|pane| pane.active_item())
                        .map(|i| i.id)
                        == Some(*item)
            })
    }

    fn on_notice(&mut self, notice: Notice, window: &mut Window, cx: &mut Context<Self>) {
        let target = notice.pane.and_then(|s| self.find_session(s));
        let view = target
            .as_ref()
            .and_then(|(r, i)| self.items.get(&(r.clone(), *i)).cloned());
        if let Some(ItemView::Terminal(view)) = &view {
            let state = match notice.kind {
                NoticeKind::ClaudeRunning => Some(ClaudeState::Running),
                NoticeKind::ClaudeStopped | NoticeKind::ClaudeNeedsInput { .. } => {
                    Some(ClaudeState::Waiting)
                }
                _ => None,
            };
            if let Some(state) = state {
                view.update(cx, |v, cx| v.set_claude_hook(state, cx));
            }
        }
        if notice.kind == NoticeKind::ClaudeRunning {
            return;
        }

        let watching = self.is_watching(&target, window);
        self.next_notice += 1;
        let notification = Notification {
            id: self.next_notice,
            project: target.as_ref().map(|(r, _)| r.clone()),
            item: target.as_ref().map(|(_, i)| *i),
            kind: notice.kind,
            at: notice.at,
            read: watching || self.drawer == Some(super::drawer::DrawerTab::Notifications),
        };
        if !watching {
            if let Some(ItemView::Terminal(view)) = &view {
                view.update(cx, |v, cx| v.mark_attention(cx));
            }
            if window.is_window_active() {
                self.show_toast(notification.id, window, cx);
            } else {
                let (title, body) = describe(
                    &notification.kind,
                    self.project_name(&notification.project).as_deref(),
                );
                crate::system_notify::post(notification.id, &title, &body);
            }
        }
        self.notifications.push(notification);
        if self.notifications.len() > KEEP {
            self.notifications.remove(0);
        }
        self.notices_changed(cx);
    }

    fn project_name(&self, root: &Option<PathBuf>) -> Option<String> {
        root.as_ref()
            .and_then(|r| r.file_name())
            .map(|n| n.to_string_lossy().into_owned())
    }

    /// A notice raised by Athena itself (not a terminal), shown as a toast and kept in the list.
    pub(super) fn local_notice(
        &mut self,
        kind: NoticeKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.next_notice += 1;
        let id = self.next_notice;
        self.notifications.push(Notification {
            id,
            project: None,
            item: None,
            kind,
            at: now_ms(),
            read: true,
        });
        self.show_toast(id, window, cx);
        self.notices_changed(cx);
    }

    pub(super) fn unread(&self) -> usize {
        self.notifications.iter().filter(|n| !n.read).count()
    }

    pub(super) fn notices_changed(&mut self, cx: &mut Context<Self>) {
        crate::system_notify::set_badge(self.unread());
        self.schedule_save(cx);
        cx.notify();
    }

    fn show_toast(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let dismiss = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(TOAST_FOR).await;
            let _ = this.update(cx, |this, cx| {
                this.toasts.retain(|t| t.id != id);
                cx.notify();
            });
        });
        self.toasts.push(Toast {
            id,
            _dismiss: dismiss,
        });
        if self.toasts.len() > MAX_TOASTS {
            self.toasts.remove(0);
        }
    }

    /// Brings the notice's project and tab forward and marks it read.
    pub(super) fn open_notification(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(n) = self.notifications.iter_mut().find(|n| n.id == id) else {
            return;
        };
        n.read = true;
        let target = n.project.clone().zip(n.item);
        self.toasts.retain(|t| t.id != id);
        if let Some((root, item)) = target {
            self.focus_item(&root, item, window, cx);
        }
        self.notices_changed(cx);
    }

    fn focus_item(
        &mut self,
        root: &Path,
        item: ItemId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.workspace.projects.iter().position(|p| p.root == root) else {
            return;
        };
        self.switch_to(index, cx);
        let found = self.workspace.projects[index]
            .layout
            .as_ref()
            .and_then(|l| {
                l.panes().into_iter().find_map(|p| {
                    p.items
                        .iter()
                        .position(|i| i.id == item)
                        .map(|ix| (p.id, ix))
                })
            });
        if let Some((pane, ix)) = found {
            self.activate_tab(pane, ix, window, cx);
        }
    }

    fn kind_color(&self, kind: &NoticeKind, cx: &Context<Self>) -> Hsla {
        let c = &cx.theme().color;
        match kind {
            NoticeKind::CommandFinished { exit_code: 0, .. } => c.success,
            NoticeKind::CommandFinished { .. } => c.danger,
            NoticeKind::ClaudeNeedsInput { .. } | NoticeKind::ClaudeStopped => c.warning,
            _ => c.content_muted,
        }
    }

    pub(super) fn render_toasts(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.toasts.is_empty() {
            return None;
        }
        let t = cx.theme().clone();
        let cards: Vec<AnyElement> = self
            .toasts
            .iter()
            .filter_map(|toast| self.notifications.iter().find(|n| n.id == toast.id))
            .map(|n| {
                let (title, body) = describe(&n.kind, self.project_name(&n.project).as_deref());
                let id = n.id;
                let card = div()
                    .id(("toast", id))
                    .w(px(360.))
                    .flex()
                    .bg(t.color.surface)
                    .border_1()
                    .border_color(t.color.border)
                    .rounded(t.shape.radius_panel)
                    .shadow(vec![t.popover_shadow()])
                    .overflow_hidden()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_notification(id, window, cx)
                    }))
                    .child(div().w(px(2.)).flex_none().bg(self.kind_color(&n.kind, cx)))
                    .child(
                        div()
                            .flex_1()
                            .p(px(12.))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(t.typography.body)
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(t.typography.caption)
                                    .text_color(t.color.content_muted)
                                    .child(body),
                            ),
                    );
                motion::animate_if(
                    t.motion.reduced,
                    card,
                    ("toast-in", id),
                    Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
                    |el, d| el.opacity(d).top(px(2. * (1. - d))),
                )
            })
            .collect();
        Some(
            div()
                .absolute()
                .right(px(16.))
                .bottom(px(16.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .children(cards)
                .into_any_element(),
        )
    }

    /// The Notifications tab's list, newest first.
    pub(super) fn render_notifications(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let rows: Vec<AnyElement> = self
            .notifications
            .iter()
            .rev()
            .map(|n| {
                let (title, body) = describe(&n.kind, self.project_name(&n.project).as_deref());
                let id = n.id;
                div()
                    .id(("notice", id))
                    .h(px(32.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .cursor_pointer()
                    .hover(|s| s.bg(t.color.surface_hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_notification(id, window, cx)
                    }))
                    .child(
                        div()
                            .size(px(6.))
                            .flex_none()
                            .bg(self.kind_color(&n.kind, cx)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::MEDIUM)
                            .child(title),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_size(t.typography.caption)
                            .text_color(t.color.content_muted)
                            .child(body),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(t.typography.caption)
                            .text_color(t.color.content_disabled)
                            .child(ago(n.at)),
                    )
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        div()
            .id("notices")
            .size_full()
            .overflow_y_scroll()
            .text_size(t.typography.body)
            .children(rows)
            .when(empty, |el| {
                el.flex().items_center().justify_center().child(
                    div()
                        .text_size(t.typography.caption)
                        .text_color(t.color.content_muted)
                        .child(
                            "Finished commands and Claude sessions waiting for you show up here.",
                        ),
                )
            })
            .into_any_element()
    }

    pub(super) fn clear_notifications(&mut self, cx: &mut Context<Self>) {
        self.notifications.clear();
        self.notices_changed(cx);
    }

    pub(super) fn mark_all_read(&mut self, cx: &mut Context<Self>) {
        if self.notifications.iter().any(|n| !n.read) {
            self.notifications.iter_mut().for_each(|n| n.read = true);
            self.notices_changed(cx);
        }
    }
}
