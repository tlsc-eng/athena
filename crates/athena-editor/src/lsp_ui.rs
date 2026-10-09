use std::ops::Range;
use std::time::Duration;

use athena_ui::motion;
use athena_ui::{ActiveTheme, InputEvent, TextInput};
use gpui::{
    Action, Animation, AnyElement, App, Context, Corner, Entity, Focusable, KeyBinding, Pixels,
    Point, Subscription, Task, Window, actions, anchored, deferred, div, point, prelude::*, px,
};

use crate::buffer::Buffer;
use crate::element::GUTTER_PAD;
use crate::view::{EditorEvent, EditorView};

/// The cursor rests this long before other uses of its symbol are asked for, as in VS Code.
const HIGHLIGHT_DELAY: Duration = Duration::from_millis(250);
/// Typing or scrolling pauses this long before inlay hints are asked for again.
const INLAY_DELAY: Duration = Duration::from_millis(300);
/// Hints are asked for this many lines above and below the screen, so short scrolls need none.
const INLAY_MARGIN: usize = 100;
/// Typing pauses this long before semantic tokens are asked for again; until they come, the
/// last ones stay on their names as the text moves.
const SEMANTIC_DELAY: Duration = Duration::from_millis(300);

// Handled by the shell, which talks to the language server, as they bubble up from the editor.
actions!(
    editor,
    [
        RenameSymbol,
        ShowCodeActions,
        GoToImplementation,
        GoToTypeDefinition,
        ShowCallHierarchy,
        ShowTypeHierarchy
    ]
);

actions!(editor, [FormatSelection]);

/// The cursor rests this long before the tag names edited with it are asked for.
const LINKED_DELAY: Duration = Duration::from_millis(150);

/// A question for the language server that the shell answers through the method each names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LspRequest {
    /// Fill in a suggestion of the list `request` brought; answer with
    /// [`EditorView::resolved_completion`].
    ResolveCompletion { request: u64, index: usize },
    /// The ranges selection grows through around each caret's zero-based line and UTF-16
    /// column; answer with [`EditorView::show_selection_ranges`].
    SelectionRanges {
        request: u64,
        positions: Vec<(u32, u32)>,
    },
    /// Edits that format `start..end`; answer with [`EditorView::formatted_selection`].
    FormatSelection {
        request: u64,
        start: (u32, u32),
        end: (u32, u32),
        tab_size: u32,
        insert_spaces: bool,
    },
    /// The ranges typed together with the one at a position, such as a tag's closing name;
    /// answer with [`EditorView::show_linked_editing`].
    LinkedEditing {
        request: u64,
        line: u32,
        character: u32,
    },
    /// The file's semantic tokens; answer with [`EditorView::show_semantic_tokens`].
    SemanticTokens { request: u64 },
}

/// A semantic token where a language server put it: zero-based line, UTF-16 start and length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticSpan {
    pub line: u32,
    pub start: u32,
    pub length: u32,
    pub token: crate::Token,
}

/// Steps along one line of a rope from UTF-16 column to UTF-16 column, as tokens on a line come
/// in order; a column past the line's end stops at its end.
struct LineCursor<'a> {
    chars: ropey::iter::Chars<'a>,
    utf16: usize,
    byte: usize,
}

impl LineCursor<'_> {
    fn advance(&mut self, to: usize) -> usize {
        while self.utf16 < to {
            match self.chars.next() {
                Some(c) if c != '\n' && c != '\r' => {
                    self.utf16 += c.len_utf16();
                    self.byte += c.len_utf8();
                }
                _ => {
                    self.utf16 = usize::MAX;
                    break;
                }
            }
        }
        self.byte
    }
}

/// The byte ranges `spans` cover in `rope`, sorted and apart; a span out of order, empty, past
/// the end, or overlapping the one before is left out.
pub(crate) fn place_semantic(
    rope: &ropey::Rope,
    spans: &[SemanticSpan],
) -> Vec<(Range<usize>, crate::Token)> {
    let mut out: Vec<(Range<usize>, crate::Token)> = Vec::with_capacity(spans.len());
    let mut line_no = None;
    let mut cursor: Option<LineCursor> = None;
    for s in spans {
        let line = s.line as usize;
        if line >= rope.len_lines() || line_no.is_some_and(|l| l > line) {
            continue;
        }
        if line_no != Some(line) {
            line_no = Some(line);
            cursor = Some(LineCursor {
                chars: rope.line(line).chars(),
                utf16: 0,
                byte: rope.line_to_byte(line),
            });
        }
        let Some(c) = cursor.as_mut() else {
            continue;
        };
        if (s.start as usize) < c.utf16 {
            continue;
        }
        let start = c.advance(s.start as usize);
        let end = c.advance(s.start as usize + s.length as usize);
        if start < end && out.last().is_none_or(|(r, _)| r.end <= start) {
            out.push((start..end, s.token));
        }
    }
    out
}

/// An edit that linked editing repeats in the other ranges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkedEdit<'a> {
    Type(&'a str),
    Backspace,
    Delete,
}

