use std::cmp::Reverse;
use std::ops::Range;
use std::time::Duration;

use athena_ui::ActiveTheme;
use athena_ui::motion::{self, Opening};
use gpui::{
    Animation, AnyElement, Context, Corner, FontWeight, HighlightStyle, Hsla, IntoElement,
    ScrollWheelEvent, StyledText, Task, anchored, deferred, div, point, prelude::*, px,
};

use crate::buffer::{Buffer, Cursors, is_word};
use crate::hover::HoverBlock;
use crate::lsp_ui::LspRequest;
use crate::view::{EditorEvent, EditorView};

/// Typing pauses this long before suggestions are asked for.
const TYPING_DELAY: Duration = Duration::from_millis(100);
const MAX_ROWS: usize = 10;
const ROW_HEIGHT: f32 = 22.;
const WIDTH: f32 = 420.;
const DOCS_WIDTH: f32 = 340.;

/// A change a language server asks for, in its zero-based lines and UTF-16 columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerEdit {
    pub start: (u32, u32),
    pub end: (u32, u32),
    pub text: String,
}

/// One suggestion; snippets arrive already reduced to plain text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub label: String,
    /// The protocol's CompletionItemKind number.
    pub kind: Option<u32>,
    pub detail: Option<String>,
    pub filter_text: String,
    pub sort_text: String,
    pub text: String,
    /// What `text` replaces, as the server counts; without it, the word before the cursor.
    pub range: Option<((u32, u32), (u32, u32))>,
    /// The part of `text` to select once inserted, in chars.
    pub select: Option<Range<usize>>,
    /// A snippet's tab stops: each one's number and range in chars of `text`, `$0` numbered 0.
    pub stops: Vec<(u32, Range<usize>)>,
    pub additional_edits: Vec<ServerEdit>,
    pub preselect: bool,
    pub documentation: Vec<HoverBlock>,
    /// The server fills in more (documentation, an import to add) once asked to resolve it.
    pub resolve: bool,
}

/// What `completionItem/resolve` added to a suggestion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    pub detail: Option<String>,
    pub documentation: Vec<HoverBlock>,
    pub additional_edits: Vec<ServerEdit>,
}

struct Menu {
    /// The request that brought these items, which resolving them refers to.
    request: u64,
    /// Where the word being completed starts, in chars.
    start: usize,
    items: Vec<Completion>,
    /// Indexes into `items` that match what was typed, best first, with the label chars matched.
    matches: Vec<(usize, Vec<usize>)>,
    selected: usize,
    first_row: usize,
    incomplete: bool,
    opened: Opening,
    /// Items asked to resolve, whose answers are on their way.
    asked: Vec<usize>,
}

#[derive(Default)]
pub(crate) struct Completing {
    menu: Option<Menu>,
    /// The request in flight and where its word starts.
    pending: Option<(u64, usize)>,
    requests: u64,
    timer: Option<Task<()>>,
    /// Characters that ask for suggestions at once; `None` while no language server has the file.
    triggers: Option<Vec<String>>,
    /// An item accepted before it resolved: its request and index, the buffer version its
    /// insertion left, and the line it was inserted on; resolved edits above it still apply.
    late: Option<(u64, usize, u64, u32)>,
}

/// How well `query` matches `candidate` as a case-insensitive subsequence whose first char starts a
/// word of `candidate`, as VS Code requires; higher is better. Returns the chars matched.
pub(crate) fn fuzzy_match(query: &str, candidate: &str) -> Option<(i32, Vec<usize>)> {
    let cand: Vec<char> = candidate.chars().collect();
    let query: Vec<char> = query.chars().collect();
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    // Word starts are worth jumping to, unless that leaves later chars unmatched.
    [true, false]
        .into_iter()
        .filter_map(|prefer_words| align(&query, &cand, prefer_words))
        .max_by_key(|(score, _)| *score)
}

