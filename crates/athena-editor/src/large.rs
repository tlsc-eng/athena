use std::cell::Cell;
use std::fs::File;
use std::io;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::SystemTime;

use athena_ui::{ActiveTheme, InputEvent, TextInput, empty_state};
use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, HighlightStyle, KeyBinding, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Render, ScrollWheelEvent, StyledText, Subscription,
    Task, Window, actions, canvas, div, prelude::*, px,
};

use crate::buffer::MAX_FILE;

/// Larger files are not opened at all.
const MAX_LARGE: u64 = 2 * 1024 * 1024 * 1024;
/// The index notes where a line starts at least every this many lines…
const STRIDE: u64 = 1024;
/// …and every this many bytes, so finding any line's start never reads far.
const MARK_BYTES: u64 = 64 * 1024;
/// Bytes of a line shown; the rest of a longer line is cut off.
const SHOWN_BYTES: usize = 16 * 1024;
/// Bytes indexed per background step, between redraws that show progress.
const INDEX_STEP: u64 = 64 * 1024 * 1024;
const BLOCK: usize = 1024 * 1024;
const FIND_BLOCK: u64 = 4 * 1024 * 1024;
/// Bytes past a find block's end read with it, so a match across the boundary is still seen.
const FIND_OVERLAP: u64 = 4096;
const LINE_HEIGHT_RATIO: f32 = 1.5;
const SCROLLBAR: f32 = 10.;

actions!(
    large_file,
    [LineUp, LineDown, PageUp, PageDown, FileStart, FileEnd, Find]
);

pub(crate) fn init(cx: &mut App) {
    let ctx = Some("LargeFile");
    cx.bind_keys([
        KeyBinding::new("up", LineUp, ctx),
        KeyBinding::new("down", LineDown, ctx),
        KeyBinding::new("pageup", PageUp, ctx),
        KeyBinding::new("pagedown", PageDown, ctx),
        KeyBinding::new("cmd-up", FileStart, ctx),
        KeyBinding::new("cmd-down", FileEnd, ctx),
        KeyBinding::new("cmd-f", Find, ctx),
    ]);
}

/// Whether `path` is too large for the editor and opens in [`LargeFileView`] instead.
pub fn is_large_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > MAX_FILE)
}

/// Where lines start in a file, filled in step by step so the file is usable while it is read.
#[derive(Clone, Debug)]
struct Index {
    /// (line, offset of its first byte), ascending from (0, 0).
    marks: Vec<(u64, u64)>,
    breaks: u64,
    scanned: u64,
    done: bool,
}

impl Index {
    fn new() -> Self {
        Self {
            marks: vec![(0, 0)],
            breaks: 0,
            scanned: 0,
            done: false,
        }
    }

    /// Reads up to `budget` more bytes of `file`, which is `len` long.
    fn step(&mut self, file: &File, len: u64, budget: u64) -> io::Result<()> {
        let mut buf = vec![0; BLOCK];
        let end = (self.scanned + budget).min(len);
        let mut last = *self.marks.last().unwrap_or(&(0, 0));
        while self.scanned < end {
            let want = ((end - self.scanned) as usize).min(BLOCK);
            let n = file.read_at(&mut buf[..want], self.scanned)?;
            if n == 0 {
                // The file shrank since it was measured.
                self.done = true;
                return Ok(());
            }
            for (i, _) in buf[..n].iter().enumerate().filter(|(_, b)| **b == b'\n') {
                self.breaks += 1;
                let start = self.scanned + i as u64 + 1;
                if self.breaks - last.0 >= STRIDE || start - last.1 >= MARK_BYTES {
                    last = (self.breaks, start);
                    self.marks.push(last);
                }
            }
            self.scanned += n as u64;
        }
        self.done = self.scanned >= len;
        Ok(())
    }

    /// Lines known so far; the last line, with no break after it, counts once the end is reached.
    fn lines(&self) -> u64 {
        self.breaks + u64::from(self.done)
    }

