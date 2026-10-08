use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{self, Osc52, Term, TermMode};
use alacritty_terminal::vte::ansi::{ClearMode, Handler, Processor, StdSyncHandler};
use anyhow::Result;
use athena_ui::TerminalColors;

use crate::colors;
use crate::pty::{LocalPty, PtyEvent};

const SCROLLBACK_LINES: usize = 10_000;
const MAX_TITLE: usize = 256;

/// Collects events the parser raises so they are handled after each `advance`, outside the borrow.
#[derive(Clone, Default)]
pub struct Listener(Rc<RefCell<Vec<Event>>>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        self.0.borrow_mut().push(event);
    }
}

#[derive(Clone, Copy, PartialEq)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: f32,
    pub cell_height: f32,
}

pub struct Terminal {
    term: Term<Listener>,
    parser: Processor<StdSyncHandler>,
    events: Listener,
    pty: LocalPty,
    size: GridSize,
    pub title: Option<String>,
    pub exit: Option<Option<i32>>,
    pub bell: bool,
}

impl Terminal {
    pub fn spawn(cwd: &Path, size: GridSize) -> Result<(Self, async_channel::Receiver<PtyEvent>)> {
        let (pty, output) = LocalPty::spawn(cwd, size.rows, size.cols)?;
        let events = Listener::default();
        let config = term::Config {
            scrolling_history: SCROLLBACK_LINES,
            // Copy requests reach the listener and are dropped unless the pane opts in; reads never.
            osc52: Osc52::OnlyCopy,
            ..Default::default()
        };
        let dims = TermSize::new(size.cols as usize, size.rows as usize);
        let term = Term::new(config, &dims, events.clone());
        let terminal = Self {
            term,
            parser: Processor::new(),
            events,
            pty,
            size,
            title: None,
            exit: None,
            bell: false,
        };
        Ok((terminal, output))
    }

    pub fn term(&self) -> &Term<Listener> {
        &self.term
    }

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    pub fn handle(&mut self, event: PtyEvent, palette: &TerminalColors) {
        match event {
            PtyEvent::Output(bytes) => {
                self.parser.advance(&mut self.term, &bytes);
                self.drain_events(palette);
            }
            PtyEvent::Exited(code) => self.exit = Some(code),
        }
    }

    fn drain_events(&mut self, palette: &TerminalColors) {
        let events: Vec<Event> = self.events.0.borrow_mut().drain(..).collect();
        for event in events {
            match event {
                Event::Title(title) => self.title = Some(sanitize_title(&title)),
                Event::ResetTitle => self.title = None,
                Event::PtyWrite(reply) => self.pty.write(reply.into_bytes()),
                Event::ColorRequest(index, format) => {
                    let color = match index {
                        256 => palette.foreground,
                        257 => palette.background,
                        258 => palette.cursor,
                        i => colors::indexed(i, self.term.colors(), palette),
                    };
                    self.pty
                        .write(format(colors::hsla_to_rgb(color)).into_bytes());
                }
                Event::TextAreaSizeRequest(format) => {
                    self.pty.write(format(self.window_size()).into_bytes());
                }
                Event::Bell => self.bell = true,
                _ => {}
            }
        }
    }

    /// Sends user input, snapping the view back to the live screen first.
    pub fn input(&mut self, bytes: Vec<u8>) {
        if self.exit.is_some() {
            return;
        }
        self.term.scroll_display(Scroll::Bottom);
        self.pty.write(bytes);
    }

    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let bytes = if self.mode().contains(TermMode::BRACKETED_PASTE) {
            // Strip any embedded end marker so pasted text cannot break out of the bracket.
            let inner = text.replace("\x1b[201~", "");
            format!("\x1b[200~{inner}\x1b[201~").into_bytes()
        } else {
            text.into_bytes()
        };
        self.input(bytes);
    }

    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    pub fn clear_scrollback(&mut self) {
        self.term.clear_screen(ClearMode::Saved);
        if !self.mode().contains(TermMode::ALT_SCREEN) {
            self.pty.write(vec![0x0c]);
        }
    }

    pub fn resize(&mut self, size: GridSize) {
        if size == self.size {
            return;
        }
        let grid_changed = (size.cols, size.rows) != (self.size.cols, self.size.rows);
        self.size = size;
        if grid_changed {
            self.term
                .resize(TermSize::new(size.cols as usize, size.rows as usize));
            self.pty.resize(size.rows, size.cols);
        }
    }

    fn window_size(&self) -> WindowSize {
        WindowSize {
            num_lines: self.size.rows,
            num_cols: self.size.cols,
            cell_width: self.size.cell_width as u16,
            cell_height: self.size.cell_height as u16,
        }
    }
}

fn sanitize_title(title: &str) -> String {
    title
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_TITLE)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_lose_control_characters_and_length() {
        assert_eq!(sanitize_title("a\x1b]0;b\x07c"), "a]0;bc");
        assert_eq!(sanitize_title(&"x".repeat(1000)).len(), MAX_TITLE);
    }
}
