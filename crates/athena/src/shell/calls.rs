use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use athena_lsp::{Call, CallItem, Position};
use athena_ui::{ActiveTheme, Theme, Tooltip};
use gpui::{
    AnyElement, ClickEvent, Context, FontWeight, Hsla, MouseButton, div, prelude::*, px,
    uniform_list,
};

use super::Shell;
use super::drawer::DrawerTab;
use super::lsp::{NO_SERVER, document_key};

const ROW_HEIGHT: f32 = 24.;
const INDENT: f32 = 16.;

/// Never repeats, so an answer about any earlier tree, even one since replaced, is dropped.
fn next_generation() -> u64 {
    static GENERATIONS: AtomicU64 = AtomicU64::new(0);
    GENERATIONS.fetch_add(1, Ordering::Relaxed)
}

/// The call hierarchy the References tab shows after Shift+Alt+H, as VS Code's Call Hierarchy view.
pub(super) struct Calls {
    /// The project it was asked from; other projects show the tab empty.
    pub(super) root: PathBuf,
    incoming: bool,
    /// The function asked about first, then every caller or callee loaded so far.
    nodes: Vec<Node>,
    expanded: HashSet<usize>,
    selected: Option<usize>,
    /// Changes when the direction flips or a new hierarchy replaces this one.
    generation: u64,
}

struct Node {
    call: Call,
    depth: usize,
    /// The file the call sites are in: the caller's for incoming calls, the parent's for outgoing.
    sites_in: PathBuf,
    children: Children,
}

enum Children {
    Unasked,
    Loading,
    Found(Vec<usize>),
    Failed(String),
}

impl Calls {
    fn new(root: PathBuf, item: CallItem) -> Self {
        let sites_in = item.path.clone();
        Self {
            root,
            incoming: true,
            nodes: vec![Node {
                call: Call {
                    item,
                    ranges: Vec::new(),
                },
                depth: 0,
                sites_in,
                children: Children::Unasked,
            }],
            expanded: HashSet::new(),
            selected: Some(0),
            generation: next_generation(),
        }
    }

    /// Visible nodes in tree order: each expanded node's loaded children follow it.
    fn rows(&self) -> Vec<usize> {
        let mut rows = Vec::new();
        let mut stack = vec![0];
        while let Some(i) = stack.pop() {
            rows.push(i);
            if self.expanded.contains(&i)
                && let Children::Found(children) = &self.nodes[i].children
            {
                stack.extend(children.iter().rev());
            }
        }
        rows
    }

    /// Where a row opens: a call site, or the function itself when it has none.
    fn target(&self, i: usize) -> Option<(PathBuf, Position)> {
        let node = self.nodes.get(i)?;
        Some(match node.call.ranges.first() {
            Some(site) => (node.sites_in.clone(), site.start),
            None => (node.call.item.path.clone(), node.call.item.selection.start),
        })
    }

    fn add_children(&mut self, parent: usize, calls: Vec<Call>) {
        let Some(node) = self.nodes.get(parent) else {
            return;
        };
        let depth = node.depth + 1;
        let parent_file = node.call.item.path.clone();
        let mut children = Vec::with_capacity(calls.len());
        for call in calls {
            let sites_in = match self.incoming {
                true => call.item.path.clone(),
                false => parent_file.clone(),
            };
            children.push(self.nodes.len());
            self.nodes.push(Node {
                call,
                depth,
                sites_in,
                children: Children::Unasked,
            });
        }
        self.nodes[parent].children = Children::Found(children);
    }

    fn answer(&mut self, generation: u64, index: usize, found: Result<Vec<Call>, String>) {
        if generation != self.generation {
            return;
        }
        match found {
            Ok(list) => self.add_children(index, list),
            Err(why) => {
                tracing::debug!("call hierarchy failed: {why}");
                if let Some(node) = self.nodes.get_mut(index) {
                    node.children = Children::Failed(why);
                }
            }
        }
    }
}