/// Ranges that are edited together, such as a JSX element's opening and closing tag names.
#[derive(Default)]
pub(crate) struct Linked {
    enabled: bool,
    shown: Anchored<()>,
    /// What the text of every range must stay, from the server, else a tag name's characters.
    pattern: Option<regex::Regex>,
    /// The cursor and buffer version last asked about, or waited on.
    asked: Option<(usize, u64)>,
    /// The request in flight and the buffer version it was asked for.
    pending: Option<(u64, u64)>,
    requests: u64,
    timer: Option<Task<()>>,
}

/// The server's linked ranges in chars, each start before its end; one alone links nothing.
fn linked_ranges(b: &Buffer, ranges: Vec<((u32, u32), (u32, u32))>) -> Vec<(Range<usize>, ())> {
    if ranges.len() < 2 {
        return Vec::new();
    }
    ranges
        .into_iter()
        .map(|(a, z)| {
            let (a, z) = (b.char_at_utf16(a.0, a.1), b.char_at_utf16(z.0, z.1));
            (a.min(z)..a.max(z), ())
        })
        .collect()
}

/// Where the carets mirroring `selection` go in the other `ranges`, if `edit` at it keeps every
/// range the same text; empty when the selection is in none of them or the edit would leave one.
fn mirrors(
    b: &Buffer,
    ranges: &[Range<usize>],
    selection: crate::buffer::Selection,
    edit: LinkedEdit,
    pattern: Option<&regex::Regex>,
) -> Vec<crate::buffer::Selection> {
    let s = selection.range();
    let Some(own) = ranges.iter().find(|r| r.start <= s.start && s.end <= r.end) else {
        return Vec::new();
    };
    let text = b.text(own.clone());
    if ranges.iter().any(|r| b.text(r.clone()) != text) {
        return Vec::new();
    }
    let keeps = match edit {
        // Removing the char before or after would reach past the name.
        LinkedEdit::Backspace => !s.is_empty() || s.start > own.start,
        LinkedEdit::Delete => !s.is_empty() || s.end < own.end,
        LinkedEdit::Type(typed) => {
            let at = s.start - own.start;
            let mut after: String = text.chars().take(at).collect();
            after.push_str(typed);
            after.extend(text.chars().skip(s.end - own.start));
            match pattern {
                Some(re) => re
                    .find(&after)
                    .is_some_and(|m| m.range() == (0..after.len())),
                None => after
                    .chars()
                    .all(|c| c.is_alphanumeric() || "-_.:$".contains(c)),
            }
        }
    };
    if !keeps {
        return Vec::new();
    }
    ranges
        .iter()
        .filter(|r| *r != own)
        .map(|r| crate::buffer::Selection {
            anchor: r.start + (selection.anchor - own.start),
            head: r.start + (selection.head - own.start),
        })
        .collect()
}

/// The name typed into the rename field, sent up to the shell to rename with.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = editor, no_json)]
pub struct ConfirmRename {
    pub name: String,
}

const RENAME_WIDTH: f32 = 260.;

pub(crate) fn init(cx: &mut App) {
    let ctx = Some("Editor");
    cx.bind_keys([
        KeyBinding::new("f2", RenameSymbol, ctx),
        KeyBinding::new("cmd-.", ShowCodeActions, ctx),
        KeyBinding::new("cmd-f12", GoToImplementation, ctx),
        KeyBinding::new("shift-alt-h", ShowCallHierarchy, ctx),
        KeyBinding::new("cmd-k cmd-f", FormatSelection, ctx),
    ]);
}

/// Char ranges a language server gave for one buffer version, followed through later edits.
pub(crate) struct Anchored<T> {
    version: u64,
    items: Vec<(Range<usize>, T)>,
    /// Text typed at a range's edges joins it, as it does a snippet's placeholder.
    grow: bool,
}

impl<T> Default for Anchored<T> {
    fn default() -> Self {
        Self {
            version: 0,
            items: Vec::new(),
            grow: false,
        }
    }
}

impl<T: Clone> Anchored<T> {
    pub(crate) fn new(b: &Buffer, items: Vec<(Range<usize>, T)>) -> Self {
        Self {
            version: b.version(),
            items,
            grow: false,
        }
    }

    pub(crate) fn growing(b: &Buffer, items: Vec<(Range<usize>, T)>) -> Self {
        Self {
            grow: true,
            ..Self::new(b, items)
        }
    }

    /// The ranges where they are in `b` now; none once the edits since are no longer kept.
    pub(crate) fn now(&self, b: &Buffer) -> Vec<(Range<usize>, T)> {
        let Some(edits) = b.edits_since(self.version) else {
            return Vec::new();
        };
        let edits: Vec<_> = edits.collect();
        let follow = |r: Range<usize>, e: &&crate::buffer::Edit| {
            let typed_at = |at: usize| e.removed == 0 && at == e.at;
            let start = match !self.grow && typed_at(r.start) {
                true => r.start + e.inserted,
                false => e.map(r.start),
            };
            let end = match self.grow && typed_at(r.end) {
                true => r.end + e.inserted,
                false => e.map(r.end),
            };
            start..end.max(start)
        };
        self.items
            .iter()
            .map(|(r, t)| (edits.iter().fold(r.clone(), follow), t.clone()))
            .collect()
    }

    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// A use of the symbol at the cursor, in zero-based lines and UTF-16 columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Occurrence {
    pub start: (u32, u32),
    pub end: (u32, u32),
    /// The symbol is assigned here.
    pub write: bool,
}

