use gpui::{
    AnyElement, App, ClickEvent, ElementId, FontWeight, IntoElement, MouseButton, MouseDownEvent,
    RenderOnce, SharedString, Window, div, prelude::*,
};

use super::{ClickHandler, Tooltip};
use crate::{ActiveTheme, Ellipsis, Size, Space};

/// One row of a list or tree: indent, a leading icon or dot, a label, meta text and trailing
/// controls, with an optional second line of detail.
#[derive(IntoElement)]
pub struct ListRow {
    id: ElementId,
    label: SharedString,
    indent: usize,
    leading: Option<AnyElement>,
    meta: Option<SharedString>,
    trailing: Option<AnyElement>,
    detail: Option<AnyElement>,
    emphasized: bool,
    tooltip: Option<SharedString>,
    selected: bool,
    group: Option<SharedString>,
    on_click: Option<ClickHandler>,
    on_context_menu: Option<MenuHandler>,
}

type MenuHandler = Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App) + 'static>;

impl ListRow {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            indent: 0,
            leading: None,
            meta: None,
            trailing: None,
            detail: None,
            emphasized: false,
            tooltip: None,
            selected: false,
            group: None,
            on_click: None,
            on_context_menu: None,
        }
    }

    pub fn indent(mut self, level: usize) -> Self {
        self.indent = level;
        self
    }

    pub fn leading(mut self, leading: impl IntoElement) -> Self {
        self.leading = Some(leading.into_any_element());
        self
    }

    pub fn meta(mut self, meta: impl Into<SharedString>) -> Self {
        self.meta = Some(meta.into());
        self
    }

    /// Controls after the meta text, which keep their own clicks.
    pub fn trailing(mut self, trailing: impl IntoElement) -> Self {
        self.trailing = Some(trailing.into_any_element());
        self
    }

    /// A second line under the label, for what does not fit beside it in a narrow column.
    pub fn detail(mut self, detail: impl IntoElement) -> Self {
        self.detail = Some(detail.into_any_element());
        self
    }

    /// Medium weight and full-strength text, for a row that wants attention.
    pub fn emphasized(mut self, emphasized: bool) -> Self {
        self.emphasized = emphasized;
        self
    }

    pub fn tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Names the row, so a control inside it can appear only while the row is hovered.
    pub fn group(mut self, group: impl Into<SharedString>) -> Self {
        self.group = Some(group.into());
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(f));
        self
    }

    pub fn on_context_menu(
        mut self,
        f: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_context_menu = Some(Box::new(f));
        self
    }
}

impl RenderOnce for ListRow {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = cx.theme();
        let c = &t.color;
        let pad = t.space(Space::Md) + t.space(Space::Lg) * self.indent as f32;
        let bright = self.selected || self.emphasized;
        let line = div()
            .h(t.size(Size::Row))
            .w_full()
            .flex()
            .items_center()
            .gap(t.space(Space::Sm))
            .pl(pad)
            .pr(t.space(Space::Md))
            .children(self.leading)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .ellipsis()
                    .when(self.emphasized, |el| el.font_weight(FontWeight::MEDIUM))
                    .child(self.label),
            )
            .children(
                self.meta
                    .map(|m| div().flex_none().text_color(c.content_muted).child(m)),
            )
            .children(self.trailing);
        let tooltip = self.tooltip;
        let row = div()
            .id(self.id)
            .when_some(self.group, |el, group| el.group(group))
            .w_full()
            .flex_none()
            .flex()
            .flex_col()
            .rounded(t.shape.radius_control)
            .cursor_pointer()
            .text_size(t.typography.caption)
            .text_color(if bright {
                c.content
            } else {
                c.content_secondary
            })
            .when(self.selected, |el| el.bg(c.surface_accent))
            .when(!self.selected, |el| {
                el.hover(|s| s.bg(c.surface_hover).text_color(c.content))
            })
            .when_some(tooltip, |el, text| {
                el.tooltip(move |_, cx| Tooltip::view(text.clone(), cx))
            })
            .child(line)
            .children(self.detail.map(|d| {
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(t.space(Space::Sm))
                    .overflow_hidden()
                    .pl(pad + t.space(Space::Xxl) + t.space(Space::Xs))
                    .pr(t.space(Space::Md))
                    .pb(t.space(Space::Xs))
                    .text_color(c.content_muted)
                    .child(d)
            }));
        let row = match self.on_context_menu {
            Some(f) => row.on_mouse_down(MouseButton::Right, move |ev, window, cx| {
                cx.stop_propagation();
                f(ev, window, cx)
            }),
            None => row,
        };
        match self.on_click {
            Some(f) => row.on_click(f),
            None => row,
        }
    }
}
