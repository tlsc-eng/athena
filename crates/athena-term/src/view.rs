use std::ops::Range;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use anyhow::anyhow;
use athena_proto::{ClientMsg, Connection, ErrorKind, PaneId, Process, ServerMsg};
use athena_ui::{ActiveTheme, ButtonKind, empty_state};
use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Render, ScrollWheelEvent, Task, UTF16Selection, Window, actions, div,
    prelude::*, px,
};

use crate::element::TerminalElement;
use crate::keys;
use crate::links;
use crate::terminal::{GridSize, Link, PaneEvent, Terminal, Transport};

actions!(terminal, [Copy, Paste, ClearScrollback]);

const BATCH_BYTES: usize = 2 * 1024 * 1024;

/// Upper bound on one `Input` frame, well under the protocol's frame limit.
const INPUT_CHUNK: usize = 256 * 1024;

/// A daemon that dies again this soon after a reconnect is not retried automatically.
const RECONNECT_COOLDOWN: Duration = Duration::from_secs(10);

const SESSION_LOST: &[u8] = b"\x1b[2m[previous session ended; started a new shell]\x1b[0m\r\n";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-c", Copy, Some("Terminal")),
        KeyBinding::new("cmd-v", Paste, Some("Terminal")),
        KeyBinding::new("cmd-k", ClearScrollback, Some("Terminal")),
    ]);
}

pub enum TerminalEvent {
    /// The view is now bound to this daemon pane; persist it to re-attach after a relaunch.
    Attached(PaneId),
    /// Label, bell or Claude state changed; tab strips and the project rail should redraw.
    Changed,
}

/// How long a Claude Code session may stay silent before it counts as waiting for the user.
const CLAUDE_IDLE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeState {
    Running,
    Waiting,
}

struct MuxTransport {
    conn: Arc<Connection>,
    pane: PaneId,
}

impl Transport for MuxTransport {
    fn write(&self, bytes: Vec<u8>) {
        for chunk in bytes.chunks(INPUT_CHUNK) {
            let _ = self.conn.send(&ClientMsg::Input {
                pane: self.pane,
                data: chunk.to_vec(),
            });
        }
    }

    fn resize(&self, rows: u16, cols: u16) {
        let _ = self.conn.send(&ClientMsg::Resize {
            pane: self.pane,
            rows,
            cols,
        });
    }
}

/// One shell session, owned by the `athena-mux` daemon and rendered as a pane.
pub struct TerminalView {
    terminal: Option<Terminal>,
    error: Option<String>,
    cwd: PathBuf,
    pane: Option<PaneId>,
    conn: Option<Arc<Connection>>,
    session_lost: bool,
    last_reconnect: Option<Instant>,
    pub(crate) focus: FocusHandle,
    pub(crate) marked: String,
    pub(crate) cursor_bounds: Option<Bounds<Pixels>>,
    pub(crate) origin: gpui::Point<Pixels>,
    pub(crate) hovered_link: Option<Link>,
    foreground: Option<Process>,
    selecting: bool,
    claude_state: Option<ClaudeState>,
    /// What Claude Code's own hooks last reported; trusted over the output-silence guess.
    claude_hook: Option<ClaudeState>,
    /// Typed into the shell once it is ready, for tabs opened to run a command.
    pending_input: Option<Vec<u8>>,
    grid: GridSize,
    scroll_remainder: f32,
    _claude_timer: Option<Task<()>>,
    _io: Option<Task<()>>,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl TerminalView {
    /// Re-attaches to `pane` when given and still alive, otherwise starts a new shell in `cwd`.
    pub fn new(cwd: PathBuf, pane: Option<PaneId>, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            terminal: None,
            error: None,
            cwd,
            pane,
            conn: None,
            session_lost: false,
            last_reconnect: None,
            focus: cx.focus_handle(),
            marked: String::new(),
            cursor_bounds: None,
            grid: GridSize {
                cols: 80,
                rows: 24,
                cell_width: 8.,
                cell_height: 18.,
            },
            origin: gpui::Point::default(),
            hovered_link: None,
            foreground: None,
            selecting: false,
            claude_state: None,
            claude_hook: None,
            pending_input: None,
            scroll_remainder: 0.,
            _claude_timer: None,
            _io: None,
        };
        view.connect(cx);
        view
    }

