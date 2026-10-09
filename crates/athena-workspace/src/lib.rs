pub mod gh;
pub mod git;
mod layout;
mod persist;
mod project;
mod scope;
pub mod watch;

pub use layout::{
    Axis, DiffBase, Direction, Divider, Item, ItemId, ItemKind, Layout, MIN_PANE, Node, NodePath,
    Pane, PaneId, Rect, ViewState,
};
pub use persist::{is_corrupt, load, save, set_aside};
pub use project::{LinterTrust, PANEL_IDS, Panel, Project, git_branch};
pub use scope::{denied, resolve_in_roots};

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
    /// Cmd+S formats through the language server first; `None` does so for Go only.
    #[serde(default)]
    pub format_on_save: Option<bool>,
    /// Long lines wrap in editor tabs that have not chosen; Markdown wraps either way.
    #[serde(default)]
    pub word_wrap: bool,
    /// Claude Code may connect to Athena as its IDE, to show proposed edits and read diagnostics.
    #[serde(default)]
    pub ide_integration: bool,
    #[serde(default)]
    pub ui: UiState,
    /// Roots of closed projects, most recently closed first, for Open Recent.
    #[serde(default)]
    pub recent: Vec<PathBuf>,
    #[serde(default)]
    pub theme: ThemeChoice,
}

/// The workspace fields settings.json may set; workspace.json keeps them as the fallback.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Preferences {
    pub autosave_delay_ms: u64,
    pub format_on_save: Option<bool>,
    pub word_wrap: bool,
    pub ide_integration: bool,
    pub theme: ThemeChoice,
}

/// Which colour theme to show; `System` follows macOS's light or dark appearance.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

/// Open Recent keeps this many folders, as VS Code does by default.
pub const MAX_RECENT: usize = 20;

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
    /// Editor and terminal font size, in 1px steps from the default.
    pub font_zoom: i32,
    /// Interface size, in 10% steps from the default.
    pub zoom_level: i32,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            tree_width: 240.,
            drawer_height: 240.,
            tree_visible: true,
            font_zoom: 0,
            zoom_level: 0,
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
            format_on_save: None,
            word_wrap: false,
            ide_integration: false,
            ui: UiState::default(),
            recent: Vec::new(),
            theme: ThemeChoice::System,
        }
    }
}

impl Workspace {
    pub fn preferences(&self) -> Preferences {
        Preferences {
            autosave_delay_ms: self.autosave_delay_ms,
            format_on_save: self.format_on_save,
            word_wrap: self.word_wrap,
            ide_integration: self.ide_integration,
            theme: self.theme,
        }
    }

    pub fn set_preferences(&mut self, p: Preferences) {
        self.autosave_delay_ms = p.autosave_delay_ms;
        self.format_on_save = p.format_on_save;
        self.word_wrap = p.word_wrap;
        self.ide_integration = p.ide_integration;
        self.theme = p.theme;
    }

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
                self.projects.push(Project::new(root.clone()));
                self.projects.len() - 1
            }
        };
        self.recent.retain(|r| canonical(r) != root);
        self.active = Some(index);
        index
    }

    pub fn close_project(&mut self, index: usize) {
        if index >= self.projects.len() {
            return;
        }
        let closed = self.projects.remove(index).root;
        self.recent.retain(|r| *r != closed);
        self.recent.insert(0, closed);
        self.recent.truncate(MAX_RECENT);
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
        let active = self.active;
        let mut kept: Vec<Project> = Vec::new();
        let mut kept_active = None;
        for (i, mut project) in std::mem::take(&mut self.projects).into_iter().enumerate() {
            if !project.root.is_dir() {
                continue;
            }
            let root = canonical(&project.root);
            if root != project.root {
                if let Some(layout) = project.layout.as_mut() {
                    rebase_files(layout, &project.root, &root);
                }
                project.root = root;
            }
            match kept.iter().position(|k| k.root == project.root) {
                Some(j) => {
                    kept[j].linters = kept[j].linters.strictest(project.linters);
                    // The active copy's tabs are the ones the user was looking at.
                    if active == Some(i) {
                        if project.layout.is_some() {
                            kept[j].layout = project.layout;
                        }
                        if !project.panel.is_empty() {
                            kept[j].panel = project.panel;
                        }
                        kept_active = Some(j);
                    }
                }
                None => {
                    if active == Some(i) {
                        kept_active = Some(kept.len());
                    }
                    kept.push(project);
                }
            }
        }
        self.projects = kept;
        self.active = kept_active.or(if self.projects.is_empty() {
            None
        } else {
            Some(0)
        });
    }
}

