use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use athena_lsp::{Position, Symbol, symbol_kind_label};
use athena_ui::{ActiveTheme, InputEvent, TextInput, Tooltip};
use gpui::{
    AnyElement, Context, Entity, Focusable, FontWeight, MouseButton, ScrollStrategy, Subscription,
    UniformListScrollHandle, Window, actions, div, prelude::*, px, uniform_list,
};

use super::Shell;
use super::breadcrumbs::symbol_chain;
use super::calls::symbol_badge;
use super::lsp::document_key;

actions!(athena, [ShowOutline, ShowExplorer]);

const ROW_HEIGHT: f32 = 22.;
const INDENT: f32 = 12.;

/// The tree area's Outline view: the active editor's symbols, as VS Code's Outline lists them.
#[derive(Default)]
pub(super) struct OutlineState {
    /// The tree area shows the outline instead of the files.
    pub(super) showing: bool,
    filter: Option<Entity<TextInput>>,
    _subscriptions: Vec<Subscription>,
    /// Closed symbols by file and the names leading to them, which survive edits that renumber.
    collapsed: HashSet<(PathBuf, Vec<String>)>,
    /// The row arrows moved to; `None` follows the cursor.
    picked: Option<usize>,
    scroll: UniformListScrollHandle,
    /// The symbol last scrolled into view, so following the cursor scrolls only when it moves.
    followed: Option<(PathBuf, usize)>,
}

/// A symbol as the outline draws it.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    index: usize,
    depth: usize,
    has_children: bool,
    open: bool,
}

/// The names from the outermost symbol down to `index`.
fn name_path(symbols: &[Symbol], index: usize) -> Vec<String> {
    let mut names = vec![symbols[index].name.clone()];
    let mut at = symbols[index].parent;
    while let Some(parent) = at {
        names.push(symbols[parent].name.clone());
        at = symbols[parent].parent;
    }
    names.reverse();
    names
}

/// The rows shown for `symbols` (parents before children): with a filter, the matches and the
/// symbols holding them, all open; without, everything outside a closed symbol.
fn outline_rows(symbols: &[Symbol], filter: &str, closed: impl Fn(usize) -> bool) -> Vec<Row> {
    let depth = |i: usize| {
        let mut d = 0;
        let mut at = symbols[i].parent;
        while let Some(p) = at {
            d += 1;
            at = symbols[p].parent;
        }
        d
    };
    let has_children: Vec<bool> = {
        let mut has = vec![false; symbols.len()];
        for s in symbols {
            if let Some(p) = s.parent {
                has[p] = true;
            }
        }
        has
    };
    let needle = filter.trim().to_lowercase();
    if !needle.is_empty() {
        let mut keep = vec![false; symbols.len()];
        for (i, s) in symbols.iter().enumerate() {
            if s.name.to_lowercase().contains(&needle) {
                let mut at = Some(i);
                while let Some(k) = at {
                    keep[k] = true;
                    at = symbols[k].parent;
                }
            }
        }
        return (0..symbols.len())
            .filter(|&i| keep[i])
            .map(|i| Row {
                index: i,
                depth: depth(i),
                has_children: has_children[i],
                open: true,
            })
            .collect();
    }
    let mut rows = Vec::new();
    let mut hidden_under: Option<usize> = None;
    for (i, &has) in has_children.iter().enumerate() {
        let d = depth(i);
        if hidden_under.is_some_and(|h| d > h) {
            continue;
        }
        hidden_under = None;
        let open = !closed(i);
        if has && !open {
            hidden_under = Some(d);
        }
        rows.push(Row {
            index: i,
            depth: d,
            has_children: has,
            open,
        });
    }
    rows
}

impl Shell {
    /// Explorer or Outline in the tree area, showing the tree first if it was hidden.
    pub(super) fn show_outline(
        &mut self,
        outline: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.outline.showing = outline;
        if !self.workspace.ui.tree_visible {
            self.toggle_tree(cx);
        }
        if outline {
            let filter = self.outline_filter(cx);
            window.focus(&filter.focus_handle(cx));
        }
        cx.notify();
    }

    fn outline_filter(&mut self, cx: &mut Context<Self>) -> Entity<TextInput> {
        if let Some(filter) = &self.outline.filter {
            return filter.clone();
        }
        let filter = cx.new(|cx| TextInput::new("Filter symbols", cx));
        self.outline._subscriptions =
            vec![
                cx.subscribe(&filter, |this, input, event: &InputEvent, cx| match event {
                    InputEvent::Changed => {
                        this.outline.picked = None;
                        cx.notify();
                    }
                    InputEvent::Up => this.step_outline(-1, cx),
                    InputEvent::Down => this.step_outline(1, cx),
                    InputEvent::Submit | InputEvent::SubmitBeside => this.open_picked_symbol(cx),
                    InputEvent::Cancel => input.update(cx, |i, cx| i.set_text("", cx)),
                }),
            ];
        self.outline.filter = Some(filter.clone());
        filter
    }