    fn mark_at_line(&self, line: u64) -> (u64, u64) {
        let i = self.marks.partition_point(|m| m.0 <= line);
        self.marks[i.saturating_sub(1)]
    }

    fn mark_at_offset(&self, offset: u64) -> (u64, u64) {
        let i = self.marks.partition_point(|m| m.1 <= offset);
        self.marks[i.saturating_sub(1)]
    }
}

/// Fills `buf` from `offset` unless the file ends first; the bytes read.
fn read_full_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read_at(&mut buf[n..], offset + n as u64)? {
            0 => break,
            read => n += read,
        }
    }
    Ok(n)
}

/// Where `line` starts, once the index has reached it.
fn line_start(file: &File, index: &Index, line: u64) -> io::Result<Option<u64>> {
    if line >= index.lines() {
        return Ok(None);
    }
    let (mut at, mut offset) = index.mark_at_line(line);
    let mut buf = vec![0; MARK_BYTES as usize];
    while at < line {
        let n = file.read_at(&mut buf, offset)?;
        if n == 0 {
            return Ok(None);
        }
        for (i, _) in buf[..n].iter().enumerate().filter(|(_, b)| **b == b'\n') {
            at += 1;
            if at == line {
                return Ok(Some(offset + i as u64 + 1));
            }
        }
        offset += n as u64;
    }
    Ok(Some(offset))
}

/// The line holding byte `offset`.
fn line_of(file: &File, index: &Index, offset: u64) -> io::Result<u64> {
    let (mut line, mut at) = index.mark_at_offset(offset);
    let mut buf = vec![0; BLOCK];
    while at < offset {
        let want = ((offset - at) as usize).min(BLOCK);
        let n = file.read_at(&mut buf[..want], at)?;
        if n == 0 {
            break;
        }
        line += buf[..n].iter().filter(|b| **b == b'\n').count() as u64;
        at += n as u64;
    }
    Ok(line)
}

/// One line as read for display: where it starts and its first [`SHOWN_BYTES`] bytes.
struct ShownLine {
    start: u64,
    bytes: Vec<u8>,
}

/// Up to `count` lines from `first`, each cut at [`SHOWN_BYTES`] and without its line break.
fn read_lines(file: &File, index: &Index, first: u64, count: usize) -> io::Result<Vec<ShownLine>> {
    let mut out = Vec::new();
    let Some(mut start) = line_start(file, index, first)? else {
        return Ok(out);
    };
    let mut buf = vec![0; SHOWN_BYTES + 1];
    for line in first..(first + count as u64).min(index.lines()) {
        let n = read_full_at(file, &mut buf, start)?;
        let (mut shown, next) = match buf[..n].iter().position(|b| *b == b'\n') {
            Some(i) => (&buf[..i], Some(start + i as u64 + 1)),
            None => (&buf[..n.min(SHOWN_BYTES)], None),
        };
        if let [rest @ .., b'\r'] = shown {
            shown = rest;
        }
        out.push(ShownLine {
            start,
            bytes: shown.to_vec(),
        });
        start = match next {
            Some(next) => next,
            None => match line_start(file, index, line + 1)? {
                Some(next) => next,
                None => break,
            },
        };
    }
    Ok(out)
}

/// Bytes as shown: invalid UTF-8 replaced, tabs expanded to the next multiple of four and other
/// control characters drawn as their Unicode pictures (NUL as ␀).
fn display_text(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut col = 0;
    for c in String::from_utf8_lossy(bytes).chars() {
        if c == '\t' {
            let n = 4 - col % 4;
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(match c {
                '\0'..='\x1f' => char::from_u32(0x2400 + c as u32).unwrap_or(c),
                c => c,
            });
            col += 1;
        }
    }
    out
}

