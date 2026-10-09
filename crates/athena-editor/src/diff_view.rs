use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use athena_ui::{ActiveTheme, ContextMenu, MenuItem, SyntaxColors, Theme, Tooltip, empty_state};
use gpui::{
    App, ClipboardItem, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, HighlightStyle, Hsla, IntoElement, KeyBinding, MouseButton, MouseDownEvent,
    MouseMoveEvent, Pixels, Point, Render, ScrollStrategy, ScrollWheelEvent, SharedString,
    StyledText, Subscription, Task, UniformListScrollHandle, Window, actions, div, point,
    prelude::*, px, uniform_list,
};
use ropey::Rope;

use crate::diff::{self, Change, Row, Shown};
use crate::syntax::{Lang, Syntax, Token};

actions!(
    diff,
    [
        NextChange,
        PrevChange,
        ToggleInline,
        LineUp,
        LineDown,
        PageUp,
        PageDown,
        Top,
        Bottom,
        AcceptProposal,
        RejectProposal,
        CopySelection,
        SelectAllLines,
        ToggleUnchanged
    ]
);

pub(crate) fn init(cx: &mut App) {
    let ctx = Some("DiffView");
    cx.bind_keys([
        KeyBinding::new("alt-f5", NextChange, ctx),
        KeyBinding::new("shift-alt-f5", PrevChange, ctx),
        KeyBinding::new("up", LineUp, ctx),
        KeyBinding::new("down", LineDown, ctx),
        KeyBinding::new("pageup", PageUp, ctx),
        KeyBinding::new("pagedown", PageDown, ctx),
        KeyBinding::new("cmd-up", Top, ctx),
        KeyBinding::new("cmd-down", Bottom, ctx),
        KeyBinding::new("cmd-enter", AcceptProposal, ctx),
        KeyBinding::new("cmd-backspace", RejectProposal, ctx),
        KeyBinding::new("cmd-c", CopySelection, ctx),
        KeyBinding::new("cmd-a", SelectAllLines, ctx),
    ]);
}

/// Only this much of a very long line is drawn; minified files would otherwise shape megabytes.
const MAX_DRAWN: usize = 4_000;
/// Word highlights are skipped past this many paired lines; a rewrite is red and green anyway.
const MAX_WORD_PAIRS: usize = 20_000;
const TAB_WIDTH: usize = 4;
const TOOLBAR: f32 = 32.;
/// Unchanged lines kept next to each change when the rest are hidden, as VS Code keeps.
const CONTEXT: usize = 3;
/// Diffs with more rows than this start with unchanged regions hidden.
const LARGE_DIFF: usize = 500;
const SIGN_WIDTH: f32 = 14.;

/// A hunk action, carrying the full text the file or index should have afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffEvent {
    /// New index contents with one change staged; `expected` is what the index held when diffed.
    Stage {
        contents: String,
        expected: String,
    },
    /// New index contents with one staged change taken back out.
    Unstage {
        contents: String,
        expected: String,
    },
    /// New file contents with one change undone; `expected` is what the file held when diffed.
    Revert {
        contents: String,
        expected: String,
    },
    OpenFile,
    /// The user accepted a proposed change.
    Accept,
    /// The user rejected a proposed change.
    Reject,
}

/// Which hunk buttons a diff offers; it depends on what the two sides are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HunkActions {
    pub stage: bool,
    pub unstage: bool,
    pub revert: bool,
}

struct Side {
    text: String,
    rope: Rope,
    /// Byte range of each line, terminator included.
    lines: Vec<Range<usize>>,
    syntax: Option<Syntax>,
}

impl Side {
    fn new(text: String, lang: Option<Lang>) -> Self {
        let mut lines = Vec::new();
        let mut at = 0;
        for line in diff::lines(&text) {
            lines.push(at..at + line.len());
            at += line.len();
        }
        let rope = Rope::from_str(&text);
        let syntax = lang.map(|l| Syntax::new(l, &rope));
        Self {
            text,
            rope,
            lines,
            syntax,
        }
    }

    fn line(&self, i: usize) -> &str {
        &self.text[self.lines[i].clone()]
    }

    fn line_strs(&self) -> Vec<&str> {
        self.lines.iter().map(|r| &self.text[r.clone()]).collect()
    }
}

type WordRanges = (Vec<Range<usize>>, Vec<Range<usize>>);

struct Loaded {
    old: Side,
    new: Side,
    changes: Vec<Change>,
    split: Vec<Row>,
    unified: Vec<Row>,
    /// Changed byte ranges of paired old and new lines, by (old line, new line).
    words: HashMap<(usize, usize), WordRanges>,
    /// The widest drawn line, in columns.
    columns: usize,
    added: usize,
    removed: usize,
}

impl Loaded {
    fn compute(old: String, new: String, lang: Option<Lang>) -> Self {
        let old = Side::new(old, lang);
        let new = Side::new(new, lang);
        let (a, b) = (old.line_strs(), new.line_strs());
        let changes = diff::diff_lines(&a, &b);
        let split = diff::side_by_side(&changes, a.len(), b.len());
        let unified = diff::inline(&changes, a.len(), b.len());
        let mut words = HashMap::new();
        for row in &split {
            if words.len() >= MAX_WORD_PAIRS {
                break;
            }
            if let Row::Line {
                old: Some(o),
                new: Some(n),
                change: Some(_),
            } = *row
            {
                words.insert((o, n), diff::diff_words(a[o], b[n]));
            }
        }
        let columns = a
            .iter()
            .chain(&b)
            .map(|l| columns(diff::display(l)))
            .max()
            .unwrap_or(0);
        let added = changes.iter().map(|c| c.new.len()).sum();
        let removed = changes.iter().map(|c| c.old.len()).sum();
        Self {
            old,
            new,
            changes,
            split,
            unified,
            words,
            columns,
            added,
            removed,
        }
    }