    /// The active editor's symbols and the rows they show as, if its server listed any.
    fn outline_view(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(PathBuf, Rc<Vec<Symbol>>, Vec<Row>)> {
        let editor = self.focused_editor()?;
        let symbols = self.breadcrumb_symbols(&editor, cx)?;
        let doc = document_key(editor.read(cx).path());
        let filter = self
            .outline
            .filter
            .as_ref()
            .map(|f| f.read(cx).text().to_string())
            .unwrap_or_default();
        let collapsed = &self.outline.collapsed;
        let rows = outline_rows(&symbols, &filter, |i| {
            collapsed.contains(&(doc.clone(), name_path(&symbols, i)))
        });
        Some((doc, symbols, rows))
    }

    fn step_outline(&mut self, by: isize, cx: &mut Context<Self>) {
        let Some((doc, symbols, rows)) = self.outline_view(cx) else {
            return;
        };
        if rows.is_empty() {
            return;
        }
        let current = self.outline.picked.or_else(|| {
            let cursor = self.focused_editor()?.read(cx).cursor_utf16()?;
            let at = Position {
                line: cursor.0,
                character: cursor.1,
            };
            let active = *symbol_chain(&symbols, at).last()?;
            rows.iter().position(|r| r.index == active)
        });
        let next = match current {
            Some(i) => (i as isize + by).clamp(0, rows.len() as isize - 1) as usize,
            None => 0,
        };
        self.outline.picked = Some(next);
        self.outline.followed = Some((doc, rows[next].index));
        self.outline
            .scroll
            .scroll_to_item(next, ScrollStrategy::Center);
        cx.notify();
    }

    fn open_picked_symbol(&mut self, cx: &mut Context<Self>) {
        let Some((_, symbols, rows)) = self.outline_view(cx) else {
            return;
        };
        let Some(row) = rows.get(self.outline.picked.unwrap_or(0)) else {
            return;
        };
        let s = &symbols[row.index];
        self.lsp.jump = Some((self.project_spelling(&s.path), s.range.start));
        cx.notify();
    }

    /// The Explorer / Outline switch at the top of the tree area.
    pub(super) fn render_tree_switch(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let option = |id: &'static str, label: &'static str, outline: bool| {
            let active = self.outline.showing == outline;
            div()
                .id(id)
                .h(px(22.))
                .px(px(8.))
                .flex()
                .items_center()
                .rounded(t.shape.radius_control)
                .cursor_pointer()
                .text_color(if active {
                    t.color.content
                } else {
                    t.color.content_muted
                })
                .when(active, |el| {
                    el.bg(t.color.surface_active)
                        .font_weight(FontWeight::MEDIUM)
                })
                .when(!active, |el| {
                    el.hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                })
                .on_click(
                    cx.listener(move |this, _, window, cx| this.show_outline(outline, window, cx)),
                )
                .child(label)
        };
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .child(option("tree-explorer", "Explorer", false))
            .child(option("tree-outline", "Outline", true))
            .into_any_element()
    }

    /// The Outline view: a filter, then the symbols, the one holding the cursor highlighted.
    pub(super) fn render_outline(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let filter = self.outline_filter(cx);
        let message = |text: String| {
            div()
                .px(px(12.))
                .py(px(8.))
                .text_size(t.typography.caption)
                .text_color(t.color.content_muted)
                .child(text)
                .into_any_element()
        };
        let editor = self.focused_editor();
        let body = match (&editor, self.outline_view(cx)) {
            (None, _) => message("The active editor cannot provide outline information.".into()),
            (Some(editor), None) => {
                let doc = document_key(editor.read(cx).path());
                match self.document_client(&doc) {
                    Some(_) => message("Loading document symbols…".into()),
                    None => message(super::lsp::NO_SERVER.into()),
                }
            }
            (Some(editor), Some((_, symbols, rows))) if rows.is_empty() => {
                let name = file_name(editor.read(cx).path());
                match symbols.is_empty() {
                    true => message(format!("No symbols found in document '{name}'.")),
                    false => message("No symbols match the filter.".into()),
                }
            }
            (Some(editor), Some((doc, symbols, rows))) => {
                let cursor = editor.read(cx).cursor_utf16();
                let active = cursor.and_then(|(line, character)| {
                    symbol_chain(&symbols, Position { line, character })
                        .last()
                        .copied()
                });
                let picked = self.outline.picked;
                if picked.is_none()
                    && let Some(active) = active
                    && self.outline.followed != Some((doc.clone(), active))
                    && let Some(at) = rows.iter().position(|r| r.index == active)
                {
                    self.outline.followed = Some((doc.clone(), active));
                    self.outline
                        .scroll
                        .scroll_to_item(at, ScrollStrategy::Center);
                }
                self.render_outline_rows(doc, symbols, rows, active, cx)
            }
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div().flex_none().px(px(8.)).py(px(6.)).child(
                    div()
                        .h(px(24.))
                        .px(px(6.))
                        .flex()
                        .items_center()
                        .rounded(t.shape.radius_control)
                        .bg(t.color.surface_sunken)
                        .border_1()
                        .border_color(t.color.border)
                        .text_size(t.typography.caption)
                        .child(filter),
                ),
            )
            .child(body)
            .into_any_element()
    }

    fn render_outline_rows(
        &mut self,
        doc: PathBuf,
        symbols: Rc<Vec<Symbol>>,
        rows: Vec<Row>,
        active: Option<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let picked = self.outline.picked;
        uniform_list(
            "outline",
            rows.len(),
            cx.processor(move |_this, range: std::ops::Range<usize>, _window, cx| {
                range
                    .map(|n| {
                        let row = &rows[n];
                        let s = &symbols[row.index];
                        let current = match picked {
                            Some(p) => p == n,
                            None => active == Some(row.index),
                        };
                        let (badge, badge_color) = symbol_badge(s.kind, &t);
                        let key = (doc.clone(), name_path(&symbols, row.index));
                        let (path, at) = (s.path.clone(), s.range.start);
                        let chevron = match (row.has_children, row.open) {
                            (false, _) => "",
                            (true, true) => "▾",
                            (true, false) => "▸",
                        };
                        div()
                            .id(("outline", n))
                            .w_full()
                            .h(px(ROW_HEIGHT))
                            .pl(px(8. + INDENT * row.depth as f32))
                            .pr(px(8.))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .cursor_pointer()
                            .text_size(t.typography.caption)
                            .text_color(if current {
                                t.color.accent
                            } else {
                                t.color.content_secondary
                            })
                            .when(current, |el| el.bg(t.color.surface_accent))
                            .when(!current, |el| {
                                el.hover(|s| {
                                    s.bg(t.color.surface_hover).text_color(t.color.content)
                                })
                            })
                            .tooltip({
                                let tip = format!("{} ({})", s.name, symbol_kind_label(s.kind));
                                move |_, cx| Tooltip::view(tip.clone(), cx)
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.outline.picked = None;
                                this.lsp.jump = Some((this.project_spelling(&path), at));
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .id(("outline-chevron", n))
                                    .w(px(10.))
                                    .flex_none()
                                    .text_color(t.color.content_muted)
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        if !this.outline.collapsed.remove(&key) {
                                            this.outline.collapsed.insert(key.clone());
                                        }
                                        cx.notify();
                                    }))
                                    .child(chevron),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(14.))
                                    .text_color(badge_color)
                                    .child(badge),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(s.name.clone()),
                            )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.outline.scroll.clone())
        .flex_1()
        .into_any_element()
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use athena_lsp::Range;

    fn symbol(name: &str, line: u32, parent: Option<usize>) -> Symbol {
        let at = Position { line, character: 0 };
        Symbol {
            name: name.into(),
            kind: 12,
            container: None,
            path: PathBuf::from("/p/a.go"),
            range: Range { start: at, end: at },
            scope: Range { start: at, end: at },
            parent,
        }
    }

    fn list() -> Vec<Symbol> {
        vec![
            symbol("Server", 1, None),
            symbol("Start", 2, Some(0)),
            symbol("addr", 3, Some(1)),
            symbol("Stop", 5, Some(0)),
            symbol("main", 9, None),
        ]
    }

    fn shown(rows: &[Row]) -> Vec<(usize, usize)> {
        rows.iter().map(|r| (r.index, r.depth)).collect()
    }

    #[test]
    fn closed_symbols_hide_what_they_hold() {
        let symbols = list();
        let rows = outline_rows(&symbols, "", |_| false);
        assert_eq!(shown(&rows), [(0, 0), (1, 1), (2, 2), (3, 1), (4, 0)]);
        assert!(rows[0].has_children && rows[0].open && !rows[4].has_children);
        let rows = outline_rows(&symbols, "", |i| i == 1);
        assert_eq!(shown(&rows), [(0, 0), (1, 1), (3, 1), (4, 0)]);
        assert!(!rows[1].open);
        let rows = outline_rows(&symbols, "", |i| i == 0);
        assert_eq!(shown(&rows), [(0, 0), (4, 0)]);
    }

    #[test]
    fn a_filter_keeps_matches_and_the_symbols_holding_them() {
        let symbols = list();
        let rows = outline_rows(&symbols, "ADD", |_| true);
        assert_eq!(shown(&rows), [(0, 0), (1, 1), (2, 2)], "closed ones open");
        assert!(outline_rows(&symbols, "nothing", |_| false).is_empty());
        assert_eq!(name_path(&symbols, 2), ["Server", "Start", "addr"]);
    }
}