fn read_range(file: &File, range: Range<u64>) -> io::Result<Vec<u8>> {
    let mut buf = vec![0; (range.end - range.start) as usize];
    let n = read_full_at(file, &mut buf, range.start)?;
    buf.truncate(n);
    Ok(buf)
}

/// The first match at or after `from`, wrapping past the end, in blocks so memory stays small.
fn find_forward(
    file: &File,
    len: u64,
    re: &regex::bytes::Regex,
    from: u64,
) -> io::Result<Option<Range<u64>>> {
    for (lo, hi) in [(from.min(len), len), (0, from.min(len))] {
        let mut start = lo;
        while start < hi {
            let end = (start + FIND_BLOCK).min(hi);
            let bytes = read_range(file, start..(end + FIND_OVERLAP).min(len))?;
            if let Some(m) = re
                .find_iter(&bytes)
                .find(|m| (m.start() as u64) < end - start)
            {
                return Ok(Some(start + m.start() as u64..start + m.end() as u64));
            }
            start = end;
        }
    }
    Ok(None)
}

/// The last match starting before `before`, wrapping past the start to the end.
fn find_backward(
    file: &File,
    len: u64,
    re: &regex::bytes::Regex,
    before: u64,
) -> io::Result<Option<Range<u64>>> {
    for (lo, hi) in [(0, before.min(len)), (before.min(len), len)] {
        let mut end = hi;
        while end > lo {
            let start = end.saturating_sub(FIND_BLOCK).max(lo);
            let bytes = read_range(file, start..(end + FIND_OVERLAP).min(len))?;
            if let Some(m) = re
                .find_iter(&bytes)
                .filter(|m| (m.start() as u64) < end - start)
                .last()
            {
                return Ok(Some(start + m.start() as u64..start + m.end() as u64));
            }
            end = start;
        }
    }
    Ok(None)
}

/// A file too large for the editor, read in pages as it scrolls.
struct Opened {
    file: Arc<File>,
    len: u64,
    stamp: (Option<SystemTime>, u64),
    index: Index,
}

/// Checks the file is one this view can show and opens it.
fn open_file(path: &Path) -> Result<Opened, String> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let file = File::open(path).map_err(|e| format!("{name}: {e}"))?;
    let meta = file.metadata().map_err(|e| format!("{name}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{name} is not a regular file"));
    }
    if meta.len() > MAX_LARGE {
        return Err(format!("{name} is larger than 2 GB"));
    }
    let head = read_range(&file, 0..8192).map_err(|e| format!("{name}: {e}"))?;
    if head.contains(&0) {
        return Err(format!("{name} looks like a binary file"));
    }
    Ok(Opened {
        file: Arc::new(file),
        len: meta.len(),
        stamp: (meta.modified().ok(), meta.len()),
        index: Index::new(),
    })
}

struct FindBar {
    input: Entity<TextInput>,
    _events: Subscription,
    hit: Option<Range<u64>>,
    hit_line: Option<u64>,
    note: Option<&'static str>,
    _search: Option<Task<()>>,
}

/// A read-only view of a file over the editor's size limit: no highlighting, language features
/// or folding, but fast scrolling and find over the whole file.
pub struct LargeFileView {
    path: PathBuf,
    focus: FocusHandle,
    opened: Result<Opened, String>,
    /// The first line shown, with a fraction for smooth scrolling.
    top: f64,
    scroll_x: f32,
    viewport: Rc<Cell<Bounds<Pixels>>>,
    find: Option<FindBar>,
    dragging_bar: bool,
    _indexing: Option<Task<()>>,
}