    /// Tab label: the running program, or for an idle shell its title or folder.
    pub fn label(&self) -> String {
        if self.is_claude() {
            return "Claude".into();
        }
        let shell_idle = self.foreground.as_ref().is_none_or(|p| is_shell(&p.name));
        if !shell_idle {
            return self
                .foreground
                .as_ref()
                .map(|p| p.name.clone())
                .unwrap_or_default();
        }
        if let Some(title) = self
            .terminal
            .as_ref()
            .and_then(|t| t.title.clone())
            .filter(|t| !t.is_empty())
        {
            return title;
        }
        let cwd = self
            .foreground
            .as_ref()
            .and_then(|p| p.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone());
        cwd.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Terminal".into())
    }

    pub fn claude_state(&self) -> Option<ClaudeState> {
        self.claude_state
    }

    /// A bell rang since the user last looked at this terminal.
    pub fn has_bell(&self) -> bool {
        self.terminal.as_ref().is_some_and(|t| t.bell)
    }

    pub fn text_lines(&self, count: usize) -> Vec<String> {
        self.terminal
            .as_ref()
            .map(|t| t.text_lines(count))
            .unwrap_or_default()
    }

    /// The foreground program's name and working directory, as the daemon last reported them.
    pub fn foreground(&self) -> Option<(String, Option<PathBuf>)> {
        self.foreground
            .as_ref()
            .map(|p| (p.name.clone(), p.cwd.clone()))
    }

    pub fn foreground_pid(&self) -> Option<i32> {
        self.foreground.as_ref().map(|p| p.pid)
    }

    /// The daemon pane this view shows, once attached.
    pub fn session(&self) -> Option<PaneId> {
        self.pane
    }

    /// Types `text` into the shell as soon as it is attached.
    pub fn run_on_start(&mut self, text: impl Into<String>) {
        self.pending_input = Some(text.into().into_bytes());
    }

    /// Records Claude Code's state as reported by its hooks.
    pub fn set_claude_hook(&mut self, state: ClaudeState, cx: &mut Context<Self>) {
        self.claude_hook = Some(state);
        self.refresh_claude(cx);
        cx.notify();
    }

    /// Marks the tab until the user looks at it, as a bell does.
    pub fn mark_attention(&mut self, cx: &mut Context<Self>) {
        if let Some(terminal) = self.terminal.as_mut() {
            terminal.bell = true;
            cx.emit(TerminalEvent::Changed);
            cx.notify();
        }
    }

    fn is_claude(&self) -> bool {
        self.foreground.as_ref().is_some_and(|p| {
            p.name == "claude"
                || p.path.ends_with("claude")
                || p.path.to_string_lossy().contains("/claude/versions/")
        })
    }

    fn current_claude_state(&self) -> Option<ClaudeState> {
        if !self.is_claude() {
            return None;
        }
        if let Some(state) = self.claude_hook {
            return Some(state);
        }
        let terminal = self.terminal.as_ref()?;
        if terminal.bell || terminal.last_output.elapsed() >= CLAUDE_IDLE {
            Some(ClaudeState::Waiting)
        } else {
            Some(ClaudeState::Running)
        }
    }

