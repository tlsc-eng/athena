use std::ops::Range;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use anyhow::anyhow;
use athena_proto::{ClientMsg, ConnectError, Connection, ErrorKind, PaneId, Process, ServerMsg};
use athena_ui::motion::{self, Closing, Opening};
use athena_ui::{
    ActiveTheme, ButtonKind, ContextMenu, InputEvent, MenuItem, TextInput, Tooltip, empty_state,
};
use gpui::{
    Animation, App, Bounds, ClipboardItem, Context, CursorStyle, DismissEvent, Entity,
    EventEmitter, FocusHandle, Focusable, IntoElement, KeyBinding, KeyDownEvent, Modifiers,
    ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Render, ScrollWheelEvent, SharedString, Subscription, Task, UTF16Selection, Window, actions,
    div, prelude::*, px,
};

use crate::element::{RowCache, TerminalElement};
use crate::keys;
use crate::links;
use crate::mouse::{self, MouseEvent};
use crate::search::{Search, Span};
use crate::terminal::{GridSize, Link, NoOutput, PaneEvent, SCROLLBACK_LINES, Terminal, Transport};

actions!(
    terminal,
    [
        Copy,
        Paste,
        ClearScrollback,
        SelectAll,
        Find,
        FindNext,
        FindPrevious,
        ToggleMatchCase,
        ToggleRegex,
        ScrollToPreviousCommand,
        ScrollToNextCommand,
        CopyLastCommandOutput
    ]
);

const BATCH_BYTES: usize = 2 * 1024 * 1024;

/// Upper bound on one `Input` frame, well under the protocol's frame limit.
const INPUT_CHUNK: usize = 256 * 1024;

/// A daemon that dies again this soon after a reconnect is not retried automatically.
const RECONNECT_COOLDOWN: Duration = Duration::from_secs(10);

/// Pause before reattaching after the daemon dropped a terminal that fell behind.
const LAG_RETRY: Duration = Duration::from_millis(250);

/// Waits between connection attempts before a terminal reports the daemon unavailable.
const CONNECT_RETRIES: [Duration; 2] = [Duration::from_millis(500), Duration::from_secs(1)];

/// How often a stale terminal checks whether the older daemon has gone.
const STALE_POLL: Duration = Duration::from_secs(1);

const SESSION_LOST: &[u8] = b"\x1b[2m[previous session ended; started a new shell]\x1b[0m\r\n";

/// New output re-runs an open search at most this often; a full scrollback scan takes ~10 ms.
const SEARCH_RERUN: Duration = Duration::from_millis(150);

pub fn init(cx: &mut App) {
    let find = Some("TerminalFind");
    cx.bind_keys([
        KeyBinding::new("cmd-c", Copy, Some("Terminal")),
        KeyBinding::new("cmd-v", Paste, Some("Terminal")),
        KeyBinding::new("cmd-k", ClearScrollback, Some("Terminal")),
        KeyBinding::new("cmd-a", SelectAll, Some("Terminal")),
        KeyBinding::new("cmd-f", Find, Some("Terminal")),
        KeyBinding::new("cmd-g", FindNext, Some("Terminal")),
        KeyBinding::new("cmd-shift-g", FindPrevious, Some("Terminal")),
        KeyBinding::new("cmd-up", ScrollToPreviousCommand, Some("Terminal")),
        KeyBinding::new("cmd-down", ScrollToNextCommand, Some("Terminal")),
        KeyBinding::new("shift-enter", FindPrevious, find),
        KeyBinding::new("alt-c", ToggleMatchCase, find),
        KeyBinding::new("alt-r", ToggleRegex, find),
    ]);
}

pub enum TerminalEvent {
    /// The view is now bound to this daemon pane; persist it to re-attach after a relaunch.
    Attached(PaneId),
    /// Label, bell or Claude state changed; tab strips and the project rail should redraw.
    Changed,
    /// The right-click menu opened or closed; it is drawn in-window, under native web views.
    ContextMenu { open: bool },
    /// Something the user asked for could not be done, worth a toast.
    Notice { title: String, body: String },
    /// Cmd+click on a file named in the output, with its one-based line and column if given;
    /// `path` stays relative when neither the shell's folder nor the project has it.
    OpenFile {
        path: PathBuf,
        line: Option<u32>,
        column: Option<u32>,
    },
}

/// How long a Claude Code session may stay silent before it counts as waiting for the user.
const CLAUDE_IDLE: Duration = Duration::from_secs(3);

/// An older daemon still runs this terminal's shell after an upgrade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stale {
    Offered,
    Kept,
    Restarting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeState {
    Running,
    Waiting,
}