impl Shell {
    /// Shift+Alt+H: the callers of the function at the cursor, in the References tab.
    pub(super) fn show_call_hierarchy(&mut self, cx: &mut Context<Self>) {
        const TITLE: &str = "No call hierarchy";
        let Some(editor) = self.focused_editor() else {
            return;
        };
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, &editor, cx);
        let Some(client) = self.document_client(&doc) else {
            return self.lsp_failed(TITLE, NO_SERVER.into(), cx);
        };
        if !client.supports("/callHierarchyProvider") {
            return self.lsp_failed(
                TITLE,
                "This file's language server does not offer a call hierarchy.".into(),
                cx,
            );
        }
        let (Some((line, character)), Some(root)) =
            (editor.read(cx).cursor_utf16(), self.active_root())
        else {
            return;
        };
        let at = Position { line, character };
        tracing::debug!(path = %doc.display(), line, character, "call hierarchy");
        cx.spawn(async move |this, cx| {
            let found = client.prepare_call_hierarchy(&doc, at).await;
            let _ = this.update(cx, |this, cx| match found {
                Ok(items) => match items.into_iter().next() {
                    Some(item) => {
                        this.lsp.calls = Some(Calls::new(root, item));
                        this.lsp.calls_client = Some(client);
                        this.lsp.showing_calls = true;
                        this.show_drawer_tab(DrawerTab::References, cx);
                        this.toggle_call(0, cx);
                    }
                    None => this.lsp_failed(
                        TITLE,
                        "No function or method is under the cursor.".into(),
                        cx,
                    ),
                },
                Err(why) => this.lsp_failed(TITLE, why, cx),
            });
        })
        .detach();
    }

    /// Opens or closes a node, asking the server for its calls the first time it opens.
    fn toggle_call(&mut self, index: usize, cx: &mut Context<Self>) {
        let (Some(calls), Some(client)) = (self.lsp.calls.as_mut(), self.lsp.calls_client.clone())
        else {
            return;
        };
        let Some(node) = calls.nodes.get_mut(index) else {
            return;
        };
        if !calls.expanded.insert(index) {
            calls.expanded.remove(&index);
            cx.notify();
            return;
        }
        if !matches!(node.children, Children::Unasked | Children::Failed(_)) {
            cx.notify();
            return;
        }
        node.children = Children::Loading;
        let (item, incoming, generation) =
            (node.call.item.clone(), calls.incoming, calls.generation);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let found = match incoming {
                true => client.incoming_calls(&item).await,
                false => client.outgoing_calls(&item).await,
            };
            let _ = this.update(cx, |this, cx| {
                if let Some(calls) = this.lsp.calls.as_mut() {
                    calls.answer(generation, index, found);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Shows callers or callees of the same function, as VS Code's direction toggle does.
    fn set_call_direction(&mut self, incoming: bool, cx: &mut Context<Self>) {
        let Some(calls) = self.lsp.calls.as_mut() else {
            return;
        };
        if calls.incoming == incoming {
            return;
        }
        calls.incoming = incoming;
        calls.generation = next_generation();
        calls.nodes.truncate(1);
        calls.nodes[0].children = Children::Unasked;
        calls.expanded.clear();
        calls.selected = Some(0);
        self.toggle_call(0, cx);
    }

    fn active_calls(&self) -> Option<&Calls> {
        let root = self.active_root()?;
        self.lsp.calls.as_ref().filter(|c| c.root == root)
    }

    /// The direction toggle in the drawer's header, while the tab shows a call hierarchy.
    pub(super) fn render_calls_header(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let calls = self.active_calls()?;
        let t = cx.theme().clone();
        let option = |id: &'static str, label: &'static str, incoming: bool| {
            let active = calls.incoming == incoming;
            div()
                .id(id)
                .h(t.ui(22.))
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
                .when(active, |el| el.bg(t.color.surface_active))
                .when(!active, |el| {
                    el.hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                })
                .tooltip(move |_, cx| Tooltip::view(format!("Show {label} Calls"), cx))
                .on_click(cx.listener(move |this, _, _, cx| this.set_call_direction(incoming, cx)))
                .child(label)
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(4.))
                .child(div().mr(px(6.)).text_color(t.color.content_muted).child(
                    match calls.incoming {
                        true => format!("Callers of {}", calls.nodes[0].call.item.name),
                        false => format!("Calls from {}", calls.nodes[0].call.item.name),
                    },
                ))
                .child(option("calls-incoming", "Incoming", true))
                .child(option("calls-outgoing", "Outgoing", false))
                .into_any_element(),
        )
    }

    /// The call tree: a chevron opens a function's own callers or callees; a row opens the call.
    pub(super) fn render_calls(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let calls = self.active_calls()?;
        let t = cx.theme().clone();
        let rows = calls.rows();
        let roots: Vec<PathBuf> = [document_key(&calls.root), calls.root.clone()].into();
        let entries: Vec<RowView> = rows
            .iter()
            .map(|&i| RowView::of(calls, i, &roots))
            .collect();
        let empty = match &calls.nodes[0].children {
            Children::Found(c) if c.is_empty() && calls.expanded.contains(&0) => {
                Some(match calls.incoming {
                    true => "No callers found.",
                    false => "No calls found.",
                })
            }
            _ => None,
        };
        let list = uniform_list(
            "call-hierarchy",
            entries.len(),
            cx.processor(move |_this, range: std::ops::Range<usize>, _window, cx| {
                entries[range]
                    .iter()
                    .map(|row| {
                        let (index, selected) = (row.index, row.selected);
                        let (badge, badge_color) = symbol_badge(row.kind, &t);
                        div()
                            .id(("call", index))
                            .w_full()
                            .h(t.ui(ROW_HEIGHT))
                            .pl(t.ui(8. + INDENT * row.depth as f32))
                            .pr(px(12.))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .cursor_pointer()
                            .text_size(t.typography.caption)
                            .hover(|s| s.bg(t.color.surface_hover))
                            .when(selected, |el| el.bg(t.color.surface_active))
                            .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                                let Some(calls) = this.lsp.calls.as_mut() else {
                                    return;
                                };
                                let Some(target) = calls.target(index) else {
                                    return;
                                };
                                calls.selected = Some(index);
                                if e.click_count() == 2 {
                                    return this.toggle_call(index, cx);
                                }
                                this.lsp.jump = Some(target);
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .id(("call-chevron", index))
                                    .w(px(12.))
                                    .flex_none()
                                    .text_color(t.color.content_muted)
                                    .hover(|s| s.text_color(t.color.content))
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.toggle_call(index, cx);
                                    }))
                                    .child(row.chevron),
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
                                    .flex_none()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(if selected {
                                        t.color.accent
                                    } else {
                                        t.color.content_secondary
                                    })
                                    .child(row.name.clone()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_color(t.color.content_muted)
                                    .child(row.detail.clone()),
                            )
                            .children(row.status.clone().map(|s| {
                                div().flex_none().text_color(t.color.content_muted).child(s)
                            }))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .size_full();
        Some(
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(list)
                .children(empty.map(|text| {
                    div()
                        .px(px(12.))
                        .py(px(8.))
                        .text_size(cx.theme().typography.caption)
                        .text_color(cx.theme().color.content_muted)
                        .child(text)
                }))
                .into_any_element(),
        )
    }
}

/// A glyph for a SymbolKind, coloured as the editor colours that kind of name.
pub(super) fn symbol_badge(kind: u32, t: &Theme) -> (&'static str, Hsla) {
    let s = &t.syntax;
    match kind {
        6 | 9 | 12 => ("ƒ", s.function),
        7 | 8 | 20 => ("◆", s.property),
        5 | 10 | 11 | 23 | 26 => ("T", s.type_),
        2..=4 => ("{}", s.type_),
        13 => ("x", s.text),
        14 | 22 => ("c", s.constant),
        15..=18 => ("c", s.string_special),
        _ => ("·", t.color.content_muted),
    }
}

/// One row as drawn, computed before the list renders so the closure owns its data.
struct RowView {
    index: usize,
    depth: usize,
    kind: u32,
    name: String,
    detail: String,
    chevron: &'static str,
    status: Option<String>,
    selected: bool,
}

impl RowView {
    fn of(calls: &Calls, i: usize, roots: &[PathBuf]) -> Self {
        let node = &calls.nodes[i];
        let open = calls.expanded.contains(&i);
        let chevron = match (&node.children, open) {
            (Children::Found(c), _) if c.is_empty() => "",
            (_, true) => "▾",
            (_, false) => "▸",
        };
        let status = match &node.children {
            Children::Loading => Some("Loading…".to_string()),
            Children::Failed(why) => Some(why.clone()),
            _ if node.call.ranges.len() > 1 => Some(format!("{} calls", node.call.ranges.len())),
            _ => None,
        };
        let place = format!(
            "{}:{}",
            shown(&node.call.item.path, roots),
            node.call.item.selection.start.line.saturating_add(1)
        );
        let detail = match &node.call.item.detail {
            Some(d) => format!("{d}  {place}"),
            None => place,
        };
        Self {
            index: i,
            depth: node.depth,
            kind: node.call.item.kind,
            name: node.call.item.name.clone(),
            detail,
            chevron,
            status,
            selected: calls.selected == Some(i),
        }
    }
}

fn shown(path: &Path, roots: &[PathBuf]) -> String {
    roots
        .iter()
        .find_map(|root| path.strip_prefix(root).ok())
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use athena_lsp::Range;

    fn item(name: &str, file: &str, line: u32) -> CallItem {
        let at = Position { line, character: 5 };
        CallItem {
            name: name.into(),
            kind: 12,
            detail: None,
            path: PathBuf::from(file),
            selection: Range { start: at, end: at },
            raw: serde_json::Value::Null,
        }
    }

    fn call(name: &str, file: &str, sites: &[u32]) -> Call {
        let at = |line| Position { line, character: 2 };
        Call {
            item: item(name, file, 1),
            ranges: sites
                .iter()
                .map(|&l| Range {
                    start: at(l),
                    end: at(l),
                })
                .collect(),
        }
    }

    fn calls(incoming: bool) -> Calls {
        let mut calls = Calls::new(PathBuf::from("/p"), item("helper", "/p/h.go", 3));
        calls.incoming = incoming;
        calls
    }

    #[test]
    fn an_answer_about_a_replaced_tree_is_dropped() {
        let mut c = calls(true);
        c.add_children(0, vec![call("main", "/p/main.go", &[7])]);
        c.add_children(1, vec![call("run", "/p/run.go", &[2])]);
        c.add_children(2, vec![call("init", "/p/init.go", &[4])]);
        let asked = c.generation;
        let mut c = Calls::new(PathBuf::from("/p"), item("other", "/p/o.go", 1));
        c.answer(asked, 3, Ok(vec![call("late", "/p/late.go", &[1])]));
        c.answer(asked, 3, Err("gone".into()));
        assert_eq!(c.nodes.len(), 1);
        assert!(matches!(c.nodes[0].children, Children::Unasked));
        assert_eq!(c.target(3), None, "a row drawn before the tree changed");
    }

    #[test]
    fn a_function_on_the_last_possible_line_still_draws() {
        let mut c = calls(true);
        c.nodes[0].call.item.selection.start.line = u32::MAX;
        assert!(
            RowView::of(&c, 0, &[])
                .detail
                .ends_with(&u32::MAX.to_string())
        );
    }

    #[test]
    fn rows_follow_expanded_nodes_in_tree_order() {
        let mut c = calls(true);
        assert_eq!(c.rows(), [0]);
        c.add_children(
            0,
            vec![
                call("main", "/p/main.go", &[7]),
                call("run", "/p/run.go", &[2, 9]),
            ],
        );
        assert_eq!(c.rows(), [0], "closed until expanded");
        c.expanded.insert(0);
        assert_eq!(c.rows(), [0, 1, 2]);
        c.add_children(1, vec![call("init", "/p/init.go", &[4])]);
        c.expanded.insert(1);
        assert_eq!(c.rows(), [0, 1, 3, 2]);
        assert_eq!(c.nodes[3].depth, 2);
        c.expanded.remove(&0);
        assert_eq!(c.rows(), [0]);
    }

    #[test]
    fn incoming_rows_open_the_call_in_the_caller_and_outgoing_ones_in_the_parent() {
        let mut c = calls(true);
        c.add_children(0, vec![call("main", "/p/main.go", &[7])]);
        assert_eq!(
            c.target(1),
            Some((
                PathBuf::from("/p/main.go"),
                Position {
                    line: 7,
                    character: 2
                }
            ))
        );
        assert_eq!(
            c.target(0),
            Some((
                PathBuf::from("/p/h.go"),
                Position {
                    line: 3,
                    character: 5
                }
            )),
            "the function itself"
        );
        let mut c = calls(false);
        c.add_children(0, vec![call("leaf", "/p/leaf.go", &[4, 4])]);
        assert_eq!(
            c.target(1),
            Some((
                PathBuf::from("/p/h.go"),
                Position {
                    line: 4,
                    character: 2
                }
            ))
        );
        assert_eq!(
            RowView::of(&c, 1, &[PathBuf::from("/p")]).status.as_deref(),
            Some("2 calls")
        );
        assert_eq!(
            RowView::of(&c, 1, &[PathBuf::from("/p")]).detail,
            "leaf.go:2"
        );
    }
}
