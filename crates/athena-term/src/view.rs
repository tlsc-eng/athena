use std::ops::Range;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use anyhow::anyhow;
use athena_proto::{ClientMsg, Connection, ErrorKind, PaneId, ServerMsg};
use athena_ui::{ActiveTheme, ButtonKind, empty_state};
use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, Focusable, IntoElement, KeyBinding,
    KeyDownEvent, MouseButton, Pixels, Render, ScrollWheelEvent, Task, UTF16Selection, Window,
    actions, div, prelude::*, px,
};

use crate::element::TerminalElement;
use crate::keys;
use crate::terminal::{GridSize, PaneEvent, Terminal, Transport};

actions!(terminal, [Paste, ClearScrollback]);

const BATCH_BYTES: usize = 2 * 1024 * 1024;

/// Upper bound on one `Input` frame, well under the protocol's frame limit.
const INPUT_CHUNK: usize = 256 * 1024;

/// A daemon that dies again this soon after a reconnect is not retried automatically.
const RECONNECT_COOLDOWN: Duration = Duration::from_secs(10);

const SESSION_LOST: &[u8] = b"\x1b[2m[previous session ended; started a new shell]\x1b[0m\r\n";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-v", Paste, Some("Terminal")),
        KeyBinding::new("cmd-k", ClearScrollback, Some("Terminal")),
    ]);
}

pub enum TerminalEvent {
    /// The view is now bound to this daemon pane; persist it to re-attach after a relaunch.
    Attached(PaneId),
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
    grid: GridSize,
    scroll_remainder: f32,
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
            scroll_remainder: 0.,
            _io: None,
        };
        view.connect(cx);
        view
    }

    pub fn title(&self) -> Option<&str> {
        self.terminal.as_ref()?.title.as_deref()
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
                if let Some(terminal) = self.terminal.as_mut() {
                    terminal.handle(PaneEvent::Output(data), &palette);
                }
            }
            ServerMsg::ReplayDone { .. } => {
                if let Some(terminal) = self.terminal.as_mut() {
                    terminal.replaying = false;
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
            ServerMsg::Hello { .. } | ServerMsg::Panes { .. } => {}
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
            terminal.input(bytes);
            cx.stop_propagation();
            cx.notify();
        }
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
        let root = div()
            .id("terminal")
            .track_focus(&self.focus)
            .key_context("Terminal")
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::clear_scrollback))
            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _| window.focus(&this.focus)),
            )
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
        .children(self.render_status(cx))
    }
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
fn open_connection() -> anyhow::Result<(Connection, UnixStream)> {
    let daemon = std::env::current_exe()?.with_file_name("athena-mux");
    let socket = athena_proto::socket_path()?;
    let log = athena_proto::log_path()?;
    athena_proto::connect_or_spawn(&socket, &daemon, &log).map_err(|e| anyhow!(e))
}

fn read_messages(mut reader: UnixStream) -> async_channel::Receiver<ServerMsg> {
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