/// The find bar: its input and the search it drives.
struct FindBar {
    input: Entity<TextInput>,
    search: Search,
    /// A re-run for new output is already scheduled.
    rerun_pending: bool,
    /// Output scrolled a full scrollback, so matches are found again before the next frame.
    stale: bool,
    _subscription: Subscription,
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
    stale: Option<Stale>,
    cwd: PathBuf,
    pane: Option<PaneId>,
    conn: Option<Arc<Connection>>,
    /// Closed by the user; a shell still being started for it is ended as soon as it arrives.
    closed: bool,
    session_lost: bool,
    last_reconnect: Option<Instant>,
    pub(crate) focus: FocusHandle,
    pub(crate) marked: String,
    pub(crate) cursor_bounds: Option<Bounds<Pixels>>,
    pub(crate) origin: gpui::Point<Pixels>,
    pub(crate) hovered_link: Option<Link>,
    pub(crate) rows: RowCache,
    foreground: Option<Process>,
    selecting: bool,
    claude_state: Option<ClaudeState>,
    /// What Claude Code's own hooks last reported; trusted over the output-silence guess.
    claude_hook: Option<ClaudeState>,
    /// Typed into the shell once it is ready, for tabs opened to run a command.
    pending_input: Option<Vec<u8>>,
    grid: GridSize,
    scroll_remainder: f32,
    find: Option<FindBar>,
    find_opening: Option<Opening>,
    /// A dismissed find bar, still drawn while it fades out.
    find_closing: Option<(FindBar, Closing)>,
    find_generation: u64,
    /// Query and toggles of the last closed find bar, restored when it reopens.
    last_find: Option<(String, bool, bool)>,
    /// The button whose press went to the program, so its release does too.
    mouse_press: Option<mouse::Button>,
    /// The cell of the last report, so motion is sent once per cell.
    mouse_cell: Option<(usize, usize)>,
    _claude_timer: Option<Task<()>>,
    _io: Option<Task<()>>,
    context_menu: Option<(Entity<ContextMenu>, Subscription)>,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl TerminalView {
    /// Re-attaches to `pane` when given and still alive, otherwise starts a new shell in `cwd`.
    pub fn new(cwd: PathBuf, pane: Option<PaneId>, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            terminal: None,
            error: None,
            stale: None,
            cwd,
            pane,
            conn: None,
            closed: false,
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
            rows: RowCache::default(),
            foreground: None,
            selecting: false,
            claude_state: None,
            claude_hook: None,
            pending_input: None,
            scroll_remainder: 0.,
            find: None,
            find_opening: None,
            find_closing: None,
            find_generation: 0,
            last_find: None,
            mouse_press: None,
            mouse_cell: None,
            _claude_timer: None,
            _io: None,
            context_menu: None,
        };
        view.connect(cx);
        view
    }

