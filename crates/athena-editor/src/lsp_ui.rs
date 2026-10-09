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
        let items: Vec<(Range<usize>, ())> = match ranges.len() {
            0 | 1 => Vec::new(),
            _ => ranges
                .into_iter()
                .map(|(a, z)| (b.char_at_utf16(a.0, a.1)..b.char_at_utf16(z.0, z.1), ()))
                .collect(),
        };
        self.linked.shown = Anchored::growing(&b, items);
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
