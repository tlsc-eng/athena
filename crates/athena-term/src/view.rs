use std::ops::Range;
use std::path::PathBuf;

use alacritty_terminal::term::TermMode;
use athena_ui::{ActiveTheme, ButtonKind, empty_state};
use gpui::{
    App, Bounds, Context, FocusHandle, Focusable, IntoElement, KeyBinding, KeyDownEvent,
    MouseButton, Pixels, Render, ScrollWheelEvent, Task, UTF16Selection, Window, actions, div,
    prelude::*, px,
};

use crate::element::TerminalElement;
use crate::keys;
use crate::terminal::{GridSize, Terminal};

actions!(terminal, [Paste, ClearScrollback]);

const BATCH_BYTES: usize = 2 * 1024 * 1024;

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-v", Paste, Some("Terminal")),
        KeyBinding::new("cmd-k", ClearScrollback, Some("Terminal")),
    ]);
}

/// One shell session rendered as a pane.
pub struct TerminalView {
    terminal: Option<Terminal>,
    error: Option<String>,
    cwd: PathBuf,
    pub(crate) focus: FocusHandle,
    pub(crate) marked: String,
    pub(crate) cursor_bounds: Option<Bounds<Pixels>>,
    grid: GridSize,
    scroll_remainder: f32,
    _io: Option<Task<()>>,
}

impl TerminalView {
    pub fn new(cwd: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            terminal: None,
            error: None,
            cwd,
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
        view.start(cx);
        view
    }

    pub fn title(&self) -> Option<&str> {
        self.terminal.as_ref()?.title.as_deref()
    }

    pub(crate) fn terminal(&self) -> Option<&Terminal> {
        self.terminal.as_ref()
    }

    pub(crate) fn resize(&mut self, grid: GridSize) {
        self.grid = grid;
        if let Some(terminal) = self.terminal.as_mut() {
            terminal.resize(grid);
        }
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let (terminal, output) = match Terminal::spawn(&self.cwd, self.grid) {
            Ok(pair) => pair,
            Err(err) => {
                self.terminal = None;
                self.error = Some(format!("{err:#}"));
                return;
            }
        };
        self.terminal = Some(terminal);
        self.error = None;
        self._io = Some(cx.spawn(async move |this, cx| {
            while let Ok(first) = output.recv().await {
                // Drain what is already queued so one frame covers a burst of output.
                let mut batch = vec![first];
                let mut bytes = 0;
                while bytes < BATCH_BYTES {
                    let Ok(next) = output.try_recv() else { break };
                    if let crate::pty::PtyEvent::Output(chunk) = &next {
                        bytes += chunk.len();
                    }
                    batch.push(next);
                }
                let alive = this.update(cx, |this, cx| {
                    let palette = cx.theme().terminal.clone();
                    if let Some(terminal) = this.terminal.as_mut() {
                        for event in batch {
                            terminal.handle(event, &palette);
                        }
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        if terminal.exit.is_some() {
            if event.keystroke.key == "enter" {
                self.start(cx);
                cx.stop_propagation();
                cx.notify();
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
                        this.start(cx);
                        cx.notify();
                    }));
            return root.items_center().justify_center().child(empty_state(
                "Could not start a shell",
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
