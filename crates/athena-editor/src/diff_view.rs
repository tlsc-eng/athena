use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use athena_ui::{ActiveTheme, SyntaxColors, Theme, Tooltip, empty_state};
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle, Hsla,
    IntoElement, KeyBinding, Render, ScrollStrategy, ScrollWheelEvent, SharedString, StyledText,
    Task, UniformListScrollHandle, Window, actions, div, point, prelude::*, px, uniform_list,
};
use ropey::Rope;

use crate::diff::{self, Change, Row};
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
        Bottom
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
    ]);
}

/// Only this much of a very long line is drawn; minified files would otherwise shape megabytes.
const MAX_DRAWN: usize = 4_000;
/// Word highlights are skipped past this many paired lines; a rewrite is red and green anyway.
const MAX_WORD_PAIRS: usize = 20_000;
const TAB_WIDTH: usize = 4;
const TOOLBAR: f32 = 32.;

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
    actions: HunkActions,
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
            actions,
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
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn label(&self) -> String {
        self.title.to_string()
    }

    /// The new side's text as last diffed, to check a file has not moved on before reverting.
    pub fn new_text(&self) -> Option<&str> {
        Some(&self.loaded.as_ref()?.new.text)
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

    fn header_row(&self, change: usize) -> Option<usize> {
        self.rows()?.iter().position(|r| *r == Row::Header(change))
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
                let headers: Vec<(usize, usize)> = self
                    .rows()
                    .unwrap_or_default()
                    .iter()
                    .enumerate()
                    .filter_map(|(i, r)| match r {
                        Row::Header(c) => Some((i, *c)),
                        Row::Line { .. } => None,
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
        f32::from(cx.theme().typography.code) * 0.6
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
        Token::Variable => syntax.text,
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

#[derive(Clone, Copy, PartialEq)]
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
        div().w(px(14.)).flex_none().text_color(color).child(glyph)
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
                tool("diff-open", "Open File", "Open the file in the editor")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DiffEvent::OpenFile))),
            )
    }

    fn toggle_inline(&mut self, cx: &mut Context<Self>) {
        self.inline = !self.inline;
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
        let digits = loaded
            .old
            .lines
            .len()
            .max(loaded.new.lines.len())
            .max(1)
            .ilog10()
            + 1;
        let gutter = (digits as f32 + 1.) * self.char_width(cx) + 8.;
        let inline = self.inline;
        let count = if inline {
            loaded.unified.len()
        } else {
            loaded.split.len()
        };
        uniform_list(
            "diff-rows",
            count,
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                let p = Palette::new(&t);
                let rows = if inline {
                    &loaded.unified
                } else {
                    &loaded.split
                };
                range
                    .map(|ix| {
                        let row = div().h(px(row_h)).w_full().flex();
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
                                    let bg = if which == Which::Old {
                                        p.removed
                                    } else {
                                        p.added
                                    };
                                    let word_bg = if which == Which::Old {
                                        p.removed_word
                                    } else {
                                        p.added_word
                                    };
                                    let side = if which == Which::Old {
                                        &loaded.old
                                    } else {
                                        &loaded.new
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
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
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| window.focus(&this.focus)),
            )
            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
            .child(self.render_toolbar(cx))
            .child(self.render_body(cx))
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
