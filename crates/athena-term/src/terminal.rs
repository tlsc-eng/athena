use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{self, Osc52, Term, TermMode};
use alacritty_terminal::vte::ansi::{ClearMode, Handler, Processor, StdSyncHandler};
use athena_ui::TerminalColors;

use crate::{colors, links};

const SCROLLBACK_LINES: usize = 10_000;
const MAX_TITLE: usize = 256;
/// The daemon notices a new foreground program up to two 500 ms ticks after it starts.
const TITLE_GRACE: Duration = Duration::from_secs(1);

/// Collects events the parser raises so they are handled after each `advance`, outside the borrow.
#[derive(Clone, Default)]
pub struct Listener(Rc<RefCell<Vec<Event>>>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        self.0.borrow_mut().push(event);
    }
}

/// Where keystrokes and size changes go; the daemon connection in practice.
pub trait Transport {
    fn write(&self, bytes: Vec<u8>);
    fn resize(&self, rows: u16, cols: u16);
}

pub enum PaneEvent {
    Output(Vec<u8>),
    Exited(Option<i32>),
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
    transport: Box<dyn Transport>,
    size: GridSize,
    /// While replaying history, replies to old queries must not reach the live shell.
    pub replaying: bool,
    pub title: Option<String>,
    title_at: Option<Instant>,
    pub exit: Option<Option<i32>>,
    pub bell: bool,
    pub last_output: Instant,
    /// OSC 52 copy requests are applied only after the user allows them for this terminal.
    pub allow_clipboard: bool,
    pub clipboard_write: Option<String>,
    pub blocked_clipboard: Option<String>,
}

/// A link under the pointer: viewport row, column range and target.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub row: usize,
    pub start: usize,
    pub end: usize,
    pub uri: String,
}

impl Terminal {
    pub fn new(size: GridSize, transport: Box<dyn Transport>) -> Self {
        let events = Listener::default();
        let config = term::Config {
            scrolling_history: SCROLLBACK_LINES,
            // Copy requests reach the listener and are dropped unless the pane opts in; reads never.
            osc52: Osc52::OnlyCopy,
            ..Default::default()
        };
        let dims = TermSize::new(size.cols as usize, size.rows as usize);
        let term = Term::new(config, &dims, events.clone());
        Self {
            term,
            parser: Processor::new(),
            events,
            transport,
            size,
            replaying: false,
            title: None,
            title_at: None,
            exit: None,
            bell: false,
            last_output: Instant::now(),
            allow_clipboard: false,
            clipboard_write: None,
            blocked_clipboard: None,
        }
    }

    pub fn term(&self) -> &Term<Listener> {
        &self.term
    }

    pub fn size(&self) -> GridSize {
        self.size
    }

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// Drops a title set before the foreground program changed, so the last program's title
    /// does not label the next one; a title set by the new program as it started stays.
    pub fn forget_stale_title(&mut self) {
        if self.title_at.is_none_or(|at| at.elapsed() > TITLE_GRACE) {
            self.title = None;
        }
    }

    pub fn handle(&mut self, event: PaneEvent, palette: &TerminalColors) {
        match event {
            PaneEvent::Output(bytes) => {
                if !self.replaying {
                    self.last_output = Instant::now();
                }
                self.parser.advance(&mut self.term, &bytes);
                self.drain_events(palette);
            }
            PaneEvent::Exited(code) => self.exit = Some(code),
        }
    }