/// Other uses of the symbol at the cursor, and whether each writes it.
#[derive(Default)]
pub(crate) struct Occurrences {
    pub(crate) shown: Anchored<bool>,
    /// The cursor and buffer version last asked about, or waited on.
    asked: Option<(usize, u64)>,
    /// The request in flight and the buffer version it was asked for.
    pending: Option<(u64, u64)>,
    requests: u64,
    timer: Option<Task<()>>,
}

/// A note drawn inside the text, positioned as language servers count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inlay {
    pub position: (u32, u32),
    /// What is drawn, padding included.
    pub text: String,
    /// It names the type of what comes before it, so the caret stays on that side of it.
    pub is_type: bool,
}

#[derive(Default)]
pub(crate) struct Inlays {
    /// Each hint's text and whether it is a type.
    pub(crate) shown: Anchored<(String, bool)>,
    enabled: bool,
    /// The buffer version and lines last asked about, or waited on.
    asked: Option<(u64, Range<usize>)>,
    /// The request in flight and the buffer version it was asked for.
    pending: Option<(u64, u64)>,
    requests: u64,
    timer: Option<Task<()>>,
}

/// Semantic tokens: whether to ask, and the request in flight.
#[derive(Default)]
pub(crate) struct Semantic {
    enabled: bool,
    /// The buffer version last asked about, or waited on.
    asked: Option<u64>,
    /// Ask even if another view of the buffer already has this version's tokens.
    forced: bool,
    /// The request in flight and the buffer version it was asked for.
    pending: Option<(u64, u64)>,
    requests: u64,
    timer: Option<Task<()>>,
    placing: Option<Task<()>>,
}

/// The inline field F2 opens over a symbol.
pub(crate) struct RenameBox {
    input: Entity<TextInput>,
    /// Where the symbol starts, in chars, so the field follows edits above it.
    at: usize,
    original: String,
    opened: motion::Opening,
    _subscriptions: Vec<Subscription>,
}

impl EditorView {
    /// VS Code's `editor.linkedEditing`: typing in a tag name renames its matching tag.
    pub fn set_linked_editing(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.linked.enabled != enabled {
            self.linked = Linked {
                enabled,
                ..Linked::default()
            };
            cx.notify();
        }
    }