fn align(query: &[char], cand: &[char], prefer_words: bool) -> Option<(i32, Vec<usize>)> {
    let boundary = |i: usize| {
        i == 0
            || !cand[i - 1].is_alphanumeric()
            || (cand[i - 1].is_lowercase() && cand[i].is_uppercase())
    };
    let same = |a: char, b: char| a.to_lowercase().eq(b.to_lowercase());
    let first = query[0];
    let start = (0..cand.len()).find(|&i| boundary(i) && same(cand[i], first))?;
    let mut matched = vec![start];
    let mut score = if start == 0 { 12 } else { 4 } + i32::from(cand[start] == first);
    let mut at = start + 1;
    for &q in &query[1..] {
        let next =
            |word: bool| (at..cand.len()).find(|&i| same(cand[i], q) && (!word || boundary(i)));
        let i = match next(false)? {
            i if i == at || !prefer_words => i,
            i => next(true).unwrap_or(i),
        };
        score += 1 + i32::from(cand[i] == q);
        if i == at {
            score += 5;
        } else if boundary(i) {
            score += 3;
        } else {
            score -= (i - at).min(4) as i32;
        }
        matched.push(i);
        at = i + 1;
    }
    Some((score, matched))
}

/// The resolved edits elsewhere that still fit once an item was inserted on `line`: those
/// above it, which the insertion did not move.
fn edits_above(edits: Vec<ServerEdit>, line: u32) -> Vec<ServerEdit> {
    edits.into_iter().filter(|e| e.end.0 < line).collect()
}

impl Menu {
    /// The selected item, if it still needs resolving and has not been asked about; noted as asked.
    fn take_unresolved(&mut self) -> Option<usize> {
        let &(index, _) = self.matches.get(self.selected)?;
        if !self.items[index].resolve || self.asked.contains(&index) {
            return None;
        }
        self.asked.push(index);
        Some(index)
    }

    fn refilter(&mut self, query: &str) {
        let mut scored: Vec<(i32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| Some((fuzzy_match(query, &item.filter_text)?.0, i)))
            .collect();
        scored.sort_by(|a, b| {
            let (x, y) = (&self.items[a.1], &self.items[b.1]);
            (Reverse(a.0), &x.sort_text, &x.label).cmp(&(Reverse(b.0), &y.sort_text, &y.label))
        });
        self.matches = scored
            .into_iter()
            .map(|(_, i)| {
                let chars = fuzzy_match(query, &self.items[i].label).map_or_else(Vec::new, |m| m.1);
                (i, chars)
            })
            .collect();
        self.selected = if query.is_empty() {
            self.matches
                .iter()
                .position(|(i, _)| self.items[*i].preselect)
                .unwrap_or(0)
        } else {
            0
        };
        self.first_row = self.selected.saturating_sub(MAX_ROWS - 1);
    }

    fn step(&mut self, by: isize) {
        let len = self.matches.len() as isize;
        if len == 0 {
            return;
        }
        self.selected = (self.selected as isize + by).rem_euclid(len) as usize;
        if self.selected < self.first_row {
            self.first_row = self.selected;
        } else if self.selected >= self.first_row + MAX_ROWS {
            self.first_row = self.selected + 1 - MAX_ROWS;
        }
    }
}

impl EditorView {
    /// Lets typing ask for suggestions, and which characters ask at once; `None` turns that off.
    pub fn set_completion_triggers(&mut self, triggers: Option<Vec<String>>) {
        self.completing.triggers = triggers;
    }

    /// Whether a language server has the file, so typing asks it for suggestions.
    pub(crate) fn completing_attached(&self) -> bool {
        self.completing.triggers.is_some()
    }

    /// A list is on screen, so Up, Down, Enter and Tab go to it.
    pub(crate) fn completion_open(&self) -> bool {
        self.completing
            .menu
            .as_ref()
            .is_some_and(|m| !m.matches.is_empty())
    }

    pub(crate) fn dismiss_completion(&mut self, cx: &mut Context<Self>) -> bool {
        self.completing.pending = None;
        self.completing.timer = None;
        let was_open = self.completing.menu.take().is_some();
        if was_open {
            cx.notify();
        }
        was_open
    }

    /// Ctrl+Space: asks for suggestions for the word before the cursor.
    pub(crate) fn complete_now(&mut self, cx: &mut Context<Self>) {
        let Some(start) = self.buf().map(|b| b.word_start(self.cursor.head())) else {
            return;
        };
        self.request_completion(start, None, cx);
    }

