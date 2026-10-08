use std::ops::Range;
use std::time::Duration;

use athena_ui::ActiveTheme;
use athena_ui::motion::{self, Opening};
use gpui::{
    Animation, AnyElement, Context, Corner, FontWeight, IntoElement, Pixels, Point, Task, anchored,
    deferred, div, point, prelude::*, px,
};

use crate::view::{EditorEvent, EditorView};

/// The pointer rests this long on a word before its documentation is asked for.
const HOVER_DELAY: Duration = Duration::from_millis(500);
/// Grace for the pointer to cross between the word and the popover without it closing.
const HIDE_DELAY: Duration = Duration::from_millis(300);
const MAX_WIDTH: f32 = 520.;
const MAX_HEIGHT: f32 = 320.;

/// Documentation from a language server, as plain paragraphs and code blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoverBlock {
    Text(String),
    Code(String),
}

struct Shown {
    /// The word described, in chars.
    word: Range<usize>,
    blocks: Vec<HoverBlock>,
    /// Opened from the keyboard: stays until the cursor moves, whatever the pointer does.
    keyboard: bool,
    opened: Opening,
}

#[derive(Default)]
pub(crate) struct Hovering {
    shown: Option<Shown>,
    /// The request in flight and the word it asks about.
    pending: Option<(u64, Range<usize>, bool)>,
    /// The word the pointer is resting on, waiting out the delay.
    resting: Option<Range<usize>>,
    requests: u64,
    rest_timer: Option<Task<()>>,
    hide_timer: Option<Task<()>>,
    /// The pointer is over the popover, which keeps it open whatever order hover events come in.
    over_popover: bool,
}

impl EditorView {
    /// The identifier under a window position, from last frame's layout; none past a line's end.
    fn word_at_position(&self, position: Point<Pixels>) -> Option<Range<usize>> {
        let layout = self.layout.as_ref()?;
        let b = self.buf()?;
        let y = position.y - layout.origin.y + px(self.scroll.y);
        if y < px(0.) {
            return None;
        }
        let row = (y / layout.line_height).floor() as usize;
        if row >= self.display.row_count(b.len_lines()) {
            return None;
        }
        let line = self.display.line_of(row);
        let x = position.x - layout.text_left + px(self.scroll.x);
        if x < px(0.) {
            return None;
        }
        let col = layout.row(row)?.col_under(x)?;
        b.word_at(b.line_start(line) + col)
    }

    /// Window position of the top-left of `char`, if its line was drawn last frame.
    pub(crate) fn char_origin(&self, char: usize) -> Option<Point<Pixels>> {
        let layout = self.layout.as_ref()?;
        let b = self.buf()?;
        let char = char.min(b.len_chars());
        let line = b.line_of(char);
        // Last frame's line may be shorter than the text is now.
        let col = b.column_of(char);
        let r = layout.row_holding(line, col)?;
        let x = layout.text_left + r.x_for(col) - px(self.scroll.x);
        let y = layout.origin.y + layout.line_height * r.row as f32 - px(self.scroll.y);
        Some(point(x, y))
    }