impl Focusable for LargeFileView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl LargeFileView {
    pub fn open(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            opened: open_file(&path),
            path,
            focus: cx.focus_handle(),
            top: 0.,
            scroll_x: 0.,
            viewport: Rc::default(),
            find: None,
            dragging_bar: false,
            _indexing: None,
        };
        view.start_indexing(cx);
        view
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Opens the file again if it changed on disk since it was read.
    pub fn reload_if_changed(&mut self, cx: &mut Context<Self>) {
        let now = std::fs::metadata(&self.path)
            .ok()
            .map(|m| (m.modified().ok(), m.len()));
        if now == self.opened.as_ref().ok().map(|o| o.stamp) {
            return;
        }
        self.opened = open_file(&self.path);
        if let Some(find) = &mut self.find {
            find.hit = None;
            find.hit_line = None;
        }
        self.start_indexing(cx);
        cx.notify();
    }

    fn start_indexing(&mut self, cx: &mut Context<Self>) {
        let Ok(opened) = &self.opened else {
            self._indexing = None;
            return;
        };
        let (file, len) = (opened.file.clone(), opened.len);
        self._indexing = Some(cx.spawn(async move |this, cx| {
            let mut index = Index::new();
            loop {
                let file = file.clone();
                let (stepped, result) = cx
                    .background_spawn(async move {
                        let result = index.step(&file, len, INDEX_STEP);
                        (index, result)
                    })
                    .await;
                index = stepped;
                let failed = result.err().map(|e| format!("{e}"));
                let done = index.done || failed.is_some();
                let shown = index.clone();
                // A reload replaces this task, dropping it, so the index is always for this file.
                let alive = this.update(cx, |this, cx| {
                    match (&mut this.opened, failed) {
                        (Ok(_), Some(e)) => this.opened = Err(e),
                        (Ok(o), None) => o.index = shown,
                        (Err(_), _) => {}
                    }
                    cx.notify();
                });
                if alive.is_err() || done {
                    return;
                }
            }
        }));
    }

    fn lines(&self) -> u64 {
        self.opened.as_ref().map_or(0, |o| o.index.lines())
    }

    fn line_height(&self, cx: &App) -> f32 {
        (f32::from(cx.theme().typography.code) * LINE_HEIGHT_RATIO).round()
    }

    fn visible_lines(&self, cx: &App) -> f64 {
        (f32::from(self.viewport.get().size.height) / self.line_height(cx)) as f64
    }

    fn scroll_to(&mut self, top: f64, cx: &mut Context<Self>) {
        let max = self.lines().saturating_sub(1) as f64;
        self.top = top.clamp(0., max.max(0.));
        cx.notify();
    }

    fn scroll_by_lines(&mut self, lines: f64, cx: &mut Context<Self>) {
        self.scroll_to(self.top + lines, cx);
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let lh = self.line_height(cx);
        let delta = event.delta.pixel_delta(px(lh));
        self.scroll_x = (self.scroll_x - f32::from(delta.x)).clamp(0., SHOWN_BYTES as f32 * 10.);
        self.scroll_by_lines(-f64::from(f32::from(delta.y)) / lh as f64, cx);
    }

    /// Moves the view so the scrollbar's thumb centres on `y`.
    fn scroll_to_bar(&mut self, y: Pixels, cx: &mut Context<Self>) {
        let bounds = self.viewport.get();
        let height = f32::from(bounds.size.height).max(1.);
        let at = (f32::from(y - bounds.top()) / height).clamp(0., 1.) as f64;
        let top = at * self.lines() as f64 - self.visible_lines(cx) / 2.;
        self.scroll_to(top, cx);
    }