    /// The new line paired with an old one in a changed block, and the other way round.
    fn partner(&self, old: Option<usize>, new: Option<usize>, change: usize) -> (usize, usize) {
        let c = &self.changes[change];
        match (old, new) {
            (Some(o), Some(n)) => (o, n),
            (Some(o), None) => (o, c.new.start + (o - c.old.start)),
            (None, Some(n)) => (c.old.start + (n - c.new.start), n),
            (None, None) => (usize::MAX, usize::MAX),
        }
    }
}

fn columns(line: &str) -> usize {
    line.chars().take(MAX_DRAWN).fold(0, |col, c| {
        if c == '\t' {
            col + TAB_WIDTH - col % TAB_WIDTH
        } else {
            col + 1
        }
    })
}

/// Two versions of a file compared side by side or inline, read-only, with per-change actions.
pub struct DiffView {
    path: PathBuf,
    title: SharedString,
    old_label: SharedString,
    new_label: SharedString,
    /// Why the old side is not what the tab names, shown under the toolbar.
    note: Option<SharedString>,
    actions: HunkActions,
    /// The new side is a change someone is waiting for the user to accept or reject.
    proposal: bool,
    inline: bool,
    loaded: Option<Rc<Loaded>>,
    error: Option<SharedString>,
    /// The texts being diffed, by hash, so a reload of the same texts does not restart it.
    computing: Option<(u64, Task<()>)>,
    /// A hunk action was sent and the texts it was made from have not been reloaded yet.
    acting: bool,
    /// The change hunk navigation last moved to; scrolling by hand forgets it.
    current: Option<usize>,
    scrolled_once: bool,
    scroll: UniformListScrollHandle,
    h_offset: f32,
    focus: FocusHandle,
    /// The user's Hide Unchanged choice; `None` hides them in large diffs only.
    hide_unchanged: Option<bool>,
    /// Old lines starting unchanged stretches the user unfolded.
    unfolded: HashSet<usize>,
    /// What the list draws: rows of the current layout, with folded stretches.
    shown: Rc<Vec<Shown>>,
    selection: Option<Selection>,
    selecting: bool,
    /// Width of one column of the code font, measured each frame.
    cell: f32,
    context_menu: Option<(Entity<ContextMenu>, Subscription)>,
}

/// Selected text on one side, between two (row, column) points of the current layout's rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    side: Which,
    anchor: (usize, usize),
    head: (usize, usize),
}

impl Selection {
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        match self.anchor <= self.head {
            true => (self.anchor, self.head),
            false => (self.head, self.anchor),
        }
    }

    /// The drawn columns selected on row `row`, if any.
    fn columns(&self, row: usize) -> Option<Range<usize>> {
        let (from, to) = self.ordered();
        if row < from.0 || row > to.0 {
            return None;
        }
        let a = if row == from.0 { from.1 } else { 0 };
        let b = if row == to.0 { to.1 } else { usize::MAX };
        (a < b).then_some(a..b)
    }
}

impl EventEmitter<DiffEvent> for DiffView {}

