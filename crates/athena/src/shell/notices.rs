use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use athena_proto::{ClientMsg, Notice, NoticeKind, PaneId as Session, ServerMsg};
use athena_term::ClaudeState;
use athena_ui::ActiveTheme;
use athena_ui::motion::{self, Closing};
use athena_workspace::{ItemId, ItemKind, Panel, Project, Rect};
use gpui::{
    Animation, AnyElement, Bounds, Context, FontWeight, Hsla, Pixels, Task, Window, canvas, div,
    prelude::*, px,
};
use serde::{Deserialize, Serialize};

use super::Shell;
use super::item::ItemView;

const KEEP: usize = 200;
const TOAST_FOR: Duration = Duration::from_secs(5);
/// Recovered files are announced at launch, when the user may not be looking yet.
const RECOVERY_TOAST_FOR: Duration = Duration::from_secs(30);
/// A toast that offers an action stays long enough to be noticed from another pane.
const ACTION_TOAST_FOR: Duration = Duration::from_secs(15);
const MAX_TOASTS: usize = 3;
const RECONNECT_AFTER: Duration = Duration::from_secs(2);

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(super) struct Notification {
    id: u64,
    project: Option<PathBuf>,
    /// Kept rather than the tab's id, which changes when the terminal moves to or from the panel.
    #[serde(default)]
    session: Option<Session>,
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
    /// Title and body of a toast that is not kept in the Notifications list.
    transient: Option<(String, String)>,
    /// Files a click opens, for a toast that is about files rather than a notification.
    open: Vec<PathBuf>,
    action: Option<ToastAction>,
    closing: Option<Closing>,
    _dismiss: Task<()>,
}

type ShellAction = dyn Fn(&mut Shell, &mut Window, &mut Context<Shell>);

