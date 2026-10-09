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
        GoToTypeDefinition
    ]
);

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
    ]);
}

/// Char ranges a language server gave for one buffer version, followed through later edits.
pub(crate) struct Anchored<T> {
    version: u64,
    items: Vec<(Range<usize>, T)>,
}

impl<T> Default for Anchored<T> {
    fn default() -> Self {
        Self {
            version: 0,
            items: Vec::new(),
        }
    }
}

impl<T: Clone> Anchored<T> {
    fn new(b: &Buffer, items: Vec<(Range<usize>, T)>) -> Self {
        Self {
            version: b.version(),
            items,
        }
    }

    /// The ranges where they are in `b` now; none once the edits since are no longer kept.
    pub(crate) fn now(&self, b: &Buffer) -> Vec<(Range<usize>, T)> {
        let Some(edits) = b.edits_since(self.version) else {
            return Vec::new();
        };
        let edits: Vec<_> = edits.collect();
        // Text typed at either edge of a range stays outside it.
        let follow = |r: Range<usize>, e: &&crate::buffer::Edit| {
            let start = match e.removed == 0 && r.start == e.at {
                true => r.start + e.inserted,
                false => e.map(r.start),
            };
            start..e.map(r.end).max(start)
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
    use crate::buffer::Cursor;

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