impl Focusable for DiffView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl DiffView {
    pub fn new(
        path: PathBuf,
        title: impl Into<SharedString>,
        old_label: impl Into<SharedString>,
        new_label: impl Into<SharedString>,
        actions: HunkActions,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            path,
            title: title.into(),
            old_label: old_label.into(),
            new_label: new_label.into(),
            note: None,
            actions,
            proposal: false,
            inline: false,
            loaded: None,
            error: None,
            computing: None,
            acting: false,
            current: None,
            scrolled_once: false,
            scroll: UniformListScrollHandle::new(),
            h_offset: 0.,
            focus: cx.focus_handle(),
            hide_unchanged: None,
            unfolded: HashSet::new(),
            shown: Rc::default(),
            selection: None,
            selecting: false,
            cell: 0.,
            context_menu: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Offers Accept and Reject for the whole change instead of hunk actions.
    pub fn as_proposal(mut self) -> Self {
        self.proposal = true;
        self
    }

    fn decide(&mut self, accept: bool, cx: &mut Context<Self>) {
        if !self.proposal {
            return cx.propagate();
        }
        cx.emit(if accept {
            DiffEvent::Accept
        } else {
            DiffEvent::Reject
        });
    }

    pub fn label(&self) -> String {
        self.title.to_string()
    }

    /// The new side's text as last diffed, to check a file has not moved on before reverting.
    pub fn new_text(&self) -> Option<&str> {
        Some(&self.loaded.as_ref()?.new.text)
    }

    /// Names the old side, with a note when it is not the version the tab title promises.
    pub fn set_old_label(
        &mut self,
        label: impl Into<SharedString>,
        note: Option<&'static str>,
        cx: &mut Context<Self>,
    ) {
        let (label, note) = (label.into(), note.map(SharedString::from));
        if self.old_label != label || self.note != note {
            self.old_label = label;
            self.note = note;
            cx.notify();
        }
    }

    /// Diffs two texts off the main thread; the same texts again, shown or in progress, are ignored.
    pub fn set_texts(&mut self, old: String, new: String, cx: &mut Context<Self>) {
        self.acting = false;
        if self.error.is_none()
            && self
                .loaded
                .as_ref()
                .is_some_and(|l| l.old.text == old && l.new.text == new)
        {
            self.computing = None;
            return;
        }
        let key = texts_key(&old, &new);
        if self.computing.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        let lang = Lang::for_path(&self.path).or_else(|| {
            let first = new.lines().next().or_else(|| old.lines().next())?;
            Lang::for_shebang(first)
        });
        let work = cx
            .background_executor()
            .spawn(async move { Loaded::compute(old, new, lang) });
        let task = cx.spawn(async move |this, cx| {
            let loaded = work.await;
            let _ = this.update(cx, |this, cx| {
                this.computing = None;
                this.error = None;
                this.loaded = Some(Rc::new(loaded));
                this.selection = None;
                this.refresh_shown();
                if this
                    .current
                    .is_some_and(|c| c >= this.loaded.as_ref().map_or(0, |l| l.changes.len()))
                {
                    this.current = None;
                }
                if !std::mem::replace(&mut this.scrolled_once, true) {
                    this.go_to_change(0, cx);
                }
                cx.notify();
            });
        });
        self.computing = Some((key, task));
    }

    /// Shows why the versions could not be read (a binary file, a missing revision).
    pub fn set_error(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.acting = false;
        self.error = Some(message.into());
        self.loaded = None;
        self.computing = None;
        cx.notify();
    }

    fn rows(&self) -> Option<&[Row]> {
        let loaded = self.loaded.as_ref()?;
        Some(if self.inline {
            &loaded.unified
        } else {
            &loaded.split
        })
    }

    /// Whether unchanged regions are folded: the user's choice, else on for large diffs.
    fn hiding(&self) -> bool {
        self.hide_unchanged
            .unwrap_or_else(|| self.rows().is_some_and(|r| r.len() > LARGE_DIFF))
    }

    fn refresh_shown(&mut self) {
        let Some(rows) = self.rows() else {
            self.shown = Rc::default();
            return;
        };
        self.shown = Rc::new(match self.hiding() {
            true => diff::collapse(rows, CONTEXT, &self.unfolded),
            false => (0..rows.len()).map(Shown::Row).collect(),
        });
    }

    fn toggle_unchanged(&mut self, cx: &mut Context<Self>) {
        self.hide_unchanged = Some(!self.hiding());
        self.unfolded.clear();
        self.refresh_shown();
        if let Some(change) = self.current {
            self.go_to_change(change, cx);
        }
        cx.notify();
    }

    fn unfold(&mut self, rows: Range<usize>, cx: &mut Context<Self>) {
        let key = match self.rows().and_then(|r| r.get(rows.start)) {
            Some(Row::Line { old: Some(o), .. }) => *o,
            _ => return,
        };
        // A fold shown after its stretch's top context starts below the stretch's first line.
        let first = self.rows().map_or(0, |r| {
            let mut i = rows.start;
            while i > 0 && matches!(r[i - 1], Row::Line { change: None, .. }) {
                i -= 1;
            }
            match r[i] {
                Row::Line { old: Some(o), .. } => o,
                _ => key,
            }
        });
        self.unfolded.insert(first);
        self.refresh_shown();
        cx.notify();
    }

    fn header_row(&self, change: usize) -> Option<usize> {
        let rows = self.rows()?;
        self.shown
            .iter()
            .position(|s| matches!(s, Shown::Row(i) if rows[*i] == Row::Header(change)))
    }

    fn go_to_change(&mut self, change: usize, cx: &mut Context<Self>) {
        let Some(row) = self.header_row(change) else {
            return;
        };
        self.current = Some(change);
        // A few rows of context above the change, as VS Code leaves.
        self.scroll
            .scroll_to_item_strict_with_offset(row, ScrollStrategy::Top, 3);
        cx.notify();
    }

    fn step_change(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(count) = self.loaded.as_ref().map(|l| l.changes.len()) else {
            return;
        };
        if count == 0 {
            return;
        }
        let next = match self.current {
            Some(c) if forward => (c + 1) % count,
            Some(c) => (c + count - 1) % count,
            None => {
                // From where the view is scrolled to, counting the context rows left above a change.
                let top = self.top_row(cx) + 3;
                let rows = self.rows().unwrap_or_default();
                let headers: Vec<(usize, usize)> = self
                    .shown
                    .iter()
                    .enumerate()
                    .filter_map(|(i, s)| match s {
                        Shown::Row(r) => match rows[*r] {
                            Row::Header(c) => Some((i, c)),
                            Row::Line { .. } => None,
                        },
                        Shown::Hidden(_) => None,
                    })
                    .collect();
                let found = if forward {
                    headers.iter().find(|(i, _)| *i > top)
                } else {
                    headers.iter().rev().find(|(i, _)| *i < top)
                };
                found.map_or(if forward { 0 } else { count - 1 }, |(_, c)| *c)
            }
        };
        self.go_to_change(next, cx);
    }

    fn row_height(&self, cx: &App) -> f32 {
        (f32::from(cx.theme().typography.code) * 1.5).round()
    }

    fn top_row(&self, cx: &App) -> usize {
        let offset = self.scroll.0.borrow().base_handle.offset();
        (-f32::from(offset.y) / self.row_height(cx)).max(0.) as usize
    }

    fn scroll_rows(&mut self, rows: isize, cx: &mut Context<Self>) {
        let h = self.row_height(cx);
        let handle = self.scroll.0.borrow().base_handle.clone();
        let offset = handle.offset();
        let max = f32::from(handle.max_offset().height);
        let y = (f32::from(offset.y) - rows as f32 * h).clamp(-max, 0.);
        handle.set_offset(point(offset.x, px(y)));
        self.current = None;
        cx.notify();
    }

    fn page_rows(&self, cx: &App) -> isize {
        let h = self.row_height(cx);
        let view = f32::from(self.scroll.0.borrow().base_handle.bounds().size.height);
        ((view / h) as isize - 2).max(1)
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(self.row_height(cx)));
        let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
        if dy != 0. {
            self.current = None;
        }
        if dx.abs() > dy.abs()
            && let Some(loaded) = &self.loaded
        {
            let max = loaded.columns as f32 * self.char_width(cx);
            self.h_offset = (self.h_offset - dx).clamp(0., max);
            cx.notify();
        }
    }

    fn char_width(&self, cx: &App) -> f32 {
        match self.cell > 0. {
            true => self.cell,
            false => f32::from(cx.theme().typography.code) * 0.6,
        }
    }

    fn gutter_width(&self, loaded: &Loaded, cx: &App) -> f32 {
        let digits = loaded
            .old
            .lines
            .len()
            .max(loaded.new.lines.len())
            .max(1)
            .ilog10()
            + 1;
        (digits as f32 + 1.) * self.char_width(cx) + 8.
    }