    /// Recomputes Claude state now and again once the idle window passes, emitting on change.
    fn refresh_claude(&mut self, cx: &mut Context<Self>) {
        let state = self.current_claude_state();
        if state != self.claude_state {
            self.claude_state = state;
            cx.emit(TerminalEvent::Changed);
        }
        if state == Some(ClaudeState::Running) {
            self._claude_timer = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(CLAUDE_IDLE + Duration::from_millis(100))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.refresh_claude(cx);
                    cx.notify();
                });
            }));
        }
    }

    /// Ends the shell for good, as when its project is closed.
    pub fn kill(&mut self) {
        if let (Some(conn), Some(pane)) = (&self.conn, self.pane.take()) {
            let _ = conn.send(&ClientMsg::Kill { pane });
        }
    }

    pub(crate) fn terminal(&self) -> Option<&Terminal> {
        self.terminal.as_ref()
    }

    pub(crate) fn resize(&mut self, grid: GridSize) {
        self.grid = grid;
        if let Some(terminal) = self.terminal.as_mut()
            && !terminal.replaying
        {
            terminal.resize(grid);
        }
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        let first = match self.pane {
            Some(pane) => ClientMsg::Attach { pane },
            None => self.spawn_msg(),
        };
        self._io = Some(cx.spawn(async move |this, cx| {
            let connected = cx
                .background_executor()
                .spawn(async move {
                    let (conn, reader) = open_connection()?;
                    conn.send(&first)?;
                    anyhow::Ok((conn, reader))
                })
                .await;
            let (conn, reader) = match connected {
                Ok(pair) => pair,
                Err(err) => {
                    let _ = this.update(cx, |this, cx| {
                        this.error = Some(format!("{err:#}"));
                        cx.notify();
                    });
                    return;
                }
            };
            let conn = Arc::new(conn);
            if this
                .update(cx, |this, _| this.conn = Some(conn.clone()))
                .is_err()
            {
                return;
            }

            let messages = read_messages(reader);
            while let Ok(first) = messages.recv().await {
                // Drain what is already queued so one frame covers a burst of output.
                let mut batch = vec![first];
                let mut bytes = 0;
                while bytes < BATCH_BYTES {
                    let Ok(next) = messages.try_recv() else { break };
                    if let ServerMsg::Output { data, .. } = &next {
                        bytes += data.len();
                    }
                    batch.push(next);
                }
                let alive = this.update(cx, |this, cx| {
                    for msg in batch {
                        this.on_message(msg, cx);
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.conn = None;
                let recent = this
                    .last_reconnect
                    .is_some_and(|t| t.elapsed() < RECONNECT_COOLDOWN);
                if recent {
                    this.error = Some("Lost the connection to the session daemon.".into());
                } else {
                    // The daemon died; its shells went with it, so the attach below starts fresh ones.
                    this.last_reconnect = Some(Instant::now());
                    this.connect(cx);
                }
                cx.notify();
            });
        }));
    }

    fn spawn_msg(&self) -> ClientMsg {
        ClientMsg::Spawn {
            cwd: self.cwd.clone(),
            rows: self.grid.rows,
            cols: self.grid.cols,
        }
    }

    fn on_message(&mut self, msg: ServerMsg, cx: &mut Context<Self>) {
        let palette = cx.theme().terminal.clone();
        match msg {
            ServerMsg::Spawned { pane } => {
                self.pane = Some(pane);
                self.send(ClientMsg::Attach { pane });
                cx.emit(TerminalEvent::Attached(pane));
            }
            ServerMsg::Attached { pane, rows, cols } => {
                let Some(conn) = self.conn.clone() else {
                    return;
                };
                // Start at the daemon's size so the replayed screen lays out as it was drawn.
                let size = GridSize {
                    rows,
                    cols,
                    ..self.grid
                };
                let mut terminal = Terminal::new(size, Box::new(MuxTransport { conn, pane }));
                terminal.replaying = true;
                self.terminal = Some(terminal);
            }
            ServerMsg::Output { data, .. } => {
                let Some(terminal) = self.terminal.as_mut() else {
                    return;
                };
                let had_bell = terminal.bell;
                terminal.handle(PaneEvent::Output(data), &palette);
                if let Some(text) = terminal.clipboard_write.take() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                if terminal.bell != had_bell {
                    cx.emit(TerminalEvent::Changed);
                }
                if !terminal.replaying {
                    self.refresh_claude(cx);
                }
            }
            ServerMsg::Foreground { process, .. } => {
                self.foreground = process;
                if !self.is_claude() {
                    self.claude_hook = None;
                }
                self.refresh_claude(cx);
                cx.emit(TerminalEvent::Changed);
            }
            ServerMsg::ReplayDone { .. } => {
                if let Some(terminal) = self.terminal.as_mut() {
                    terminal.replaying = false;
                    if let Some(bytes) = self.pending_input.take() {
                        terminal.input(bytes);
                    }
                    if std::mem::take(&mut self.session_lost) {
                        terminal.handle(PaneEvent::Output(SESSION_LOST.to_vec()), &palette);
                    }
                    let unchanged = (terminal.size().rows, terminal.size().cols)
                        == (self.grid.rows, self.grid.cols);
                    terminal.resize(self.grid);
                    if unchanged {
                        terminal.force_redraw();
                    }
                }
            }
            ServerMsg::Exited { code, .. } => {
                if let Some(terminal) = self.terminal.as_mut() {
                    terminal.handle(PaneEvent::Exited(code), &palette);
                }
            }
            ServerMsg::Error {
                kind: ErrorKind::NoSuchPane(_),
            } => {
                self.pane = None;
                self.session_lost = true;
                self.send(self.spawn_msg());
            }
            ServerMsg::Error { kind } => self.error = Some(kind.to_string()),
            ServerMsg::Hello { .. } | ServerMsg::Panes { .. } | ServerMsg::Notice(_) => {}
        }
    }

    fn send(&self, msg: ClientMsg) {
        if let Some(conn) = &self.conn {
            let _ = conn.send(&msg);
        }
    }

    fn restart(&mut self, cx: &mut Context<Self>) {
        self.kill();
        self.terminal = None;
        if self.conn.is_some() {
            self.send(self.spawn_msg());
        } else {
            self.connect(cx);
        }
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        if terminal.exit.is_some() {
            if event.keystroke.key == "enter" {
                self.restart(cx);
                cx.stop_propagation();
            }
            return;
        }
        if let Some(bytes) = keys::to_esc(&event.keystroke, terminal.mode()) {
            terminal.clear_selection();
            terminal.input(bytes);
            cx.stop_propagation();
            cx.notify();
        }
    }

    /// Viewport cell coordinates (fractional) of a window position.
    fn cell_position(&self, position: gpui::Point<Pixels>) -> (f32, f32) {
        let local = position - self.origin;
        (
            f32::from(local.x) / self.grid.cell_width,
            f32::from(local.y) / self.grid.cell_height,
        )
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let (col, row) = self.cell_position(event.position);
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        if event.modifiers.platform
            && let Some(link) = terminal.link_at(row.max(0.) as usize, col.max(0.) as usize)
        {
            if links::openable(&link.uri) {
                cx.open_url(&link.uri);
            }
            return;
        }
        let (point, side) = terminal.point_at(col, row);
        terminal.start_selection(event.click_count, point, side);
        self.selecting = true;
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let (col, row) = self.cell_position(event.position);
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        if self.selecting && event.pressed_button == Some(MouseButton::Left) {
            let (point, side) = terminal.point_at(col, row);
            terminal.update_selection(point, side);
            cx.notify();
            return;
        }
        let link = (event.modifiers.platform && col >= 0. && row >= 0.)
            .then(|| terminal.link_at(row as usize, col as usize))
            .flatten();
        if link != self.hovered_link {
            self.hovered_link = link;
            cx.notify();
        }
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.selecting) {
            return;
        }
        if let Some(terminal) = self.terminal.as_mut()
            && !terminal.has_selection()
        {
            terminal.clear_selection();
            cx.notify();
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        match self.terminal.as_ref().and_then(Terminal::selection_text) {
            Some(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            None => cx.propagate(),
        }
    }

    fn allow_clipboard(&mut self, cx: &mut Context<Self>) {
        if let Some(terminal) = self.terminal.as_mut() {
            terminal.allow_clipboard = true;
            if let Some(text) = terminal.blocked_clipboard.take() {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            cx.notify();
        }
    }

    fn render_clipboard_notice(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        self.terminal.as_ref()?.blocked_clipboard.as_ref()?;
        let t = cx.theme();
        Some(
            div()
                .flex_none()
                .h(px(32.))
                .px(px(8.))
                .flex()
                .items_center()
                .justify_between()
                .border_t_1()
                .border_color(t.color.border)
                .bg(t.color.surface)
                .text_size(t.typography.caption)
                .text_color(t.color.content_muted)
                .child("A program in this terminal tried to set the clipboard.")
                .child(
                    div()
                        .flex()
                        .gap(px(4.))
                        .child(
                            athena_ui::Button::new(
                                "clipboard-allow",
                                "Allow for this terminal",
                                ButtonKind::Secondary,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.allow_clipboard(cx))),
                        )
                        .child(
                            athena_ui::Button::new(
                                "clipboard-dismiss",
                                "Dismiss",
                                ButtonKind::Ghost,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(terminal) = this.terminal.as_mut() {
                                    terminal.blocked_clipboard = None;
                                }
                                cx.notify();
                            })),
                        ),
                ),
        )
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        let line_height = self.grid.cell_height;
        let dy: f32 = event.delta.pixel_delta(px(line_height)).y.into();
        self.scroll_remainder += dy / line_height;
        let lines = self.scroll_remainder.trunc() as i32;
        self.scroll_remainder -= lines as f32;
        if lines == 0 {
            return;
        }
        let mode = terminal.mode();
        if mode.contains(TermMode::ALT_SCREEN) {
            // Full-screen apps without mouse reporting get arrow keys, as in Terminal.app.
            if mode.contains(TermMode::ALTERNATE_SCROLL) && !mode.intersects(TermMode::MOUSE_MODE) {
                let app = mode.contains(TermMode::APP_CURSOR);
                let key: &[u8] = match (lines > 0, app) {
                    (true, true) => b"\x1bOA",
                    (true, false) => b"\x1b[A",
                    (false, true) => b"\x1bOB",
                    (false, false) => b"\x1b[B",
                };
                terminal.input(key.repeat(lines.unsigned_abs() as usize));
            }
        } else {
            terminal.scroll(lines);
        }
        cx.notify();
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let text = cx.read_from_clipboard().and_then(|item| item.text());
        if let (Some(text), Some(terminal)) = (text, self.terminal.as_mut()) {
            terminal.paste(&text);
            cx.notify();
        }
    }

    fn clear_scrollback(&mut self, _: &ClearScrollback, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = self.terminal.as_mut() {
            terminal.clear_scrollback();
            cx.notify();
        }
    }

    fn render_status(&self, cx: &App) -> Option<impl IntoElement> {
        let code = self.terminal.as_ref()?.exit?;
        let t = cx.theme();
        let label = match code {
            Some(code) => format!("Shell exited with code {code}. Press Enter to start a new one."),
            None => "Shell exited. Press Enter to start a new one.".to_string(),
        };
        Some(
            div()
                .flex_none()
                .h(px(28.))
                .px(px(8.))
                .flex()
                .items_center()
                .border_t_1()
                .border_color(t.color.border)
                .text_size(t.typography.caption)
                .text_color(t.color.content_muted)
                .child(label),
        )
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let focused = self.focus.is_focused(window);
        if focused
            && let Some(terminal) = self.terminal.as_mut()
            && std::mem::take(&mut terminal.bell)
        {
            cx.emit(TerminalEvent::Changed);
        }
        let root = div()
            .id("terminal")
            .track_focus(&self.focus)
            .key_context("Terminal")
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::clear_scrollback))
            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
            .cursor(if self.hovered_link.is_some() {
                CursorStyle::PointingHand
            } else {
                CursorStyle::IBeam
            })
            .size_full()
            .flex()
            .flex_col()
            .bg(t.terminal.background);

        if let Some(error) = &self.error {
            let retry =
                athena_ui::Button::new("terminal-retry", "Try again", ButtonKind::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.conn = None;
                        this.connect(cx);
                        cx.notify();
                    }));
            return root.items_center().justify_center().child(empty_state(
                "Terminal unavailable",
                error.clone(),
                Some(retry),
                cx,
            ));
        }

        root.child(
            div()
                .flex_1()
                .min_h_0()
                .p(px(8.))
                .child(TerminalElement::new(cx.entity(), focused)),
        )
        .children(self.render_clipboard_notice(cx))
        .children(self.render_status(cx))
    }
}

