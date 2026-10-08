use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{
    Animation, App, Context, Corner, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyBinding, Pixels, Point, Render, SharedString, Size, Subscription, Window,
    actions, anchored, deferred, div, prelude::*, px, size,
};

use crate::{ActiveTheme, motion};

actions!(context_menu, [Dismiss, Up, Down, Confirm]);

const WIDTH: f32 = 220.;
const ROW: f32 = 26.;
const SEPARATOR: f32 = 9.;
const PADDING: f32 = 4.;
const MARGIN: f32 = 8.;

static GENERATION: AtomicU64 = AtomicU64::new(0);

pub(crate) fn init(cx: &mut App) {
    let ctx = Some("ContextMenu");
    cx.bind_keys([
        KeyBinding::new("escape", Dismiss, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("enter", Confirm, ctx),
    ]);
}

type SelectHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// One row of a [`ContextMenu`], or a separator line.
#[derive(Clone)]
pub enum MenuItem {
    Entry {
        label: SharedString,
        hint: Option<SharedString>,
        disabled: bool,
        on_select: SelectHandler,
    },
    Separator,
}

impl MenuItem {
    pub fn new(
        label: impl Into<SharedString>,
        on_select: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self::Entry {
            label: label.into(),
            hint: None,
            disabled: false,
            on_select: Rc::new(on_select),
        }
    }

    pub fn separator() -> Self {
        Self::Separator
    }

    /// Right-aligned shortcut text such as `⌘C`.
    pub fn hint(mut self, text: impl Into<SharedString>) -> Self {
        if let Self::Entry { hint, .. } = &mut self {
            *hint = Some(text.into());
        }
        self
    }

    pub fn disabled(mut self, value: bool) -> Self {
        if let Self::Entry { disabled, .. } = &mut self {
            *disabled = value;
        }
        self
    }

    fn selectable(&self) -> bool {
        matches!(
            self,
            Self::Entry {
                disabled: false,
                ..
            }
        )
    }
}

/// Right-click menu drawn above everything; emits [`DismissEvent`] exactly once when it closes.
pub struct ContextMenu {
    items: Vec<MenuItem>,
    position: Point<Pixels>,
    selected: Option<usize>,
    focus: FocusHandle,
    restore: Option<FocusHandle>,
    generation: u64,
    dismissed: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DismissEvent> for ContextMenu {}

impl Focusable for ContextMenu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ContextMenu {
    /// Opens at `position` in window coordinates and takes focus once the current event has finished.
    pub fn build(
        position: Point<Pixels>,
        items: Vec<MenuItem>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx: &mut Context<Self>| {
            let focus = cx.focus_handle();
            let subscriptions = vec![
                cx.on_blur(&focus, window, |this: &mut Self, _, cx| this.finish(cx)),
                cx.observe_window_activation(window, |this, window, cx| {
                    if !window.is_window_active() {
                        this.close(window, cx);
                    }
                }),
            ];
            // After the opening click, so focus it moved (e.g. into a clicked pane) is what we restore.
            cx.defer_in(window, |this: &mut Self, window, cx| {
                this.restore = window.focused(cx);
                window.focus(&this.focus);
            });
            Self {
                items,
                position,
                selected: None,
                focus,
                restore: None,
                generation: GENERATION.fetch_add(1, Ordering::Relaxed),
                dismissed: false,
                _subscriptions: subscriptions,
            }
        })
    }

    /// Closes the menu and returns focus to whatever held it before.
    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let restore = self.restore.take();
        self.finish(cx);
        if let Some(handle) = restore {
            window.focus(&handle);
        }
    }

    fn finish(&mut self, cx: &mut Context<Self>) {
        if self.dismissed {
            return;
        }
        self.dismissed = true;
        cx.emit(DismissEvent);
        cx.notify();
    }

    fn step(&mut self, step: isize, cx: &mut Context<Self>) {
        if let Some(next) = next_selectable(&self.items, self.selected, step) {
            self.selected = Some(next);
            cx.notify();
        }
    }

    fn confirm(&mut self, index: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(MenuItem::Entry {
            disabled: false,
            on_select,
            ..
        }) = index.and_then(|i| self.items.get(i))
        else {
            return;
        };
        let on_select = on_select.clone();
        // Close first so a handler that moves focus (an inline rename field) keeps it.
        self.close(window, cx);
        on_select(window, cx);
    }
}

