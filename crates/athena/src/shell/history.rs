use std::path::PathBuf;

use athena_workspace::ItemId;
use gpui::{Context, Window};

use super::Shell;
use super::item::ItemView;

/// Places kept for going back; VS Code keeps a similar bounded list.
const CAP: usize = 100;

/// A tab to come back to and, for editors, the zero-based cursor line and column.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Loc {
    pub root: PathBuf,
    pub item: ItemId,
    pub cursor: Option<(u32, u32)>,
}

/// Back and forward lists of places, like a browser's.
#[derive(Default)]
pub(super) struct History {
    back: Vec<Loc>,
    forward: Vec<Loc>,
    /// Set while a step is being taken, so the tab switch it makes is not recorded.
    pub navigating: bool,
}

impl History {
    /// Remembers where the user was just before moving elsewhere; that move forgets the forward list.
    pub fn record(&mut self, loc: Loc) {
        if self.navigating || self.back.last() == Some(&loc) {
            return;
        }
        push_capped(&mut self.back, loc);
        self.forward.clear();
    }

    /// Drops places in a project whose tab ids are about to be reused.
    pub fn forget_root(&mut self, root: &std::path::Path) {
        self.back.retain(|l| l.root != root);
        self.forward.retain(|l| l.root != root);
    }

    /// Steps from `current`, skipping places that are gone or that are where the user already is.
    pub fn step(
        &mut self,
        forward: bool,
        current: Option<Loc>,
        exists: impl Fn(&Loc) -> bool,
    ) -> Option<Loc> {
        let (from, to) = match forward {
            true => (&mut self.forward, &mut self.back),
            false => (&mut self.back, &mut self.forward),
        };
        while let Some(loc) = from.pop() {
            if !exists(&loc) || current.as_ref() == Some(&loc) {
                continue;
            }
            if let Some(current) = current {
                push_capped(to, current);
            }
            return Some(loc);
        }
        None
    }
}

fn push_capped(list: &mut Vec<Loc>, loc: Loc) {
    list.push(loc);
    if list.len() > CAP {
        list.remove(0);
    }
}

impl Shell {
    fn current_loc(&self, cx: &Context<Self>) -> Option<Loc> {
        let project = self.workspace.active_project()?;
        let item = project.layout.as_ref()?.focused_pane()?.active_item()?;
        let cursor = match self.items.get(&(project.root.clone(), item.id)) {
            Some(ItemView::Editor(editor)) => editor
                .read(cx)
                .cursor()
                .map(|(line, col, _)| (line.saturating_sub(1), col.saturating_sub(1))),
            _ => None,
        };
        Some(Loc {
            root: project.root.clone(),
            item: item.id,
            cursor,
        })
    }

    /// Call before switching tabs or jumping, so Back returns here.
    pub(super) fn record_location(&mut self, cx: &Context<Self>) {
        if let Some(loc) = self.current_loc(cx) {
            self.history.record(loc);
        }
    }

    /// Mouse back/forward and ⌃- / ⌃⇧-: a focused browser preview walks its own history instead.
    pub(super) fn navigate(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ItemView::Preview(preview)) = self.focused_item_view() {
            let preview = preview.read(cx);
            match forward {
                true => preview.forward(),
                false => preview.back(),
            }
            return;
        }
        let current = self.current_loc(cx);
        let workspace = &self.workspace;
        let exists = |loc: &Loc| {
            workspace.projects.iter().any(|p| {
                p.root == loc.root
                    && p.layout
                        .as_ref()
                        .is_some_and(|l| l.find_item(loc.item).is_some())
            })
        };
        let Some(loc) = self.history.step(forward, current, exists) else {
            return;
        };
        self.history.navigating = true;
        if let Some(index) = self
            .workspace
            .projects
            .iter()
            .position(|p| p.root == loc.root)
        {
            self.switch_to(index, cx);
        }
        let found = self
            .workspace
            .active_project()
            .and_then(|p| p.layout.as_ref())
            .and_then(|l| l.find_item(loc.item));
        if let Some((pane, index)) = found {
            self.zoomed = None;
            self.activate_tab(pane, index, window, cx);
        }
        if let Some((line, col)) = loc.cursor
            && let Some(ItemView::Editor(editor)) = self.items.get(&(loc.root, loc.item))
        {
            editor.update(cx, |e, cx| e.go_to_position(line, col, cx));
        }
        self.history.navigating = false;
    }

    fn focused_item_view(&self) -> Option<ItemView> {
        let project = self.workspace.active_project()?;
        let item = project.layout.as_ref()?.focused_pane()?.active_item()?;
        self.items.get(&(project.root.clone(), item.id)).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(item: u64) -> Loc {
        Loc {
            root: "/p".into(),
            item: ItemId(item),
            cursor: None,
        }
    }

    #[test]
    fn back_then_forward_returns_to_where_it_started() {
        let mut h = History::default();
        h.record(at(1));
        h.record(at(2));
        assert_eq!(h.step(false, Some(at(3)), |_| true), Some(at(2)));
        assert_eq!(h.step(false, Some(at(2)), |_| true), Some(at(1)));
        assert_eq!(h.step(false, Some(at(1)), |_| true), None);
        assert_eq!(h.step(true, Some(at(1)), |_| true), Some(at(2)));
        assert_eq!(h.step(true, Some(at(2)), |_| true), Some(at(3)));
        assert_eq!(h.step(true, Some(at(3)), |_| true), None);
    }

    #[test]
    fn a_new_move_forgets_the_forward_list() {
        let mut h = History::default();
        h.record(at(1));
        h.step(false, Some(at(2)), |_| true);
        h.record(at(1));
        assert_eq!(h.step(true, Some(at(5)), |_| true), None);
    }

    #[test]
    fn closed_tabs_and_the_current_place_are_skipped() {
        let mut h = History::default();
        h.record(at(1));
        h.record(at(2));
        h.record(at(3));
        assert_eq!(
            h.step(false, Some(at(3)), |l| l.item != ItemId(2)),
            Some(at(1))
        );
    }

    #[test]
    fn repeats_are_kept_once_and_the_list_is_bounded() {
        let mut h = History::default();
        h.record(at(1));
        h.record(at(1));
        assert_eq!(h.back.len(), 1);
        for i in 0..250 {
            h.record(at(i));
        }
        assert_eq!(h.back.len(), CAP);
        assert_eq!(h.back[0], at(150));
    }

    #[test]
    fn forgetting_a_project_drops_only_its_places() {
        let mut h = History::default();
        h.record(at(1));
        h.record(Loc {
            root: "/q".into(),
            ..at(1)
        });
        h.forget_root(std::path::Path::new("/p"));
        assert_eq!(h.back.len(), 1);
        assert_eq!(h.back[0].root, PathBuf::from("/q"));
    }

    #[test]
    fn nothing_is_recorded_while_navigating() {
        let mut h = History {
            navigating: true,
            ..History::default()
        };
        h.record(at(1));
        assert!(h.back.is_empty());
    }

    #[test]
    fn the_same_tab_at_another_line_is_a_separate_place() {
        let mut h = History::default();
        let mut moved = at(1);
        moved.cursor = Some((40, 0));
        h.record(at(1));
        h.record(moved.clone());
        assert_eq!(h.step(false, Some(at(2)), |_| true), Some(moved));
    }
}
