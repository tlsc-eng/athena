pub mod git;
mod layout;
mod persist;
mod project;
mod scope;
pub mod watch;

pub use layout::{
    Axis, Direction, Divider, Item, ItemId, ItemKind, Layout, MIN_PANE, Node, NodePath, Pane,
    PaneId, Rect,
};
pub use persist::{load, save};
pub use project::{Project, git_branch};
pub use scope::{denied, resolve_in_roots};

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Workspace {
    pub projects: Vec<Project>,
    pub active: Option<usize>,
    pub window: Option<WindowState>,
    /// The user agreed to show Claude plan usage, which reads Claude Code's Keychain sign-in.
    #[serde(default)]
    pub usage_indicator: bool,
    /// Editors save this long after the last keystroke; 0 turns auto save off.
    #[serde(default = "default_autosave_delay_ms")]
    pub autosave_delay_ms: u64,
    #[serde(default)]
    pub ui: UiState,
}

pub const DEFAULT_AUTOSAVE_DELAY_MS: u64 = 1000;

fn default_autosave_delay_ms() -> u64 {
    DEFAULT_AUTOSAVE_DELAY_MS
}

/// Panel sizes and visibility, kept off `WindowState` because bounds changes overwrite that wholesale.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(default)]
pub struct UiState {
    pub tree_width: f32,
    pub drawer_height: f32,
    pub tree_visible: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            tree_width: 240.,
            drawer_height: 240.,
            tree_visible: true,
        }
    }
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

impl Default for Workspace {
    fn default() -> Self {
        Self {
            projects: Vec::new(),
            active: None,
            window: None,
            usage_indicator: false,
            autosave_delay_ms: DEFAULT_AUTOSAVE_DELAY_MS,
            ui: UiState::default(),
        }
    }
}

impl Workspace {
    /// Adds `root` (or focuses it if already open) and returns its index.
    pub fn add_project(&mut self, root: PathBuf) -> usize {
        let root = canonical(&root);
        // Roots saved by older builds may be spelled through a symlink such as /tmp.
        let index = match self
            .projects
            .iter()
            .position(|p| canonical(&p.root) == root)
        {
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

    /// Drops projects whose folder no longer exists and spells roots canonically, merging projects
    /// that turn out to be the same folder; keeps the active one if it survives.
    pub fn prune_missing(&mut self) {
        let active_root = self.active_project().map(|p| canonical(&p.root));
        self.projects.retain(|p| Path::new(&p.root).is_dir());
        let mut seen = HashSet::new();
        self.projects.retain_mut(|p| {
            p.root = canonical(&p.root);
            seen.insert(p.root.clone())
        });
        self.active = active_root
            .and_then(|r| self.projects.iter().position(|p| p.root == r))
            .or(if self.projects.is_empty() {
                None
            } else {
                Some(0)
            });
    }
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real folder and a symlink to it, like /private/tmp and /tmp.
    fn linked_dirs(test: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("athena-ws-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        (real.canonicalize().unwrap(), link)
    }

    #[test]
    fn a_folder_reached_through_a_symlink_is_the_same_project() {
        let (real, link) = linked_dirs("add");
        let mut w = Workspace::default();
        w.projects.push(Project::new(link.clone()));
        assert_eq!(w.add_project(real.clone()), 0);
        assert_eq!(w.add_project(link), 0);
        assert_eq!(w.projects.len(), 1);
        std::fs::remove_dir_all(real.parent().unwrap()).unwrap();
    }

    #[test]
    fn loading_merges_roots_that_name_the_same_folder() {
        let (real, link) = linked_dirs("prune");
        let mut w = Workspace::default();
        w.projects.push(Project::new(link));
        w.projects.push(Project::new(real.clone()));
        w.active = Some(1);
        w.prune_missing();
        assert_eq!(w.projects.len(), 1);
        assert_eq!(w.projects[0].root, real);
        assert_eq!(w.active, Some(0));
        std::fs::remove_dir_all(real.parent().unwrap()).unwrap();
    }

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
    fn files_without_autosave_get_the_default() {
        let old = r#"{"projects":[],"active":null,"window":null}"#;
        let w: Workspace = serde_json::from_str(old).unwrap();
        assert_eq!(w.autosave_delay_ms, 1000);
        assert_eq!(Workspace::default().autosave_delay_ms, 1000);
        let off = r#"{"projects":[],"active":null,"window":null,"autosave_delay_ms":0}"#;
        assert_eq!(
            serde_json::from_str::<Workspace>(off)
                .unwrap()
                .autosave_delay_ms,
            0
        );
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