    /// Asks, once the cursor rests outside the ranges already known, which ranges are typed
    /// together with the one under it; called every frame.
    pub(crate) fn schedule_linked(&mut self, focused: bool, cx: &mut Context<Self>) {
        let tags = matches!(
            self.lang(),
            Some(crate::Lang::Tsx | crate::Lang::JavaScript | crate::Lang::Html)
        );
        if !self.linked.enabled || !focused || !tags || !self.completing_attached() {
            return;
        }
        let Some(version) = self.version() else {
            return;
        };
        let head = self.cursor.head();
        let key = (head, version);
        if self.linked.asked == Some(key) || self.cursor.is_multi() {
            return;
        }
        self.linked.asked = Some(key);
        let inside = self.buf().is_some_and(|b| {
            self.linked
                .shown
                .now(&b)
                .iter()
                .any(|(r, _)| r.start <= head && head <= r.end)
        });
        if inside {
            return;
        }
        self.linked.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LINKED_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.linked.timer = None;
                let Some((line, character)) = this.cursor_utf16() else {
                    return;
                };
                this.linked.requests += 1;
                let request = this.linked.requests;
                this.linked.pending = Some((request, version));
                cx.emit(EditorEvent::Lsp(LspRequest::LinkedEditing {
                    request,
                    line,
                    character,
                }));
            });
        }));
    }

    /// The answer to [`LspRequest::LinkedEditing`]; dropped if the text changed since.
    pub fn show_linked_editing(
        &mut self,
        request: u64,
        ranges: Vec<((u32, u32), (u32, u32))>,
        word_pattern: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some((asked, version)) = self.linked.pending else {
            return;
        };
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        let b = shared.buffer.borrow();
        if asked != request || b.version() != version {
            return;
        }
        self.linked.pending = None;
        self.linked.shown = Anchored::growing(&b, linked_ranges(&b, ranges));
        self.linked.pattern = word_pattern.and_then(|p| regex::Regex::new(&p).ok());
        cx.notify();
    }

    /// Runs a typing edit at every caret, and, with linked editing, at the same place in the
    /// other ranges linked to the one it is in, as one undo step.
    pub(crate) fn edit_linked(
        &mut self,
        edit: LinkedEdit,
        cx: &mut Context<Self>,
        op: impl FnMut(&mut Buffer, &mut crate::buffer::Cursor),
    ) {
        self.follow_edits();
        let extra = match (self.linked.enabled, self.cursor.is_multi(), self.buf()) {
            (true, false, Some(b)) => {
                let ranges: Vec<Range<usize>> = self
                    .linked
                    .shown
                    .now(&b)
                    .into_iter()
                    .map(|(r, _)| r)
                    .collect();
                mirrors(
                    &b,
                    &ranges,
                    self.cursor.selection(),
                    edit,
                    self.linked.pattern.as_ref(),
                )
            }
            _ => Vec::new(),
        };
        self.with_buffer(cx, |b, c| {
            if extra.is_empty() {
                return b.edit_each(c, op);
            }
            let mut all = vec![*c.primary()];
            all.extend(extra.into_iter().map(|selection| crate::buffer::Cursor {
                selection,
                ..Default::default()
            }));
            c.set(all, 0);
            b.edit_each(c, op);
            c.collapse();
        });
    }

    /// Cmd+K Cmd+F: asks the server to format the selection, or the cursor's line.
    pub(crate) fn format_selection(&mut self, cx: &mut Context<Self>) {
        let Some((version, start, end, indent)) = self.buf().map(|b| {
            let s = self.cursor.selection().range();
            let range = match s.is_empty() {
                true => {
                    let line = b.line_of(s.start);
                    b.line_start(line)
                        ..b.line_start(line)
                            + b.line(line).trim_end_matches(['\n', '\r']).chars().count()
                }
                false => s,
            };
            (
                b.version(),
                b.utf16_position(range.start),
                b.utf16_position(range.end),
                b.indent,
            )
        }) else {
            return;
        };
        self.format_requests += 1;
        let request = self.format_requests;
        self.range_format = Some((request, version));
        let (tab_size, insert_spaces) = match indent {
            crate::buffer::Indent::Tab => (crate::display::TAB_WIDTH as u32, false),
            crate::buffer::Indent::Spaces(n) => (n as u32, true),
        };
        cx.emit(EditorEvent::Lsp(LspRequest::FormatSelection {
            request,
            start,
            end,
            tab_size,
            insert_spaces,
        }));
    }

    /// The answer to [`LspRequest::FormatSelection`]: applied as one undo step if the text has
    /// not changed since it was asked.
    pub fn formatted_selection(
        &mut self,
        request: u64,
        edits: Vec<crate::ServerEdit>,
        cx: &mut Context<Self>,
    ) {
        if self.range_format.map(|(asked, _)| asked) != Some(request) {
            return;
        }
        let Some((_, version)) = self.range_format.take() else {
            return;
        };
        if self.version() == Some(version) {
            self.apply_server_edits(&edits, cx);
        }
    }

    /// The word under the cursor: its zero-based line and UTF-16 start, and its text.
    pub fn word_at_cursor(&self) -> Option<((u32, u32), String)> {
        let b = self.buf()?;
        let head = self.cursor.head();
        let word = b
            .word_at(head)
            .or_else(|| head.checked_sub(1).and_then(|h| b.word_at(h)))?;
        Some((b.utf16_position(word.start), b.text(word)))
    }

    /// The text between two zero-based line and UTF-16 column positions.
    pub fn text_between(&self, start: (u32, u32), end: (u32, u32)) -> Option<String> {
        let b = self.buf()?;
        let (a, z) = (
            b.char_at_utf16(start.0, start.1),
            b.char_at_utf16(end.0, end.1),
        );
        Some(b.text(a.min(z)..z.max(a)))
    }

    /// Where a menu for the cursor opens, in window coordinates: under its line.
    pub fn cursor_anchor(&self) -> Option<Point<Pixels>> {
        let origin = self.char_origin(self.cursor.head())?;
        Some(point(
            origin.x,
            origin.y + self.layout.as_ref()?.line_height,
        ))
    }

    /// Shows the rename field over the symbol starting at `start`, holding `name` selected so
    /// typing replaces it; Enter dispatches [`ConfirmRename`], Escape or clicking away cancels.
    pub fn show_rename(
        &mut self,
        start: (u32, u32),
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(at) = self.buf().map(|b| b.char_at_utf16(start.0, start.1)) else {
            return;
        };
        self.hide_hover(cx);
        self.dismiss_completion(cx);
        let input = cx.new(|cx| {
            let mut input = TextInput::new("New name", cx);
            input.set_text(name.clone(), cx);
            input.select_all(cx);
            input
        });
        let submit = cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| match event {
                InputEvent::Submit | InputEvent::SubmitBeside => {
                    let name = input.read(cx).text().trim().to_string();
                    let changed = this.rename.as_ref().is_some_and(|r| r.original != name);
                    this.close_rename(window, cx);
                    if changed && !name.is_empty() {
                        window.dispatch_action(Box::new(ConfirmRename { name }), cx);
                    }
                }
                InputEvent::Cancel => this.close_rename(window, cx),
                _ => {}
            },
        );
        let blur = cx.on_blur(&input.focus_handle(cx), window, |this, _, cx| {
            if this.rename.take().is_some() {
                cx.notify();
            }
        });
        window.focus(&input.focus_handle(cx));
        self.rename = Some(RenameBox {
            input,
            at,
            original: name,
            opened: motion::Opening::now(),
            _subscriptions: vec![submit, blur],
        });
        cx.notify();
    }

    fn close_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.rename.take().is_some() {
            window.focus(&self.focus);
            cx.notify();
        }
    }

    pub(crate) fn render_rename(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rename = self.rename.as_ref()?;
        let origin = self.char_origin(rename.at)?;
        let line_height = self.layout.as_ref()?.line_height;
        let t = cx.theme();
        let panel = div()
            .id("rename-symbol")
            .occlude()
            .w(px(RENAME_WIDTH))
            .p(px(4.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_panel)
            .shadow(vec![t.popover_shadow()])
            .font_family(t.typography.ui.clone())
            .text_size(t.typography.caption)
            .child(
                div()
                    .h(px(24.))
                    .px(px(6.))
                    .flex()
                    .items_center()
                    .bg(t.color.surface_sunken)
                    .border_1()
                    .border_color(t.color.accent)
                    .rounded(t.shape.radius_control)
                    .font_family(t.typography.mono.clone())
                    .child(rename.input.clone()),
            )
            .child(
                div()
                    .px(px(2.))
                    .text_color(t.color.content_muted)
                    .child("Enter to rename, Escape to cancel"),
            );
        let panel = motion::animate_enter(
            t.motion.reduced,
            rename.opened.running(t.motion.fast),
            panel,
            "rename-symbol-open",
            Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d),
        );
        Some(
            deferred(
                anchored()
                    .position(point(origin.x - px(5.), origin.y + line_height + px(2.)))
                    .anchor(Corner::TopLeft)
                    .snap_to_window_with_margin(px(8.))
                    .child(panel),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    /// Asks, once the cursor has rested, where else its symbol is used; called every frame.
    pub(crate) fn schedule_occurrences(&mut self, focused: bool, cx: &mut Context<Self>) {
        let attached = self.completing_attached();
        let Some(version) = self.version().filter(|_| focused && attached) else {
            return;
        };
        let key = (self.cursor.head(), version);
        if self.occurrences.asked == Some(key) || self.cursor.is_multi() {
            return;
        }
        self.occurrences.asked = Some(key);
        self.occurrences.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HIGHLIGHT_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.occurrences.timer = None;
                let Some((line, character)) = this.cursor_utf16() else {
                    return;
                };
                this.occurrences.requests += 1;
                let request = this.occurrences.requests;
                this.occurrences.pending = Some((request, version));
                cx.emit(EditorEvent::DocumentHighlight {
                    request,
                    line,
                    character,
                });
            });
        }));
    }

    /// The answer to an [`EditorEvent::DocumentHighlight`]; dropped if the text changed since.
    pub fn show_document_highlights(
        &mut self,
        request: u64,
        ranges: Vec<Occurrence>,
        cx: &mut Context<Self>,
    ) {
        let Some((asked, version)) = self.occurrences.pending else {
            return;
        };
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        let b = shared.buffer.borrow();
        if asked != request || b.version() != version {
            return;
        }
        self.occurrences.pending = None;
        let items: Vec<(Range<usize>, bool)> = ranges
            .into_iter()
            .map(|o| {
                let (a, z) = (o.start, o.end);
                (
                    b.char_at_utf16(a.0, a.1)..b.char_at_utf16(z.0, z.1),
                    o.write,
                )
            })
            .filter(|(r, _)| !r.is_empty())
            .collect();
        if items.is_empty() && self.occurrences.shown.is_empty() {
            return;
        }
        self.occurrences.shown = Anchored::new(&b, items);
        cx.notify();
    }

    /// Lets the editor ask for inlay hints, or hides them.
    pub fn set_inlay_hints(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.inlays.enabled == enabled {
            return;
        }
        self.inlays = Inlays {
            enabled,
            ..Inlays::default()
        };
        cx.notify();
    }

    /// Asks for inlay hints again, as after the server's settings changed.
    pub fn refresh_inlay_hints(&mut self, cx: &mut Context<Self>) {
        self.inlays.asked = None;
        cx.notify();
    }

    /// Asks for the hints around the lines on screen once typing or scrolling pauses; called
    /// every frame.
    pub(crate) fn schedule_inlays(&mut self, cx: &mut Context<Self>) {
        if !self.inlays.enabled || !self.completing_attached() {
            return;
        }
        let (Some(version), Some(lines)) = (self.version(), self.buf().map(|b| b.len_lines()))
        else {
            return;
        };
        let Some((first, last)) = self.layout.as_ref().and_then(|l| {
            let first = l.rows.first()?.line;
            Some((first, l.rows.last()?.line))
        }) else {
            return;
        };
        if let Some((v, asked)) = &self.inlays.asked
            && *v == version
            && asked.start <= first
            && last < asked.end
        {
            return;
        }
        let window = first.saturating_sub(INLAY_MARGIN)..(last + INLAY_MARGIN + 1).min(lines);
        self.inlays.asked = Some((version, window.clone()));
        self.inlays.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(INLAY_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.inlays.timer = None;
                this.inlays.requests += 1;
                let request = this.inlays.requests;
                this.inlays.pending = Some((request, version));
                cx.emit(EditorEvent::InlayHints {
                    request,
                    start_line: window.start as u32,
                    end_line: window.end as u32,
                });
            });
        }));
    }

    /// The answer to an [`EditorEvent::InlayHints`]; dropped if the text changed since.
    pub fn show_inlay_hints(&mut self, request: u64, hints: Vec<Inlay>, cx: &mut Context<Self>) {
        let Some((asked, version)) = self.inlays.pending else {
            return;
        };
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        let b = shared.buffer.borrow();
        if asked != request || b.version() != version || !self.inlays.enabled {
            return;
        }
        self.inlays.pending = None;
        let items: Vec<(Range<usize>, (String, bool))> = hints
            .into_iter()
            .map(|h| {
                let at = b.char_at_utf16(h.position.0, h.position.1);
                let text = h.text.replace(['\n', '\t'], " ");
                (at..at, (text, h.is_type))
            })
            .collect();
        if items.is_empty() && self.inlays.shown.is_empty() {
            return;
        }
        self.inlays.shown = Anchored::new(&b, items);
        cx.notify();
    }

    /// VS Code's `editor.semanticHighlighting.enabled`: colour names as the language server
    /// classifies them, over the syntax colours.
    pub fn set_semantic_highlighting(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.semantic.enabled == enabled {
            return;
        }
        self.semantic = Semantic {
            enabled,
            ..Semantic::default()
        };
        if !enabled && let Some(shared) = self.buffer.clone() {
            shared
                .buffer
                .borrow_mut()
                .set_semantic_tokens(Vec::new(), None);
            shared.parsed.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }

    /// Asks for semantic tokens again, as after the server said they are out of date.
    pub fn refresh_semantic_tokens(&mut self, cx: &mut Context<Self>) {
        self.semantic.asked = None;
        self.semantic.forced = true;
        cx.notify();
    }

    /// Asks for the file's semantic tokens once typing pauses after a change; called every
    /// frame. A file's first tokens are asked for at once.
    pub(crate) fn schedule_semantic(&mut self, cx: &mut Context<Self>) {
        if !self.semantic.enabled || !self.completing_attached() {
            return;
        }
        let Some(version) = self.version() else {
            return;
        };
        if self.semantic.asked == Some(version) {
            return;
        }
        self.semantic.asked = Some(version);
        let had = self.buf().and_then(|b| b.semantic_version());
        // Another view of the same buffer may have brought this version's tokens already.
        if had == Some(version) && !self.semantic.forced {
            return;
        }
        let delay = if had.is_none() && self.semantic.requests == 0 {
            Duration::ZERO
        } else {
            SEMANTIC_DELAY
        };
        self.semantic.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                this.semantic.timer = None;
                this.semantic.forced = false;
                this.semantic.requests += 1;
                let request = this.semantic.requests;
                this.semantic.pending = Some((request, version));
                cx.emit(EditorEvent::Lsp(LspRequest::SemanticTokens { request }));
            });
        }));
    }

    /// The answer to [`LspRequest::SemanticTokens`], placed in the text off the UI thread; an
    /// answer for text that changed since is dropped and asked for again.
    pub fn show_semantic_tokens(
        &mut self,
        request: u64,
        spans: Vec<SemanticSpan>,
        cx: &mut Context<Self>,
    ) {
        let Some((asked, version)) = self.semantic.pending else {
            return;
        };
        if asked != request || !self.semantic.enabled {
            return;
        }
        self.semantic.pending = None;
        let Some(rope) = self
            .buf()
            .filter(|b| b.version() == version)
            .map(|b| b.rope().clone())
        else {
            self.semantic.asked = None;
            cx.notify();
            return;
        };
        self.semantic.placing = Some(cx.spawn(async move |this, cx| {
            let placed = cx
                .background_spawn(async move { place_semantic(&rope, &spans) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.semantic.placing = None;
                let Some(shared) = this.buffer.clone() else {
                    return;
                };
                if shared.buffer.borrow().version() != version {
                    this.semantic.asked = None;
                    cx.notify();
                    return;
                }
                shared
                    .buffer
                    .borrow_mut()
                    .set_semantic_tokens(placed, Some(version));
                shared.parsed.update(cx, |_, cx| cx.notify());
            });
        }));
    }

    /// Shows the code action lightbulb on a zero-based line, or hides it.
    pub fn set_lightbulb(&mut self, line: Option<u32>, cx: &mut Context<Self>) {
        let line = line.map(|l| l as usize);
        if self.lightbulb != line {
            self.lightbulb = line;
            cx.notify();
        }
    }

    /// A click on the lightbulb in the gutter asks for code actions; true if it was one.
    pub(crate) fn click_lightbulb(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (Some(line), Some(layout)) = (self.lightbulb, self.layout.as_ref()) else {
            return false;
        };
        if position.x < layout.origin.x || position.x >= layout.origin.x + px(GUTTER_PAD) {
            return false;
        }
        let row = self.display.row_of(line);
        let top = layout.origin.y + layout.line_height * row as f32 - px(self.scroll.y);
        if position.y < top || position.y >= top + layout.line_height {
            return false;
        }
        window.dispatch_action(Box::new(ShowCodeActions), cx);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{Cursor, Selection};

    fn span(line: u32, start: u32, length: u32, token: crate::Token) -> SemanticSpan {
        SemanticSpan {
            line,
            start,
            length,
            token,
        }
    }

    #[test]
    fn semantic_spans_land_on_bytes_past_wide_chars_tabs_and_line_ends() {
        use crate::Token::{Function, Parameter, Type, Variable};
        let text = "a😀b\tcd\r\n日本 x\nlast";
        let rope = ropey::Rope::from_str(text);
        let placed = place_semantic(
            &rope,
            &[
                span(0, 0, 1, Variable),
                span(0, 3, 1, Function),
                span(0, 5, 2, Type),
                span(0, 6, 9, Parameter),
                span(1, 0, 2, Type),
                span(1, 3, 1, Variable),
                span(0, 0, 1, Variable),
                span(2, 2, 10, Function),
                span(9, 0, 1, Type),
            ],
        );
        let shown: Vec<(&str, crate::Token)> =
            placed.iter().map(|(r, t)| (&text[r.clone()], *t)).collect();
        assert_eq!(
            shown,
            [
                ("a", Variable),
                ("b", Function),
                ("cd", Type),
                ("日本", Type),
                ("x", Variable),
                ("st", Function),
            ],
            "an emoji is two UTF-16 units; overlaps, out-of-order lines and lines past the end go"
        );
    }

    #[test]
    fn semantic_tokens_follow_typing_until_the_next_answer() {
        let mut b = Buffer::new(
            "package main\n\nfunc f(n int) int { return n }\n",
            Some("/p/main.go".into()),
        );
        let placed = place_semantic(b.rope(), &[span(2, 7, 1, crate::Token::Parameter)]);
        b.set_semantic_tokens(placed, Some(b.version()));
        let painted = |b: &Buffer, needle: &str| {
            let text = b.text(0..b.len_chars());
            let at = text.find(needle).unwrap();
            b.highlights(0..b.len_lines())
                .into_iter()
                .rfind(|(r, _)| r.contains(&at))
                .map(|(_, t)| t)
        };
        assert_eq!(painted(&b, "n int"), Some(crate::Token::Parameter));
        let mut c = Cursor::at(b.line_start(2) + 5);
        b.insert(&mut c, "oo");
        assert_eq!(painted(&b, "n int"), Some(crate::Token::Parameter));
        assert_ne!(b.semantic_version(), Some(b.version()), "now out of date");
    }

    /// The cost semantic tokens add: decoding a 2k-line file's tokens off the UI thread, and on
    /// the UI thread, moving them with each keystroke and painting 60 lines.
    /// `cargo test -p athena-editor --release --lib -- --ignored semantic_cost --nocapture`
    #[test]
    #[ignore]
    fn semantic_cost_on_a_2k_line_go_file() {
        use std::time::Instant;
        for functions in [400, 2000] {
            let text: String = (0..functions)
            .map(|i| format!("func f{i}(alpha int, beta string) int {{\n\tgamma := alpha * {i}\n\t// note {i}\n\treturn gamma + len(beta)\n}}\n"))
            .collect();
            println!("{} lines", functions * 5);
            let mut spans = Vec::new();
            for (line, text) in text.lines().enumerate() {
                let mut at = 0;
                for word in text.split(|c: char| !c.is_alphanumeric()) {
                    if !word.is_empty() {
                        spans.push(span(
                            line as u32,
                            at,
                            word.len() as u32,
                            crate::Token::Variable,
                        ));
                    }
                    at += word.len() as u32 + 1;
                }
            }
            let data: Vec<u32> = spans
                .iter()
                .scan((0u32, 0u32), |(line, start), s| {
                    let dl = s.line - *line;
                    let ds = if dl == 0 { s.start - *start } else { s.start };
                    (*line, *start) = (s.line, s.start);
                    Some([dl, ds, s.length, 0, 0])
                })
                .flatten()
                .collect();
            let ms = |t: Instant| t.elapsed().as_secs_f64() * 1e3;
            let t = Instant::now();
            let decoded = athena_lsp_free_decode(&data);
            let placed = place_semantic(&ropey::Rope::from_str(&text), &decoded);
            println!(
                "decode + place {} tokens (background): {:.2} ms",
                placed.len(),
                ms(t)
            );
            for layer in [false, true, false, true] {
                let mut b = Buffer::new(&text, Some("/p/main.go".into()));
                if layer {
                    b.set_semantic_tokens(placed.clone(), Some(b.version()));
                }
                let mut c = Cursor::at(b.line_start(functions * 5 / 2) + 1);
                let mut keys = Vec::new();
                for ch in "x := alpha + beta\n".repeat(40).chars() {
                    let t = Instant::now();
                    match ch {
                        '\n' => b.newline(&mut c),
                        ch => b.type_char(&mut c, ch),
                    }
                    let first = b.line_of(c.head()).saturating_sub(30);
                    std::hint::black_box(b.highlights(first..first + 60));
                    keys.push(ms(t));
                }
                keys.sort_by(f64::total_cmp);
                let avg = keys.iter().sum::<f64>() / keys.len() as f64;
                println!(
                    "keystroke + 60-line highlights, semantic layer {layer}: avg {avg:.3} ms, p99 {:.3} ms, max {:.3} ms",
                    keys[keys.len() * 99 / 100],
                    keys[keys.len() - 1]
                );
            }
        }
    }

    /// The protocol's relative decoding, as athena-lsp does it, without depending on it.
    fn athena_lsp_free_decode(data: &[u32]) -> Vec<SemanticSpan> {
        let (mut line, mut start) = (0, 0);
        data.chunks(5)
            .map(|t| {
                if t[0] > 0 {
                    (line, start) = (line + t[0], t[1]);
                } else {
                    start += t[1];
                }
                span(line, start, t[2], crate::Token::Variable)
            })
            .collect()
    }

    #[test]
    fn linked_ranges_given_end_first_are_put_in_order() {
        let b = Buffer::new("<div>x</div>", None);
        assert_eq!(
            linked_ranges(&b, vec![((0, 4), (0, 1)), ((0, 8), (0, 11))]),
            [(1..4, ()), (8..11, ())]
        );
        assert!(linked_ranges(&b, vec![((0, 1), (0, 4))]).is_empty());
    }

    #[test]
    fn typing_in_a_tag_name_reaches_its_partner_only_while_it_stays_a_name() {
        let b = Buffer::new("<div>x</div>", None);
        let ranges = [1..4, 8..11];
        let at = |i| Selection { anchor: i, head: i };
        assert_eq!(
            mirrors(&b, &ranges, at(4), LinkedEdit::Type("x"), None),
            [at(11)]
        );
        assert_eq!(
            mirrors(&b, &ranges, at(9), LinkedEdit::Type("-"), None),
            [at(2)]
        );
        assert!(
            mirrors(&b, &ranges, at(4), LinkedEdit::Type(" "), None).is_empty(),
            "a space ends the name"
        );
        assert!(
            mirrors(&b, &ranges, at(1), LinkedEdit::Backspace, None).is_empty(),
            "would delete the <"
        );
        assert_eq!(
            mirrors(&b, &ranges, at(2), LinkedEdit::Backspace, None),
            [at(9)]
        );
        assert!(mirrors(&b, &ranges, at(11), LinkedEdit::Delete, None).is_empty());
        assert!(
            mirrors(&b, &ranges, at(6), LinkedEdit::Type("y"), None).is_empty(),
            "outside both"
        );
        let whole = Selection { anchor: 1, head: 4 };
        assert_eq!(
            mirrors(&b, &ranges, whole, LinkedEdit::Type("span"), None),
            [Selection {
                anchor: 8,
                head: 11
            }]
        );
        let digits = regex::Regex::new("[0-9]+").unwrap();
        assert!(mirrors(&b, &ranges, at(4), LinkedEdit::Type("1"), Some(&digits)).is_empty());
        let unequal = Buffer::new("<div>x</dvi>", None);
        assert!(mirrors(&unequal, &ranges, at(4), LinkedEdit::Type("x"), None).is_empty());
    }

    #[test]
    fn found_ranges_follow_edits_made_after_they_were_found() {
        let mut b = Buffer::new("count = count + 1\n", None);
        let found = Anchored::new(&b, vec![(0..5, true), (8..13, false)]);
        let mut c = Cursor::at(0);
        b.insert(&mut c, "😀 ");
        assert_eq!(found.now(&b), vec![(2..7, true), (10..15, false)]);
        let mut c = Cursor::at(12);
        b.insert(&mut c, "x");
        assert_eq!(
            found.now(&b)[1],
            (10..16, false),
            "typing inside a use widens it"
        );
        let mut c = Cursor::at(16);
        b.insert(&mut c, "y");
        let mut c = Cursor::at(10);
        b.insert(&mut c, "z");
        assert_eq!(
            found.now(&b)[1],
            (11..17, false),
            "typing at its edges does not"
        );
        let point = Anchored::new(&b, vec![(5..5, ())]);
        let mut c = Cursor::at(5);
        b.insert(&mut c, "ab");
        assert_eq!(
            point.now(&b),
            vec![(7..7, ())],
            "a point moves past text typed at it"
        );
    }
}
