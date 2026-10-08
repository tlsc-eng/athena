use athena_ui::motion;
use athena_ui::{ActiveTheme, InputEvent, TextInput};
use gpui::{
    Action, Animation, AnyElement, App, Context, Corner, Entity, Focusable, KeyBinding, Pixels,
    Point, Subscription, Window, actions, anchored, deferred, div, point, prelude::*, px,
};

use crate::element::GUTTER_PAD;
use crate::view::EditorView;

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