    /// Tab label: the title the program set, else the running program, else the folder.
    pub fn label(&self) -> String {
        if let Some(title) = self
            .terminal
            .as_ref()
            .and_then(|t| t.title.clone())
            .filter(|t| !t.is_empty())
        {
            return title;
        }
        // Claude Code's executable is named after its version.
        if self.is_claude() {
            return "Claude".into();
        }
        if let Some(p) = self.foreground.as_ref().filter(|p| !is_shell(&p.name)) {
            return p.name.clone();
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

    /// An older session daemon still runs this terminal's shell, so it cannot attach.
    pub fn is_stale(&self) -> bool {
        self.stale.is_some()
    }

    /// The daemon pane this view shows, once attached.
    pub fn session(&self) -> Option<PaneId> {
        self.pane
    }

    /// Sends bytes to the shell now, as if typed.
    pub fn type_text(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if let Some(terminal) = self.terminal.as_mut() {
            terminal.input(bytes);
            cx.notify();
        }
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
        self.closed = true;
        if let (Some(conn), Some(pane)) = (&self.conn, self.pane.take()) {
            let _ = conn.send(&ClientMsg::Kill { pane });
        }
    }

    pub(crate) fn terminal(&self) -> Option<&Terminal> {
        self.terminal.as_ref()
    }

    pub(crate) fn terminal_mut(&mut self) -> Option<&mut Terminal> {
        self.terminal.as_mut()
    }

    pub(crate) fn resize(&mut self, grid: GridSize, cx: &mut Context<Self>) {
        let reflow = (grid.cols, grid.rows) != (self.grid.cols, self.grid.rows);
        self.grid = grid;
        if let Some(terminal) = self.terminal.as_mut()
            && !terminal.replaying
        {
            terminal.resize(grid);
            if reflow {
                self.schedule_search(cx);
            }
        }
    }

    /// Hangs up the daemon connection; dropping it alone leaves its reader thread blocked.
    fn disconnect(&mut self) {
        if let Some(conn) = self.conn.take() {
            conn.close();
        }
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        self.disconnect();
        self.error = None;
        self.set_stale(None, cx);
        let first = match self.pane {
            Some(pane) => ClientMsg::Attach { pane },
            None => self.spawn_msg(),
        };
        self._io = Some(cx.spawn(async move |this, cx| {
            let mut retries = CONNECT_RETRIES.iter();
            let connected = loop {
                let first = first.clone();
                let attempt = cx
                    .background_executor()
                    .spawn(async move {
                        let (conn, reader) = open_connection()?;
                        conn.send(&first)?;
                        anyhow::Ok((conn, reader))
                    })
                    .await;
                match (attempt, retries.next()) {
                    (Err(err), Some(delay)) if !is_stale_daemon(&err) => {
                        tracing::warn!(
                            "session daemon not reachable ({err:#}); retrying in {delay:?}"
                        );
                        cx.background_executor().timer(*delay).await;
                    }
                    (attempt, _) => break attempt,
                }
            };
            let (conn, reader) = match connected {
                Ok(pair) => pair,
                Err(err) if is_stale_daemon(&err) => {
                    tracing::info!("terminal belongs to an older session daemon: {err:#}");
                    let _ = this.update(cx, |this, cx| this.set_stale(Some(Stale::Offered), cx));
                    return;
                }
                Err(err) => {
                    tracing::warn!("terminal could not connect to the session daemon: {err:#}");
                    let _ = this.update(cx, |this, cx| {
                        this.error = Some(format!("{err:#}"));
                        cx.notify();
                    });
                    return;
                }
            };
            let conn = Arc::new(conn);
            let daemon_pid = conn.daemon_pid;
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
            // The same daemon still answering means it hung up on us for falling behind, and
            // the shell is still there to reattach to; that is no crash to back off from.
            let dropped = cx
                .background_executor()
                .spawn(async move { daemon_running(daemon_pid) })
                .await;
            if dropped {
                cx.background_executor().timer(LAG_RETRY).await;
                let _ = this.update(cx, |this, cx| {
                    tracing::info!(pane = ?this.pane, "session daemon dropped a lagging terminal; reattaching");
                    this.connect(cx);
                    cx.notify();
                });
                return;
            }
            let _ = this.update(cx, |this, cx| {
                this.conn = None;
                let recent = this
                    .last_reconnect
                    .is_some_and(|t| t.elapsed() < RECONNECT_COOLDOWN);
                tracing::warn!(pane = ?this.pane, reconnect = !recent, "lost the session daemon");
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

    fn set_stale(&mut self, stale: Option<Stale>, cx: &mut Context<Self>) {
        if self.stale == stale {
            return;
        }
        let was_stale = self.stale.is_some();
        self.stale = stale;
        if stale.is_some() && !was_stale {
            self.watch_stale(cx);
        }
        cx.emit(TerminalEvent::Changed);
        cx.notify();
    }

    /// Reconnects once the older daemon has gone, whoever stopped it.
    fn watch_stale(&mut self, cx: &mut Context<Self>) {
        self._io = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(STALE_POLL).await;
                let stale = cx
                    .background_executor()
                    .spawn(async move {
                        let socket = athena_proto::socket_path()?;
                        let busy = matches!(
                            athena_proto::connect(&socket),
                            Err(ConnectError::VersionMismatch { panes: 1.., .. })
                        );
                        anyhow::Ok(busy)
                    })
                    .await
                    .unwrap_or(true);
                if !stale {
                    let _ = this.update(cx, |this, cx| this.connect(cx));
                    return;
                }
            }
        }));
    }

    /// Ends the older daemon and its shells, then starts this terminal on a new one.
    fn restart_sessions(&mut self, cx: &mut Context<Self>) {
        self.set_stale(Some(Stale::Restarting), cx);
        self._io = Some(cx.spawn(async move |this, cx| {
            let stopped = cx
                .background_executor()
                .spawn(async move {
                    let socket = athena_proto::socket_path()?;
                    anyhow::Ok(athena_proto::stop_daemon(&socket)?)
                })
                .await;
            if let Err(err) = stopped {
                tracing::warn!("could not stop the older session daemon: {err:#}");
            }
            let _ = this.update(cx, |this, cx| this.connect(cx));
        }));
    }

    fn render_stale(&self, stale: Stale, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, body) = match stale {
            Stale::Offered => (
                "This session ran on an older Athena",
                "Its shell still runs in the previous session daemon, which this version cannot \
                 attach to. Restarting ends those shells and starts new ones here.",
            ),
            Stale::Kept => (
                "Older sessions are still running",
                "This terminal connects once the previous session daemon has exited, or when \
                 you restart sessions.",
            ),
            Stale::Restarting => (
                "Restarting sessions",
                "Stopping the previous session daemon.",
            ),
        };
        let restart = (stale != Stale::Restarting).then(|| {
            athena_ui::Button::new(
                "terminal-restart-sessions",
                "Restart sessions",
                ButtonKind::Primary,
            )
            .on_click(cx.listener(|this, _, _, cx| this.restart_sessions(cx)))
        });
        let keep = (stale == Stale::Offered).then(|| {
            athena_ui::Button::new("terminal-keep-sessions", "Keep", ButtonKind::Ghost)
                .on_click(cx.listener(|this, _, _, cx| this.set_stale(Some(Stale::Kept), cx)))
        });
        empty_state(title, body, None, cx).child(
            div()
                .pt(px(8.))
                .flex()
                .gap(px(8.))
                .children(restart)
                .children(keep),
        )
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
            ServerMsg::Spawned { pane } if self.closed => {
                self.send(ClientMsg::Kill { pane });
            }
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
                let had_title = terminal.title.clone();
                terminal.handle(PaneEvent::Output(data), &palette);
                if let Some(text) = terminal.clipboard_write.take() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                if terminal.bell != had_bell || terminal.title != had_title {
                    cx.emit(TerminalEvent::Changed);
                }
                if !terminal.replaying {
                    self.refresh_claude(cx);
                }
                self.search_after_output(cx);
            }
            ServerMsg::Foreground { process, .. } => {
                let was = self.foreground.as_ref().map(|p| p.pid);
                // On attach, a replayed title over an idle shell came from a program now gone.
                let attached_to_shell =
                    was.is_none() && process.as_ref().is_some_and(|p| is_shell(&p.name));
                if (was.is_some() && was != process.as_ref().map(|p| p.pid) || attached_to_shell)
                    && let Some(terminal) = self.terminal.as_mut()
                {
                    terminal.forget_stale_title();
                }
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
            } if !self.closed => {
                self.pane = None;
                self.session_lost = true;
                self.send(self.spawn_msg());
            }
            ServerMsg::Error {
                kind: ErrorKind::NoSuchPane(_),
            } => {}
            ServerMsg::Error { kind } => {
                tracing::warn!(pane = ?self.pane, "session daemon error: {kind}");
                self.error = Some(kind.to_string());
            }
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
        self.closed = false;
        self.terminal = None;
        if self.conn.is_some() {
            self.send(self.spawn_msg());
        } else {
            self.connect(cx);
        }
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Keys typed into the find bar bubble up here too; they are not for the shell.
        if !self.focus.is_focused(window) {
            return;
        }
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

    /// Whether mouse events go to the program rather than select; Shift keeps selection, as in
    /// iTerm and VS Code.
    pub(crate) fn reports_mouse(&self, modifiers: &Modifiers) -> bool {
        !modifiers.shift
            && !modifiers.platform
            && self
                .terminal
                .as_ref()
                .is_some_and(|t| t.exit.is_none() && t.mode().intersects(TermMode::MOUSE_MODE))
    }

    /// The viewport cell under a window position, clamped to the grid.
    fn mouse_cell_at(&self, position: gpui::Point<Pixels>) -> (usize, usize) {
        let (col, row) = self.cell_position(position);
        let size = self.terminal.as_ref().map_or(self.grid, Terminal::size);
        (
            (col.max(0.) as usize).min(size.cols.saturating_sub(1) as usize),
            (row.max(0.) as usize).min(size.rows.saturating_sub(1) as usize),
        )
    }

    fn report_mouse(
        &mut self,
        event: MouseEvent,
        position: gpui::Point<Pixels>,
        modifiers: &Modifiers,
    ) {
        let (col, row) = self.mouse_cell_at(position);
        self.mouse_cell = Some((col, row));
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        let mods = mouse::Mods {
            shift: modifiers.shift,
            alt: modifiers.alt,
            control: modifiers.control,
        };
        if let Some(bytes) = mouse::encode(event, col, row, mods, terminal.mode()) {
            terminal.input(bytes);
        }
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        if self.reports_mouse(&event.modifiers) {
            self.mouse_press = Some(mouse::Button::Left);
            self.report_mouse(
                MouseEvent::Press(mouse::Button::Left),
                event.position,
                &event.modifiers,
            );
            cx.notify();
            return;
        }
        let (col, row) = self.cell_position(event.position);
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        let cell = (row.max(0.) as usize, col.max(0.) as usize);
        if event.modifiers.platform
            && let Some(link) = terminal.link_at(cell.0, cell.1)
        {
            if links::openable(&link.uri) {
                cx.open_url(&link.uri);
            } else if let Some(path) = file_link_target(&link.uri) {
                cx.emit(TerminalEvent::OpenFile {
                    path,
                    line: None,
                    column: None,
                });
            }
            return;
        }
        if event.modifiers.platform
            && let Some((_, file)) = terminal.file_at(cell.0, cell.1)
            && let Some(path) = self.file_target(&file)
        {
            cx.emit(TerminalEvent::OpenFile {
                path,
                line: file.line,
                column: file.column,
            });
            return;
        }
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        let (point, side) = terminal.point_at(col, row);
        terminal.start_selection(event.click_count, point, side);
        self.selecting = true;
        cx.notify();
    }

    /// Presses of the middle and right buttons, which only a program asking for the mouse sees.
    fn mouse_down_other(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(button) = report_button(event.button) else {
            return;
        };
        if self.reports_mouse(&event.modifiers) {
            window.focus(&self.focus);
            self.mouse_press = Some(button);
            self.report_mouse(MouseEvent::Press(button), event.position, &event.modifiers);
            cx.stop_propagation();
            cx.notify();
        }
    }

    /// Reports the release of a button whose press was reported; true if it was.
    fn release_mouse(&mut self, event: &MouseUpEvent) -> bool {
        let Some(button) = report_button(event.button) else {
            return false;
        };
        if self.mouse_press != Some(button) {
            return false;
        }
        self.mouse_press = None;
        self.report_mouse(
            MouseEvent::Release(button),
            event.position,
            &event.modifiers,
        );
        true
    }

    fn mouse_up_other(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.release_mouse(event) {
            cx.notify();
        }
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let (col, row) = self.cell_position(event.position);
        if self.selecting && event.pressed_button == Some(MouseButton::Left) {
            if let Some(terminal) = self.terminal.as_mut() {
                let (point, side) = terminal.point_at(col, row);
                terminal.update_selection(point, side);
                cx.notify();
            }
            return;
        }
        if (self.mouse_press.is_some() || self.reports_mouse(&event.modifiers))
            && self.mouse_cell != Some(self.mouse_cell_at(event.position))
        {
            let held = self.mouse_press;
            self.report_mouse(MouseEvent::Move(held), event.position, &event.modifiers);
        }
        self.hover_link(event.position, event.modifiers.platform, cx);
    }

    /// Underlines the link under the pointer while Cmd is held.
    fn hover_link(&mut self, position: gpui::Point<Pixels>, cmd: bool, cx: &mut Context<Self>) {
        let (col, row) = self.cell_position(position);
        let Some(terminal) = self.terminal.as_ref() else {
            return;
        };
        let cell = (cmd && col >= 0. && row >= 0.).then_some((row as usize, col as usize));
        let link = match cell.and_then(|(r, c)| terminal.link_at(r, c)) {
            Some(link) => Some(link),
            None => match cell.and_then(|(r, c)| terminal.file_at(r, c)) {
                // Checking the disk once per link, not on every mouse move over it.
                Some((link, _)) if Some(&link) == self.hovered_link.as_ref() => Some(link),
                Some((link, file)) => self.file_target(&file).map(|_| link),
                None => None,
            },
        };
        if link != self.hovered_link {
            self.hovered_link = link;
            cx.notify();
        }
    }

    /// The file a reference names, from the shell's current folder or the project; one that is
    /// in neither but has a line number stays relative, for the project search to find.
    fn file_target(&self, file: &links::FileRef) -> Option<PathBuf> {
        let current = self.foreground.as_ref().and_then(|p| p.cwd.clone());
        let dirs: Vec<&std::path::Path> = current
            .iter()
            .map(PathBuf::as_path)
            .chain([self.cwd.as_path()])
            .collect();
        file.resolve(&dirs).or_else(|| {
            let relative = std::path::Path::new(&file.path);
            (file.line.is_some() && relative.is_relative() && !file.path.starts_with('~'))
                .then(|| relative.to_path_buf())
        })
    }

    fn modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.hover_link(window.mouse_position(), event.modifiers.platform, cx);
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.release_mouse(event) {
            cx.notify();
            return;
        }
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
        if self.terminal.is_none() {
            return;
        }
        let line_height = self.grid.cell_height;
        let dy: f32 = event.delta.pixel_delta(px(line_height)).y.into();
        self.scroll_remainder += dy / line_height;
        let lines = self.scroll_remainder.trunc() as i32;
        self.scroll_remainder -= lines as f32;
        if lines == 0 {
            return;
        }
        if self.reports_mouse(&event.modifiers) {
            let button = if lines > 0 {
                mouse::Button::WheelUp
            } else {
                mouse::Button::WheelDown
            };
            for _ in 0..lines.unsigned_abs() {
                self.report_mouse(MouseEvent::Press(button), event.position, &event.modifiers);
            }
            cx.notify();
            return;
        }
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
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
        let at_prompt = self.foreground.as_ref().is_none_or(|p| is_shell(&p.name));
        if let Some(terminal) = self.terminal.as_mut() {
            terminal.clear_scrollback(at_prompt);
            self.run_search(false);
            cx.notify();
        }
    }

    fn scroll_to_command(&mut self, up: bool, cx: &mut Context<Self>) {
        if let Some(terminal) = self.terminal.as_mut()
            && terminal.scroll_to_prompt(up)
        {
            cx.notify();
        }
    }

    /// Copies what the newest finished command printed, as the shell's OSC 133 marks bound it.
    fn copy_last_output(&mut self, cx: &mut Context<Self>) {
        let Some(terminal) = self.terminal.as_ref() else {
            return;
        };
        let body = match terminal.last_output() {
            Ok(text) => return cx.write_to_clipboard(ClipboardItem::new_string(text)),
            Err(NoOutput::NoCommand) => {
                "No command has finished here with shell integration marks (OSC 133). zsh sends \
                 them unless ATHENA_SHELL_INTEGRATION=0; the README has a snippet for bash."
            }
            Err(NoOutput::Unmarked) => {
                "The shell did not mark where the output started (OSC 133;C)."
            }
            Err(NoOutput::Gone) => "The command's output has scrolled out of the scrollback.",
        };
        cx.emit(TerminalEvent::Notice {
            title: "Nothing to copy".into(),
            body: body.into(),
        });
    }

    fn select_all(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window) {
            return;
        }
        if let Some(terminal) = self.terminal.as_mut() {
            terminal.select_all();
            cx.notify();
        }
    }

    /// Opens the find bar, or focuses it if open, seeded with a one-line selection.
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let seed = self
            .terminal
            .as_ref()
            .and_then(Terminal::selection_text)
            .filter(|s| !s.contains('\n'));
        let input = match &self.find {
            Some(find) => find.input.clone(),
            None => {
                let input = cx.new(|cx| TextInput::new("Find", cx));
                let subscription =
                    cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                        match event {
                            InputEvent::Changed => this.search_changed(cx),
                            // Newest output is at the bottom, so "next" walks up as in VS Code.
                            InputEvent::Submit | InputEvent::SubmitBeside | InputEvent::Up => {
                                this.step_search(true, cx)
                            }
                            InputEvent::Down => this.step_search(false, cx),
                            InputEvent::Cancel => {
                                this.close_search(cx);
                                window.focus(&this.focus);
                            }
                        }
                        cx.notify();
                    });
                let mut search = Search::default();
                let restored = self.last_find.take().map(|(query, case, regex)| {
                    search.case_sensitive = case;
                    search.regex = regex;
                    query
                });
                self.find = Some(FindBar {
                    input: input.clone(),
                    search,
                    rerun_pending: false,
                    stale: false,
                    _subscription: subscription,
                });
                self.find_closing = None;
                self.find_opening = Some(Opening::now());
                if seed.is_none()
                    && let Some(query) = restored
                {
                    input.update(cx, |i, cx| i.set_text(query, cx));
                }
                input
            }
        };
        if let Some(seed) = seed {
            input.update(cx, |i, cx| i.set_text(seed, cx));
        }
        window.focus(&input.focus_handle(cx));
        cx.notify();
    }

    /// Starts the find bar's fade-out; its highlights go at once.
    fn close_search(&mut self, cx: &mut Context<Self>) {
        let Some(find) = self.find.take() else {
            return;
        };
        self.last_find = Some((
            find.search.query().to_string(),
            find.search.case_sensitive,
            find.search.regex,
        ));
        self.find_generation += 1;
        let generation = self.find_generation;
        self.find_closing = Some((find, Closing::new(generation)));
        let t = cx.theme();
        let delay = motion::exit_delay(t.motion.reduced, t.motion.fast);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .find_closing
                    .as_ref()
                    .is_some_and(|(_, c)| c.generation == generation)
                {
                    this.find_closing = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn search_changed(&mut self, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        let query = find.input.read(cx).text().to_string();
        find.search.set_query(&query);
        self.run_search(true);
    }

    /// Re-runs the search around the current match, or the viewport bottom without one.
    fn run_search(&mut self, reveal: bool) {
        let (Some(find), Some(terminal)) = (self.find.as_mut(), self.terminal.as_mut()) else {
            return;
        };
        find.stale = false;
        let anchor = find.search.current_match().map(|m| *m.start());
        find.search.run(terminal.term(), anchor);
        if reveal && let Some(m) = find.search.current_match() {
            terminal.reveal(m.start().line);
        }
    }

    fn step_search(&mut self, up: bool, cx: &mut Context<Self>) {
        let (Some(find), Some(terminal)) = (self.find.as_mut(), self.terminal.as_mut()) else {
            return;
        };
        find.search.step(up);
        if let Some(m) = find.search.current_match() {
            terminal.reveal(m.start().line);
        }
        cx.notify();
    }

    fn toggle_search(&mut self, toggle: impl FnOnce(&mut Search), cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        toggle(&mut find.search);
        let query = find.search.query().to_string();
        find.search.set_query(&query);
        self.run_search(true);
        cx.notify();
    }

    /// Keeps highlights on their text as output scrolls, then re-runs the search shortly after, or
    /// before the next frame once scrollback is full.
    fn search_after_output(&mut self, cx: &mut Context<Self>) {
        let (Some(find), Some(terminal)) = (self.find.as_mut(), self.terminal.as_ref()) else {
            return;
        };
        if find.search.query().is_empty() {
            return;
        }
        if find.search.follow_scroll(terminal.term(), SCROLLBACK_LINES) {
            self.schedule_search(cx);
        } else {
            find.stale = true;
        }
    }

    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        if std::mem::replace(&mut find.rerun_pending, true) {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_RERUN).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(find) = this.find.as_mut() {
                    find.rerun_pending = false;
                }
                this.run_search(false);
                cx.notify();
            });
        })
        .detach();
    }

    /// Visible pieces of the open search's matches.
    pub(crate) fn search_spans(
        &self,
        display_offset: usize,
        rows: usize,
        cols: usize,
    ) -> Vec<Span> {
        self.find
            .as_ref()
            .map_or(Vec::new(), |f| f.search.spans(display_offset, rows, cols))
    }

    fn render_find(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (find, closing) = match (&self.find, &self.find_closing) {
            (Some(find), _) => (find, None),
            (None, Some((find, closing))) => (find, Some(*closing)),
            (None, None) => return None,
        };
        let t = cx.theme();
        let search = &find.search;
        let count = if search.query().is_empty() {
            String::new()
        } else if search.invalid {
            "Invalid pattern".to_string()
        } else {
            let more = if search.truncated { "+" } else { "" };
            match (search.current, search.matches.len()) {
                (_, 0) => "No results".to_string(),
                (Some(i), n) => format!("{} of {n}{more}", i + 1),
                (None, n) => format!("{n}{more}"),
            }
        };
        let button = |id: &'static str, label: &'static str, tip: &'static str, on: bool| {
            div()
                .id(id)
                .size(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(t.shape.radius_control)
                .border_1()
                .border_color(if on {
                    t.color.accent
                } else {
                    gpui::transparent_black()
                })
                .when(on, |d| d.bg(t.color.surface_accent))
                .text_color(if on {
                    t.color.content
                } else {
                    t.color.content_muted
                })
                .cursor_pointer()
                .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                .active(|s| s.bg(t.color.surface_active))
                .tooltip(move |_, cx| Tooltip::view(SharedString::from(tip), cx))
                .child(label)
        };
        let row = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .p(px(4.))
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_control)
            .shadow_md()
            .text_size(t.typography.caption)
            .child(
                div()
                    .w(px(220.))
                    .h(px(24.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .bg(t.color.surface_sunken)
                    .border_1()
                    .border_color(if search.invalid {
                        t.color.danger
                    } else {
                        t.color.accent
                    })
                    .rounded(t.shape.radius_control)
                    .child(find.input.clone()),
            )
            .child(
                button("find-case", "Aa", "Match Case  ⌥C", search.case_sensitive).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.toggle_search(|s| s.case_sensitive = !s.case_sensitive, cx)
                    }),
                ),
            )
            .child(
                button(
                    "find-regex",
                    ".*",
                    "Use Regular Expression  ⌥R",
                    search.regex,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.toggle_search(|s| s.regex = !s.regex, cx)),
                ),
            )
            .child(
                div()
                    .min_w(px(72.))
                    .px(px(4.))
                    .text_color(if search.invalid {
                        t.color.danger
                    } else {
                        t.color.content_muted
                    })
                    .child(count),
            )
            .child(
                button("find-prev", "↑", "Older Match  Enter", false)
                    .on_click(cx.listener(|this, _, _, cx| this.step_search(true, cx))),
            )
            .child(
                button("find-next", "↓", "Newer Match  ⇧Enter", false)
                    .on_click(cx.listener(|this, _, _, cx| this.step_search(false, cx))),
            )
            .child(
                button("find-close", "×", "Close  Esc", false).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.close_search(cx);
                        window.focus(&this.focus);
                    },
                )),
            );
        // Floats over the grid so opening it never resizes the PTY.
        let row = match closing {
            Some(closing) => motion::animate_exit(
                t.motion.reduced,
                row,
                ("terminal-find-close", closing.generation),
                t.motion.fast,
                |el, d| el.opacity(1. - d).mt(px(-8. * d)),
            ),
            None => motion::animate_enter(
                t.motion.reduced,
                self.find_opening.is_some_and(|o| o.running(t.motion.fast)),
                row,
                "terminal-find-open",
                Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
                |el, d| el.opacity(d).mt(px(-8. * (1. - d))),
            ),
        };
        Some(
            div()
                .id("terminal-find")
                .key_context("TerminalFind")
                .absolute()
                .top(px(8.))
                .right(px(16.))
                .occlude()
                .cursor(CursorStyle::Arrow)
                .on_action(cx.listener(|this, _: &FindPrevious, _, cx| this.step_search(false, cx)))
                .on_action(cx.listener(|this, _: &ToggleMatchCase, _, cx| {
                    this.toggle_search(|s| s.case_sensitive = !s.case_sensitive, cx)
                }))
                .on_action(cx.listener(|this, _: &ToggleRegex, _, cx| {
                    this.toggle_search(|s| s.regex = !s.regex, cx)
                }))
                .child(row),
        )
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