    /// The row, side and drawn column under a window position, from the list's last layout.
    fn point_at(&self, position: Point<Pixels>, cx: &App) -> Option<(usize, Which, usize)> {
        let loaded = self.loaded.as_ref()?;
        let rows = self.rows()?;
        let handle = self.scroll.0.borrow().base_handle.clone();
        let bounds = handle.bounds();
        let y = f32::from(position.y - bounds.top() - handle.offset().y);
        let at = (y / self.row_height(cx)).floor().max(0.) as usize;
        let shown = self.shown.get(at.min(self.shown.len().checked_sub(1)?))?;
        let row = match shown {
            Shown::Row(r) => *r,
            Shown::Hidden(r) => return Some((r.start, Which::New, 0)),
        };
        let x = f32::from(position.x - bounds.left());
        let gutter = self.gutter_width(loaded, cx);
        let (which, text_left) = if self.inline {
            let which = match rows[row] {
                Row::Line {
                    old: Some(_),
                    new: None,
                    change: Some(_),
                } => Which::Old,
                _ => Which::New,
            };
            (which, 2. * gutter + SIGN_WIDTH)
        } else {
            let half = (f32::from(bounds.size.width) - 1.) / 2.;
            match x < half {
                true => (Which::Old, gutter + SIGN_WIDTH),
                false => (Which::New, half + 1. + gutter + SIGN_WIDTH),
            }
        };
        let col = ((x - text_left + self.h_offset) / self.char_width(cx))
            .round()
            .max(0.) as usize;
        Some((row, which, col))
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let Some((row, which, col)) = self.point_at(event.position, cx) else {
            return;
        };
        let side = if self.inline { Which::New } else { which };
        self.selection = match self.selection {
            Some(s) if event.modifiers.shift && s.side == side => Some(Selection {
                head: (row, col),
                ..s
            }),
            _ => Some(Selection {
                side,
                anchor: (row, col),
                head: (row, col),
            }),
        };
        self.selecting = true;
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            self.selecting = false;
            return;
        }
        let Some((row, _, col)) = self.point_at(event.position, cx) else {
            return;
        };
        if let Some(s) = self.selection.as_mut()
            && s.head != (row, col)
        {
            s.head = (row, col);
            cx.notify();
        }
    }

    /// The selected text, with lines folded away inside the selection included.
    fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        let loaded = self.loaded.as_ref()?;
        let rows = self.rows()?;
        let inline = self.inline;
        let pick = |row: &Row| -> Option<&str> {
            let Row::Line { old, new, change } = *row else {
                return None;
            };
            let old_only = old.is_some() && new.is_none() && change.is_some();
            match (inline, selection.side) {
                (true, _) if old_only => old.map(|o| loaded.old.line(o)),
                (true, _) => new.map(|n| loaded.new.line(n)),
                (false, Which::Old) => old.map(|o| loaded.old.line(o)),
                (false, Which::New) => new.map(|n| loaded.new.line(n)),
            }
        };
        let text = diff::selected_text(rows, pick, selection.anchor, selection.head, TAB_WIDTH);
        (!text.is_empty()).then_some(text)
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        let Some(last) = self.rows().and_then(|r| r.len().checked_sub(1)) else {
            return;
        };
        let side = self.selection.map_or(Which::New, |s| s.side);
        self.selection = Some(Selection {
            side,
            anchor: (0, 0),
            head: (last, usize::MAX),
        });
        cx.notify();
    }

    fn open_menu(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let change = self.point_at(event.position, cx).and_then(|(row, _, _)| {
            match self.rows()?.get(row)? {
                Row::Line { change, .. } => *change,
                Row::Header(c) => Some(*c),
            }
        });
        let this = cx.entity().downgrade();
        let act = move |label: &'static str, f: fn(&mut Self, &mut Context<Self>)| {
            let this = this.clone();
            MenuItem::new(label, move |_, cx| {
                let _ = this.update(cx, f);
            })
        };
        let mut items = vec![
            act("Copy", |v, cx| v.copy(cx))
                .hint("⌘C")
                .disabled(self.selected_text().is_none()),
            act("Select All", |v, cx| v.select_all(cx)).hint("⌘A"),
        ];
        if let Some(change) = change {
            let a = self.actions;
            let mut hunk = Vec::new();
            let this = cx.entity().downgrade();
            let mut push = |on: bool, label: &'static str, kind: HunkKind| {
                if on {
                    let this = this.clone();
                    hunk.push(MenuItem::new(label, move |_, cx| {
                        let _ = this.update(cx, |v, cx| v.emit_action(change, kind, cx));
                    }));
                }
            };
            push(a.stage, "Stage This Change", HunkKind::Stage);
            push(a.unstage, "Unstage This Change", HunkKind::Unstage);
            push(a.revert, "Revert This Change", HunkKind::Revert);
            if !hunk.is_empty() {
                items.push(MenuItem::separator());
                items.extend(hunk);
            }
        }
        items.push(MenuItem::separator());
        items.push(act("Open File", |_, cx| cx.emit(DiffEvent::OpenFile)));
        let menu = ContextMenu::build(event.position, items, window, cx);
        let subscription = cx.subscribe_in(&menu, window, |this, menu, _: &DismissEvent, _, cx| {
            if this.context_menu.as_ref().is_some_and(|(m, _)| m == menu) {
                this.context_menu = None;
                cx.notify();
            }
        });
        self.context_menu = Some((menu, subscription));
        cx.notify();
    }

    fn emit_action(&mut self, change: usize, kind: HunkKind, cx: &mut Context<Self>) {
        // A second click before the reload would act on texts the first one already changed.
        if self.acting || self.computing.is_some() {
            return;
        }
        let Some(loaded) = self.loaded.clone() else {
            return;
        };
        let Some(c) = loaded.changes.get(change) else {
            return;
        };
        let (a, b) = (loaded.old.line_strs(), loaded.new.line_strs());
        let event = match kind {
            HunkKind::Stage => DiffEvent::Stage {
                contents: diff::apply_change(&a, &b, c),
                expected: loaded.old.text.clone(),
            },
            HunkKind::Unstage => DiffEvent::Unstage {
                contents: diff::revert_change(&a, &b, c),
                expected: loaded.new.text.clone(),
            },
            HunkKind::Revert => DiffEvent::Revert {
                contents: diff::revert_change(&a, &b, c),
                expected: loaded.new.text.clone(),
            },
        };
        self.acting = true;
        cx.emit(event);
    }
}

