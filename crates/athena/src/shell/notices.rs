use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use athena_proto::{ClientMsg, Notice, NoticeKind, PaneId as Session, ServerMsg};
use athena_term::ClaudeState;
use athena_ui::motion::{self, Closing};
use athena_ui::{ActiveTheme, IconButton, Size, Space};
use athena_workspace::{ItemId, ItemKind, Panel, Project, Rect};
use gpui::{
    Animation, AnyElement, App, Bounds, ClickEvent, Context, Corner, FocusHandle, FontWeight, Hsla,
    KeyDownEvent, MouseDownEvent, Pixels, Point, Task, Window, anchored, canvas, deferred, div,
    point, prelude::*, px, svg,
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

/// The Notifications list every window shows, and the ids toasts and banners share.
#[derive(Default)]
pub(super) struct Log {
    pub list: Vec<Notification>,
    next: u64,
}

impl Log {
    /// Loads saved notifications; a missing or unreadable file starts empty.
    pub fn load(path: &Path) -> Self {
        let list: Vec<Notification> = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let next = list.iter().map(|n| n.id).max().unwrap_or(0);
        Self { list, next }
    }

    pub fn next_id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn push(&mut self, notification: Notification) {
        self.list.push(notification);
        if self.list.len() > KEEP {
            self.list.remove(0);
        }
    }

    /// The project a notification is about; `None` once it has left the list.
    pub fn project(&self, id: u64) -> Option<Option<PathBuf>> {
        self.list
            .iter()
            .find(|n| n.id == id)
            .map(|n| n.project.clone())
    }

    /// Marks every notification read; false when none was unread.
    fn read_all(&mut self) -> bool {
        let unread = self.list.iter().any(|n| !n.read);
        self.list.iter_mut().for_each(|n| n.read = true);
        unread
    }

    /// Marks the session's unread notifications read and returns their ids.
    fn resolve(&mut self, session: Session) -> Vec<u64> {
        self.list
            .iter_mut()
            .filter(|n| !n.read && n.session == Some(session))
            .map(|n| {
                n.read = true;
                n.id
            })
            .collect()
    }
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

/// Listens for the daemon's notices for as long as Athena runs, handing each to its window.
pub(super) fn subscribe(cx: &mut App) -> Task<()> {
    cx.spawn(async move |cx| {
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
                        && cx
                            .update(|cx| super::windows::route_notice(notice, cx))
                            .is_err()
                    {
                        return;
                    }
                }
            }
            cx.background_executor().timer(RECONNECT_AFTER).await;
        }
    })
}

/// The notifications popover under the bell.
pub(super) struct BellState {
    open: bool,
    /// Receives Escape while the popover is open.
    focus: FocusHandle,
    restore: Option<FocusHandle>,
    dismissed_by: Option<Point<Pixels>>,
}

impl BellState {
    pub(super) fn new(cx: &mut App) -> Self {
        Self {
            open: false,
            focus: cx.focus_handle(),
            restore: None,
            dismissed_by: None,
        }
    }
}

/// Whether `click` is the press that just dismissed a popover; it must not toggle it back open.
/// The recorded press is spent either way.
pub(super) fn is_dismissing_click(
    click: &ClickEvent,
    dismissed_by: &mut Option<Point<Pixels>>,
) -> bool {
    let pressed = match click {
        ClickEvent::Mouse(m) => Some(m.down.position),
        ClickEvent::Keyboard(_) => None,
    };
    pressed.is_some() && pressed == dismissed_by.take()
}

/// How many of the given Claude states are waiting for an answer.
fn needing_input(states: impl IntoIterator<Item = Option<ClaudeState>>) -> usize {
    states
        .into_iter()
        .filter(|s| matches!(s, Some(ClaudeState::NeedsInput)))
        .count()
}

impl Shell {
    /// Claude terminals, across this window's projects, waiting for an answer.
    pub(super) fn claude_attention_count(&self) -> usize {
        needing_input(self.items.values().filter_map(|view| match view {
            ItemView::Terminal(t) => Some(self.claude_marks.get(&t.entity_id())?.state),
            _ => None,
        }))
    }

