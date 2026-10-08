use athena_ui::motion::{self, Opening};
use athena_ui::{ActiveTheme, InputEvent, TextInput};
use gpui::{
    Animation, Context, Entity, Focusable, IntoElement, Point, Subscription, Window, div,
    prelude::*, px,
};

use crate::buffer::Cursors;
use crate::view::EditorView;

/// The Ctrl+G box: moves the cursor as a line number is typed, and back on Escape.
pub(crate) struct LineJump {
    input: Entity<TextInput>,
    /// Where the cursor and scroll were, for Escape to return to.
    origin: (Cursors, Point<f32>),
    opened: Opening,
    _subscriptions: [Subscription; 2],
}

/// `line`, `line:column` or `line,column`, 1-based as people count.
pub(crate) fn parse_line_column(text: &str) -> Option<(u32, Option<u32>)> {
    let text = text.trim();
    let (line, column) = match text.split_once([':', ',']) {
        Some((line, column)) => (line, Some(column.trim())),
        None => (text, None),
    };
    let line = line.trim().parse().ok()?;
    let column = match column {
        Some("") | None => None,
        Some(c) => Some(c.parse().ok()?),
    };
    Some((line, column))
}

impl EditorView {
    pub(crate) fn open_line_jump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(jump) = &self.line_jump {
            window.focus(&jump.input.focus_handle(cx));
            return;
        }
        let input = cx.new(|cx| TextInput::new("Line[:column]", cx));
        let events = cx.subscribe_in(&input, window, |this, input, event, window, cx| {
            match event {
                InputEvent::Changed => {
                    if let Some((line, column)) = parse_line_column(input.read(cx).text()) {
                        this.jump_to(line, column, cx);
                    }
                }
                InputEvent::Submit | InputEvent::SubmitBeside => {
                    this.line_jump = None;
                    window.focus(&this.focus);
                }
                InputEvent::Cancel => {
                    if let Some(jump) = this.line_jump.take() {
                        this.cursor = jump.origin.0;
                        this.scroll = jump.origin.1;
                        this.autoscroll = false;
                    }
                    window.focus(&this.focus);
                }
                InputEvent::Up | InputEvent::Down => {}
            }
            cx.notify();
        });
        // Clicking elsewhere keeps wherever the preview went, as VS Code does.
        let blur = cx.on_blur(&input.focus_handle(cx), window, |this, _, cx| {
            if this.line_jump.take().is_some() {
                cx.notify();
            }
        });
        window.focus(&input.focus_handle(cx));
        self.line_jump = Some(LineJump {
            input,
            origin: (self.cursor.clone(), self.scroll),
            opened: Opening::now(),
            _subscriptions: [events, blur],
        });
        cx.notify();
    }

    /// Moves the cursor to a 1-based line and optional column and centres it on screen.
    pub(crate) fn jump_to(&mut self, line: u32, column: Option<u32>, cx: &mut Context<Self>) {
        self.with_buffer(cx, |b, c| {
            let line = (line.max(1) as usize - 1).min(b.len_lines().saturating_sub(1));
            let column = column.map_or(0, |c| c.max(1) as usize - 1);
            c.collapse();
            b.move_to(c.primary_mut(), b.char_at(line, column), false);
        });
        self.center_cursor = true;
    }

    pub(crate) fn render_line_jump(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let jump = self.line_jump.as_ref()?;
        let (lines, line, column) = {
            let b = self.buf()?;
            let head = self.cursor.head();
            (b.len_lines(), b.line_of(head) + 1, b.column_of(head) + 1)
        };
        let t = cx.theme();
        let panel = div()
            .w(px(380.))
            .p(px(6.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_panel)
            .shadow(vec![t.popover_shadow()])
            .text_size(t.typography.caption)
            .child(
                div()
                    .h(px(24.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .bg(t.color.surface_sunken)
                    .border_1()
                    .border_color(t.color.accent)
                    .rounded(t.shape.radius_control)
                    .child(jump.input.clone()),
            )
            .child(
                div()
                    .px(px(2.))
                    .text_color(t.color.content_muted)
                    .child(format!(
                        "Current line {line}, column {column}. Type a line between 1 and {lines}, \
                         and :column if you like."
                    )),
            );
        let panel = motion::animate_enter(
            t.motion.reduced,
            jump.opened.running(t.motion.fast),
            panel,
            "line-jump-open",
            Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d).mt(px(-6. * (1. - d))),
        );
        Some(
            div()
                .absolute()
                .top(px(8.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(panel),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lines_and_columns() {
        assert_eq!(parse_line_column("12"), Some((12, None)));
        assert_eq!(parse_line_column(" 12:5 "), Some((12, Some(5))));
        assert_eq!(parse_line_column("7,3"), Some((7, Some(3))));
        assert_eq!(parse_line_column("7:"), Some((7, None)));
        assert_eq!(parse_line_column(""), None);
        assert_eq!(parse_line_column("x"), None);
        assert_eq!(parse_line_column("3:y"), None);
    }
}