fn texts_key(old: &str, new: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (old, new).hash(&mut h);
    h.finish()
}

#[derive(Clone, Copy)]
enum HunkKind {
    Stage,
    Unstage,
    Revert,
}

/// How a token is coloured; mirrors the editor's palette so a diff reads like the file.
fn token_color(token: Token, syntax: &SyntaxColors) -> Hsla {
    match token {
        Token::Keyword => syntax.keyword,
        Token::Function | Token::Tag | Token::Link => syntax.function,
        Token::Property => syntax.property,
        Token::Type | Token::Namespace => syntax.type_,
        Token::Attribute => syntax.attribute,
        Token::String => syntax.string,
        Token::StringSpecial | Token::Number => syntax.string_special,
        Token::Constant | Token::Escape | Token::Embedded | Token::Label => syntax.constant,
        Token::VariableBuiltin => syntax.variable_builtin,
        Token::Operator => syntax.operator,
        Token::Punctuation => syntax.punctuation,
        Token::PunctuationSpecial => syntax.punctuation_special,
        Token::Comment => syntax.comment,
        Token::Heading => syntax.heading,
        Token::Error => syntax.error,
        Token::Variable | Token::Emphasis | Token::Strong => syntax.text,
        Token::Parameter => syntax.parameter,
        Token::TypeParameter => syntax.type_parameter,
    }
}

/// A token colour and whether the run is a changed word.
type RunStyle = (Option<Token>, bool);

/// A line's drawn text (tabs expanded) and its colour runs: syntax tokens plus changed words.
fn styled_line(
    side: &Side,
    line: usize,
    words: &[Range<usize>],
    word_bg: Hsla,
    t: &Theme,
) -> (SharedString, Vec<(Range<usize>, HighlightStyle)>) {
    let start = side.lines[line].start;
    let mut raw = diff::display(side.line(line));
    if raw.len() > MAX_DRAWN {
        let mut end = MAX_DRAWN;
        while !raw.is_char_boundary(end) {
            end -= 1;
        }
        raw = &raw[..end];
    }
    let mut tokens: Vec<Option<Token>> = vec![None; raw.len()];
    if let Some(syntax) = &side.syntax {
        // Outer captures come first, so inner ones overwrite them.
        for (range, token) in syntax.highlights(&side.rope, start..start + raw.len()) {
            let lo = range.start.saturating_sub(start).min(raw.len());
            let hi = range.end.saturating_sub(start).min(raw.len());
            tokens[lo..hi].fill(Some(token));
        }
    }
    let mut changed = vec![false; raw.len()];
    for r in words {
        let hi = r.end.min(raw.len());
        changed[r.start.min(hi)..hi].fill(true);
    }
    let mut text = String::with_capacity(raw.len());
    let mut runs: Vec<(Range<usize>, RunStyle)> = Vec::new();
    let mut col = 0;
    for (i, c) in raw.char_indices() {
        let style = (tokens[i], changed[i]);
        let from = text.len();
        if c == '\t' {
            let n = TAB_WIDTH - col % TAB_WIDTH;
            text.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            text.push(c);
            col += 1;
        }
        match runs.last_mut() {
            Some((r, s)) if *s == style => r.end = text.len(),
            _ => runs.push((from..text.len(), style)),
        }
    }
    let highlights = runs
        .into_iter()
        .map(|(r, (token, word))| {
            let style = HighlightStyle {
                color: Some(token.map_or(t.syntax.text, |k| token_color(k, &t.syntax))),
                font_weight: (token == Some(Token::Heading)).then_some(FontWeight::BOLD),
                background_color: word.then_some(word_bg),
                ..Default::default()
            };
            (r, style)
        })
        .collect();
    (text.into(), highlights)
}

/// Runs with a selection background laid over drawn columns `cols` (one column per char,
/// since tabs are already expanded).
fn with_selection(
    text: &str,
    runs: Vec<(Range<usize>, HighlightStyle)>,
    cols: Option<Range<usize>>,
    bg: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let Some(cols) = cols else {
        return runs;
    };
    let byte = |col: usize| text.char_indices().nth(col).map_or(text.len(), |(i, _)| i);
    let (a, b) = (byte(cols.start), byte(cols.end));
    if a >= b {
        return runs;
    }
    let mut out = Vec::with_capacity(runs.len() + 2);
    for (r, style) in runs {
        let cuts = [
            r.start,
            a.clamp(r.start, r.end),
            b.clamp(r.start, r.end),
            r.end,
        ];
        for w in cuts.windows(2) {
            if w[0] < w[1] {
                let mut style = style;
                if w[0] >= a && w[1] <= b {
                    style.background_color = Some(bg);
                }
                out.push((w[0]..w[1], style));
            }
        }
    }
    out
}

struct Palette {
    removed: Hsla,
    removed_word: Hsla,
    added: Hsla,
    added_word: Hsla,
    filler: Hsla,
}