    pub(super) fn render_attention_chip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let count = self.claude_attention_count();
        if count == 0 {
            return None;
        }
        let t = cx.theme();
        Some(
            div()
                .ml(t.space(Space::Lg))
                .child(
                    IconButton::new("attention-chip")
                        .icon(
                            div()
                                .text_color(t.color.accent)
                                .font_weight(FontWeight::MEDIUM)
                                .child(count.to_string()),
                        )
                        .label("need you")
                        .tooltip("Next Claude session needing you  ⌥⌘J")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.next_claude_attention(window, cx)
                        })),
                )
                .into_any_element(),
        )
    }

    pub(super) fn render_bell(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div().mr(t.space(Space::Md)).child(
            IconButton::new("bell")
                .icon(
                    svg()
                        .path("ui/bell.svg")
                        .size(t.space(Space::Xl))
                        .text_color(if self.bell.open {
                            t.color.content
                        } else {
                            t.color.content_muted
                        }),
                )
                .badge(self.unread())
                .active(self.bell.open)
                .tooltip("Notifications")
                .on_click(
                    cx.listener(|this, click, window, cx| this.toggle_bell(click, window, cx)),
                ),
        )
    }

    fn toggle_bell(&mut self, click: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        if is_dismissing_click(click, &mut self.bell.dismissed_by) {
            return;
        }
        if self.bell.open {
            return self.close_bell(window, cx);
        }
        self.bell.open = true;
        self.bell.restore = window.focused(cx);
        window.focus(&self.bell.focus);
        self.mark_all_read(cx);
        cx.notify();
    }

    fn close_bell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.bell.open {
            return;
        }
        self.bell.open = false;
        if let Some(handle) = self.bell.restore.take() {
            window.focus(&handle);
        }
        cx.notify();
    }

    pub(super) fn render_bell_popover(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.bell.open {
            return None;
        }
        let t = cx.theme().clone();
        let under_bell = point(
            window.viewport_size().width - t.space(Space::Md),
            t.size(Size::TitleBar) + t.space(Space::Xxs),
        );
        let panel = div()
            .id("bell-popover")
            .track_focus(&self.bell.focus)
            .occlude()
            .w(t.ui(380.))
            .h(t.ui(320.))
            .overflow_hidden()
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_panel)
            .shadow(vec![t.popover_shadow()])
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.close_bell(window, cx);
                }
            }))
            // Capture phase and no stop_propagation: the click that dismisses still reaches its target.
            .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, window, cx| {
                this.bell.dismissed_by = Some(event.position);
                this.close_bell(window, cx);
            }))
            .child(self.render_notifications(cx));
        Some(
            deferred(
                anchored()
                    .position(under_bell)
                    .anchor(Corner::TopRight)
                    .snap_to_window_with_margin(t.space(Space::Md))
                    .child(panel),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    /// Project and tab showing a daemon session, if any project holds it.
    pub(super) fn find_session(&self, session: Session) -> Option<(PathBuf, ItemId)> {
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

    pub(super) fn on_notice(
        &mut self,
        notice: Notice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = notice.pane.and_then(|s| self.find_session(s));
        let view = target
            .as_ref()
            .and_then(|(r, i)| self.items.get(&(r.clone(), *i)).cloned());
        if let Some(ItemView::Terminal(view)) = &view {
            let state = match notice.kind {
                NoticeKind::ClaudeRunning => Some(ClaudeState::Running),
                NoticeKind::ClaudeStopped => Some(ClaudeState::Finished),
                NoticeKind::ClaudeNeedsInput { .. } => Some(ClaudeState::NeedsInput),
                _ => None,
            };
            if let Some(state) = state
                && !view.update(cx, |v, cx| v.set_claude_hook(state, cx))
            {
                return;
            }
        }
        if notice.kind == NoticeKind::ClaudeRunning {
            return;
        }

        let watching = self.is_watching(&target, window);
        let notification = Notification {
            id: self.notices.borrow_mut().next_id(),
            project: target.as_ref().map(|(r, _)| r.clone()),
            session: target.as_ref().and(notice.pane),
            kind: notice.kind,
            at: notice.at,
            read: watching
                || self.bell.open
                || self.drawer == Some(super::drawer::DrawerTab::Notifications),
        };
        if !watching {
            if let Some(ItemView::Terminal(view)) = &view {
                view.update(cx, |v, cx| v.mark_attention(cx));
            }
            if crate::system_notify::available() {
                let (title, body) = describe(
                    &notification.kind,
                    self.project_name(&notification.project).as_deref(),
                );
                crate::system_notify::post(notification.id, &title, &body);
            } else {
                self.show_toast(notification.id, None, TOAST_FOR, cx);
            }
        }
        self.notices.borrow_mut().push(notification);
        self.notices_changed(cx);
    }

    fn project_name(&self, root: &Option<PathBuf>) -> Option<String> {
        root.as_ref()
            .and_then(|r| r.file_name())
            .map(|n| n.to_string_lossy().into_owned())
    }

    /// A notice raised by Athena itself (not a terminal), shown as a toast and kept in the list.
    pub(super) fn local_notice(&mut self, kind: NoticeKind, cx: &mut Context<Self>) {
        let id = self.notices.borrow_mut().next_id();
        self.notices.borrow_mut().push(Notification {
            id,
            project: None,
            session: None,
            kind,
            at: now_ms(),
            read: true,
        });
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
        let id = self.notices.borrow_mut().next_id();
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
        let id = self.notices.borrow_mut().next_id();
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
        let id = self.notices.borrow_mut().next_id();
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
        self.notices
            .borrow()
            .list
            .iter()
            .filter(|n| !n.read)
            .count()
    }

    pub(super) fn notices_changed(&mut self, cx: &mut Context<Self>) {
        crate::system_notify::set_badge(self.unread());
        self.schedule_save(cx);
        // Every window shows the same list and count.
        cx.refresh_windows();
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
        self.close_bell(window, cx);
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
        let found = self
            .notices
            .borrow_mut()
            .list
            .iter_mut()
            .find(|n| n.id == id)
            .map(|n| {
                n.read = true;
                (n.project.clone(), n.session)
            });
        let Some((project, session)) = found else {
            cx.notify();
            return;
        };
        match session.and_then(|s| self.find_session(s)) {
            Some((root, item)) => self.focus_item(&root, item, window, cx),
            None => {
                let index = project
                    .as_ref()
                    .and_then(|r| self.workspace.projects.iter().position(|p| p.root == *r));
                match index {
                    Some(index) => self.switch_to(index, cx),
                    None if project.is_some() => {
                        super::windows::open_notification_elsewhere(id, window.window_handle(), cx)
                    }
                    None => {}
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

    /// ⌥⌘J: the next Claude session waiting for an answer, else the next that has finished.
    pub(super) fn next_claude_attention(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut waiting: Vec<(u8, usize, ItemId, PathBuf)> = Vec::new();
        for ((root, id), view) in &self.items {
            let ItemView::Terminal(t) = view else {
                continue;
            };
            let urgency = match self.claude_marks.get(&t.entity_id()).and_then(|m| m.state) {
                Some(ClaudeState::NeedsInput) => 2,
                Some(ClaudeState::Finished) => 1,
                _ => continue,
            };
            if let Some(index) = self.workspace.projects.iter().position(|p| p.root == *root) {
                waiting.push((urgency, index, *id, root.clone()));
            }
        }
        let Some(top) = waiting.iter().map(|w| w.0).max() else {
            return self.transient_notice(
                "No Claude session needs you",
                "None is waiting for an answer or has finished.",
                cx,
            );
        };
        waiting.retain(|w| w.0 == top);
        waiting.sort_by_key(|w| (w.1, w.2));
        let here = self.workspace.active.map(|index| {
            let focused = self.workspace.projects[index]
                .layout
                .as_ref()
                .and_then(|l| l.focused_pane()?.active_item())
                .map(|i| i.id);
            (index, focused)
        });
        let next = waiting
            .iter()
            .find(|w| here.is_none_or(|(index, focused)| (w.1, Some(w.2)) > (index, focused)))
            .unwrap_or(&waiting[0]);
        let (root, id) = (next.3.clone(), next.2);
        self.focus_item(&root, id, window, cx);
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
        let log = self.notices.borrow();
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
                None => log.list.iter().find(|n| n.id == toast.id).map(|n| {
                    let (title, body) = describe(&n.kind, self.project_name(&n.project).as_deref());
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
                            .min_w_0()
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
                    )
                    .child(
                        div()
                            .id(("toast-close", id))
                            .flex_none()
                            .m(t.ui(8.))
                            .size(px(16.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(t.shape.radius_control)
                            .text_color(t.color.content_muted)
                            .hover(|s| s.bg(t.color.surface_active).text_color(t.color.content))
                            .tooltip(|_, cx| athena_ui::Tooltip::view("Dismiss", cx))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.dismiss_toast(id, cx);
                            }))
                            .child("×"),
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
        let log = self.notices.borrow();
        let rows: Vec<AnyElement> = log
            .list
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
        drop(log);
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
        self.notices.borrow_mut().list.clear();
        self.notices_changed(cx);
    }

    pub(super) fn mark_all_read(&mut self, cx: &mut Context<Self>) {
        if self.notices.borrow_mut().read_all() {
            self.notices_changed(cx);
        }
    }

    /// Marks a session's notifications read once its tab is looked at, and takes down their banners.
    pub(super) fn resolve_notices(&mut self, session: Session, cx: &mut Context<Self>) {
        let ids = self.notices.borrow_mut().resolve(session);
        if !ids.is_empty() {
            crate::system_notify::withdraw(&ids);
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
    fn only_sessions_waiting_for_an_answer_need_you() {
        let states = [
            Some(ClaudeState::NeedsInput),
            Some(ClaudeState::Running),
            Some(ClaudeState::Finished),
            None,
            Some(ClaudeState::NeedsInput),
        ];
        assert_eq!(needing_input(states), 2);
        assert_eq!(needing_input([]), 0);
    }

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
    fn looking_at_a_session_resolves_only_its_unread_notifications() {
        let mut log = Log::default();
        for (session, read) in [
            (Some(7), false),
            (Some(8), false),
            (Some(7), true),
            (None, false),
        ] {
            let id = log.next_id();
            log.push(Notification {
                id,
                project: Some("/p".into()),
                session,
                kind: NoticeKind::ClaudeStopped,
                at: 0,
                read,
            });
        }
        assert_eq!(log.resolve(7), vec![1]);
        assert!(log.resolve(7).is_empty());
        let unread: Vec<u64> = log.list.iter().filter(|n| !n.read).map(|n| n.id).collect();
        assert_eq!(unread, vec![2, 4]);
    }

    #[test]
    fn opening_the_list_marks_every_notification_read() {
        let mut log = Log::default();
        for read in [false, true, false] {
            let id = log.next_id();
            log.push(Notification {
                id,
                project: None,
                session: None,
                kind: NoticeKind::ClaudeStopped,
                at: 0,
                read,
            });
        }
        assert!(log.read_all());
        assert!(log.list.iter().all(|n| n.read));
        assert!(!log.read_all(), "nothing left to change");
    }

    fn click_at(x: f32, y: f32) -> ClickEvent {
        ClickEvent::Mouse(gpui::MouseClickEvent {
            down: MouseDownEvent {
                position: point(px(x), px(y)),
                ..Default::default()
            },
            up: Default::default(),
        })
    }

    #[test]
    fn a_click_whose_press_dismissed_the_popover_is_ignored_once() {
        let mut dismissed = Some(point(px(10.), px(20.)));
        assert!(!is_dismissing_click(&click_at(11., 20.), &mut dismissed));
        assert_eq!(
            dismissed, None,
            "a recorded press only matters to the next click"
        );

        let mut dismissed = Some(point(px(10.), px(20.)));
        assert!(is_dismissing_click(&click_at(10., 20.), &mut dismissed));
        assert!(!is_dismissing_click(&click_at(10., 20.), &mut dismissed));
        assert!(!is_dismissing_click(&click_at(10., 20.), &mut None));
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