    fn bar_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        self.dragging_bar = true;
        self.scroll_to_bar(event.position.y, cx);
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.dragging_bar || event.pressed_button != Some(MouseButton::Left) {
            self.dragging_bar = false;
            return;
        }
        self.scroll_to_bar(event.position.y, cx);
    }

    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(find) = &self.find {
            let input = find.input.clone();
            input.update(cx, |i, cx| i.select_all(cx));
            window.focus(&input.focus_handle(cx));
            return;
        }
        let input = cx.new(|cx| TextInput::new("Find", cx));
        let events =
            cx.subscribe_in(
                &input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Changed => {
                        if let Some(find) = &mut this.find {
                            find.note = None;
                        }
                        cx.notify();
                    }
                    InputEvent::Submit | InputEvent::SubmitBeside | InputEvent::Down => {
                        this.step_find(true, cx)
                    }
                    InputEvent::Up => this.step_find(false, cx),
                    InputEvent::Cancel => {
                        this.find = None;
                        window.focus(&this.focus);
                        cx.notify();
                    }
                },
            );
        window.focus(&input.focus_handle(cx));
        self.find = Some(FindBar {
            input,
            _events: events,
            hit: None,
            hit_line: None,
            note: None,
            _search: None,
        });
        cx.notify();
    }

    /// Searches from the current match (or the top line) in the background, wrapping at the ends.
    fn step_find(&mut self, forward: bool, cx: &mut Context<Self>) {
        let (Ok(opened), Some(find)) = (&self.opened, &mut self.find) else {
            return;
        };
        let query = find.input.read(cx).text().to_string();
        if query.is_empty() {
            return;
        }
        if !opened.index.done {
            find.note = Some("Find works once the file is read");
            return cx.notify();
        }
        let Ok(re) = regex::bytes::RegexBuilder::new(&regex::escape(&query))
            .case_insensitive(true)
            .build()
        else {
            return;
        };
        let from = match (&find.hit, forward) {
            (Some(hit), true) => hit.start + 1,
            (Some(hit), false) => hit.start,
            (None, _) => {
                let line = self.top.floor() as u64;
                line_start(&opened.file, &opened.index, line)
                    .ok()
                    .flatten()
                    .unwrap_or(0)
            }
        };
        let (file, len, index) = (opened.file.clone(), opened.len, opened.index.clone());
        find.note = Some("Searching…");
        find._search = Some(cx.spawn(async move |this, cx| {
            let found = cx
                .background_spawn(async move {
                    let hit = match forward {
                        true => find_forward(&file, len, &re, from),
                        false => find_backward(&file, len, &re, from),
                    };
                    let hit = hit.ok().flatten()?;
                    let line = line_of(&file, &index, hit.start).ok()?;
                    Some((hit, line))
                })
                .await;
            this.update(cx, |this, cx| this.show_hit(found, cx)).ok();
        }));
        cx.notify();
    }

    fn show_hit(&mut self, found: Option<(Range<u64>, u64)>, cx: &mut Context<Self>) {
        let visible = self.visible_lines(cx);
        let Some(find) = &mut self.find else {
            return;
        };
        find._search = None;
        find.note = found.is_none().then_some("No results");
        let Some((hit, line)) = found else {
            find.hit = None;
            find.hit_line = None;
            return cx.notify();
        };
        find.hit = Some(hit);
        find.hit_line = Some(line);
        let top = self.top.floor() as u64;
        if line < top || line as f64 >= self.top + visible - 1. {
            self.scroll_to(line as f64 - (visible / 3.).floor(), cx);
        }
        self.scroll_x = 0.;
        cx.notify();
    }

    fn render_banner(&self, cx: &App) -> impl IntoElement {
        let t = cx.theme();
        let (size, progress) = match &self.opened {
            Ok(o) if !o.index.done => {
                let pct = o.index.scanned * 100 / o.len.max(1);
                (o.len, format!("Reading lines… {pct}%"))
            }
            Ok(o) => (o.len, format!("{} lines", group(o.index.lines()))),
            Err(_) => (0, String::new()),
        };
        div()
            .flex_none()
            .h(px(32.))
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(8.))
            .bg(t.color.surface)
            .border_b_1()
            .border_color(t.color.border)
            .text_size(t.typography.caption)
            .child(div().size(px(6.)).flex_none().bg(t.color.warning))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(t.color.content_secondary)
                    .child(format!(
                        "Read-only: this file is {}, so highlighting, language features and \
                         folding are off.",
                        human_size(size)
                    )),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(t.color.content_muted)
                    .child(progress),
            )
    }

    fn render_find(&self, window: &Window, cx: &App) -> Option<impl IntoElement> {
        let find = self.find.as_ref()?;
        let t = cx.theme();
        let focused = find.input.focus_handle(cx).is_focused(window);
        let note = match (find.note, find.hit_line) {
            (Some(note), _) => note.to_string(),
            (None, Some(line)) => format!("Line {}", group(line + 1)),
            (None, None) => String::new(),
        };
        Some(
            div()
                .flex_none()
                .h(px(36.))
                .px(px(12.))
                .flex()
                .items_center()
                .gap(px(8.))
                .bg(t.color.surface)
                .border_b_1()
                .border_color(t.color.border)
                .text_size(t.typography.caption)
                .child(
                    div()
                        .w(px(280.))
                        .h(px(24.))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .bg(t.color.surface_sunken)
                        .border_1()
                        .border_color(if focused {
                            t.color.accent
                        } else {
                            t.color.border_strong
                        })
                        .rounded(t.shape.radius_control)
                        .child(div().flex_1().min_w_0().child(find.input.clone())),
                )
                .child(div().text_color(t.color.content_muted).child(note)),
        )
    }
}

