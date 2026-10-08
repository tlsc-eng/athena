mod layout;
mod persist;
mod project;
mod scope;

pub use layout::{
    Axis, Direction, Divider, Item, ItemId, ItemKind, Layout, MIN_PANE, Node, NodePath, Pane,
    PaneId, Rect,
};
pub use persist::{load, save};
pub use project::{Project, git_branch};
pub use scope::{denied, resolve_in_roots};

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Workspace {
    pub projects: Vec<Project>,
    pub active: Option<usize>,
    pub window: Option<WindowState>,
    /// The user agreed to show Claude plan usage, which reads Claude Code's Keychain sign-in.
    #[serde(default)]
    pub usage_indicator: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct WindowState {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub mode: WindowMode,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowMode {
    Windowed,
    Maximized,
    Fullscreen,
}

impl Workspace {
    /// Adds `root` (or focuses it if already open) and returns its index.
    pub fn add_project(&mut self, root: PathBuf) -> usize {
        let root = root.canonicalize().unwrap_or(root);
        let index = match self.projects.iter().position(|p| p.root == root) {
            Some(i) => i,
            None => {
                self.projects.push(Project::new(root));
                self.projects.len() - 1
            }
        };
        self.active = Some(index);
        index
    }

    pub fn close_project(&mut self, index: usize) {
        if index >= self.projects.len() {
            return;
        }
        self.projects.remove(index);
        self.active = match self.active {
            _ if self.projects.is_empty() => None,
            Some(a) if a > index => Some(a - 1),
            Some(a) if a == index => Some(index.min(self.projects.len() - 1)),
            other => other,
        };
    }

    pub fn activate(&mut self, index: usize) {
        if index < self.projects.len() {
            self.active = Some(index);
        }
    }

    pub fn active_project(&self) -> Option<&Project> {
        self.active.and_then(|i| self.projects.get(i))
    }

    /// Rail labels; projects whose monograms collide get the first letter where their names differ.
    pub fn monograms(&self) -> Vec<String> {
        let base: Vec<String> = self.projects.iter().map(Project::monogram).collect();
        let names: Vec<Vec<char>> = self
            .projects
            .iter()
            .map(|p| p.name().to_uppercase().chars().collect())
            .collect();
        base.iter()
            .enumerate()
            .map(|(i, mono)| {
                let rivals: Vec<usize> = (0..base.len())
                    .filter(|&j| j != i && base[j] == *mono)
                    .collect();
                if rivals.is_empty() {
                    return mono.clone();
                }
                let name = &names[i];
                let split = rivals
                    .iter()
                    .map(|&j| {
                        name.iter()
                            .zip(&names[j])
                            .take_while(|(a, b)| a == b)
                            .count()
                    })
                    .max()
                    .unwrap_or(1)
                    .max(1);
                match name.get(split) {
                    Some(c) => format!("{}{c}", name[0]),
                    None => mono.clone(),
                }
            })
            .collect()
    }

    /// Drops projects whose folder no longer exists, keeping the active one if it survives.
    pub fn prune_missing(&mut self) {
        let active_root = self.active_project().map(|p| p.root.clone());
        self.projects.retain(|p| Path::new(&p.root).is_dir());
        self.active = active_root
            .and_then(|r| self.projects.iter().position(|p| p.root == r))
            .or(if self.projects.is_empty() {
                None
            } else {
                Some(0)
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(roots: &[&str]) -> Workspace {
        let mut w = Workspace::default();
        for r in roots {
            w.add_project(PathBuf::from(r));
        }
        w
    }

    #[test]
    fn add_existing_focuses_instead_of_duplicating() {
        let mut w = ws(&["/nonexistent/a", "/nonexistent/b"]);
        assert_eq!(w.add_project("/nonexistent/a".into()), 0);
        assert_eq!(w.projects.len(), 2);
        assert_eq!(w.active, Some(0));
    }

    #[test]
    fn close_keeps_active_pointing_at_same_project() {
        let mut w = ws(&["/n/a", "/n/b", "/n/c"]);
        w.activate(2);
        w.close_project(0);
        assert_eq!(w.active_project().unwrap().root, PathBuf::from("/n/c"));
    }

    #[test]
    fn colliding_monograms_split_where_names_differ() {
        let w = ws(&["/n/hephaestus", "/n/hestia", "/n/athena"]);
        assert_eq!(w.monograms(), ["HP", "HS", "AT"]);
    }

    #[test]
    fn close_active_moves_to_neighbour() {
        let mut w = ws(&["/n/a", "/n/b"]);
        w.activate(1);
        w.close_project(1);
        assert_eq!(w.active, Some(0));
        w.close_project(0);
        assert_eq!(w.active, None);
    }
}