/// Re-spells tab paths under `from` as paths under `to`.
fn rebase_files(layout: &mut Layout, from: &Path, to: &Path) {
    let ids: Vec<ItemId> = layout.items().map(|i| i.id).collect();
    for id in ids {
        let Some(item) = layout.item_mut(id) else {
            continue;
        };
        if let ItemKind::Editor { path }
        | ItemKind::Image { path }
        | ItemKind::Rendered { path }
        | ItemKind::Diff { path, .. } = &mut item.kind
            && let Ok(rest) = path.strip_prefix(from)
        {
            *path = to.join(rest);
        }
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
        let link_file = link.join("lib.rs");
        let mut w = Workspace::default();
        w.projects.push(Project::new(link));
        w.projects.push(Project::new(real.clone()));
        w.active = Some(1);
        w.projects[1].layout = Some(Layout::new(ItemKind::Editor {
            path: real.join("main.rs"),
        }));
        w.projects[0].layout = Some(Layout::new(ItemKind::Editor {
            path: link_file.clone(),
        }));
        let terminal = |session| Item {
            id: ItemId(0),
            kind: ItemKind::Terminal { session },
            view: None,
        };
        w.projects[0].panel.adopt(terminal(Some(6)));
        w.projects[1].panel.adopt(terminal(Some(5)));
        w.prune_missing();
        assert_eq!(w.projects.len(), 1);
        assert_eq!(w.projects[0].root, real);
        assert_eq!(w.active, Some(0));
        let kept = w.projects[0].layout.as_ref().unwrap();
        assert_eq!(
            kept.items().next().unwrap().kind.file(),
            Some(&real.join("main.rs")),
            "the active copy's tabs win"
        );
        let panel: Vec<_> = w.projects[0]
            .panel
            .terminals
            .iter()
            .map(|i| &i.kind)
            .collect();
        assert_eq!(
            panel,
            [&ItemKind::Terminal { session: Some(5) }],
            "and so do its panel terminals"
        );
        std::fs::remove_dir_all(real.parent().unwrap()).unwrap();
    }

    #[test]
    fn merged_roots_keep_the_most_cautious_linter_answer() {
        use crate::LinterTrust::{Allowed, Denied, NotAsked};
        for (answers, kept) in [
            ([Allowed, Denied, NotAsked], Denied),
            ([NotAsked, Allowed, NotAsked], Allowed),
            ([NotAsked, NotAsked, NotAsked], NotAsked),
        ] {
            let (real, link) = linked_dirs("linters");
            let mut w = Workspace::default();
            for (root, answer) in [link.clone(), real.clone(), link].into_iter().zip(answers) {
                let mut project = Project::new(root);
                project.linters = answer;
                w.projects.push(project);
            }
            w.active = Some(0);
            w.prune_missing();
            assert_eq!(w.projects.len(), 1);
            assert_eq!(w.projects[0].linters, kept, "{answers:?}");
            std::fs::remove_dir_all(real.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn loading_respells_tab_paths_under_a_symlinked_root() {
        let (real, link) = linked_dirs("respell");
        let mut w = Workspace::default();
        let mut project = Project::new(link.clone());
        project.layout = Some(Layout::new(ItemKind::Editor {
            path: link.join("src/lib.rs"),
        }));
        w.projects.push(project);
        w.active = Some(0);
        w.prune_missing();
        let layout = w.projects[0].layout.as_ref().unwrap();
        assert_eq!(
            layout.items().next().unwrap().kind.file(),
            Some(&real.join("src/lib.rs"))
        );
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
        assert_eq!(w.format_on_save, None);
        assert!(!w.word_wrap);
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
    fn closed_projects_become_recent_newest_first_and_reopening_removes_them() {
        let mut w = ws(&["/n/a", "/n/b", "/n/c"]);
        w.close_project(0);
        w.close_project(0);
        assert_eq!(w.recent, [PathBuf::from("/n/b"), "/n/a".into()]);
        w.add_project("/n/a".into());
        assert_eq!(w.recent, [PathBuf::from("/n/b")]);
        let a = w.projects.iter().position(|p| p.root == Path::new("/n/a"));
        w.close_project(a.unwrap());
        assert_eq!(w.recent, [PathBuf::from("/n/a"), "/n/b".into()]);
    }

    #[test]
    fn recent_projects_are_capped_without_duplicates() {
        let mut w = Workspace::default();
        for i in 0..MAX_RECENT + 5 {
            w.add_project(PathBuf::from(format!("/n/p{i}")));
            w.close_project(0);
        }
        w.add_project("/n/p24".into());
        w.close_project(0);
        assert_eq!(w.recent.len(), MAX_RECENT);
        assert_eq!(w.recent[0], PathBuf::from("/n/p24"));
        assert_eq!(w.recent.iter().filter(|r| r.ends_with("p24")).count(), 1);
        let old: Workspace =
            serde_json::from_str(r#"{"projects":[],"active":null,"window":null}"#).unwrap();
        assert!(old.recent.is_empty());
    }

    #[test]
    fn the_theme_follows_macos_unless_one_was_chosen() {
        let old: Workspace =
            serde_json::from_str(r#"{"projects":[],"active":null,"window":null}"#).unwrap();
        assert_eq!(old.theme, ThemeChoice::System);
        let light: Workspace =
            serde_json::from_str(r#"{"projects":[],"active":null,"window":null,"theme":"Light"}"#)
                .unwrap();
        assert_eq!(light.theme, ThemeChoice::Light);
    }

    #[test]
    fn preferences_round_trip_through_the_workspace() {
        let mut w = Workspace::default();
        let mut p = w.preferences();
        p.word_wrap = true;
        p.format_on_save = Some(false);
        p.theme = ThemeChoice::Dark;
        w.set_preferences(p);
        assert_eq!(w.preferences(), p);
        assert!(w.word_wrap);
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