impl Render for LargeFileView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let root = div()
            .id("large-file")
            .key_context("LargeFile")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(t.color.surface_sunken)
            .on_action(cx.listener(|this, _: &LineUp, _, cx| this.scroll_by_lines(-1., cx)))
            .on_action(cx.listener(|this, _: &LineDown, _, cx| this.scroll_by_lines(1., cx)))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| {
                let page = (this.visible_lines(cx) - 1.).max(1.).floor();
                this.scroll_by_lines(-page, cx)
            }))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| {
                let page = (this.visible_lines(cx) - 1.).max(1.).floor();
                this.scroll_by_lines(page, cx)
            }))
            .on_action(cx.listener(|this, _: &FileStart, _, cx| this.scroll_to(0., cx)))
            .on_action(cx.listener(|this, _: &FileEnd, _, cx| {
                let end = this.lines() as f64 - this.visible_lines(cx) + 1.;
                this.scroll_to(end, cx)
            }))
            .on_action(cx.listener(|this, _: &Find, window, cx| this.open_find(window, cx)))
            .on_mouse_move(cx.listener(Self::mouse_move));
        let opened = match &self.opened {
            Ok(opened) => opened,
            Err(error) => {
                return root.items_center().justify_center().child(empty_state(
                    "Can't open this file",
                    error.clone(),
                    None,
                    cx,
                ));
            }
        };
        let lh = self.line_height(cx);
        let rows = self.visible_lines(cx).ceil() as usize + 1;
        let first = self.top.floor() as u64;
        let shown = read_lines(&opened.file, &opened.index, first, rows).unwrap_or_default();
        let total = opened.index.lines();
        let digits = total.max(1).to_string().len().max(3);
        let cell = f32::from(t.typography.code) * 0.6;
        let gutter = cell * digits as f32 + 32.;
        let offset = -((self.top - first as f64) as f32) * lh;
        let hit = self.find.as_ref().and_then(|f| f.hit.clone());
        let mut numbers = div().absolute().top(px(offset)).left_0().w(px(gutter));
        let mut texts = div()
            .absolute()
            .top(px(offset))
            .left(px(gutter + 8. - self.scroll_x));
        for (i, line) in shown.iter().enumerate() {
            let number = (first + i as u64 + 1).to_string();
            numbers = numbers.child(
                div()
                    .h(px(lh))
                    .pr(px(16.))
                    .flex()
                    .items_center()
                    .justify_end()
                    .text_color(t.syntax.line_number)
                    .child(number),
            );
            let text = display_text(&line.bytes);
            let end = line.start + line.bytes.len() as u64;
            let highlight = hit
                .clone()
                .filter(|h| h.start >= line.start && h.start < end)
                .map(|h| {
                    let a = (h.start - line.start) as usize;
                    let b = ((h.end.min(end)) - line.start) as usize;
                    let a = display_text(&line.bytes[..a]).len();
                    let b = display_text(&line.bytes[..b]).len().max(a);
                    (
                        a..b,
                        HighlightStyle {
                            background_color: Some(t.color.surface_accent),
                            ..HighlightStyle::default()
                        },
                    )
                });
            texts = texts.child(
                div()
                    .h(px(lh))
                    .flex()
                    .items_center()
                    .whitespace_nowrap()
                    .text_color(t.syntax.text)
                    .child(StyledText::new(text).with_highlights(highlight)),
            );
        }
        let viewport = self.viewport.clone();
        let entity = cx.entity();
        let measure = canvas(
            move |bounds, _, cx| {
                if viewport.get() != bounds {
                    viewport.set(bounds);
                    entity.update(cx, |_, cx| cx.notify());
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let height = f32::from(self.viewport.get().size.height);
        let visible = self.visible_lines(cx);
        let span = (total as f64).max(visible).max(1.);
        let thumb_h = ((visible / span) as f32 * height).max(24.).min(height);
        let thumb_top = (self.top / span) as f32 * height;
        let scrollbar = div()
            .id("large-file-scrollbar")
            .absolute()
            .top_0()
            .right_0()
            .h_full()
            .w(px(SCROLLBAR))
            .cursor_default()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::bar_down))
            .child(
                div()
                    .absolute()
                    .top(px(thumb_top.min(height - thumb_h).max(0.)))
                    .left(px(2.))
                    .w(px(SCROLLBAR - 4.))
                    .h(px(thumb_h))
                    .rounded(px(3.))
                    .bg(t.color.border_strong),
            );
        root.child(self.render_banner(cx))
            .children(self.render_find(window, cx))
            .child(
                div()
                    .id("large-file-body")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .font_family(t.typography.mono.clone())
                    .text_size(t.typography.code)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, _| window.focus(&this.focus)),
                    )
                    .on_scroll_wheel(cx.listener(Self::scroll_wheel))
                    .child(measure)
                    .child(texts)
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .h_full()
                            .w(px(gutter))
                            .bg(t.color.surface_sunken)
                            .child(numbers),
                    )
                    .child(scrollbar),
            )
    }
}

