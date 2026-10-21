use gpui::{
    AnyElement, App, ClickEvent, ElementId, IntoElement, RenderOnce, SharedString, Window, div,
    prelude::*,
};

use super::ClickHandler;
use crate::{ActiveTheme, Size, Space};

/// One row of a list or tree: indent, a leading icon or dot, a label and trailing meta text.
#[derive(IntoElement)]
pub struct ListRow {
    id: ElementId,
    label: SharedString,
    indent: usize,
    leading: Option<AnyElement>,
    meta: Option<SharedString>,
    selected: bool,
    on_click: Option<ClickHandler>,
}

impl ListRow {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            indent: 0,
            leading: None,
            meta: None,
            selected: false,
            on_click: None,
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

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(f));
        self
    }
}

impl RenderOnce for ListRow {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = cx.theme();
        let c = &t.color;
        let row = div()
            .id(self.id)
            .h(t.size(Size::Row))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .gap(t.space(Space::Sm))
            .pl(t.space(Space::Md) + t.space(Space::Lg) * self.indent as f32)
            .pr(t.space(Space::Md))
            .rounded(t.shape.radius_control)
            .cursor_pointer()
            .text_size(t.typography.caption)
            .text_color(if self.selected {
                c.content
            } else {
                c.content_secondary
            })
            .when(self.selected, |el| el.bg(c.surface_accent))
            .when(!self.selected, |el| {
                el.hover(|s| s.bg(c.surface_hover).text_color(c.content))
            })
            .children(self.leading)
            .child(div().flex_1().min_w_0().truncate().child(self.label))
            .children(
                self.meta
                    .map(|m| div().flex_none().text_color(c.content_muted).child(m)),
            );
        match self.on_click {
            Some(f) => row.on_click(f),
            None => row,
        }
    }
}