/// What a toast offers to do, shown as a link on it and run when it is clicked.
#[derive(Clone)]
pub(super) struct ToastAction {
    pub label: &'static str,
    pub run: std::rc::Rc<ShellAction>,
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

/// Whether toasts drawn over `toasts` would land on a pane at `pane`, a web preview's or not.
pub(super) fn covers(toasts: Bounds<Pixels>, pane: Rect) -> bool {
    let (left, top) = (f32::from(toasts.left()), f32::from(toasts.top()));
    let (right, bottom) = (f32::from(toasts.right()), f32::from(toasts.bottom()));
    left < pane.x + pane.w && pane.x < right && top < pane.y + pane.h && pane.y < bottom
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

    /// Project and tab showing a daemon session, if any project holds it.
    fn find_session(&self, session: Session) -> Option<(PathBuf, ItemId)> {
        find_session(&self.workspace.projects, session)
    }

    /// True when the user is looking at that tab right now.
    fn is_watching(&self, target: &Option<(PathBuf, ItemId)>, window: &Window) -> bool {
        let Some((root, item)) = target else {
            return false;
        };
        if Panel::holds(*item) {
            return window.is_window_active()
                && self.drawer == Some(super::drawer::DrawerTab::Terminal)
                && self.workspace.active_project().is_some_and(|p| {
                    &p.root == root && p.panel.active_item().is_some_and(|i| i.id == *item)
                });
        }
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
            session: target.as_ref().and(notice.pane),
            kind: notice.kind,
            at: notice.at,
            read: watching || self.drawer == Some(super::drawer::DrawerTab::Notifications),
        };
        if !watching {
            if let Some(ItemView::Terminal(view)) = &view {
                view.update(cx, |v, cx| v.mark_attention(cx));
            }
            if window.is_window_active() {
                self.show_toast(notification.id, None, TOAST_FOR, cx);
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
    pub(super) fn local_notice(&mut self, kind: NoticeKind, cx: &mut Context<Self>) {
        self.next_notice += 1;
        let id = self.next_notice;
        self.notifications.push(Notification {
            id,
            project: None,
            session: None,
            kind,
            at: now_ms(),
            read: true,
        });
        if self.notifications.len() > KEEP {
            self.notifications.remove(0);
        }
        self.show_toast(id, None, TOAST_FOR, cx);
        self.notices_changed(cx);
    }

    /// Offers the copies of unsaved files an earlier quit or crash kept.
    pub(super) fn announce_recovery(&mut self, cx: &mut Context<Self>) {
        let Ok(dir) = athena_proto::recovery_dir() else {
            return;
        };
        let files = athena_editor::recovery::take_unannounced(&dir);
        if files.is_empty() {
            return;
        }
        tracing::info!(
            "offering {} recovered files from {}",
            files.len(),
            dir.display()
        );
        let title = match files.len() {
            1 => "Recovered unsaved changes in 1 file".to_string(),
            n => format!("Recovered unsaved changes in {n} files"),
        };
        let body = format!("Click to open them. Copies stay in {}", dir.display());
        self.next_notice += 1;
        let id = self.next_notice;
        self.show_toast(id, Some((title, body)), RECOVERY_TOAST_FOR, cx);
        if let Some(toast) = self.toasts.iter_mut().find(|t| t.id == id) {
            toast.open = files;
        }
        cx.notify();
    }

    /// A toast for something only worth seeing now (a lookup that found nothing), not kept in the list.
    pub(super) fn transient_notice(
        &mut self,
        title: impl Into<String>,
        body: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        self.next_notice += 1;
        let id = self.next_notice;
        self.show_toast(id, Some((title.into(), body.into())), TOAST_FOR, cx);
        cx.notify();
    }

    /// A transient toast with an action; returns its id so a newer one can replace it.
    pub(super) fn action_toast(
        &mut self,
        title: impl Into<String>,
        body: impl Into<String>,
        action: ToastAction,
        cx: &mut Context<Self>,
    ) -> u64 {
        self.next_notice += 1;
        let id = self.next_notice;
        self.show_toast(id, Some((title.into(), body.into())), ACTION_TOAST_FOR, cx);
        if let Some(toast) = self.toasts.iter_mut().find(|t| t.id == id) {
            toast.action = Some(action);
        }
        cx.notify();
        id
    }

    pub(super) fn dismiss_toast(&mut self, id: u64, cx: &mut Context<Self>) {
        self.toasts.retain(|t| t.id != id);
        cx.notify();
    }

    pub(super) fn unread(&self) -> usize {
        self.notifications.iter().filter(|n| !n.read).count()
    }

    pub(super) fn notices_changed(&mut self, cx: &mut Context<Self>) {
        crate::system_notify::set_badge(self.unread());
        self.schedule_save(cx);
        cx.notify();
    }

    fn show_toast(
        &mut self,
        id: u64,
        transient: Option<(String, String)>,
        lasts: Duration,
        cx: &mut Context<Self>,
    ) {
        let dismiss = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(lasts).await;
            let Ok(delay) = this.update(cx, |this, cx| {
                if let Some(toast) = this.toasts.iter_mut().find(|t| t.id == id) {
                    toast.closing = Some(Closing::new(id));
                }
                cx.notify();
                let t = cx.theme();
                motion::exit_delay(t.motion.reduced, t.motion.fast)
            }) else {
                return;
            };
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                this.toasts.retain(|t| t.id != id);
                cx.notify();
            });
        });
        self.toasts.push(Toast {
            id,
            transient,
            open: Vec::new(),
            action: None,
            closing: None,
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
        let (files, action) = self
            .toasts
            .iter_mut()
            .find(|t| t.id == id)
            .map(|t| (std::mem::take(&mut t.open), t.action.take()))
            .unwrap_or_default();
        self.toasts.retain(|t| t.id != id);
        if let Some(action) = action {
            (action.run)(self, window, cx);
            return cx.notify();
        }
        if !files.is_empty() {
            return self.open_recovered(files, window, cx);
        }
        let Some(n) = self.notifications.iter_mut().find(|n| n.id == id) else {
            cx.notify();
            return;
        };
        n.read = true;
        let (project, session) = (n.project.clone(), n.session);
        match session.and_then(|s| self.find_session(s)) {
            Some((root, item)) => self.focus_item(&root, item, window, cx),
            None => {
                let index =
                    project.and_then(|r| self.workspace.projects.iter().position(|p| p.root == r));
                if let Some(index) = index {
                    self.switch_to(index, cx);
                }
            }
        }
        self.notices_changed(cx);
    }

    /// Opens recovered copies as tabs, or shows their folder when no project is open to hold them.
    fn open_recovered(&mut self, files: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace.active.is_none() {
            if let Ok(dir) = athena_proto::recovery_dir() {
                let _ = std::process::Command::new("open").arg(dir).spawn();
            }
            return cx.notify();
        }
        for file in files {
            self.open_file(file, window, cx);
        }
        cx.notify();
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
        if Panel::holds(item) {
            self.focus_pending = false;
            return self.activate_panel_terminal(item, window, cx);
        }
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
            .filter_map(|toast| match &toast.transient {
                Some((title, body)) => Some((
                    toast.id,
                    toast.closing,
                    title.clone(),
                    body.clone(),
                    t.color.content_muted,
                    toast.action.as_ref().map(|a| a.label),
                )),
                None => self
                    .notifications
                    .iter()
                    .find(|n| n.id == toast.id)
                    .map(|n| {
                        let (title, body) =
                            describe(&n.kind, self.project_name(&n.project).as_deref());
                        (
                            n.id,
                            toast.closing,
                            title,
                            body,
                            self.kind_color(&n.kind, cx),
                            None,
                        )
                    }),
            })
            .map(|(id, closing, title, body, color, action)| {
                let card = div()
                    .id(("toast", id))
                    .w(t.ui(360.))
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
                    .child(div().w(px(2.)).flex_none().bg(color))
                    .child(
                        div()
                            .flex_1()
                            .p(t.ui(12.))
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
                            )
                            .children(action.map(|label| {
                                div()
                                    .pt(px(4.))
                                    .text_size(t.typography.caption)
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(t.color.accent)
                                    .child(label)
                            })),
                    );
                match closing {
                    Some(_) => motion::animate_exit(
                        t.motion.reduced,
                        card,
                        ("toast-out", id),
                        t.motion.fast,
                        |el, d| el.opacity(1. - d).top(px(-2. * d)),
                    ),
                    None => motion::animate_if(
                        t.motion.reduced,
                        card,
                        ("toast-in", id),
                        Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
                        |el, d| el.opacity(d).top(px(2. * (1. - d))),
                    ),
                }
            })
            .collect();
        let area = self.toast_area.clone();
        let shell = cx.entity().downgrade();
        // Web previews are native views above gpui, so they learn where the toasts are drawn.
        let recorder = canvas(
            move |bounds, _, cx| {
                if area.replace(Some(bounds)) != Some(bounds) {
                    shell.update(cx, |_, cx| cx.notify()).ok();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        Some(
            div()
                .absolute()
                .right(px(16.))
                .bottom(px(16.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(recorder)
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

/// Project and tab showing a daemon session, if any project holds it.
fn find_session(projects: &[Project], session: Session) -> Option<(PathBuf, ItemId)> {
    let kind = ItemKind::Terminal {
        session: Some(session),
    };
    projects.iter().find_map(|p| {
        let item = p.items().find(|i| i.kind == kind)?;
        Some((p.root.clone(), item.id))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use athena_workspace::Item;
    use gpui::{point, size};

    #[test]
    fn a_notice_finds_its_terminal_after_it_moves_and_not_the_one_reusing_its_id() {
        let terminal = |session| Item {
            id: ItemId(0),
            kind: ItemKind::Terminal { session },
            view: None,
        };
        let mut project = Project::new("/p".into());
        let first = project.panel.adopt(terminal(Some(7)));
        let projects = std::slice::from_ref(&project);
        assert_eq!(find_session(projects, 7), Some(("/p".into(), first)));

        let moved = project.panel.take(first).unwrap();
        let layout = project.layout.as_mut().unwrap();
        let focused = layout.focused;
        let id = layout.add_item(focused, moved.kind).unwrap();
        let reused = project.panel.adopt(terminal(Some(8)));
        assert_eq!(reused, first, "the panel hands the freed id out again");
        let projects = std::slice::from_ref(&project);
        assert_eq!(find_session(projects, 7), Some(("/p".into(), id)));
    }

    #[test]
    fn notifications_saved_with_a_tab_id_still_load() {
        let json = r#"[{"id":3,"project":"/p","item":4294967296,
            "kind":"ClaudeStopped","at":1,"read":false}]"#;
        let list: Vec<Notification> = serde_json::from_str(json).unwrap();
        assert_eq!(list[0].session, None);
        assert_eq!(list[0].project.as_deref(), Some(Path::new("/p")));
    }

    fn pane(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn toasts_cover_only_the_panes_they_overlap() {
        // A 1280×800 window: toasts in the bottom-right corner, 360 wide.
        let toasts = Bounds::new(point(px(904.), px(600.)), size(px(360.), px(184.)));
        let right = pane(640., 36., 640., 700.);
        let left = pane(48., 36., 591., 700.);
        let above = pane(640., 36., 640., 564.);
        assert!(covers(toasts, right));
        assert!(!covers(toasts, left), "ends left of the toasts");
        assert!(!covers(toasts, above), "ends just where they start");
        assert!(
            covers(toasts, pane(900., 590., 10., 20.)),
            "a corner overlap counts"
        );
    }
}