/// 1234567 as "1,234,567".
fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn human_size(bytes: u64) -> String {
    match bytes {
        0..1_073_741_824 => format!("{:.0} MB", bytes as f64 / 1_048_576.),
        _ => format!("{:.2} GB", bytes as f64 / 1_073_741_824.),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const SIZE: u64 = 100 * 1024 * 1024;
    const TAIL: &[u8] = b"\ntail A\nneedle here\r\nlast line";

    /// A sparse 100 MB file: 1,000 numbered lines, a hole of zeros as one long line, then a tail.
    fn sparse(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("athena-large-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.log");
        let mut f = File::create(&path).unwrap();
        for i in 1..=1000 {
            writeln!(f, "line {i}").unwrap();
        }
        f.set_len(SIZE).unwrap();
        f.write_all_at(TAIL, SIZE - TAIL.len() as u64).unwrap();
        (dir, path)
    }

    fn indexed(path: &Path) -> Opened {
        let mut o = open_file(path).unwrap();
        let mut steps = 0;
        while !o.index.done {
            o.index.step(&o.file, o.len, 7 * 1024 * 1024).unwrap();
            steps += 1;
        }
        assert!(steps > 10, "indexed in steps");
        o
    }

    fn texts(o: &Opened, first: u64, count: usize) -> Vec<String> {
        read_lines(&o.file, &o.index, first, count)
            .unwrap()
            .iter()
            .map(|l| display_text(&l.bytes))
            .collect()
    }

    #[test]
    fn a_sparse_100_mb_file_pages_its_lines_from_the_index() {
        let (dir, path) = sparse("paging");
        assert!(is_large_file(&path));
        let o = indexed(&path);
        assert_eq!(o.index.lines(), 1004);
        assert!(
            o.index.marks.len() < 100,
            "a sparse index: {}",
            o.index.marks.len()
        );
        let shown = texts(&o, 998, 10);
        assert_eq!(shown.len(), 6, "stops at the last line");
        assert_eq!(&shown[..2], ["line 999", "line 1000"]);
        assert_eq!(
            shown[2],
            "␀".repeat(SHOWN_BYTES),
            "the hole is one long line, cut off"
        );
        assert_eq!(&shown[3..], ["tail A", "needle here", "last line"]);
        let tail = SIZE - TAIL.len() as u64;
        assert_eq!(line_start(&o.file, &o.index, 1001).unwrap(), Some(tail + 1));
        assert_eq!(line_start(&o.file, &o.index, 1004).unwrap(), None);
        assert_eq!(texts(&o, 0, 2), ["line 1", "line 2"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lines_show_only_as_far_as_the_index_has_read() {
        let (dir, path) = sparse("partial");
        let mut o = open_file(&path).unwrap();
        o.index.step(&o.file, o.len, 4096).unwrap();
        assert!(!o.index.done);
        let known = o.index.lines();
        assert!(known > 0 && known < 1000);
        assert_eq!(texts(&o, 0, 2000).len() as u64, known);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn find_runs_over_the_whole_file_in_both_directions_and_wraps() {
        let (dir, path) = sparse("find");
        let o = indexed(&path);
        let re = |q: &str| {
            regex::bytes::RegexBuilder::new(&regex::escape(q))
                .case_insensitive(true)
                .build()
                .unwrap()
        };
        let needle = find_forward(&o.file, o.len, &re("NEEDLE"), 0)
            .unwrap()
            .unwrap();
        assert_eq!(line_of(&o.file, &o.index, needle.start).unwrap(), 1002);
        let again = find_forward(&o.file, o.len, &re("needle"), needle.start + 1).unwrap();
        assert_eq!(again, Some(needle.clone()), "wraps round to the only match");
        let back = find_backward(&o.file, o.len, &re("needle"), 0).unwrap();
        assert_eq!(back, Some(needle));
        let start = line_start(&o.file, &o.index, 4).unwrap().unwrap();
        let first = find_forward(&o.file, o.len, &re("line 5"), 0)
            .unwrap()
            .unwrap();
        assert_eq!(first.start, start);
        let last = find_backward(&o.file, o.len, &re("line 5"), first.start)
            .unwrap()
            .unwrap();
        assert_eq!(
            line_of(&o.file, &o.index, last.start).unwrap(),
            598,
            "line 599"
        );
        assert_eq!(
            find_forward(&o.file, o.len, &re("absent"), 0).unwrap(),
            None
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn files_over_2_gb_and_binary_files_are_refused() {
        let dir = std::env::temp_dir().join(format!("athena-large-refuse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let huge = dir.join("huge.txt");
        let f = File::create(&huge).unwrap();
        f.write_all_at(b"text\n", 0).unwrap();
        f.set_len(MAX_LARGE + 1).unwrap();
        assert!(open_file(&huge).err().unwrap().contains("larger than 2 GB"));
        let bin = dir.join("data.bin");
        std::fs::write(&bin, [1, 2, 0, 3]).unwrap();
        assert!(open_file(&bin).err().unwrap().contains("binary"));
        assert!(!is_large_file(&bin));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shown_text_expands_tabs_and_replaces_bad_bytes() {
        assert_eq!(display_text(b"a\tb\t\tc"), "a   b       c");
        assert_eq!(display_text(b"caf\xE9\x01"), "caf\u{FFFD}␁");
        assert_eq!(group(1234567), "1,234,567");
        assert_eq!(group(12), "12");
    }
}