impl Render for ContextMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.dismissed {
            return div().into_any_element();
        }
        let t = cx.theme().clone();
        let menu = size(px(WIDTH), menu_height(&self.items, t.shape.hairline));
        let corner = anchor_corner(self.position, menu, window.viewport_size());
        let rows =
            self.items.iter().enumerate().map(|(i, item)| match item {
                MenuItem::Separator => div()
                    .h(px(1.))
                    .my(px((SEPARATOR - 1.) / 2.))
                    .mx(px(PADDING))
                    .bg(t.color.border)
                    .into_any_element(),
                MenuItem::Entry {
                    label,
                    hint,
                    disabled,
                    ..
                } => {
                    let selected = self.selected == Some(i);
                    let disabled = *disabled;
                    div()
                        .id(("menu-row", i))
                        .h(px(ROW))
                        .mx(px(PADDING))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .gap(px(16.))
                        .rounded(t.shape.radius_control)
                        .text_color(if disabled {
                            t.color.content_disabled
                        } else {
                            t.color.content
                        })
                        .when(selected, |el| el.bg(t.color.surface_accent))
                        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                            if *hovered {
                                this.selected = (!disabled).then_some(i);
                            } else if this.selected == Some(i) {
                                this.selected = None;
                            }
                            cx.notify();
                        }))
                        .when(!disabled, |el| {
                            el.on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm(Some(i), window, cx)
                            }))
                        })
                        .child(div().flex_1().whitespace_nowrap().child(label.clone()))
                        .children(hint.clone().map(|hint| {
                            div()
                                .flex_none()
                                .text_color(if disabled {
                                    t.color.content_disabled
                                } else {
                                    t.color.content_muted
                                })
                                .child(hint)
                        }))
                        .into_any_element()
                }
            });
        let panel =
            div()
                .id(("context-menu", self.generation))
                .track_focus(&self.focus)
                .key_context("ContextMenu")
                .on_action(cx.listener(|this, _: &Dismiss, window, cx| this.close(window, cx)))
                .on_action(cx.listener(|this, _: &Up, _, cx| this.step(-1, cx)))
                .on_action(cx.listener(|this, _: &Down, _, cx| this.step(1, cx)))
                .on_action(cx.listener(|this, _: &Confirm, window, cx| {
                    this.confirm(this.selected, window, cx)
                }))
                .occlude()
                // Capture phase and no stop_propagation: the click that dismisses still reaches its target.
                .on_mouse_down_out(cx.listener(|this, _, window, cx| this.close(window, cx)))
                .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
                .min_w(menu.width)
                .py(px(PADDING))
                .flex()
                .flex_col()
                .bg(t.color.surface)
                .border_1()
                .border_color(t.color.border)
                .rounded(t.shape.radius_panel)
                .shadow(vec![t.popover_shadow()])
                .font_family(t.typography.ui.clone())
                .text_size(t.typography.caption)
                .children(rows);
        let panel = motion::animate_if(
            t.motion.reduced,
            panel,
            ("context-menu-enter", self.generation),
            Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d),
        );
        deferred(
            anchored()
                .position(self.position)
                .anchor(corner)
                .snap_to_window_with_margin(px(MARGIN))
                .child(panel),
        )
        .with_priority(2)
        .into_any_element()
    }
}

fn menu_height(items: &[MenuItem], border: Pixels) -> Pixels {
    let rows: f32 = items
        .iter()
        .map(|item| match item {
            MenuItem::Separator => SEPARATOR,
            MenuItem::Entry { .. } => ROW,
        })
        .sum();
    px(rows + 2. * PADDING) + border * 2.
}

/// Opens away from the cursor on each axis where the menu would cross the window edge, like macOS menus.
fn anchor_corner(position: Point<Pixels>, menu: Size<Pixels>, viewport: Size<Pixels>) -> Corner {
    let margin = px(MARGIN);
    let left =
        position.x + menu.width > viewport.width - margin && position.x - menu.width >= margin;
    let up =
        position.y + menu.height > viewport.height - margin && position.y - menu.height >= margin;
    match (left, up) {
        (false, false) => Corner::TopLeft,
        (true, false) => Corner::TopRight,
        (false, true) => Corner::BottomLeft,
        (true, true) => Corner::BottomRight,
    }
}

fn next_selectable(items: &[MenuItem], from: Option<usize>, step: isize) -> Option<usize> {
    let len = items.len() as isize;
    let mut at = from.map_or(if step > 0 { -1 } else { len }, |i| i as isize);
    for _ in 0..len {
        at = (at + step).rem_euclid(len);
        if items[at as usize].selectable() {
            return Some(at as usize);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::point;

    fn items() -> Vec<MenuItem> {
        vec![
            MenuItem::new("Cut", |_, _| {}).disabled(true),
            MenuItem::new("Copy", |_, _| {}),
            MenuItem::separator(),
            MenuItem::new("Paste", |_, _| {}),
            MenuItem::new("Delete", |_, _| {}).disabled(true),
        ]
    }

    #[test]
    fn arrows_skip_separators_and_disabled_rows_and_wrap() {
        let items = items();
        assert_eq!(next_selectable(&items, None, 1), Some(1));
        assert_eq!(next_selectable(&items, None, -1), Some(3));
        assert_eq!(next_selectable(&items, Some(1), 1), Some(3));
        assert_eq!(next_selectable(&items, Some(3), 1), Some(1));
        assert_eq!(next_selectable(&items, Some(1), -1), Some(3));
    }

    #[test]
    fn a_menu_with_nothing_enabled_selects_nothing() {
        let items = vec![
            MenuItem::separator(),
            MenuItem::new("x", |_, _| {}).disabled(true),
        ];
        assert_eq!(next_selectable(&items, None, 1), None);
        assert_eq!(next_selectable(&[], None, 1), None);
    }

    #[test]
    fn height_counts_rows_separators_padding_and_border() {
        assert_eq!(
            menu_height(&items(), px(1.)),
            px(4. * ROW + SEPARATOR + 8. + 2.)
        );
    }

    #[test]
    fn menu_opens_away_from_the_edges_it_would_cross() {
        let viewport = size(px(1000.), px(800.));
        let menu = size(px(WIDTH), px(200.));
        let corner = |x, y| anchor_corner(point(px(x), px(y)), menu, viewport);
        assert_eq!(corner(100., 100.), Corner::TopLeft);
        assert_eq!(corner(900., 100.), Corner::TopRight);
        assert_eq!(corner(100., 700.), Corner::BottomLeft);
        assert_eq!(corner(900., 700.), Corner::BottomRight);
    }

    #[test]
    fn menu_taller_than_the_space_above_stays_anchored_at_the_top() {
        let viewport = size(px(1000.), px(300.));
        let menu = size(px(WIDTH), px(250.));
        assert_eq!(
            anchor_corner(point(px(100.), px(150.)), menu, viewport),
            Corner::TopLeft
        );
    }
}