impl Drop for TerminalView {
    fn drop(&mut self) {
        self.disconnect();
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
        if self.find.as_ref().is_some_and(|f| f.stale) {
            self.run_search(false);
        }
        let focused = self.focus.is_focused(window);
        if focused
            && let Some(terminal) = self.terminal.as_mut()
            && std::mem::take(&mut terminal.bell)
        {
            cx.emit(TerminalEvent::Changed);
        }
        let root =
            div()
                .id("terminal")
                .track_focus(&self.focus)
                .key_context("Terminal")
                .on_key_down(cx.listener(Self::key_down))
                .on_action(cx.listener(Self::copy))
                .on_action(cx.listener(Self::paste))
                .on_action(cx.listener(Self::clear_scrollback))
                .on_action(cx.listener(Self::select_all))
                .on_action(cx.listener(|this, _: &Find, window, cx| this.open_search(window, cx)))
                .on_action(cx.listener(|this, _: &ScrollToPreviousCommand, _, cx| {
                    this.scroll_to_command(true, cx)
                }))
                .on_action(cx.listener(|this, _: &ScrollToNextCommand, _, cx| {
                    this.scroll_to_command(false, cx)
                }))
                .on_action(
                    cx.listener(|this, _: &CopyLastCommandOutput, _, cx| this.copy_last_output(cx)),
                )
                .on_action(cx.listener(|this, _: &FindNext, window, cx| {
                    if this.find.is_some() {
                        this.step_search(true, cx)
                    } else {
                        this.open_search(window, cx)
                    }
                }))
                .on_action(cx.listener(|this, _: &FindPrevious, window, cx| {
                    if this.find.is_some() {
                        this.step_search(false, cx)
                    } else {
                        this.open_search(window, cx)
                    }
                }))
                .on_modifiers_changed(cx.listener(Self::modifiers_changed))
                .on_scroll_wheel(cx.listener(Self::scroll_wheel))
                .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                .on_mouse_down(MouseButton::Middle, cx.listener(Self::mouse_down_other))
                .on_mouse_down(MouseButton::Right, cx.listener(Self::mouse_down_other))
                .on_mouse_down(MouseButton::Right, cx.listener(Self::open_context_menu))
                .on_mouse_move(cx.listener(Self::mouse_move))
                .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
                .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
                .on_mouse_up(MouseButton::Middle, cx.listener(Self::mouse_up_other))
                .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::mouse_up_other))
                .on_mouse_up(MouseButton::Right, cx.listener(Self::mouse_up_other))
                .on_mouse_up_out(MouseButton::Right, cx.listener(Self::mouse_up_other))
                .cursor(if self.hovered_link.is_some() {
                    CursorStyle::PointingHand
                } else {
                    CursorStyle::IBeam
                })
                .size_full()
                .flex()
                .flex_col()
                .bg(t.terminal.background);

        if let Some(stale) = self.stale {
            return root
                .items_center()
                .justify_center()
                .child(self.render_stale(stale, cx));
        }
        if let Some(error) = &self.error {
            let retry =
                athena_ui::Button::new("terminal-retry", "Try again", ButtonKind::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| {
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
        .children(self.render_find(cx))
        .children(self.context_menu.as_ref().map(|(menu, _)| menu.clone()))
    }
}

/// The file a `file://` hyperlink names, if it is a regular file the editor can open; a
/// device or FIFO would block the UI thread reading it.
fn file_link_target(uri: &str) -> Option<PathBuf> {
    links::file_uri_path(uri).filter(|path| path.is_file())
}

fn report_button(button: MouseButton) -> Option<mouse::Button> {
    match button {
        MouseButton::Left => Some(mouse::Button::Left),
        MouseButton::Middle => Some(mouse::Button::Middle),
        MouseButton::Right => Some(mouse::Button::Right),
        _ => None,
    }
}

/// Whether a foreground process name is a shell, i.e. the terminal is at its prompt.
pub fn is_shell(name: &str) -> bool {
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
    // Through a Homebrew symlink, current_exe is the link and the daemon is not next to it.
    let daemon = std::env::current_exe()?
        .canonicalize()?
        .with_file_name("athena-mux");
    let socket = athena_proto::socket_path()?;
    let log = athena_proto::log_path()?;
    athena_proto::connect_or_spawn(&socket, &daemon, &log).map_err(|e| anyhow!(e))
}

/// Ends daemon shells whose tabs are gone, such as those of projects whose folder was deleted.
pub fn kill_sessions(panes: Vec<PaneId>) {
    if panes.is_empty() {
        return;
    }
    let _ = thread::Builder::new()
        .name("mux-kill".into())
        .spawn(move || {
            // No daemon running means the shells are already gone; don't start one to kill them.
            let Ok((conn, _)) = athena_proto::socket_path()
                .map_err(|e| anyhow!(e))
                .and_then(|socket| athena_proto::connect(&socket).map_err(|e| anyhow!(e)))
            else {
                return;
            };
            for pane in panes {
                let _ = conn.send(&ClientMsg::Kill { pane });
            }
            conn.close();
        });
}

/// Whether the daemon with this pid still answers on the socket.
fn daemon_running(pid: u32) -> bool {
    athena_proto::socket_path()
        .ok()
        .and_then(|socket| athena_proto::connect(&socket).ok())
        .is_some_and(|(conn, _)| conn.daemon_pid == pid)
}

/// An older daemon still running shells: offer a restart rather than report a failure.
fn is_stale_daemon(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<ConnectError>(),
        Some(ConnectError::VersionMismatch { panes: 1.., .. })
    )
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

impl TerminalView {
    fn open_context_menu(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Programs in mouse mode get the right-click instead.
        if self.terminal.is_none()
            || self.stale.is_some()
            || self.error.is_some()
            || self.reports_mouse(&event.modifiers)
        {
            return;
        }
        window.focus(&self.focus);
        let has_selection = self
            .terminal
            .as_ref()
            .and_then(Terminal::selection_text)
            .is_some();
        let has_output = self
            .terminal
            .as_ref()
            .is_some_and(|t| t.last_output().is_ok());
        let this = cx.entity().downgrade();
        let item = |label: &'static str,
                    hint: &'static str,
                    run: fn(&mut Self, &mut Window, &mut Context<Self>)| {
            let this = this.clone();
            MenuItem::new(label, move |window, cx| {
                this.update(cx, |view, cx| run(view, window, cx)).ok();
            })
            .hint(hint)
        };
        let items = vec![
            item("Copy", "⌘C", |v, w, cx| v.copy(&Copy, w, cx)).disabled(!has_selection),
            item("Paste", "⌘V", |v, w, cx| v.paste(&Paste, w, cx)),
            item("Select All", "⌘A", |v, w, cx| {
                v.select_all(&SelectAll, w, cx)
            }),
            item("Copy Last Command Output", "", |v, _, cx| {
                v.copy_last_output(cx)
            })
            .disabled(!has_output),
            MenuItem::separator(),
            item("Find", "⌘F", |v, w, cx| v.open_search(w, cx)),
            item("Clear", "⌘K", |v, w, cx| {
                v.clear_scrollback(&ClearScrollback, w, cx)
            }),
        ];
        let menu = ContextMenu::build(event.position, items, window, cx);
        let subscription = cx.subscribe_in(&menu, window, |this, menu, _: &DismissEvent, _, cx| {
            if this.context_menu.as_ref().is_some_and(|(m, _)| m == menu) {
                this.context_menu = None;
                cx.emit(TerminalEvent::ContextMenu { open: false });
                cx.notify();
            }
        });
        self.context_menu = Some((menu, subscription));
        cx.emit(TerminalEvent::ContextMenu { open: true });
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_older_daemon_with_shells_marks_the_terminal_stale() {
        let mismatch = |panes| {
            anyhow!(ConnectError::VersionMismatch {
                daemon: 2,
                panes,
                pid: 1,
            })
        };
        assert!(is_stale_daemon(&mismatch(3)));
        assert!(!is_stale_daemon(&mismatch(0)), "an idle one is replaced");
        assert!(!is_stale_daemon(&anyhow!(ConnectError::NotRunning)));
        assert!(!is_stale_daemon(&anyhow!(ConnectError::DaemonExited)));
    }

    #[test]
    fn file_hyperlinks_open_only_regular_files() {
        let file = std::env::temp_dir().join(format!("athena-link-{}.txt", std::process::id()));
        std::fs::write(&file, "x").unwrap();
        let uri = format!("file://{}", file.display());
        assert_eq!(file_link_target(&uri), Some(file.clone()));
        assert_eq!(file_link_target("file:///dev/zero"), None);
        assert_eq!(file_link_target("file:///tmp"), None);
        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn login_shells_count_as_idle() {
        assert!(is_shell("-zsh"));
        assert!(is_shell("bash"));
        assert!(!is_shell("vim"));
        assert!(!is_shell("2.1.294"));
    }
}