    fn drain_events(&mut self, palette: &TerminalColors) {
        let events: Vec<Event> = self.events.0.borrow_mut().drain(..).collect();
        for event in events {
            if self.replaying && !matches!(event, Event::Title(_) | Event::ResetTitle) {
                continue;
            }
            match event {
                Event::Title(title) => {
                    self.title = Some(sanitize_title(&title));
                    // A replayed title's age is unknown, so it counts as old.
                    self.title_at = (!self.replaying).then(Instant::now);
                }
                Event::ResetTitle => self.title = None,
                Event::PtyWrite(reply) => self.transport.write(reply.into_bytes()),
                Event::ColorRequest(index, format) => {
                    let color = match index {
                        256 => palette.foreground,
                        257 => palette.background,
                        258 => palette.cursor,
                        i => colors::indexed(i, self.term.colors(), palette),
                    };
                    self.transport
                        .write(format(colors::hsla_to_rgb(color)).into_bytes());
                }
                Event::TextAreaSizeRequest(format) => {
                    self.transport
                        .write(format(self.window_size()).into_bytes());
                }
                Event::Bell => self.bell = true,
                Event::ClipboardStore(_, text) if self.allow_clipboard => {
                    self.clipboard_write = Some(text)
                }
                Event::ClipboardStore(_, text) => self.blocked_clipboard = Some(text),
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
        self.transport.write(bytes);
    }

    /// Grid point and cell half under a viewport position given in cells.
    pub fn point_at(&self, col: f32, row: f32) -> (Point, Side) {
        let cols = self.term.columns();
        let rows = self.term.screen_lines();
        let c = (col.max(0.) as usize).min(cols.saturating_sub(1));
        let r = (row.max(0.) as usize).min(rows.saturating_sub(1));
        let side = if col - c as f32 > 0.5 {
            Side::Right
        } else {
            Side::Left
        };
        let line = Line(r as i32 - self.term.grid().display_offset() as i32);
        (Point::new(line, Column(c)), side)
    }

    pub fn start_selection(&mut self, clicks: usize, point: Point, side: Side) {
        let ty = match clicks {
            2 => SelectionType::Semantic,
            n if n >= 3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        self.term.selection = Some(Selection::new(ty, point, side));
    }

    pub fn update_selection(&mut self, point: Point, side: Side) {
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    pub fn has_selection(&self) -> bool {
        self.term.selection.as_ref().is_some_and(|s| !s.is_empty())
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string().filter(|s| !s.is_empty())
    }

    /// An OSC 8 hyperlink or a bare http(s) URL at a viewport cell.
    pub fn link_at(&self, row: usize, col: usize) -> Option<Link> {
        let offset = self.term.grid().display_offset() as i32;
        let line = Line(row as i32 - offset);
        let cols = self.term.columns();
        if col >= cols || row >= self.term.screen_lines() {
            return None;
        }
        let grid = self.term.grid();
        if let Some(link) = grid[Point::new(line, Column(col))].hyperlink() {
            let same =
                |c: usize| grid[Point::new(line, Column(c))].hyperlink().as_ref() == Some(&link);
            let start = (0..=col)
                .rev()
                .take_while(|&c| same(c))
                .last()
                .unwrap_or(col);
            let end = (col..cols).take_while(|&c| same(c)).last().unwrap_or(col) + 1;
            return Some(Link {
                row,
                start,
                end,
                uri: link.uri().to_string(),
            });
        }
        let chars: Vec<char> = (0..cols)
            .map(|c| grid[Point::new(line, Column(c))].c)
            .collect();
        let (start, end, uri) = links::url_at(&chars, col)?;
        Some(Link {
            row,
            start,
            end,
            uri,
        })
    }

    /// The last `count` lines of scrollback and screen as plain text, trailing blank lines dropped.
    pub fn text_lines(&self, count: usize) -> Vec<String> {
        let grid = self.term.grid();
        let top = -(grid.history_size() as i32);
        let bottom = self.term.screen_lines() as i32;
        let cols = self.term.columns();
        let mut lines: Vec<String> = (top..bottom)
            .map(|l| {
                let row = &grid[Line(l)];
                let text: String = (0..cols)
                    .map(|c| &row[Column(c)])
                    .filter(|cell| {
                        !cell
                            .flags
                            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                    })
                    .map(|cell| cell.c)
                    .collect();
                text.trim_end().to_string()
            })
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        let skip = lines.len().saturating_sub(count);
        lines.split_off(skip)
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
            self.transport.write(vec![0x0c]);
        }
    }

    /// Nudges the PTY size so a full-screen program repaints; replayed bytes alone can leave it stale.
    pub fn force_redraw(&self) {
        if self.mode().contains(TermMode::ALT_SCREEN) && self.size.cols > 1 {
            self.transport.resize(self.size.rows, self.size.cols - 1);
            self.transport.resize(self.size.rows, self.size.cols);
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
            self.transport.resize(size.rows, size.cols);
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
    use std::cell::RefCell;

    use athena_ui::Theme;

    use super::*;

    #[derive(Clone, Default)]
    struct Recorder(Rc<RefCell<Vec<u8>>>);

    impl Transport for Recorder {
        fn write(&self, bytes: Vec<u8>) {
            self.0.borrow_mut().extend(bytes);
        }
        fn resize(&self, _: u16, _: u16) {}
    }

    fn terminal() -> (Terminal, Recorder) {
        let sent = Recorder::default();
        let size = GridSize {
            cols: 40,
            rows: 5,
            cell_width: 8.,
            cell_height: 18.,
        };
        (Terminal::new(size, Box::new(sent.clone())), sent)
    }

    fn feed(t: &mut Terminal, bytes: &[u8]) {
        t.handle(
            PaneEvent::Output(bytes.to_vec()),
            &Theme::dark(false).terminal,
        );
    }

    #[test]
    fn clipboard_writes_wait_for_permission() {
        let (mut t, _) = terminal();
        feed(&mut t, b"\x1b]52;c;aGk=\x07");
        assert_eq!(t.blocked_clipboard.as_deref(), Some("hi"));
        assert_eq!(t.clipboard_write, None);
        t.allow_clipboard = true;
        feed(&mut t, b"\x1b]52;c;eW8=\x07");
        assert_eq!(t.clipboard_write.as_deref(), Some("yo"));
    }

    #[test]
    fn clipboard_reads_are_never_answered() {
        let (mut t, sent) = terminal();
        t.allow_clipboard = true;
        feed(&mut t, b"\x1b]52;c;?\x07");
        assert!(sent.0.borrow().is_empty());
    }

    #[test]
    fn replayed_queries_are_not_answered() {
        let (mut t, sent) = terminal();
        t.replaying = true;
        feed(&mut t, b"\x1b[c");
        assert!(sent.0.borrow().is_empty());
        t.replaying = false;
        feed(&mut t, b"\x1b[c");
        assert!(
            !sent.0.borrow().is_empty(),
            "live device-attribute query gets a reply"
        );
    }

    #[test]
    fn selection_copies_text() {
        let (mut t, _) = terminal();
        feed(&mut t, b"hello world\r\nsecond");
        let (start, side) = t.point_at(0., 0.);
        t.start_selection(1, start, side);
        let (end, side) = t.point_at(4.9, 0.);
        t.update_selection(end, side);
        assert_eq!(t.selection_text().as_deref(), Some("hello"));
        let (word, side) = t.point_at(7., 0.);
        t.start_selection(2, word, side);
        assert_eq!(t.selection_text().as_deref(), Some("world"));
    }

    #[test]
    fn reads_back_scrollback_text() {
        let (mut t, _) = terminal();
        for i in 0..12 {
            feed(&mut t, format!("line {i}\r\n").as_bytes());
        }
        assert_eq!(t.text_lines(3), vec!["line 9", "line 10", "line 11"]);
        assert_eq!(t.text_lines(100).len(), 12, "history included");
    }

    #[test]
    fn finds_osc8_and_bare_links() {
        let (mut t, _) = terminal();
        feed(
            &mut t,
            b"\x1b]8;;https://tlsc.io\x1b\\site\x1b]8;;\x1b\\ x\r\ngo http://localhost:3000 now",
        );
        let link = t.link_at(0, 2).unwrap();
        assert_eq!(
            (link.start, link.end, link.uri.as_str()),
            (0, 4, "https://tlsc.io")
        );
        let bare = t.link_at(1, 8).unwrap();
        assert_eq!(bare.uri, "http://localhost:3000");
        assert!(t.link_at(1, 0).is_none());
    }
    #[test]
    fn a_title_set_as_the_program_starts_survives_the_foreground_change() {
        let (mut t, _) = terminal();
        feed(&mut t, b"\x1b]0;claude: fix tests\x07");
        t.forget_stale_title();
        assert_eq!(t.title.as_deref(), Some("claude: fix tests"));
        t.title_at = Instant::now().checked_sub(TITLE_GRACE * 2);
        t.forget_stale_title();
        assert_eq!(t.title, None);
    }

    #[test]
    fn a_replayed_title_counts_as_old() {
        let (mut t, _) = terminal();
        t.replaying = true;
        feed(&mut t, b"\x1b]0;finished script\x07");
        t.replaying = false;
        assert_eq!(t.title.as_deref(), Some("finished script"));
        t.forget_stale_title();
        assert_eq!(t.title, None);
    }

    #[test]
    fn titles_lose_control_characters_and_length() {
        assert_eq!(sanitize_title("a\x1b]0;b\x07c"), "a]0;bc");
        assert_eq!(sanitize_title(&"x".repeat(1000)).len(), MAX_TITLE);
    }
}