pub(crate) fn is_shell(name: &str) -> bool {
    matches!(
        name.trim_start_matches('-'),
        "zsh" | "bash" | "fish" | "sh" | "dash" | "nu" | "login"
    )
}

impl gpui::EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let utf16: Vec<u16> = self.marked.encode_utf16().collect();
        let range = range.start.min(utf16.len())..range.end.min(utf16.len());
        *actual_range = Some(range.clone());
        String::from_utf16(&utf16[range]).ok()
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.marked.encode_utf16().count();
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        (!self.marked.is_empty()).then(|| 0..self.marked.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked.clear();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked.clear();
        if let Some(terminal) = self.terminal.as_mut()
            && !text.is_empty()
        {
            terminal.input(text.as_bytes().to_vec());
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = text.to_string();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.cursor_bounds
    }

    fn character_index_for_point(
        &mut self,
        _: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

/// Connects to the session daemon, starting the one installed next to this executable if needed.
pub fn open_connection() -> anyhow::Result<(Connection, UnixStream)> {
    let daemon = std::env::current_exe()?.with_file_name("athena-mux");
    let socket = athena_proto::socket_path()?;
    let log = athena_proto::log_path()?;
    athena_proto::connect_or_spawn(&socket, &daemon, &log).map_err(|e| anyhow!(e))
}

/// Frames from the daemon on a channel, read on a dedicated thread.
pub fn read_messages(mut reader: UnixStream) -> async_channel::Receiver<ServerMsg> {
    let (tx, rx) = async_channel::bounded(64);
    let spawned = thread::Builder::new()
        .name("mux-read".into())
        .spawn(move || {
            while let Ok(Some(msg)) = athena_proto::read_frame::<_, ServerMsg>(&mut reader) {
                if tx.send_blocking(msg).is_err() {
                    break;
                }
            }
        });
    if spawned.is_err() {
        rx.close();
    }
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_shells_count_as_idle() {
        assert!(is_shell("-zsh"));
        assert!(is_shell("bash"));
        assert!(!is_shell("vim"));
        assert!(!is_shell("2.1.294"));
    }
}
