use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Smallest pane edge, in pixels, that dragging or splitting may produce.
pub const MIN_PANE: f32 = 120.;
pub const DIVIDER: f32 = 1.;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaneId(pub u64);

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ItemId(pub u64);

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// Panes side by side.
    Horizontal,
    /// Panes stacked.
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ItemKind {
    /// `session` is the athena-mux pane id once the shell exists.
    Terminal {
        session: Option<u64>,
    },
    Editor {
        path: PathBuf,
    },
    Preview {
        url: String,
    },
    Image {
        path: PathBuf,
    },
    /// A Markdown or Mermaid file shown rendered.
    Rendered {
        path: PathBuf,
    },
    /// Two versions of a file compared.
    Diff {
        path: PathBuf,
        base: DiffBase,
    },
}

/// What a diff tab compares, VS Code's way: the staged change, the unstaged change, or a
/// Claude Code session's edits.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub enum DiffBase {
    /// HEAD against the index.
    Head,
    /// The index against the file on disk.
    Index,
    /// The file before a Claude Code session first edited it, against the file on disk.
    Snapshot { session: String },
    /// The file on disk against an edit Claude Code proposes and is waiting on; never saved.
    Proposal { id: String },
}

impl ItemKind {
    /// The file a tab shows, for editors and viewers.
    pub fn file(&self) -> Option<&PathBuf> {
        match self {
            Self::Editor { path }
            | Self::Image { path }
            | Self::Rendered { path }
            | Self::Diff { path, .. } => Some(path),
            Self::Terminal { .. } | Self::Preview { .. } => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Item {
    pub id: ItemId,
    pub kind: ItemKind,
    /// Where an editor tab was left, restored when its view is next created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<ViewState>,
}

/// An editor's cursor, scroll and folds, in zero-based lines and UTF-16 columns.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ViewState {
    pub cursor: (u32, u32),
    /// The first line shown, once the editor can report it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scroll_top: Option<u32>,
    /// First lines of folded regions.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub folds: Vec<u32>,
    /// The tab's own word wrap choice; `None` follows the workspace's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrap: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Pane {
    pub id: PaneId,
    pub items: Vec<Item>,
    pub active: usize,
}

impl Pane {
    pub fn active_item(&self) -> Option<&Item> {
        self.items.get(self.active)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Node {
    Leaf(Pane),
    Split {
        axis: Axis,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

/// Which child to take at each split, from the root; `false` is `first`.
pub type NodePath = Vec<bool>;

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    pub path: NodePath,
    pub axis: Axis,
    pub rect: Rect,
    /// The whole split this divider belongs to, for turning a drag position into a ratio.
    pub split: Rect,
}

/// The split tree of one project's panes.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Layout {
    pub tree: Node,
    pub focused: PaneId,
    next_id: u64,
}

impl Layout {
    pub fn new(first: ItemKind) -> Self {
        let pane = Pane {
            id: PaneId(1),
            items: vec![Item {
                id: ItemId(2),
                kind: first,
                view: None,
            }],
            active: 0,
        };
        Self {
            tree: Node::Leaf(pane),
            focused: PaneId(1),
            next_id: 3,
        }
    }

    /// Repairs a layout read from disk: empty panes go, active tabs and focus point at something
    /// that exists, and new ids never repeat old ones; `None` when no pane is left.
    pub fn validated(mut self) -> Option<Self> {
        while let Some(empty) = self
            .panes()
            .into_iter()
            .find(|p| p.items.is_empty())
            .map(|p| p.id)
        {
            if !self.close_pane(empty) {
                return None;
            }
        }
        let ids: Vec<PaneId> = self.panes().into_iter().map(|p| p.id).collect();
        for id in &ids {
            if let Some(p) = self.pane_mut(*id) {
                p.active = p.active.min(p.items.len() - 1);
            }
        }
        if self.pane(self.focused).is_none() {
            self.focused = ids[0];
        }
        let highest = ids
            .iter()
            .map(|p| p.0)
            .chain(self.items().map(|i| i.id.0))
            .max()
            .unwrap_or(0);
        self.next_id = self.next_id.max(highest + 1);
        Some(self)
    }

    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id - 1
    }

    pub fn panes(&self) -> Vec<&Pane> {
        fn walk<'a>(node: &'a Node, out: &mut Vec<&'a Pane>) {
            match node {
                Node::Leaf(p) => out.push(p),
                Node::Split { first, second, .. } => {
                    walk(first, out);
                    walk(second, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.tree, &mut out);
        out
    }

    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.panes().into_iter().flat_map(|p| p.items.iter())
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes().into_iter().find(|p| p.id == id)
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        fn walk(node: &mut Node, id: PaneId) -> Option<&mut Pane> {
            match node {
                Node::Leaf(p) if p.id == id => Some(p),
                Node::Leaf(_) => None,
                Node::Split { first, second, .. } => walk(first, id).or_else(|| walk(second, id)),
            }
        }
        walk(&mut self.tree, id)
    }

    pub fn item_mut(&mut self, id: ItemId) -> Option<&mut Item> {
        fn walk(node: &mut Node, id: ItemId) -> Option<&mut Item> {
            match node {
                Node::Leaf(p) => p.items.iter_mut().find(|i| i.id == id),
                Node::Split { first, second, .. } => walk(first, id).or_else(|| walk(second, id)),
            }
        }
        walk(&mut self.tree, id)
    }

    pub fn focused_pane(&self) -> Option<&Pane> {
        self.pane(self.focused)
    }

    /// Adds a tab to `pane`, makes it active and returns its id.
    pub fn add_item(&mut self, pane: PaneId, kind: ItemKind) -> Option<ItemId> {
        let id = ItemId(self.next());
        let pane = self.pane_mut(pane)?;
        pane.items.push(Item {
            id,
            kind,
            view: None,
        });
        pane.active = pane.items.len() - 1;
        Some(id)
    }

    /// Splits `pane`, putting a new pane holding `kind` after it; the new pane takes focus.
    pub fn split(&mut self, pane: PaneId, axis: Axis, kind: ItemKind) -> Option<PaneId> {
        self.pane(pane)?;
        let item = Item {
            id: ItemId(self.next()),
            kind,
            view: None,
        };
        self.split_off(pane, axis, item, false)
    }

    /// Puts `item` in a new pane beside `target`, before it when `first`; the new pane takes focus.
    fn split_off(&mut self, target: PaneId, axis: Axis, item: Item, first: bool) -> Option<PaneId> {
        fn walk(node: &mut Node, target: PaneId, make: &mut Option<(Axis, Pane, bool)>) -> bool {
            match node {
                Node::Leaf(p) if p.id == target => {
                    let (axis, fresh, first) = make.take().expect("split once");
                    let old = std::mem::replace(node, Node::Leaf(fresh.clone()));
                    let (a, b) = if first {
                        (Node::Leaf(fresh), old)
                    } else {
                        (old, Node::Leaf(fresh))
                    };
                    *node = Node::Split {
                        axis,
                        ratio: 0.5,
                        first: Box::new(a),
                        second: Box::new(b),
                    };
                    true
                }
                Node::Leaf(_) => false,
                Node::Split {
                    first: a,
                    second: b,
                    ..
                } => walk(a, target, make) || walk(b, target, make),
            }
        }
        let new_pane = PaneId(self.next());
        let fresh = Pane {
            id: new_pane,
            items: vec![item],
            active: 0,
        };
        if !walk(&mut self.tree, target, &mut Some((axis, fresh, first))) {
            return None;
        }
        self.focused = new_pane;
        Some(new_pane)
    }

    /// The pane holding `item` and the item's index in it.
    pub fn find_item(&self, item: ItemId) -> Option<(PaneId, usize)> {
        self.panes().into_iter().find_map(|p| {
            p.items
                .iter()
                .position(|i| i.id == item)
                .map(|index| (p.id, index))
        })
    }

    /// Takes `item` out of its pane, keeping that pane's active tab where it was.
    fn detach(&mut self, item: ItemId) -> Option<(PaneId, Item)> {
        let (pane, index) = self.find_item(item)?;
        let p = self.pane_mut(pane)?;
        let taken = p.items.remove(index);
        if p.active > index || p.active >= p.items.len() {
            p.active = p.active.saturating_sub(1);
        }
        Some((pane, taken))
    }

    /// Moves a tab so it ends up at `index` among pane `to`'s tabs (clamped), keeping its id; an
    /// emptied source pane closes. Returns false when nothing would change.
    pub fn move_item(&mut self, item: ItemId, to: PaneId, index: usize) -> bool {
        let Some((from, at)) = self.find_item(item) else {
            return false;
        };
        let Some(target) = self.pane(to) else {
            return false;
        };
        let len = target.items.len();
        let dest = if from == to {
            index.min(len - 1)
        } else {
            index.min(len)
        };
        if from == to && dest == at {
            return false;
        }
        let Some((_, moved)) = self.detach(item) else {
            return false;
        };
        let p = self.pane_mut(to).expect("target pane exists");
        p.items.insert(dest, moved);
        p.active = dest;
        if self.pane(from).is_some_and(|p| p.items.is_empty()) {
            self.close_pane(from);
        }
        self.focused = to;
        true
    }

    /// Splits `target` with `item` moved out of its pane into a new one, keeping the item's id; an
    /// emptied source pane closes. Returns the new pane, or `None` when the move would empty `target`.
    pub fn split_with_item(
        &mut self,
        target: PaneId,
        axis: Axis,
        item: ItemId,
        place_first: bool,
    ) -> Option<PaneId> {
        let (from, _) = self.find_item(item)?;
        let source_len = self.pane(from)?.items.len();
        self.pane(target)?;
        if from == target && source_len == 1 {
            return None;
        }
        let (_, moved) = self.detach(item)?;
        if source_len == 1 {
            self.close_pane(from);
        }
        self.split_off(target, axis, moved, place_first)
    }

    /// Removes an item; returns false when that emptied its pane and the pane was the last one.
    pub fn close_item(&mut self, item: ItemId) -> bool {
        let Some(pane) = self
            .panes()
            .into_iter()
            .find(|p| p.items.iter().any(|i| i.id == item))
            .map(|p| p.id)
        else {
            return true;
        };
        let p = self.pane_mut(pane).expect("pane exists");
        p.items.retain(|i| i.id != item);
        if p.items.is_empty() {
            return self.close_pane(pane);
        }
        p.active = p.active.min(p.items.len() - 1);
        true
    }

    /// Replaces the pane's parent split with its sibling; returns false if it was the only pane.
    pub fn close_pane(&mut self, pane: PaneId) -> bool {
        fn walk(node: &mut Node, target: PaneId) -> bool {
            let Node::Split { first, second, .. } = node else {
                return false;
            };
            let keep = match (&**first, &**second) {
                (Node::Leaf(p), _) if p.id == target => {
                    Some(std::mem::replace(&mut **second, Node::Leaf(dummy())))
                }
                (_, Node::Leaf(p)) if p.id == target => {
                    Some(std::mem::replace(&mut **first, Node::Leaf(dummy())))
                }
                _ => None,
            };
            match keep {
                Some(sibling) => {
                    *node = sibling;
                    true
                }
                None => walk(first, target) || walk(second, target),
            }
        }
        if matches!(&self.tree, Node::Leaf(p) if p.id == pane) {
            return false;
        }
        let rects = self
            .layout(Rect {
                x: 0.,
                y: 0.,
                w: 1000.,
                h: 1000.,
            })
            .0;
        let fallback = self
            .neighbor_in(&rects, pane, Direction::Left)
            .or_else(|| self.neighbor_in(&rects, pane, Direction::Up))
            .or_else(|| self.neighbor_in(&rects, pane, Direction::Right))
            .or_else(|| self.neighbor_in(&rects, pane, Direction::Down));
        walk(&mut self.tree, pane);
        if self.focused == pane || self.pane(self.focused).is_none() {
            self.focused = fallback.unwrap_or_else(|| self.panes()[0].id);
        }
        true
    }

    /// Lays the tree out in `bounds`, keeping each pane at least `MIN_PANE` where space allows.
    pub fn layout(&self, bounds: Rect) -> (Vec<(PaneId, Rect)>, Vec<Divider>) {
        fn walk(
            node: &Node,
            r: Rect,
            path: &mut NodePath,
            panes: &mut Vec<(PaneId, Rect)>,
            dividers: &mut Vec<Divider>,
        ) {
            match node {
                Node::Leaf(p) => panes.push((p.id, r)),
                Node::Split {
                    axis,
                    ratio,
                    first,
                    second,
                } => {
                    let len = match axis {
                        Axis::Horizontal => r.w,
                        Axis::Vertical => r.h,
                    };
                    let first_len = split_point(len, *ratio);
                    let rest = (len - first_len - DIVIDER).max(0.);
                    let (a, d, b) = match axis {
                        Axis::Horizontal => (
                            Rect { w: first_len, ..r },
                            Rect {
                                x: r.x + first_len,
                                w: DIVIDER,
                                ..r
                            },
                            Rect {
                                x: r.x + first_len + DIVIDER,
                                w: rest,
                                ..r
                            },
                        ),
                        Axis::Vertical => (
                            Rect { h: first_len, ..r },
                            Rect {
                                y: r.y + first_len,
                                h: DIVIDER,
                                ..r
                            },
                            Rect {
                                y: r.y + first_len + DIVIDER,
                                h: rest,
                                ..r
                            },
                        ),
                    };
                    dividers.push(Divider {
                        path: path.clone(),
                        axis: *axis,
                        rect: d,
                        split: r,
                    });
                    path.push(false);
                    walk(first, a, path, panes, dividers);
                    path.pop();
                    path.push(true);
                    walk(second, b, path, panes, dividers);
                    path.pop();
                }
            }
        }
        let (mut panes, mut dividers) = (Vec::new(), Vec::new());
        walk(
            &self.tree,
            bounds,
            &mut Vec::new(),
            &mut panes,
            &mut dividers,
        );
        (panes, dividers)
    }

    pub fn neighbor(&self, bounds: Rect, pane: PaneId, dir: Direction) -> Option<PaneId> {
        self.neighbor_in(&self.layout(bounds).0, pane, dir)
    }

    fn neighbor_in(
        &self,
        rects: &[(PaneId, Rect)],
        pane: PaneId,
        dir: Direction,
    ) -> Option<PaneId> {
        let (_, from) = rects.iter().find(|(id, _)| *id == pane)?;
        let overlaps = |r: &Rect| match dir {
            Direction::Left | Direction::Right => r.y < from.y + from.h && r.y + r.h > from.y,
            Direction::Up | Direction::Down => r.x < from.x + from.w && r.x + r.w > from.x,
        };
        let ahead = |r: &Rect| match dir {
            Direction::Left => r.x + r.w <= from.x + 0.5,
            Direction::Right => r.x >= from.x + from.w - 0.5,
            Direction::Up => r.y + r.h <= from.y + 0.5,
            Direction::Down => r.y >= from.y + from.h - 0.5,
        };
        // Nearest along the direction first, then the top-most or left-most of equally near panes.
        let key = |r: &Rect| match dir {
            Direction::Left => (from.x - (r.x + r.w), r.y),
            Direction::Right => (r.x - (from.x + from.w), r.y),
            Direction::Up => (from.y - (r.y + r.h), r.x),
            Direction::Down => (r.y - (from.y + from.h), r.x),
        };
        rects
            .iter()
            .filter(|(id, r)| *id != pane && overlaps(r) && ahead(r))
            .min_by(|(_, a), (_, b)| {
                let (ka, kb) = (key(a), key(b));
                ka.0.total_cmp(&kb.0).then(ka.1.total_cmp(&kb.1))
            })
            .map(|(id, _)| *id)
    }

    /// The ratio of the split at `path`, or `None` if no split is there.
    pub fn ratio_at(&self, path: &[bool]) -> Option<f32> {
        let mut node = &self.tree;
        for &second in path {
            let Node::Split {
                first, second: s, ..
            } = node
            else {
                return None;
            };
            node = if second { s } else { first };
        }
        match node {
            Node::Split { ratio, .. } => Some(*ratio),
            Node::Leaf(_) => None,
        }
    }

    /// Sets the ratio of the split at `path`, clamped so neither side drops below `MIN_PANE`.
    pub fn set_ratio(&mut self, path: &[bool], ratio: f32, split_len: f32) {
        let mut node = &mut self.tree;
        for &second in path {
            let Node::Split {
                first, second: s, ..
            } = node
            else {
                return;
            };
            node = if second { s } else { first };
        }
        if let Node::Split { ratio: r, .. } = node {
            *r = clamp_ratio(ratio, split_len);
        }
    }
}

fn dummy() -> Pane {
    Pane {
        id: PaneId(0),
        items: Vec::new(),
        active: 0,
    }
}

pub fn clamp_ratio(ratio: f32, len: f32) -> f32 {
    if len < 2. * MIN_PANE + DIVIDER {
        return 0.5;
    }
    let min = MIN_PANE / len;
    ratio.clamp(min, 1. - min)
}

fn split_point(len: f32, ratio: f32) -> f32 {
    (clamp_ratio(ratio, len) * (len - DIVIDER)).floor().max(0.)
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: Rect = Rect {
        x: 0.,
        y: 0.,
        w: 1000.,
        h: 600.,
    };

    fn term() -> ItemKind {
        ItemKind::Terminal { session: None }
    }

    fn ids(l: &Layout) -> Vec<u64> {
        l.panes().iter().map(|p| p.id.0).collect()
    }

    #[test]
    fn a_tab_that_follows_the_wrap_default_writes_no_wrap_field() {
        let state = ViewState::default();
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("wrap"), "{json}");
        let chosen = ViewState {
            wrap: Some(false),
            ..ViewState::default()
        };
        let json = serde_json::to_string(&chosen).unwrap();
        assert_eq!(serde_json::from_str::<ViewState>(&json).unwrap(), chosen);
    }

    #[test]
    fn split_puts_new_pane_after_and_focuses_it() {
        let mut l = Layout::new(term());
        let p1 = l.focused;
        let p2 = l.split(p1, Axis::Horizontal, term()).unwrap();
        assert_eq!(l.focused, p2);
        assert_eq!(ids(&l), vec![p1.0, p2.0]);
        let (rects, dividers) = l.layout(B);
        assert_eq!(
            rects[0].1,
            Rect {
                x: 0.,
                y: 0.,
                w: 499.,
                h: 600.
            }
        );
        assert_eq!(
            rects[1].1,
            Rect {
                x: 500.,
                y: 0.,
                w: 500.,
                h: 600.
            }
        );
        assert_eq!(dividers.len(), 1);
    }

    #[test]
    fn closing_a_pane_promotes_its_sibling() {
        let mut l = Layout::new(term());
        let p1 = l.focused;
        let p2 = l.split(p1, Axis::Horizontal, term()).unwrap();
        let p3 = l.split(p2, Axis::Vertical, term()).unwrap();
        assert!(l.close_pane(p2));
        assert_eq!(ids(&l), vec![p1.0, p3.0]);
        assert!(matches!(
            &l.tree,
            Node::Split {
                axis: Axis::Horizontal,
                ..
            }
        ));
        assert!(l.close_pane(p3));
        assert!(matches!(&l.tree, Node::Leaf(p) if p.id == p1));
        assert_eq!(l.focused, p1);
        assert!(
            !l.close_pane(p1),
            "the last pane cannot be removed from the tree"
        );
    }

    #[test]
    fn closing_last_item_closes_pane() {
        let mut l = Layout::new(term());
        let p1 = l.focused;
        let p2 = l.split(p1, Axis::Horizontal, term()).unwrap();
        let only = l.pane(p2).unwrap().items[0].id;
        let extra = l.add_item(p1, term()).unwrap();
        assert_eq!(l.pane(p1).unwrap().active, 1);
        assert!(l.close_item(extra));
        assert_eq!(l.pane(p1).unwrap().items.len(), 1);
        assert!(l.close_item(only));
        assert_eq!(ids(&l), vec![p1.0]);
        let last = l.pane(p1).unwrap().items[0].id;
        assert!(!l.close_item(last));
    }

    #[test]
    fn neighbors_follow_geometry() {
        let mut l = Layout::new(term());
        let left = l.focused;
        let right = l.split(left, Axis::Horizontal, term()).unwrap();
        let bottom_right = l.split(right, Axis::Vertical, term()).unwrap();
        assert_eq!(l.neighbor(B, left, Direction::Right), Some(right));
        assert_eq!(l.neighbor(B, bottom_right, Direction::Up), Some(right));
        assert_eq!(l.neighbor(B, bottom_right, Direction::Left), Some(left));
        assert_eq!(l.neighbor(B, left, Direction::Left), None);
    }

    #[test]
    fn ratio_keeps_minimum_pane_size() {
        let mut l = Layout::new(term());
        let p1 = l.focused;
        l.split(p1, Axis::Horizontal, term());
        l.set_ratio(&[], 0.01, 1000.);
        let (rects, _) = l.layout(B);
        assert!(rects[0].1.w >= MIN_PANE - 1.);
        l.set_ratio(&[], 0.99, 1000.);
        let (rects, _) = l.layout(B);
        assert!(rects[1].1.w >= MIN_PANE - 1.);
        assert_eq!(clamp_ratio(0.9, 200.), 0.5);
    }

    #[test]
    fn ratio_at_reads_the_split_on_the_path() {
        let mut l = Layout::new(term());
        let p1 = l.focused;
        let p2 = l.split(p1, Axis::Horizontal, term()).unwrap();
        l.split(p2, Axis::Vertical, term());
        l.set_ratio(&[true], 0.3, 600.);
        assert_eq!(l.ratio_at(&[]), Some(0.5));
        assert_eq!(l.ratio_at(&[true]), Some(0.3));
        assert_eq!(l.ratio_at(&[false]), None, "a pane has no ratio");
        assert_eq!(l.ratio_at(&[true, true, false]), None);
    }

    fn item_ids(l: &Layout, pane: PaneId) -> Vec<u64> {
        l.pane(pane).unwrap().items.iter().map(|i| i.id.0).collect()
    }

    #[test]
    fn move_item_keeps_item_id_and_collapses_empty_pane() {
        let mut l = Layout::new(term());
        let p1 = l.focused;
        let p2 = l.split(p1, Axis::Horizontal, term()).unwrap();
        let moving = l.pane(p2).unwrap().items[0].clone();
        assert!(l.move_item(moving.id, p1, 99));
        assert_eq!(ids(&l), vec![p1.0], "the emptied pane closes");
        let p = l.pane(p1).unwrap();
        assert_eq!(p.items.last(), Some(&moving), "same id, same kind");
        assert_eq!(p.active, p.items.len() - 1);
        assert_eq!(l.focused, p1);
    }

    #[test]
    fn move_item_into_same_pane_reorders() {
        let mut l = Layout::new(term());
        let p1 = l.focused;
        let a = l.pane(p1).unwrap().items[0].id;
        let b = l.add_item(p1, term()).unwrap();
        let c = l.add_item(p1, term()).unwrap();
        assert!(l.move_item(a, p1, 2));
        assert_eq!(item_ids(&l, p1), vec![b.0, c.0, a.0]);
        assert_eq!(l.pane(p1).unwrap().active, 2);
        assert!(l.move_item(a, p1, 0));
        assert_eq!(item_ids(&l, p1), vec![a.0, b.0, c.0]);
        assert!(
            !l.move_item(a, p1, 0),
            "dropping a tab on itself does nothing"
        );
    }

    #[test]
    fn move_item_keeps_the_source_panes_active_tab() {
        let mut l = Layout::new(term());
        let p1 = l.focused;
        let a = l.pane(p1).unwrap().items[0].id;
        let b = l.add_item(p1, term()).unwrap();
        let c = l.add_item(p1, term()).unwrap();
        let p2 = l.split(p1, Axis::Horizontal, term()).unwrap();
        assert_eq!(l.pane(p1).unwrap().active, 2);
        assert!(l.move_item(a, p2, 0));
        assert_eq!(item_ids(&l, p1), vec![b.0, c.0]);
        assert_eq!(l.pane(p1).unwrap().active, 1, "c stays active");
        assert_eq!(l.pane(p2).unwrap().items[0].id, a);
        assert_eq!(l.pane(p2).unwrap().active, 0);
    }

    #[test]
    fn split_with_item_places_first_when_asked() {
        let mut l = Layout::new(term());
        let p1 = l.focused;
        let a = l.pane(p1).unwrap().items[0].id;
        let b = l.add_item(p1, term()).unwrap();
        let p2 = l.split_with_item(p1, Axis::Horizontal, b, true).unwrap();
        assert_eq!(ids(&l), vec![p2.0, p1.0]);
        assert_eq!(item_ids(&l, p2), vec![b.0]);
        assert_eq!(item_ids(&l, p1), vec![a.0]);
        assert_eq!(l.focused, p2);
        let p3 = l.split_with_item(p1, Axis::Vertical, b, false).unwrap();
        assert_eq!(ids(&l), vec![p1.0, p3.0], "b's old pane closed");
        assert_eq!(item_ids(&l, p3), vec![b.0]);
    }

    #[test]
    fn split_with_item_refuses_to_empty_its_own_pane() {
        let mut l = Layout::new(term());
        let p1 = l.focused;
        let only = l.pane(p1).unwrap().items[0].id;
        let before = l.clone();
        assert_eq!(l.split_with_item(p1, Axis::Horizontal, only, false), None);
        assert_eq!(l, before);
    }

    #[test]
    fn round_trips_through_json() {
        let mut l = Layout::new(ItemKind::Terminal { session: Some(42) });
        let p1 = l.focused;
        l.split(
            p1,
            Axis::Vertical,
            ItemKind::Editor {
                path: "/x/main.go".into(),
            },
        );
        l.add_item(
            l.focused,
            ItemKind::Image {
                path: "/x/logo.png".into(),
            },
        );
        l.add_item(
            l.focused,
            ItemKind::Rendered {
                path: "/x/README.md".into(),
            },
        );
        let json = serde_json::to_string(&l).unwrap();
        assert_eq!(serde_json::from_str::<Layout>(&json).unwrap(), l);
    }
}