impl Palette {
    fn new(t: &Theme) -> Self {
        Self {
            removed: t.color.danger.opacity(0.12),
            removed_word: t.color.danger.opacity(0.32),
            added: t.color.success.opacity(0.10),
            added_word: t.color.success.opacity(0.30),
            filler: t.color.border.opacity(0.25),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Which {
    Old,
    New,
}

impl DiffView {
    #[allow(clippy::too_many_arguments)]
    fn cell(
        &self,
        loaded: &Loaded,
        which: Which,
        line: Option<usize>,
        partner: Option<(usize, usize)>,
        selected: Option<Range<usize>>,
        gutter: f32,
        t: &Theme,
        p: &Palette,
    ) -> gpui::Div {
        let changed = partner.is_some();
        let (side, bg, word_bg) = match which {
            Which::Old => (&loaded.old, p.removed, p.removed_word),
            Which::New => (&loaded.new, p.added, p.added_word),
        };
        let Some(line) = line else {
            return div().flex_1().min_w_0().h_full().bg(p.filler);
        };
        let words = partner
            .and_then(|pair| loaded.words.get(&pair))
            .map(|(o, n)| if which == Which::Old { o } else { n })
            .map(Vec::as_slice)
            .unwrap_or_default();
        let (text, runs) = styled_line(side, line, words, word_bg, t);
        let runs = with_selection(&text, runs, selected, t.color.surface_accent);
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .when(changed, |el| el.bg(bg))
            .child(self.number(Some(line), gutter, changed, t))
            .child(self.sign(changed.then_some(which), t))
            .child(self.text(text, runs))
    }

    fn number(&self, line: Option<usize>, gutter: f32, changed: bool, t: &Theme) -> gpui::Div {
        div()
            .w(px(gutter))
            .flex_none()
            .flex()
            .justify_end()
            .pr(px(6.))
            .text_color(if changed {
                t.syntax.line_number_active
            } else {
                t.syntax.line_number
            })
            .children(line.map(|n| (n + 1).to_string()))
    }

    fn sign(&self, which: Option<Which>, t: &Theme) -> gpui::Div {
        let (glyph, color) = match which {
            Some(Which::Old) => ("−", t.color.danger),
            Some(Which::New) => ("+", t.color.success),
            None => ("", t.color.content_muted),
        };
        div()
            .w(px(SIGN_WIDTH))
            .flex_none()
            .text_color(color)
            .child(glyph)
    }

    fn text(&self, text: SharedString, runs: Vec<(Range<usize>, HighlightStyle)>) -> gpui::Div {
        div().flex_1().min_w_0().overflow_hidden().child(
            div()
                .ml(px(-self.h_offset))
                .whitespace_nowrap()
                .child(StyledText::new(text).with_highlights(runs)),
        )
    }

    fn header(
        &self,
        ix: usize,
        change: usize,
        loaded: &Loaded,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let c = &loaded.changes[change];
        let range = |r: &Range<usize>| match r.len() {
            0 => format!("{}", r.start),
            1 => format!("{}", r.start + 1),
            n => format!("{},{}", r.start + 1, n),
        };
        let summary = format!("@@ −{} +{} @@", range(&c.old), range(&c.new));
        let current = self.current == Some(change);
        let a = self.actions;
        let button = |id: &'static str, label: &'static str, tip: &'static str, kind: HunkKind| {
            div()
                .id((id, ix))
                .px(px(6.))
                .rounded(t.shape.radius_control)
                .cursor_pointer()
                .text_color(t.color.content_muted)
                .hover(|s| s.bg(t.color.surface_active).text_color(t.color.content))
                .tooltip(move |_, cx| Tooltip::view(tip, cx))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.emit_action(change, kind, cx);
                }))
                .child(label)
        };
        div()
            .id(("diff-header", ix))
            .size_full()
            .flex()
            .items_center()
            .gap(px(4.))
            .pl(px(8.))
            .pr(px(8.))
            .bg(t.color.surface)
            .border_l_2()
            .border_color(if current {
                t.color.accent
            } else {
                gpui::transparent_black()
            })
            .font_family(t.typography.ui.clone())
            .text_size(t.typography.caption)
            .child(div().text_color(t.color.content_muted).child(summary))
            .child(div().flex_1())
            .when(a.stage, |el| {
                el.child(button(
                    "diff-stage",
                    "Stage",
                    "Stage this change",
                    HunkKind::Stage,
                ))
            })
            .when(a.unstage, |el| {
                el.child(button(
                    "diff-unstage",
                    "Unstage",
                    "Take this change out of the index",
                    HunkKind::Unstage,
                ))
            })
            .when(a.revert, |el| {
                el.child(button(
                    "diff-revert",
                    "Revert",
                    "Undo this change in the file (a copy is kept)",
                    HunkKind::Revert,
                ))
            })
            .into_any_element()
    }

    /// The bar standing in for folded unchanged lines; clicking it shows them.
    fn fold_bar(
        &self,
        at: usize,
        hidden: Range<usize>,
        row: gpui::Div,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let n = hidden.len();
        let label = format!("⋯ {n} unchanged line{}", if n == 1 { "" } else { "s" });
        row.child(
            div()
                .id(("diff-fold", at))
                .size_full()
                .flex()
                .items_center()
                .pl(px(12.))
                .bg(t.color.surface)
                .font_family(t.typography.ui.clone())
                .text_size(t.typography.caption)
                .text_color(t.color.content_muted)
                .cursor_pointer()
                .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                .tooltip(|_, cx| Tooltip::view("Show these lines", cx))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, _, cx| this.unfold(hidden.clone(), cx)))
                .child(label),
        )
        .into_any_element()
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let stats = self.loaded.as_ref().map(|l| {
            div()
                .flex()
                .gap(px(6.))
                .child(
                    div()
                        .text_color(t.color.success)
                        .child(format!("+{}", l.added)),
                )
                .child(
                    div()
                        .text_color(t.color.danger)
                        .child(format!("−{}", l.removed)),
                )
        });
        let count = self.loaded.as_ref().map_or(0, |l| l.changes.len());
        let position = match (self.current, count) {
            (_, 0) => String::new(),
            (Some(c), n) => format!("{} of {n}", c + 1),
            (None, 1) => "1 change".into(),
            (None, n) => format!("{n} changes"),
        };
        let tool = |id: &'static str, label: &'static str, tip: &'static str| {
            div()
                .id(id)
                .h(px(24.))
                .px(px(8.))
                .flex()
                .items_center()
                .rounded(t.shape.radius_control)
                .cursor_pointer()
                .text_color(t.color.content_muted)
                .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                .tooltip(move |_, cx| Tooltip::view(tip, cx))
                .child(label)
        };
        div()
            .h(px(TOOLBAR))
            .flex_none()
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(8.))
            .border_b_1()
            .border_color(t.color.border)
            .bg(t.color.surface)
            .text_size(t.typography.caption)
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(t.color.content_secondary)
                    .child(format!("{} ↔ {}", self.old_label, self.new_label)),
            )
            .children(stats)
            .child(div().flex_1())
            .child(div().text_color(t.color.content_muted).child(position))
            .child(
                tool("diff-prev", "↑", "Previous change (⇧⌥F5)")
                    .on_click(cx.listener(|this, _, _, cx| this.step_change(false, cx))),
            )
            .child(
                tool("diff-next", "↓", "Next change (⌥F5)")
                    .on_click(cx.listener(|this, _, _, cx| this.step_change(true, cx))),
            )
            .child(
                tool(
                    "diff-mode",
                    if self.inline {
                        "Side by Side"
                    } else {
                        "Inline"
                    },
                    "Switch between side-by-side and inline",
                )
                .on_click(cx.listener(|this, _, _, cx| this.toggle_inline(cx))),
            )
            .child(
                tool(
                    "diff-unchanged",
                    if self.hiding() {
                        "Show Unchanged"
                    } else {
                        "Hide Unchanged"
                    },
                    "Fold unchanged lines down to 3 lines around each change",
                )
                .on_click(cx.listener(|this, _, _, cx| this.toggle_unchanged(cx))),
            )
            .child(
                tool("diff-open", "Open File", "Open the file in the editor")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DiffEvent::OpenFile))),
            )
            .when(self.proposal, |el| {
                el.child(
                    tool("diff-reject", "Reject", "Reject the change (⌘⌫)")
                        .on_click(cx.listener(|this, _, _, cx| this.decide(false, cx))),
                )
                .child(
                    div()
                        .id("diff-accept")
                        .h(px(24.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .rounded(t.shape.radius_control)
                        .cursor_pointer()
                        .bg(t.color.accent)
                        .text_color(t.color.content_on_accent)
                        .font_weight(FontWeight::MEDIUM)
                        .hover(|s| s.bg(t.color.accent_hover))
                        .active(|s| s.bg(t.color.accent_pressed))
                        .tooltip(|_, cx| Tooltip::view("Accept the change (⌘↵)", cx))
                        .on_click(cx.listener(|this, _, _, cx| this.decide(true, cx)))
                        .child("Accept"),
                )
            })
    }

    fn toggle_inline(&mut self, cx: &mut Context<Self>) {
        self.inline = !self.inline;
        self.selection = None;
        self.refresh_shown();
        if let Some(change) = self.current {
            self.go_to_change(change, cx);
        }
        cx.notify();
    }

    fn render_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if let Some(error) = &self.error {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(empty_state("Can't show this diff", error.clone(), None, cx))
                .into_any_element();
        }
        let Some(loaded) = self.loaded.clone() else {
            return div().flex_1().into_any_element();
        };
        if loaded.changes.is_empty() {
            let body = format!("{} and {} are the same.", self.old_label, self.new_label);
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(empty_state("No changes", body, None, cx))
                .into_any_element();
        }
        let t = cx.theme().clone();
        let row_h = self.row_height(cx);
        let gutter = self.gutter_width(&loaded, cx);
        let inline = self.inline;
        let shown = self.shown.clone();
        let selection = self.selection;
        uniform_list(
            "diff-rows",
            shown.len(),
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                let p = Palette::new(&t);
                let rows = if inline {
                    &loaded.unified
                } else {
                    &loaded.split
                };
                range
                    .map(|at| {
                        let row = div().h(px(row_h)).w_full().flex();
                        let ix = match &shown[at] {
                            Shown::Row(ix) => *ix,
                            Shown::Hidden(hidden) => {
                                return this.fold_bar(at, hidden.clone(), row, &t, cx);
                            }
                        };
                        let selected = |which: Which| {
                            selection
                                .filter(|s| inline || s.side == which)
                                .and_then(|s| s.columns(ix))
                        };
                        match rows[ix] {
                            Row::Header(change) => row
                                .child(this.header(ix, change, &loaded, &t, cx))
                                .into_any_element(),
                            Row::Line { old, new, change } => {
                                let pair = change.map(|c| loaded.partner(old, new, c));
                                if inline {
                                    let which = match (old, new, change) {
                                        (Some(_), None, Some(_)) => Which::Old,
                                        _ => Which::New,
                                    };
                                    let line = if which == Which::Old { old } else { new };
                                    let changed = change.is_some();
                                    let (bg, word_bg, side) = match which {
                                        Which::Old => (p.removed, p.removed_word, &loaded.old),
                                        Which::New => (p.added, p.added_word, &loaded.new),
                                    };
                                    let words = pair
                                        .and_then(|pair| loaded.words.get(&pair))
                                        .map(|(o, n)| if which == Which::Old { o } else { n })
                                        .map(Vec::as_slice)
                                        .unwrap_or_default();
                                    let (text, runs) = match line {
                                        Some(l) => styled_line(side, l, words, word_bg, &t),
                                        None => (SharedString::default(), Vec::new()),
                                    };
                                    let runs = with_selection(
                                        &text,
                                        runs,
                                        selected(which),
                                        t.color.surface_accent,
                                    );
                                    row.when(changed, |el| el.bg(bg))
                                        .child(this.number(old, gutter, changed, &t))
                                        .child(this.number(new, gutter, changed, &t))
                                        .child(this.sign(changed.then_some(which), &t))
                                        .child(this.text(text, runs))
                                        .into_any_element()
                                } else {
                                    row.child(this.cell(
                                        &loaded,
                                        Which::Old,
                                        old,
                                        pair,
                                        selected(Which::Old),
                                        gutter,
                                        &t,
                                        &p,
                                    ))
                                    .child(div().w(px(1.)).h_full().bg(t.color.border))
                                    .child(this.cell(
                                        &loaded,
                                        Which::New,
                                        new,
                                        pair,
                                        selected(Which::New),
                                        gutter,
                                        &t,
                                        &p,
                                    ))
                                    .into_any_element()
                                }
                            }
                        }
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.scroll.clone())
        .flex_1()
        .font_family(cx.theme().typography.mono.clone())
        .text_size(cx.theme().typography.code)
        .into_any_element()
    }
}

impl Render for DiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let text_system = window.text_system().clone();
        let font = gpui::font(t.typography.mono.clone());
        if let Ok(size) =
            text_system.advance(text_system.resolve_font(&font), t.typography.code, 'm')
        {
            self.cell = f32::from(size.width);
        }
        div()
            .id("diff-view")
            .size_full()
            .flex()
            .flex_col()
            .bg(t.color.surface_sunken)
            .track_focus(&self.focus)
            .key_context("DiffView")
            .on_action(cx.listener(|this, _: &NextChange, _, cx| this.step_change(true, cx)))
            .on_action(cx.listener(|this, _: &PrevChange, _, cx| this.step_change(false, cx)))
            .on_action(cx.listener(|this, _: &ToggleInline, _, cx| this.toggle_inline(cx)))
            .on_action(cx.listener(|this, _: &LineUp, _, cx| this.scroll_rows(-1, cx)))
            .on_action(cx.listener(|this, _: &LineDown, _, cx| this.scroll_rows(1, cx)))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| {
                let n = this.page_rows(cx);
                this.scroll_rows(-n, cx)
            }))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| {
                let n = this.page_rows(cx);
                this.scroll_rows(n, cx)
            }))
            .on_action(cx.listener(|this, _: &Top, _, cx| this.scroll_rows(isize::MIN / 2, cx)))
            .on_action(cx.listener(|this, _: &Bottom, _, cx| this.scroll_rows(isize::MAX / 2, cx)))
            .on_action(cx.listener(|this, _: &AcceptProposal, _, cx| this.decide(true, cx)))
            .on_action(cx.listener(|this, _: &RejectProposal, _, cx| this.decide(false, cx)))
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| this.copy(cx)))
            .on_action(cx.listener(|this, _: &SelectAllLines, _, cx| this.select_all(cx)))
            .on_action(cx.listener(|this, _: &ToggleUnchanged, _, cx| this.toggle_unchanged(cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _| window.focus(&this.focus)),
            )
            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
            .child(self.render_toolbar(cx))
            .children(self.note.clone().map(|note| {
                div()
                    .flex_none()
                    .px(px(12.))
                    .py(px(6.))
                    .border_b_1()
                    .border_color(t.color.border)
                    .bg(t.color.warning.opacity(0.10))
                    .text_size(t.typography.caption)
                    .text_color(t.color.content_secondary)
                    .child(note)
            }))
            .child(
                div()
                    .id("diff-body")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .cursor_text()
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::open_menu))
                    .on_mouse_move(cx.listener(Self::mouse_move))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, _| this.selecting = false),
                    )
                    .child(self.render_body(cx)),
            )
            .children(self.context_menu.as_ref().map(|(menu, _)| menu.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(text: &str) -> Side {
        Side::new(text.to_string(), Lang::for_path(Path::new("x.go")))
    }

    #[test]
    fn tabs_expand_to_the_next_stop_and_runs_cover_the_drawn_text() {
        let t = Theme::dark(true);
        let s = side("package main\n\tx := \"a\"\n");
        let (text, runs) = styled_line(&s, 1, &[], t.color.success, &t);
        assert_eq!(text.as_ref(), "    x := \"a\"");
        assert_eq!(runs.first().unwrap().0.start, 0);
        assert_eq!(runs.last().unwrap().0.end, text.len());
        assert!(runs.windows(2).all(|w| w[0].0.end == w[1].0.start));
        let string = runs
            .iter()
            .find(|(r, _)| &text[r.clone()] == "\"a\"")
            .expect("the string literal is its own run");
        assert_eq!(string.1.color, Some(t.syntax.string));
    }

    #[test]
    fn changed_words_get_a_background_and_crlf_is_not_drawn() {
        let t = Theme::dark(true);
        let s = side("a := 1\r\n");
        let (text, runs) = styled_line(&s, 0, std::slice::from_ref(&(5..6)), t.color.danger, &t);
        assert_eq!(text.as_ref(), "a := 1");
        let marked: String = runs
            .iter()
            .filter(|(_, h)| h.background_color.is_some())
            .map(|(r, _)| &text[r.clone()])
            .collect();
        assert_eq!(marked, "1");
    }

    #[test]
    fn a_selection_covers_whole_middle_rows_and_splits_runs_at_its_ends() {
        let s = Selection {
            side: Which::New,
            anchor: (5, 2),
            head: (3, 4),
        };
        assert_eq!(s.columns(3), Some(4..usize::MAX));
        assert_eq!(s.columns(4), Some(0..usize::MAX));
        assert_eq!(s.columns(5), Some(0..2));
        assert_eq!(s.columns(6), None);
        let t = Theme::dark(true);
        let style = HighlightStyle::default();
        let runs = with_selection("héllo", vec![(0..6, style)], Some(1..3), t.color.accent);
        let marked: Vec<_> = runs
            .iter()
            .map(|(r, h)| (r.clone(), h.background_color.is_some()))
            .collect();
        assert_eq!(marked, vec![(0..1, false), (1..4, true), (4..6, false)]);
    }

    #[test]
    fn a_changed_block_pairs_lines_for_word_highlights() {
        let loaded = Loaded::compute(
            "a\nold one\nc\n".into(),
            "a\nnew one\nadded\nc\n".into(),
            None,
        );
        assert_eq!(loaded.changes.len(), 1);
        assert_eq!((loaded.added, loaded.removed), (2, 1));
        let (old, new) = &loaded.words[&(1, 1)];
        assert_eq!((old.len(), new.len()), (1, 1));
        assert_eq!((old[0].clone(), new[0].clone()), (0..3, 0..3));
        assert_eq!(loaded.partner(None, Some(2), 0), (2, 2));
        assert!(!loaded.words.contains_key(&(2, 2)));
    }
}