    /// Follows the pointer: resting on a word asks about it, leaving the shown one hides it.
    pub(crate) fn hover_pointer(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let word = self.word_at_position(position);
        if let Some(shown) = &self.hovering.shown {
            if word.as_ref() == Some(&shown.word) {
                self.hovering.hide_timer = None;
                return;
            }
            if !shown.keyboard && self.hovering.hide_timer.is_none() {
                self.schedule_hover_hide(cx);
            }
        }
        if word == self.hovering.resting {
            return;
        }
        self.hovering.resting = word.clone();
        self.hovering.rest_timer = word.map(|word| {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(HOVER_DELAY).await;
                let _ = this.update(cx, |this, cx| this.request_hover(word, false, cx));
            })
        });
    }

    /// The pointer left the editor; the popover stays only if the pointer went onto it.
    pub(crate) fn hover_left(&mut self, cx: &mut Context<Self>) {
        self.hovering.resting = None;
        self.hovering.rest_timer = None;
        if self.hovering.shown.as_ref().is_some_and(|s| !s.keyboard) {
            self.schedule_hover_hide(cx);
        }
    }

    /// Cmd+K Cmd+I: describes the symbol at the cursor.
    pub(crate) fn hover_at_cursor(&mut self, cx: &mut Context<Self>) {
        let head = self.cursor.head();
        let word = self.buf().and_then(|b| {
            b.word_at(head)
                .or_else(|| b.word_at(head.saturating_sub(1)))
        });
        self.request_hover(word.unwrap_or(head..head), true, cx);
    }

    fn request_hover(&mut self, word: Range<usize>, keyboard: bool, cx: &mut Context<Self>) {
        let Some((line, character)) = self.buf().map(|b| b.utf16_position(word.start)) else {
            return;
        };
        self.hovering.requests += 1;
        let request = self.hovering.requests;
        self.hovering.pending = Some((request, word, keyboard));
        cx.emit(EditorEvent::Hover {
            request,
            line,
            character,
        });
    }

    /// The answer to a [`EditorEvent::Hover`]; dropped if the pointer has moved on since.
    pub fn show_hover(&mut self, request: u64, blocks: Vec<HoverBlock>, cx: &mut Context<Self>) {
        let Some((asked, word, keyboard)) = self.hovering.pending.take() else {
            return;
        };
        if asked != request || blocks.is_empty() {
            return;
        }
        if !keyboard && self.hovering.resting.as_ref() != Some(&word) {
            return;
        }
        self.hovering.hide_timer = None;
        self.hovering.shown = Some(Shown {
            word,
            blocks,
            keyboard,
            opened: Opening::now(),
        });
        cx.notify();
    }

    pub(crate) fn hide_hover(&mut self, cx: &mut Context<Self>) -> bool {
        self.hovering.pending = None;
        self.hovering.hide_timer = None;
        self.hovering.over_popover = false;
        let was_shown = self.hovering.shown.take().is_some();
        if was_shown {
            cx.notify();
        }
        was_shown
    }

    fn schedule_hover_hide(&mut self, cx: &mut Context<Self>) {
        self.hovering.hide_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HIDE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                if !this.hovering.over_popover {
                    this.hide_hover(cx);
                }
            });
        }));
    }

    pub(crate) fn render_hover(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let shown = self.hovering.shown.as_ref()?;
        let origin = self.char_origin(shown.word.start)?;
        let line_height = self.layout.as_ref()?.line_height;
        let t = cx.theme();
        let blocks = shown.blocks.iter().map(|block| match block {
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
                .font_weight(FontWeight::NORMAL)
                .whitespace_nowrap()
                .overflow_hidden()
                .child(code.clone())
                .into_any_element(),
        });
        let panel = div()
            .id("hover-docs")
            .occlude()
            .max_w(px(MAX_WIDTH))
            .max_h(px(MAX_HEIGHT))
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
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.hovering.over_popover = *hovered;
                if *hovered {
                    this.hovering.hide_timer = None;
                } else if this.hovering.shown.as_ref().is_some_and(|s| !s.keyboard) {
                    this.schedule_hover_hide(cx);
                }
            }))
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .children(blocks);
        let panel = motion::animate_enter(
            t.motion.reduced,
            shown.opened.running(t.motion.fast),
            panel,
            "hover-docs-open",
            Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d),
        );
        // Above the word, as VS Code shows it, unless the line is too near the top.
        let above = origin.y - px(MAX_HEIGHT / 2.) > px(0.);
        let (position, corner) = if above {
            (point(origin.x, origin.y - px(2.)), Corner::BottomLeft)
        } else {
            (
                point(origin.x, origin.y + line_height + px(2.)),
                Corner::TopLeft,
            )
        };
        Some(
            deferred(
                anchored()
                    .position(position)
                    .anchor(corner)
                    .snap_to_window_with_margin(px(8.))
                    .child(panel),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}
