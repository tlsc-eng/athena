use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{self, Osc52, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{
    ClearMode, Handler, NamedPrivateMode, PrivateMode, Processor, StdSyncHandler,
};
use athena_ui::TerminalColors;

use crate::marks::{self, Command, Commands, Mark, Scanner};
use crate::{colors, links, search};

pub(crate) const SCROLLBACK_LINES: usize = 10_000;
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

/// Viewport rows changed since the last frame.
#[derive(Debug, PartialEq)]
pub enum Damage {
    Full,
    Rows(Vec<usize>),
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
    marks: Scanner,
    commands: Commands,
    /// Where the last `A` mark put the prompt, from [`Self::cursor_abs`], until it is tagged.
    prompt_start: Option<usize>,
    /// The newest tagged prompt: its command id and, until its command starts, where it was tagged.
    prompt: Option<(u32, Option<usize>)>,
    /// Lines pushed out of the full scrollback while a position was held.
    dropped: usize,
    /// A [`marks::PROBE`] is still in the main screen's scrollback.
    probe_out: bool,
}

/// Why the last command's output cannot be copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoOutput {
    /// No command finished with shell-integration marks in this terminal.
    NoCommand,
    /// The shell marked the end of the command but not where its output began.
    Unmarked,
    /// The command's prompt has scrolled out of the scrollback.
    Gone,
    /// The terminal's width changed since, so its output rewrapped.
    Reflowed,
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
            marks: Scanner::default(),
            commands: Commands::default(),
            prompt_start: None,
            prompt: None,
            dropped: 0,
            probe_out: false,
        }
    }

    pub fn term(&self) -> &Term<Listener> {
        &self.term
    }

    /// What changed since the last call, which forgets it.
    pub fn take_damage(&mut self) -> Damage {
        let damage = match self.term.damage() {
            TermDamage::Full => Damage::Full,
            TermDamage::Partial(lines) => Damage::Rows(lines.map(|l| l.line).collect()),
        };
        self.term.reset_damage();
        damage
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
                // Each mark is recorded where the cursor is once the bytes before it are parsed.
                let mut from = 0;
                for (end, mark) in self.marks.feed(&bytes) {
                    self.parse(&bytes[from..end]);
                    from = end;
                    self.on_mark(mark);
                }
                self.parse(&bytes[from..]);
                self.tag_prompt(false);
                self.drain_events(palette);
            }
            PaneEvent::Exited(code) => self.exit = Some(code),
        }
    }

    fn on_mark(&mut self, mark: Mark) {
        if self.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        match mark {
            Mark::Prompt => {
                if let Some((id, _)) = self.prompt
                    && self
                        .commands
                        .get(id)
                        .is_some_and(|c| c.output_start.is_none() && c.exit.is_none())
                {
                    self.commands.forget(id);
                }
                self.prompt_start = Some(self.cursor_abs());
            }
            Mark::Command => self.tag_prompt(true),
            Mark::Output => {
                self.tag_prompt(true);
                let cursor = self.term.grid().cursor.point.line;
                if let Some((id, line)) = self.find_prompt(true)
                    && let Some(command) = self.commands.get_mut(id)
                {
                    command.output_start = Some(cursor.0 - line.0);
                }
                // Only a redraw before the command starts needs it, and holding it costs a probe.
                if let Some((_, abs)) = &mut self.prompt {
                    *abs = None;
                }
            }
            Mark::Finished(code) => {
                let cursor = self.term.grid().cursor.point;
                let end = cursor.line.0 + i32::from(cursor.column.0 > 0);
                // Output can be long enough to move the prompt, so it is looked up, not assumed.
                let found = self.find_prompt(false);
                if let Some(command) = self.prompt.and_then(|(id, _)| self.commands.get_mut(id)) {
                    command.exit = Some(code);
                    command.output_end = found.map(|(_, line)| end - line.0);
                }
            }
        }
    }

    fn cursor_abs(&self) -> usize {
        let grid = self.term.grid();
        self.dropped + grid.history_size() + grid.cursor.point.line.0.max(0) as usize
    }

    /// The grid line an absolute position from [`Self::cursor_abs`] is on now, if still shown.
    fn line_at_abs(&self, abs: usize) -> Option<Line> {
        let from_top = i32::try_from(abs.checked_sub(self.dropped)?).ok()?;
        let line = Line(from_top - self.term.grid().history_size() as i32);
        (self.term.topmost_line() <= line && line <= self.term.bottommost_line()).then_some(line)
    }

    fn holds_position(&self) -> bool {
        self.prompt_start.is_some() || self.prompt.is_some_and(|(_, abs)| abs.is_some())
    }

    fn forget_positions(&mut self) {
        self.prompt_start = None;
        if let Some((_, abs)) = &mut self.prompt {
            *abs = None;
        }
    }

    fn parse(&mut self, bytes: &[u8]) {
        self.counting_drops(|t| t.parser.advance(&mut t.term, bytes));
    }

    /// Runs `change`, adding the lines it pushed out of a full scrollback to `dropped`, which
    /// alacritty does not count; held positions are forgotten when that cannot be told.
    fn counting_drops(&mut self, change: impl FnOnce(&mut Self)) {
        let main = !self.mode().contains(TermMode::ALT_SCREEN);
        if main && self.probe_out {
            // Left behind when a program switched to the alternate screen.
            self.take_probe();
        }
        let held = self.holds_position();
        let before = self.term.grid().history_size();
        if held && main && before > 0 {
            self.term.grid_mut()[Line(-1)][Column(0)].push_zerowidth(marks::PROBE);
            self.probe_out = true;
        }
        change(self);
        if !held {
            return;
        }
        if self.mode().contains(TermMode::ALT_SCREEN) {
            if main {
                self.forget_positions();
            }
            return;
        }
        let after = self.term.grid().history_size();
        let moved = if self.probe_out {
            self.take_probe()
        } else {
            None
        };
        let gone = match moved {
            Some(up) if main => usize::try_from(before as isize + up - after as isize).ok(),
            None if main && before == 0 && after < SCROLLBACK_LINES => Some(0),
            _ => None,
        };
        match gone {
            Some(lines) => self.dropped += lines,
            None => self.forget_positions(),
        }
    }

    /// Removes the probe and returns how many lines above the newest scrollback line it now is.
    fn take_probe(&mut self) -> Option<isize> {
        self.probe_out = false;
        let top = self.term.topmost_line();
        let mut line = self.term.bottommost_line();
        while line >= top {
            let cell = &mut self.term.grid_mut()[line][Column(0)];
            if let Some(zerowidth) = cell.zerowidth()
                && zerowidth.contains(&marks::PROBE)
            {
                let kept: Vec<char> = zerowidth
                    .iter()
                    .copied()
                    .filter(|&c| c != marks::PROBE)
                    .collect();
                // Cell has no way to remove one zero-width character, only all of them.
                let (c, flags) = (cell.c, cell.flags);
                cell.clear_wide();
                (cell.c, cell.flags) = (c, flags);
                kept.into_iter().for_each(|c| cell.push_zerowidth(c));
                return Some(-1 - line.0 as isize);
            }
            line -= 1;
        }
        None
    }

    /// Tags the line the last `A` mark started on, once the prompt is drawn there or `now`.
    fn tag_prompt(&mut self, now: bool) {
        let Some(line) = self.prompt_start.and_then(|abs| self.line_at_abs(abs)) else {
            return;
        };
        let cell = &self.term.grid()[line][Column(0)];
        if !now && cell.c == ' ' && cell.zerowidth().is_none() {
            return;
        }
        let id = self.commands.start();
        self.term.grid_mut()[line][Column(0)].push_zerowidth(marks::tag(id));
        self.prompt = self.prompt_start.take().map(|abs| (id, Some(abs)));
    }

    /// The command tagged on a grid line.
    pub fn tag_at(&self, line: Line) -> Option<u32> {
        self.term.grid()[line][Column(0)]
            .zerowidth()?
            .iter()
            .find_map(|&c| marks::tag_id(c))
    }

    /// The newest prompt's line, searched up from the cursor; `retag` puts the tag back when the
    /// shell redrew the prompt (a transient prompt) since it was tagged.
    fn find_prompt(&mut self, retag: bool) -> Option<(u32, Line)> {
        let (id, abs) = self.prompt?;
        let top = self.term.topmost_line();
        let mut line = self.term.grid().cursor.point.line;
        while line >= top {
            if self.tag_at(line) == Some(id) {
                return Some((id, line));
            }
            line -= 1;
        }
        let line = abs
            .and_then(|abs| self.line_at_abs(abs))
            .filter(|_| retag)?;
        self.term.grid_mut()[line][Column(0)].push_zerowidth(marks::tag(id));
        Some((id, line))
    }

    /// The command whose prompt starts on a grid line, for its gutter mark.
    pub fn command_at(&self, line: Line) -> Option<Command> {
        self.commands.get(self.tag_at(line)?).copied()
    }

    /// Scrolls the previous (or next) prompt to the top of the view; past the last one it
    /// returns to the live screen. False when there is nowhere to go.
    pub fn scroll_to_prompt(&mut self, up: bool) -> bool {
        let offset = self.term.grid().display_offset() as i32;
        let top = Line(-offset);
        let lines = self.term.topmost_line().0..=self.term.bottommost_line().0;
        let mut prompts = lines.map(Line).filter(|&l| self.tag_at(l).is_some());
        let target = if up {
            prompts.rfind(|&l| l < top)
        } else {
            prompts.find(|&l| l > top)
        };
        let wanted = match target {
            Some(line) => (-line.0).max(0),
            None if up => return false,
            None => 0,
        };
        if wanted == offset {
            return false;
        }
        self.term.scroll_display(Scroll::Delta(wanted - offset));
        true
    }

    /// The text the newest finished command printed, as the shell marked it.
    pub fn last_output(&self) -> Result<String, NoOutput> {
        let (id, command) = self
            .commands
            .newest_first()
            .find_map(|id| Some((id, *self.commands.get(id)?)).filter(|(_, c)| c.exit.is_some()))
            .ok_or(NoOutput::NoCommand)?;
        if command.reflowed {
            return Err(NoOutput::Reflowed);
        }
        let (Some(start), Some(end)) = (command.output_start, command.output_end) else {
            return Err(if command.output_start.is_none() {
                NoOutput::Unmarked
            } else {
                NoOutput::Gone
            });
        };
        let top = self.term.topmost_line();
        let mut line = self.term.bottommost_line();
        while line >= top && self.tag_at(line) != Some(id) {
            line -= 1;
        }
        if line < top {
            return Err(NoOutput::Gone);
        }
        let (first, last) = (Line(line.0 + start), Line(line.0 + end - 1));
        if last < first {
            return Ok(String::new());
        }
        let text = self.term.bounds_to_string(
            Point::new(first, Column(0)),
            Point::new(
                last.min(self.term.bottommost_line()),
                self.term.last_column(),
            ),
        );
        Ok(strip_tags(&text).trim_end_matches('\n').to_string())
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
        self.term
            .selection_to_string()
            .map(|s| strip_tags(&s))
            .filter(|s| !s.is_empty())
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

    /// A file named in plain output at a viewport cell, such as `src/main.go:12:5`.
    pub fn file_at(&self, row: usize, col: usize) -> Option<(Link, links::FileRef)> {
        let line = Line(row as i32 - self.term.grid().display_offset() as i32);
        let cols = self.term.columns();
        if col >= cols || row >= self.term.screen_lines() {
            return None;
        }
        let grid = self.term.grid();
        let chars: Vec<char> = (0..cols)
            .map(|c| grid[Point::new(line, Column(c))].c)
            .collect();
        let (start, end, file) = links::file_at(&chars, col)?;
        let link = Link {
            row,
            start,
            end,
            uri: chars[start..end].iter().collect(),
        };
        Some((link, file))
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

    /// Clears scrollback and, outside full-screen programs, the screen above the cursor line.
    pub fn clear_scrollback(&mut self, at_prompt: bool) {
        self.term.clear_screen(ClearMode::Saved);
        self.forget_positions();
        if self.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        if at_prompt {
            self.transport.write(vec![0x0c]);
            return;
        }
        // A running program ignores ^L, so its cursor line moves to the top here instead, unless
        // the program set a scroll region that would shuffle its lines rather than drop them.
        let row = self.term.grid().cursor.point.line.0;
        let region = self.scroll_region();
        if row > 0 && region.start == 0 && row < region.end {
            self.term.scroll_up(row as usize);
            self.term.move_up(row as usize);
            self.term.clear_screen(ClearMode::Saved);
        }
    }

    /// The DECSTBM scroll region in screen lines, which alacritty only reveals through origin mode.
    fn scroll_region(&mut self) -> Range<i32> {
        let cursor = self.term.grid().cursor.clone();
        let origin = PrivateMode::Named(NamedPrivateMode::Origin);
        let was_set = self.mode().contains(TermMode::ORIGIN);
        self.term.set_private_mode(origin);
        let top = self.term.grid().cursor.point.line.0;
        self.term.goto(self.term.screen_lines() as i32, 0);
        let bottom = self.term.grid().cursor.point.line.0 + 1;
        if !was_set {
            self.term.unset_private_mode(origin);
        }
        self.term.grid_mut().cursor = cursor;
        top..bottom
    }

    pub fn select_all(&mut self) {
        let start = Point::new(self.term.topmost_line(), Column(0));
        let end = Point::new(self.term.bottommost_line(), self.term.last_column());
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        self.term.selection = Some(selection);
    }

    /// Scrolls the display so a grid line is in view.
    pub fn reveal(&mut self, line: Line) {
        let delta = search::reveal_delta(&self.term, line);
        if delta != 0 {
            self.term.scroll_display(Scroll::Delta(delta));
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
        let reflow = size.cols != self.size.cols;
        let grid_changed = reflow || size.rows != self.size.rows;
        self.size = size;
        if grid_changed {
            let dims = TermSize::new(size.cols as usize, size.rows as usize);
            if reflow || self.mode().contains(TermMode::ALT_SCREEN) {
                self.term.resize(dims);
                self.forget_positions();
            } else {
                self.counting_drops(|t| t.term.resize(dims));
            }
            if reflow {
                self.commands.reflow();
            }
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

/// `text` without the zero-width tags marking prompt lines.
fn strip_tags(text: &str) -> String {
    text.chars().filter(|&c| !marks::is_athenas(c)).collect()
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
    fn damage_names_the_rows_written_and_is_forgotten_once_taken() {
        let (mut t, _) = terminal();
        assert_eq!(
            t.take_damage(),
            Damage::Full,
            "a new terminal draws everything"
        );
        feed(&mut t, b"\x1b[3;1Hhi");
        let Damage::Rows(rows) = t.take_damage() else {
            panic!("expected partial damage")
        };
        assert!(rows.contains(&2), "{rows:?}");
        assert!(rows.iter().all(|&r| r == 0 || r == 2), "{rows:?}");
        let Damage::Rows(rows) = t.take_damage() else {
            panic!("expected partial damage")
        };
        assert_eq!(rows, [2], "only the cursor row once nothing was written");
    }

    #[test]
    fn scrolling_back_and_resizing_damage_everything() {
        let (mut t, _) = terminal();
        for i in 0..12 {
            feed(&mut t, format!("line {i}\r\n").as_bytes());
        }
        let _ = t.take_damage();
        t.scroll(2);
        assert_eq!(t.take_damage(), Damage::Full);
        t.resize(GridSize {
            cols: 30,
            ..t.size()
        });
        assert_eq!(t.take_damage(), Damage::Full);
    }

    #[test]
    fn clearing_at_the_prompt_drops_scrollback_and_asks_the_shell_to_redraw() {
        let (mut t, sent) = terminal();
        for i in 0..12 {
            feed(&mut t, format!("line {i}\r\n").as_bytes());
        }
        t.clear_scrollback(true);
        assert_eq!(t.term().grid().history_size(), 0);
        assert_eq!(*sent.0.borrow(), [0x0c]);
    }

    #[test]
    fn clearing_under_a_running_program_keeps_only_its_cursor_line() {
        let (mut t, sent) = terminal();
        for i in 0..12 {
            feed(&mut t, format!("line {i}\r\n").as_bytes());
        }
        feed(&mut t, b"progress 40%");
        t.clear_scrollback(false);
        assert!(sent.0.borrow().is_empty(), "the program is left alone");
        assert_eq!(t.text_lines(100), ["progress 40%"]);
        assert_eq!(t.term().grid().cursor.point.line, Line(0));
        feed(&mut t, b"\rprogress 50%\r\nnext");
        assert_eq!(t.text_lines(100), ["progress 50%", "next"]);
    }

    #[test]
    fn clearing_inside_a_scroll_region_below_the_top_leaves_the_screen() {
        let (mut t, _) = terminal();
        for i in 0..8 {
            feed(&mut t, format!("line {i}\r\n").as_bytes());
        }
        feed(&mut t, b"\x1b[Hheader\x1b[2;5r\x1b[4;1Hbody");
        t.clear_scrollback(false);
        assert_eq!(t.term().grid().history_size(), 0);
        assert_eq!(
            t.text_lines(100),
            ["header", "line 5", "line 6", "body 7"],
            "no line moves inside the region"
        );
        assert_eq!(t.term().grid().cursor.point, Point::new(Line(3), Column(4)));
        feed(&mut t, b"\x1b[5;1H\n");
        assert_eq!(
            t.text_lines(100),
            ["header", "line 6", "body 7"],
            "the region the program set still holds"
        );
    }

    #[test]
    fn clearing_above_a_status_line_keeps_the_status_line() {
        let (mut t, _) = terminal();
        feed(
            &mut t,
            b"\x1b[5;1Hstatus\x1b[1;4r\x1b[1;1Hone\r\ntwo\r\nthree",
        );
        t.clear_scrollback(false);
        assert_eq!(t.text_lines(100), ["three", "", "", "", "status"]);
        assert_eq!(t.term().grid().cursor.point.line, Line(0));
        assert!(
            !t.mode().contains(TermMode::ORIGIN),
            "probing the region leaves no mode set"
        );
    }

    #[test]
    fn clearing_on_a_status_line_below_the_region_leaves_the_screen() {
        let (mut t, _) = terminal();
        feed(&mut t, b"one\r\ntwo\x1b[1;4r\x1b[5;1Hstatus");
        let screen = t.text_lines(5);
        t.clear_scrollback(false);
        assert_eq!(t.text_lines(100), screen);
        assert_eq!(t.term().grid().cursor.point.line, Line(4));
    }

    #[test]
    fn clearing_a_full_screen_program_leaves_its_screen() {
        let (mut t, sent) = terminal();
        feed(&mut t, b"\x1b[?1049hmenu");
        t.clear_scrollback(true);
        assert!(sent.0.borrow().is_empty());
        assert_eq!(t.text_lines(100), ["menu"]);
    }

    #[test]
    fn select_all_covers_scrollback_and_screen() {
        let (mut t, _) = terminal();
        for i in 0..8 {
            feed(&mut t, format!("line {i}\r\n").as_bytes());
        }
        feed(&mut t, b"$ ");
        t.select_all();
        let text = t.selection_text().unwrap();
        assert!(text.starts_with("line 0\nline 1\n"), "{text:?}");
        assert!(text.trim_end().ends_with("line 7\n$"), "{text:?}");
    }

    #[test]
    fn titles_lose_control_characters_and_length() {
        assert_eq!(sanitize_title("a\x1b]0;b\x07c"), "a]0;bc");
        assert_eq!(sanitize_title(&"x".repeat(1000)).len(), MAX_TITLE);
    }

    const PROMPT: &[u8] = b"\x1b]133;A\x07$ \x1b]133;B\x07";

    fn run(t: &mut Terminal, command: &str, output: &[&str], code: i32) {
        feed(t, PROMPT);
        feed(t, format!("{command}\r\n\x1b]133;C\x07").as_bytes());
        for line in output {
            feed(t, format!("{line}\r\n").as_bytes());
        }
        feed(t, format!("\x1b]133;D;{code}\x07").as_bytes());
    }

    fn prompts(t: &Terminal) -> Vec<(i32, Option<Option<i32>>)> {
        (t.term.topmost_line().0..=t.term.bottommost_line().0)
            .filter_map(|l| Some((l, t.command_at(Line(l))?.exit)))
            .collect()
    }

    #[test]
    fn shell_marks_tag_prompts_with_their_exit_status_and_output() {
        let (mut t, _) = terminal();
        run(&mut t, "ls", &["a", "b"], 0);
        run(&mut t, "false", &[], 1);
        feed(&mut t, PROMPT);
        assert_eq!(
            prompts(&t),
            [(0, Some(Some(0))), (3, Some(Some(1))), (4, None)]
        );
        assert_eq!(t.last_output(), Ok(String::new()));
        let (mut t, _) = terminal();
        run(&mut t, "ls", &["a", "b"], 0);
        assert_eq!(t.last_output(), Ok("a\nb".to_string()));
    }

    #[test]
    fn tags_never_reach_copied_text() {
        let (mut t, _) = terminal();
        run(&mut t, "ls", &["a"], 0);
        t.select_all();
        let text = t.selection_text().unwrap();
        assert!(text.starts_with("$ ls\na"), "{text:?}");
        assert!(text.chars().all(|c| marks::tag_id(c).is_none()));
    }

    #[test]
    fn nerd_font_icons_survive_copying() {
        let (mut t, _) = terminal();
        run(&mut t, "ls", &["\u{F0493} src", "\u{E5FF} \u{F1AF0}"], 0);
        assert_eq!(
            t.last_output(),
            Ok("\u{F0493} src\n\u{E5FF} \u{F1AF0}".to_string())
        );
        t.select_all();
        assert!(t.selection_text().unwrap().contains("\u{F0493} src"));
    }

    #[test]
    fn marks_split_across_chunks_land_in_the_same_place() {
        let (mut whole, _) = terminal();
        run(&mut whole, "ls", &["a", "b"], 2);
        let (mut bytewise, _) = terminal();
        let mut bytes = PROMPT.to_vec();
        bytes.extend(b"ls\r\n\x1b]133;C\x07a\r\nb\r\n\x1b]133;D;2\x1b\\");
        for b in bytes {
            feed(&mut bytewise, &[b]);
        }
        assert_eq!(prompts(&bytewise), prompts(&whole));
        assert_eq!(bytewise.last_output(), Ok("a\nb".to_string()));
    }

    #[test]
    fn a_redrawn_prompt_is_tagged_again_when_the_command_starts() {
        let (mut t, _) = terminal();
        feed(&mut t, PROMPT);
        feed(
            &mut t,
            b"ls\r\x1b[2K> ls\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07",
        );
        assert_eq!(prompts(&t), [(0, Some(Some(0)))]);
        assert_eq!(t.last_output(), Ok("out".to_string()));
    }

    #[test]
    fn prompts_are_found_after_the_scrollback_fills() {
        let (mut t, _) = terminal();
        let filler: Vec<String> = (0..SCROLLBACK_LINES + 50).map(|i| format!("{i}")).collect();
        let filler: Vec<&str> = filler.iter().map(String::as_str).collect();
        run(&mut t, "seq", &filler, 0);
        assert_eq!(
            t.last_output(),
            Err(NoOutput::Gone),
            "its prompt was dropped"
        );
        run(&mut t, "printf", &["x", "y"], 0);
        let output: Vec<String> = (0..300).map(|i| format!("line {i}")).collect();
        let output: Vec<&str> = output.iter().map(String::as_str).collect();
        run(&mut t, "long", &output, 0);
        assert_eq!(t.term.grid().history_size(), SCROLLBACK_LINES);
        assert_eq!(t.last_output(), Ok(output.join("\n")));

        feed(&mut t, PROMPT);
        assert!(t.scroll_to_prompt(true));
        let top = Line(-(t.term.grid().display_offset() as i32));
        assert!(
            t.command_at(top).is_some(),
            "the long command's prompt is at the top"
        );
        assert!(t.scroll_to_prompt(true));
        let higher = Line(-(t.term.grid().display_offset() as i32));
        assert!(higher < top && t.command_at(higher).is_some());
        assert!(t.scroll_to_prompt(false));
        assert_eq!(Line(-(t.term.grid().display_offset() as i32)), top);
        assert!(
            t.scroll_to_prompt(false),
            "past the last prompt is the live screen"
        );
        assert_eq!(t.term.grid().display_offset(), 0);
        assert!(!t.scroll_to_prompt(false));
    }

    fn fill_scrollback(t: &mut Terminal) {
        feed(t, "x\r\n".repeat(SCROLLBACK_LINES + 10).as_bytes());
        assert_eq!(t.term.grid().history_size(), SCROLLBACK_LINES);
    }

    #[test]
    fn a_redrawn_prompt_is_found_once_the_scrollback_is_full() {
        let (mut t, _) = terminal();
        fill_scrollback(&mut t);
        for _ in 0..2 {
            feed(&mut t, PROMPT);
            feed(
                &mut t,
                b"ls\r\x1b[2K> ls\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07",
            );
            assert_eq!(t.last_output(), Ok("out".to_string()));
        }
        let shown = t.term.topmost_line().0..=t.term.bottommost_line().0;
        let probes = shown
            .flat_map(|l| {
                t.term.grid()[Line(l)][Column(0)]
                    .zerowidth()
                    .unwrap_or_default()
                    .to_vec()
            })
            .filter(|&c| c == marks::PROBE)
            .count();
        assert_eq!(probes, 0, "no probe is left in the grid");
    }

    #[test]
    fn growing_the_window_keeps_a_held_prompt_and_rewrapping_forgets_output_lines() {
        let (mut t, _) = terminal();
        fill_scrollback(&mut t);
        feed(&mut t, PROMPT);
        let mut taller = t.size();
        taller.rows += 3;
        t.resize(taller);
        feed(
            &mut t,
            b"ls\r\x1b[2K> ls\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07",
        );
        assert_eq!(t.last_output(), Ok("out".to_string()));
        let mut narrower = t.size();
        narrower.cols -= 10;
        t.resize(narrower);
        assert_eq!(t.last_output(), Err(NoOutput::Reflowed));
        run(&mut t, "ls", &["a"], 0);
        assert_eq!(t.last_output(), Ok("a".to_string()));
    }

    #[test]
    fn athenas_own_zsh_marks_without_b_are_enough() {
        let (mut t, _) = terminal();
        // precmd sends A before zle draws the prompt, in a later write; preexec sends C with the
        // command line in base64.
        feed(&mut t, b"\x1b]133;A\x07");
        feed(&mut t, b"% ");
        feed(&mut t, b"ls\r\n\x1b]133;C;bHM=\x07a\r\n");
        feed(&mut t, b"\x1b]133;D;0\x07\x1b]133;A\x07");
        feed(&mut t, b"% ");
        assert_eq!(prompts(&t), [(0, Some(Some(0))), (2, None)]);
        assert_eq!(t.last_output(), Ok("a".to_string()));
    }

    #[test]
    fn a_prompt_left_without_running_anything_gets_no_mark() {
        let (mut t, _) = terminal();
        feed(
            &mut t,
            b"\x1b]133;A\x07% \r\n\x1b]133;A\x07% ^C\r\n\x1b]133;A\x07% ",
        );
        assert_eq!(prompts(&t), [(2, None)]);
    }

    #[test]
    fn full_screen_programs_and_unmarked_output_are_left_alone() {
        let (mut t, _) = terminal();
        assert_eq!(t.last_output(), Err(NoOutput::NoCommand));
        feed(
            &mut t,
            b"\x1b[?1049h\x1b]133;A\x07$ \x1b]133;B\x07\x1b[?1049l",
        );
        assert!(prompts(&t).is_empty());
        feed(&mut t, PROMPT);
        feed(&mut t, b"ls\r\nx\r\n\x1b]133;D;0\x07");
        assert_eq!(t.last_output(), Err(NoOutput::Unmarked));
    }
}