    /// Reacts to text just typed at the cursor: narrows the open list, or asks for one.
    pub(crate) fn completion_after_typing(&mut self, typed: &str, cx: &mut Context<Self>) {
        let Some(triggers) = self.completing.triggers.as_ref() else {
            return;
        };
        let mut chars = typed.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            self.dismiss_completion(cx);
            return;
        };
        if triggers.iter().any(|t| t.ends_with(c)) {
            self.dismiss_completion(cx);
            let head = self.cursor.head();
            self.request_completion(head, Some(c.to_string()), cx);
            return;
        }
        if !is_word(c) {
            self.dismiss_completion(cx);
            return;
        }
        if self.completing.menu.is_some() {
            self.refilter_completion(cx);
            return;
        }
        if self.completing.pending.is_some() {
            return;
        }
        let head = self.cursor.head();
        let Some(start) = self.buf().map(|b| b.word_start(head)) else {
            return;
        };
        // Numbers are words too, but nothing completes them.
        if self.buf().is_some_and(|b| {
            b.text(start..head)
                .starts_with(|c: char| c.is_ascii_digit())
        }) {
            return;
        }
        self.completing.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TYPING_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.completing.timer = None;
                this.request_completion(start, None, cx);
            });
        }));
    }

    /// The word typed so far from `start`, or `None` once the cursor has left it.
    fn completion_query(&self, start: usize) -> Option<String> {
        let head = self.cursor.head();
        let b = self.buf()?;
        let inside = head >= start
            && b.line_of(head) == b.line_of(start)
            && b.text(start..head).chars().all(is_word);
        inside.then(|| b.text(start..head))
    }

    /// Re-narrows the open list after the word changed, closing it once the cursor leaves the word.
    pub(crate) fn refilter_completion(&mut self, cx: &mut Context<Self>) {
        let Some(start) = self.completing.menu.as_ref().map(|m| m.start) else {
            return;
        };
        let Some(query) = self.completion_query(start) else {
            self.dismiss_completion(cx);
            return;
        };
        let Some(menu) = self.completing.menu.as_mut() else {
            return;
        };
        menu.refilter(&query);
        if menu.incomplete {
            self.request_completion(start, None, cx);
        } else if menu.matches.is_empty() {
            self.dismiss_completion(cx);
        }
        cx.notify();
    }

    fn request_completion(
        &mut self,
        start: usize,
        trigger: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let head = self.cursor.head();
        let Some((line, character)) = self.buf().map(|b| b.utf16_position(head)) else {
            return;
        };
        self.completing.requests += 1;
        let request = self.completing.requests;
        self.completing.pending = Some((request, start));
        cx.emit(EditorEvent::Complete {
            request,
            line,
            character,
            trigger,
        });
    }

    /// The answer to a [`EditorEvent::Complete`]; dropped if the cursor has left the word since.
    pub fn show_completions(
        &mut self,
        request: u64,
        items: Vec<Completion>,
        incomplete: bool,
        cx: &mut Context<Self>,
    ) {
        if self.completing.pending.as_ref().map(|p| p.0) != Some(request) {
            return;
        }
        let Some((_, start)) = self.completing.pending.take() else {
            return;
        };
        let Some(query) = self.completion_query(start) else {
            self.dismiss_completion(cx);
            return;
        };
        let opened = self
            .completing
            .menu
            .as_ref()
            .map_or_else(Opening::now, |m| m.opened);
        let mut menu = Menu {
            request,
            start,
            items,
            matches: Vec::new(),
            selected: 0,
            first_row: 0,
            incomplete,
            opened,
            asked: Vec::new(),
        };
        menu.refilter(&query);
        if menu.matches.is_empty() {
            self.dismiss_completion(cx);
            return;
        }
        self.completing.menu = Some(menu);
        self.resolve_selected(cx);
        cx.notify();
    }

    pub(crate) fn step_completion(&mut self, by: isize, cx: &mut Context<Self>) {
        if let Some(menu) = self.completing.menu.as_mut() {
            menu.step(by);
            self.resolve_selected(cx);
            cx.notify();
        }
    }

    /// Asks the server to fill in the selected suggestion, for its documentation beside the list.
    fn resolve_selected(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = self.completing.menu.as_mut() else {
            return;
        };
        let Some(index) = menu.take_unresolved() else {
            return;
        };
        let request = menu.request;
        cx.emit(EditorEvent::Lsp(LspRequest::ResolveCompletion {
            request,
            index,
        }));
    }

    /// The answer to [`LspRequest::ResolveCompletion`]: the open list shows it, and an item
    /// accepted meanwhile gets its edits elsewhere (an auto-import) if nothing was typed since.
    pub fn resolved_completion(
        &mut self,
        request: u64,
        index: usize,
        resolved: Resolved,
        cx: &mut Context<Self>,
    ) {
        if let Some(menu) = self
            .completing
            .menu
            .as_mut()
            .filter(|m| m.request == request)
            && let Some(item) = menu.items.get_mut(index)
        {
            if resolved.detail.is_some() {
                item.detail = resolved.detail;
            }
            if !resolved.documentation.is_empty() {
                item.documentation = resolved.documentation;
            }
            item.additional_edits = resolved.additional_edits;
            item.resolve = false;
            cx.notify();
            return;
        }
        let Some((asked, at, version, line)) = self.completing.late else {
            return;
        };
        if (asked, at) != (request, index) {
            return;
        }
        self.completing.late = None;
        if self.version() != Some(version) {
            return;
        }
        let above = edits_above(resolved.additional_edits, line);
        if !above.is_empty() {
            self.apply_server_edits(&above, cx);
        }
    }

    /// Inserts the selected suggestion, with any edits it brings elsewhere (imports), as one undo step.
    pub(crate) fn accept_completion(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        let Some(menu) = self.completing.menu.take() else {
            return;
        };
        self.completing.pending = None;
        self.completing.timer = None;
        let Some((item, _)) = menu.matches.get(index.unwrap_or(menu.selected)) else {
            return;
        };
        let index = *item;
        let item = menu.items[index].clone();
        let start = menu.start;
        let line = self.buf().map(|b| b.utf16_position(start).0);
        let single = !self.cursor.is_multi();
        let mut snippet = None;
        self.with_buffer(cx, |b, c| {
            let head = c.head();
            // The server's range was for the word when it was asked; it now ends at the cursor.
            let main = match item.range {
                Some((from, to)) => {
                    let from = b.char_at_utf16(from.0, from.1);
                    from..b.char_at_utf16(to.0, to.1).max(head)
                }
                None => start..head,
            };
            // Other carets replace as many chars before them as the primary's word, as VS Code does.
            let typed = head.saturating_sub(main.start);
            let mut edits = vec![(main, item.text.clone())];
            edits.extend(item.additional_edits.iter().map(|e| {
                let from = b.char_at_utf16(e.start.0, e.start.1);
                (from..b.char_at_utf16(e.end.0, e.end.1), e.text.clone())
            }));
            complete_carets(b, c, &edits, typed, item.select.clone());
            // The first stop is selected, so the snippet starts that far before the selection.
            if let (true, Some(select)) = (single, &item.select) {
                let base = c.selection().range().start.saturating_sub(select.start);
                let len = item.text.chars().count();
                snippet = crate::snippet::Snippet::new(b, base, len, &item.stops);
            }
        });
        self.snippet = snippet;
        self.completing.late = match (item.resolve, self.version(), line) {
            (true, Some(version), Some(line)) => {
                if !menu.asked.contains(&index) {
                    cx.emit(EditorEvent::Lsp(LspRequest::ResolveCompletion {
                        request: menu.request,
                        index,
                    }));
                }
                Some((menu.request, index, version, line))
            }
            _ => None,
        };
        // A function just completed into its parentheses shows its parameters, as in VS Code.
        let head = self.cursor.head();
        if self
            .buf()
            .is_some_and(|b| head > 0 && b.text(head - 1..head) == "(")
        {
            self.request_signature(cx);
        }
    }

    fn scroll_completion(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let Some(menu) = self.completing.menu.as_mut() else {
            return;
        };
        let rows = f32::from(event.delta.pixel_delta(px(ROW_HEIGHT)).y) / ROW_HEIGHT;
        let max = menu.matches.len().saturating_sub(MAX_ROWS) as isize;
        let first = (menu.first_row as isize - rows.round() as isize).clamp(0, max);
        menu.first_row = first as usize;
        cx.notify();
    }

    pub(crate) fn render_completion(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.completing.menu.as_ref()?;
        if menu.matches.is_empty() {
            return None;
        }
        let origin = self.char_origin(menu.start)?;
        let line_height = self.layout.as_ref()?.line_height;
        let t = cx.theme();
        let rows = menu
            .matches
            .iter()
            .enumerate()
            .skip(menu.first_row)
            .take(MAX_ROWS)
            .map(|(row, (index, matched))| {
                let item = &menu.items[*index];
                let selected = row == menu.selected;
                let (letter, color) = kind_badge(item.kind, t);
                let highlights = matched
                    .iter()
                    .filter_map(|&c| {
                        let (at, ch) = item.label.char_indices().nth(c)?;
                        Some((
                            at..at + ch.len_utf8(),
                            HighlightStyle {
                                color: Some(t.color.accent),
                                font_weight: Some(FontWeight::BOLD),
                                ..Default::default()
                            },
                        ))
                    })
                    .collect::<Vec<_>>();
                div()
                    .id(("completion", row))
                    .h(px(ROW_HEIGHT))
                    .px(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded(t.shape.radius_control)
                    .cursor_pointer()
                    .when(selected, |el| el.bg(t.color.surface_accent))
                    .when(!selected, |el| el.hover(|s| s.bg(t.color.surface_hover)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.accept_completion(Some(row), cx);
                    }))
                    .child(
                        div()
                            .flex_none()
                            .w(px(16.))
                            .text_color(color)
                            .font_weight(FontWeight::BOLD)
                            .child(letter),
                    )
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(WIDTH * 0.6))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_color(t.color.content)
                            .child(StyledText::new(item.label.clone()).with_highlights(highlights)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .flex()
                            .justify_end()
                            .text_color(t.color.content_muted)
                            .children(item.detail.clone()),
                    )
            });
        let shown = menu.matches.len().min(MAX_ROWS) as f32;
        let height = px(shown * ROW_HEIGHT + 10.);
        let panel = div()
            .id("completion-list")
            .occlude()
            .w(px(WIDTH))
            .p(px(4.))
            .flex()
            .flex_col()
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_panel)
            .shadow(vec![t.popover_shadow()])
            .font_family(t.typography.mono.clone())
            .text_size(t.typography.caption)
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                cx.stop_propagation();
                this.scroll_completion(event, cx);
            }))
            .children(rows);
        let docs = menu
            .matches
            .get(menu.selected)
            .map(|(i, _)| &menu.items[*i])
            .filter(|item| !item.documentation.is_empty())
            .map(|item| {
                let blocks = item.documentation.iter().map(|block| match block {
                    HoverBlock::Text(text) => div()
                        .px(px(10.))
                        .py(px(6.))
                        .text_color(t.color.content_secondary)
                        .child(text.clone())
                        .into_any_element(),
                    HoverBlock::Code(code) => div()
                        .px(px(10.))
                        .py(px(6.))
                        .bg(t.color.surface_sunken)
                        .font_family(t.typography.mono.clone())
                        .text_color(t.color.content)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(code.clone())
                        .into_any_element(),
                });
                div()
                    .id("completion-docs")
                    .occlude()
                    .w(px(DOCS_WIDTH))
                    // As tall as a full list, so a short list still shows the documentation.
                    .max_h(px(MAX_ROWS as f32 * ROW_HEIGHT + 10.))
                    .overflow_y_scroll()
                    .py(px(4.))
                    .flex()
                    .flex_col()
                    .bg(t.color.surface)
                    .border_1()
                    .border_color(t.color.border)
                    .rounded(t.shape.radius_panel)
                    .shadow(vec![t.popover_shadow()])
                    .font_family(t.typography.ui.clone())
                    .text_size(t.typography.caption)
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .children(blocks)
            });
        // Below the line, or above it when the list would run off the bottom of the editor.
        let below = point(origin.x - px(6.), origin.y + line_height + px(2.));
        let bottom = self.layout.as_ref()?.origin.y + self.viewport.height;
        let (position, corner) = if below.y + height > bottom && origin.y - height > px(0.) {
            (point(below.x, origin.y - px(2.)), Corner::BottomLeft)
        } else {
            (below, Corner::TopLeft)
        };
        // Documentation sits beside the list, as VS Code shows the selected item's details.
        let row = div()
            .flex()
            .gap(px(4.))
            .when(corner == Corner::BottomLeft, |el| el.items_end())
            .child(panel)
            .children(docs);
        let row = motion::animate_enter(
            t.motion.reduced,
            menu.opened.running(t.motion.fast),
            row,
            "completion-open",
            Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d),
        );
        Some(
            deferred(
                anchored()
                    .position(position)
                    .anchor(corner)
                    .snap_to_window_with_margin(px(8.))
                    .child(row),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}

/// A letter for the kind of suggestion, coloured as the editor colours that kind of symbol.
fn kind_badge(kind: Option<u32>, t: &athena_ui::Theme) -> (&'static str, Hsla) {
    let s = &t.syntax;
    match kind {
        Some(2..=4) => ("ƒ", s.function),
        Some(5 | 10) => ("◆", s.property),
        Some(6) => ("x", s.text),
        Some(7 | 8 | 13 | 22 | 25) => ("T", s.type_),
        Some(9) => ("{}", s.type_),
        Some(14) => ("k", s.keyword),
        Some(12 | 20 | 21) => ("c", s.constant),
        Some(15) => ("⧉", s.string),
        _ => ("·", t.color.content_muted),
    }
}

/// Applies a completion at every caret: the primary takes `edits` (its own first, in offsets of
/// the text before any caret changed) and the others replace the `typed` chars before them.
fn complete_carets(
    b: &mut Buffer,
    cs: &mut Cursors,
    edits: &[(Range<usize>, String)],
    typed: usize,
    select: Option<Range<usize>>,
) {
    let Some((_, text)) = edits.first() else {
        return;
    };
    let primary = cs.primary_index();
    b.edit_carets(cs, false, |b, c, i| {
        if i == primary {
            // Carets later in the text completed first, moving any edit past them.
            let edits: Vec<_> = edits
                .iter()
                .map(|(r, t)| (b.since_batch(r.clone()), t.clone()))
                .collect();
            b.apply_edits(c, &edits, select.clone());
        } else if c.selection.is_empty() {
            let at = c.head();
            let from = at - typed.min(b.column_of(at));
            b.replace_range(c, from..at, text);
        } else {
            b.insert(c, text);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Cursor;

    #[test]
    fn a_completion_edit_past_later_carets_lands_where_the_server_meant() {
        let mut b = Buffer::new("pr\npr\n// end\n", None);
        let mut cs = Cursors::new(Cursor::at(5));
        cs.add(Cursor::at(2));
        assert_eq!(cs.primary_index(), 0);
        let end = b.len_chars();
        let edits = vec![
            (0..2, "print".to_string()),
            (end..end, "use x\n".to_string()),
        ];
        complete_carets(&mut b, &mut cs, &edits, 2, None);
        assert_eq!(b.full_text(), "print\nprint\n// end\nuse x\n");
        assert!(b.undo_all(&mut cs));
        assert_eq!(b.full_text(), "pr\npr\n// end\n", "one undo step");
    }

    #[test]
    fn a_snippet_completed_below_an_added_import_finds_its_stops() {
        let mut b = Buffer::new("package main\n\nfunc main() { fmt.Pr }\n", None);
        let mut cs = Cursors::new(Cursor::at(34));
        let text = "Println(a)".to_string();
        let edits = vec![
            (32..34, text.clone()),
            (14..14, "import \"fmt\"\n\n".to_string()),
        ];
        complete_carets(&mut b, &mut cs, &edits, 2, Some(8..9));
        let base = cs.selection().range().start - 8;
        assert_eq!(b.text(base..base + text.len()), text);
        assert_eq!(b.line(4), "func main() { fmt.Println(a) }");
        let snippet = crate::snippet::Snippet::new(&b, base, text.len(), &[(1, 8..9)]);
        assert!(snippet.is_some());
    }

    fn item(label: &str, sort: &str) -> Completion {
        Completion {
            label: label.into(),
            kind: Some(3),
            detail: None,
            filter_text: label.into(),
            sort_text: sort.into(),
            text: label.into(),
            range: None,
            select: None,
            stops: Vec::new(),
            additional_edits: Vec::new(),
            preselect: false,
            documentation: Vec::new(),
            resolve: false,
        }
    }

    #[test]
    fn fuzzy_matching_starts_at_a_word_and_prefers_prefixes() {
        assert_eq!(fuzzy_match("pl", "Println").map(|m| m.1), Some(vec![0, 5]));
        assert!(
            fuzzy_match("ln", "Println").is_none(),
            "must start at a word"
        );
        assert!(fuzzy_match("x", "Println").is_none());
        assert_eq!(fuzzy_match("fb", "fooBar").map(|m| m.1), Some(vec![0, 3]));
        assert_eq!(fuzzy_match("rd", "read_dir").map(|m| m.1), Some(vec![0, 5]));
        assert_eq!(
            fuzzy_match("rdx", "readx_dir").map(|m| m.1),
            Some(vec![0, 3, 4])
        );
        assert_eq!(fuzzy_match("", "x"), Some((0, Vec::new())));
        let prefix = fuzzy_match("pri", "Println").unwrap().0;
        let scattered = fuzzy_match("pri", "PageRuleItem").unwrap().0;
        assert!(prefix > scattered, "{prefix} vs {scattered}");
    }

    #[test]
    fn the_list_narrows_and_ranks_as_you_type() {
        let mut menu = Menu {
            start: 0,
            items: vec![
                item("Sprintf", "0"),
                item("Println", "2"),
                item("Printf", "1"),
                item("PageRuleItem", "0"),
            ],
            matches: Vec::new(),
            selected: 0,
            first_row: 0,
            incomplete: false,
            opened: Opening::now(),
            asked: Vec::new(),
            request: 0,
        };
        let labels = |m: &Menu| -> Vec<String> {
            m.matches
                .iter()
                .map(|(i, _)| m.items[*i].label.clone())
                .collect()
        };
        menu.refilter("");
        assert_eq!(
            labels(&menu),
            ["PageRuleItem", "Sprintf", "Printf", "Println"],
            "an empty word keeps the server's order"
        );
        menu.refilter("pri");
        assert_eq!(labels(&menu), ["Printf", "Println", "PageRuleItem"]);
        menu.refilter("pln");
        assert_eq!(labels(&menu), ["Println"]);
        assert_eq!(menu.matches[0].1, vec![0, 5, 6]);
        menu.refilter("q");
        assert!(menu.matches.is_empty());
    }

    #[test]
    fn the_selected_item_is_resolved_once_and_only_if_it_needs_it() {
        let mut needs = item("useState", "0");
        needs.resolve = true;
        let mut menu = Menu {
            start: 0,
            items: vec![
                needs.clone(),
                item("useEffect", "1"),
                Completion {
                    sort_text: "2".into(),
                    ..needs
                },
            ],
            matches: Vec::new(),
            selected: 0,
            first_row: 0,
            incomplete: false,
            opened: Opening::now(),
            asked: Vec::new(),
            request: 3,
        };
        menu.refilter("");
        assert_eq!(menu.take_unresolved(), Some(0));
        assert_eq!(menu.take_unresolved(), None, "already asked");
        menu.step(1);
        assert_eq!(menu.take_unresolved(), None, "complete as listed");
        menu.step(1);
        assert_eq!(menu.take_unresolved(), Some(2));
    }

    #[test]
    fn an_auto_import_resolved_after_accepting_applies_only_above_the_completion() {
        let edit = |line: u32| ServerEdit {
            start: (line, 0),
            end: (line, 0),
            text: "import x\n".into(),
        };
        let kept = edits_above(vec![edit(0), edit(5), edit(9)], 5);
        assert_eq!(kept, vec![edit(0)]);
    }

    #[test]
    fn stepping_wraps_and_scrolls_the_window_of_rows() {
        let mut menu = Menu {
            start: 0,
            items: (0..15).map(|i| item(&format!("a{i:02}"), "")).collect(),
            matches: Vec::new(),
            selected: 0,
            first_row: 0,
            incomplete: false,
            opened: Opening::now(),
            asked: Vec::new(),
            request: 0,
        };
        menu.refilter("a");
        menu.step(-1);
        assert_eq!((menu.selected, menu.first_row), (14, 5));
        menu.step(1);
        assert_eq!((menu.selected, menu.first_row), (0, 0));
        for _ in 0..10 {
            menu.step(1);
        }
        assert_eq!((menu.selected, menu.first_row), (10, 1));
    }
}
