use std::rc::Rc;

use gpui::{
    App, ElementId, FontWeight, IntoElement, RenderOnce, SharedString, Window, div, prelude::*,
};

use crate::{ActiveTheme, Size, Space, Tooltip};

type SelectHandler = Rc<dyn Fn(usize, &mut Window, &mut App) + 'static>;

/// Keeps a stale selection on a real segment.
fn clamp_selected(selected: usize, len: usize) -> usize {
    selected.min(len.saturating_sub(1))
}

/// A switch of N labelled segments, one selected. It fills its container; segments share the
/// width and truncate their labels, which a tooltip spells out.
#[derive(IntoElement)]
pub struct SegmentedControl {
    id: ElementId,
    segments: Vec<SharedString>,
    selected: usize,
    on_select: Option<SelectHandler>,
}

impl SegmentedControl {
    pub fn new(
        id: impl Into<ElementId>,
        segments: impl IntoIterator<Item = impl Into<SharedString>>,
        selected: usize,
    ) -> Self {
        Self {
            id: id.into(),
            segments: segments.into_iter().map(Into::into).collect(),
            selected,
            on_select: None,
        }
    }

    pub fn on_select(mut self, f: impl Fn(usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for SegmentedControl {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = cx.theme();
        let c = &t.color;
        let selected = clamp_selected(self.selected, self.segments.len());
        div()
            .id(self.id.clone())
            .w_full()
            .flex()
            .items_center()
            .gap(t.space(Space::Xxs))
            .children(self.segments.into_iter().enumerate().map(|(ix, label)| {
                let active = ix == selected;
                let on_select = self.on_select.clone();
                div()
                    .id((self.id.clone(), ix.to_string()))
                    .h(t.size(Size::Row))
                    .flex_1()
                    .min_w_0()
                    .px(t.space(Space::Md))
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .rounded(t.shape.radius_control)
                    .cursor_pointer()
                    .text_color(if active { c.content } else { c.content_muted })
                    .when(active, |el| {
                        el.bg(c.surface_active).font_weight(FontWeight::MEDIUM)
                    })
                    .when(!active, |el| {
                        el.hover(|s| s.bg(c.surface_hover).text_color(c.content))
                    })
                    .when_some(on_select, |el, f| {
                        el.on_click(move |_, window, cx| f(ix, window, cx))
                    })
                    .tooltip({
                        let label = label.clone();
                        move |_, cx| Tooltip::view(label.clone(), cx)
                    })
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(label),
                    )
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_stays_on_a_segment() {
        assert_eq!(clamp_selected(0, 2), 0);
        assert_eq!(clamp_selected(1, 2), 1);
        assert_eq!(clamp_selected(5, 2), 1);
        assert_eq!(clamp_selected(3, 0), 0);
    }
}
