use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{ItemKind, Project, UiState, WindowState, Workspace, canonical};

/// One window's projects, named by root, with its own bounds and panel sizes.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WindowEntry {
    pub roots: Vec<PathBuf>,
    #[serde(default)]
    pub active: Option<PathBuf>,
    #[serde(default)]
    pub window: Option<WindowState>,
    #[serde(default)]
    pub ui: UiState,
}

impl Workspace {
    /// One workspace per window, each with the app-wide fields, plus the parked projects.
    /// A project no window names goes to the first one, so nothing in the file is lost.
    pub fn into_windows(mut self) -> (Vec<Workspace>, Vec<Project>) {
        let entries = std::mem::take(&mut self.windows);
        let parked = std::mem::take(&mut self.parked);
        if entries.is_empty() {
            return (vec![self], parked);
        }
        let projects = std::mem::take(&mut self.projects);
        let roots: Vec<Vec<PathBuf>> = entries
            .iter()
            .map(|e| e.roots.iter().map(|r| canonical(r)).collect())
            .collect();
        let mut shares: Vec<Vec<Project>> = vec![Vec::new(); entries.len()];
        for project in projects {
            let root = canonical(&project.root);
            let at = roots.iter().position(|r| r.contains(&root)).unwrap_or(0);
            shares[at].push(project);
        }
        let mut windows: Vec<Workspace> = entries
            .iter()
            .zip(shares)
            .filter(|(_, share)| !share.is_empty())
            .map(|(entry, share)| self.window_of(entry, share))
            .collect();
        if windows.is_empty() {
            windows.push(self.window_of(&entries[0], Vec::new()));
        }
        (windows, parked)
    }

    fn window_of(&self, entry: &WindowEntry, projects: Vec<Project>) -> Workspace {
        let active = entry.active.as_deref().map(canonical);
        let index = projects
            .iter()
            .position(|p| Some(canonical(&p.root)) == active)
            .or((!projects.is_empty()).then_some(0));
        Workspace {
            projects,
            active: index,
            window: entry.window,
            ui: self.app_ui(entry.ui),
            windows: Vec::new(),
            parked: Vec::new(),
            recent: self.recent.clone(),
            ..self.app_fields()
        }
    }

    /// `ui` with the zoom levels taken from here, as they apply to every window.
    fn app_ui(&self, ui: UiState) -> UiState {
        UiState {
            font_zoom: self.ui.font_zoom,
            zoom_level: self.ui.zoom_level,
            ..ui
        }
    }

    /// The fields every window shares, with empty per-window ones, for struct update syntax.
    fn app_fields(&self) -> Workspace {
        Workspace {
            projects: Vec::new(),
            active: None,
            window: None,
            usage_indicator: self.usage_indicator,
            autosave_delay_ms: self.autosave_delay_ms,
            format_on_save: self.format_on_save,
            word_wrap: self.word_wrap,
            ide_integration: self.ide_integration,
            ui: self.ui,
            recent: Vec::new(),
            theme: self.theme,
            windows: Vec::new(),
            parked: Vec::new(),
        }
    }

    /// The file for several windows: app-wide fields from `app`, and the first window's projects
    /// first so the top-level `active`, `window` and `ui` describe it.
    pub fn join(app: &Workspace, windows: &[Workspace], parked: &[Project]) -> Workspace {
        let first = windows.first();
        let entries = match windows.len() {
            0 | 1 => Vec::new(),
            _ => windows
                .iter()
                .map(|w| WindowEntry {
                    roots: w.projects.iter().map(|p| p.root.clone()).collect(),
                    active: w.active_project().map(|p| p.root.clone()),
                    window: w.window,
                    ui: app.app_ui(w.ui),
                })
                .collect(),
        };
        Workspace {
            projects: windows.iter().flat_map(|w| w.projects.clone()).collect(),
            active: first.and_then(|w| w.active),
            window: first.and_then(|w| w.window),
            ui: app.app_ui(first.map_or(app.ui, |w| w.ui)),
            windows: entries,
            parked: parked.to_vec(),
            recent: app.recent.clone(),
            ..app.app_fields()
        }
    }
}

/// Removes and returns the parked project at `root`, so reopening it keeps its tabs and shells.
pub fn take_parked(parked: &mut Vec<Project>, root: &Path) -> Option<Project> {
    let root = canonical(root);
    let at = parked.iter().position(|p| canonical(&p.root) == root)?;
    Some(parked.remove(at))
}

/// Drops parked projects that left Open Recent, returning the shells they leave behind.
pub fn retain_parked(parked: &mut Vec<Project>, recent: &[PathBuf]) -> Vec<u64> {
    let mut orphaned = Vec::new();
    parked.retain(|p| {
        let kept = recent.contains(&p.root);
        if !kept {
            orphaned.extend(p.items().filter_map(|i| match i.kind {
                ItemKind::Terminal { session } => session,
                _ => None,
            }));
        }
        kept
    });
    orphaned
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Item, ItemId, Layout, WindowMode};

    fn bounds(x: f32) -> Option<WindowState> {
        Some(WindowState {
            x,
            y: 40.,
            width: 1200.,
            height: 800.,
            mode: WindowMode::Windowed,
        })
    }

    fn window(roots: &[&str], active: usize, x: f32) -> Workspace {
        let mut w = Workspace::default();
        for r in roots {
            w.projects.push(Project::new(PathBuf::from(r)));
        }
        w.active = (!roots.is_empty()).then_some(active);
        w.window = bounds(x);
        w
    }

    fn roots(w: &Workspace) -> Vec<&Path> {
        w.projects.iter().map(|p| p.root.as_path()).collect()
    }

    #[test]
    fn a_file_without_windows_opens_one_window_with_every_project() {
        let old = window(&["/n/a", "/n/b", "/n/c"], 2, 10.);
        let (windows, parked) = old.clone().into_windows();
        assert_eq!(windows, [old]);
        assert!(parked.is_empty());
    }

    #[test]
    fn one_window_saves_the_shape_older_builds_wrote() {
        let w = window(&["/n/a"], 0, 10.);
        let saved = Workspace::join(&w, std::slice::from_ref(&w), &[]);
        assert_eq!(saved, w);
        let json = serde_json::to_value(&saved).unwrap();
        assert!(json.get("windows").is_none());
        assert!(json.get("parked").is_none());
    }

    #[test]
    fn several_windows_round_trip_and_keep_every_project_at_the_top_level() {
        let mut app = window(&["/n/a", "/n/b"], 1, 10.);
        app.recent = vec![PathBuf::from("/n/old")];
        app.ui.font_zoom = 2;
        app.ui.tree_width = 300.;
        app.word_wrap = true;
        let mut second = window(&["/n/c", "/n/d"], 1, 500.);
        second.ui.tree_visible = false;
        let empty = window(&[], 0, 900.);
        let mut parked = Project::new(PathBuf::from("/n/old"));
        parked.panel.adopt(Item {
            id: ItemId(0),
            kind: ItemKind::Terminal { session: Some(7) },
            view: None,
        });
        let saved = Workspace::join(
            &app,
            &[app.clone(), second.clone(), empty.clone()],
            std::slice::from_ref(&parked),
        );

        assert_eq!(roots(&saved), ["/n/a", "/n/b", "/n/c", "/n/d"]);
        assert_eq!((saved.active, saved.window), (Some(1), bounds(10.)));
        assert_eq!(saved.ui.tree_width, 300.);

        let text = serde_json::to_string(&saved).unwrap();
        let (windows, kept) = serde_json::from_str::<Workspace>(&text)
            .unwrap()
            .into_windows();
        assert_eq!(kept, [parked]);
        assert_eq!(
            windows.len(),
            2,
            "a window with no projects is not restored"
        );
        assert_eq!(windows[0], app);
        assert_eq!(roots(&windows[1]), ["/n/c", "/n/d"]);
        assert_eq!(windows[1].active, Some(1));
        assert_eq!(windows[1].window, bounds(500.));
        assert!(!windows[1].ui.tree_visible);
        assert_eq!(windows[1].ui.font_zoom, 2, "zoom is app-wide");
        assert_eq!(windows[1].recent, app.recent);
        assert!(windows[1].word_wrap);
    }

    #[test]
    fn projects_no_window_names_go_to_the_first_and_stale_roots_are_ignored() {
        let mut w = window(&["/n/a", "/n/b", "/n/c"], 0, 10.);
        w.windows = vec![
            WindowEntry {
                roots: vec!["/n/gone".into()],
                active: Some("/n/gone".into()),
                window: bounds(10.),
                ui: UiState::default(),
            },
            WindowEntry {
                roots: vec!["/n/b".into(), "/n/b".into()],
                active: None,
                window: bounds(500.),
                ui: UiState::default(),
            },
            WindowEntry {
                roots: vec!["/n/b".into()],
                active: Some("/n/b".into()),
                window: bounds(900.),
                ui: UiState::default(),
            },
        ];
        let (windows, _) = w.into_windows();
        assert_eq!(windows.len(), 2);
        assert_eq!(roots(&windows[0]), ["/n/a", "/n/c"]);
        assert_eq!(windows[0].active, Some(0));
        assert_eq!(windows[0].window, bounds(10.));
        assert_eq!(
            roots(&windows[1]),
            ["/n/b"],
            "the first window naming a root wins"
        );
        assert_eq!(windows[1].window, bounds(500.));
    }

    #[test]
    fn a_file_whose_windows_name_nothing_still_opens_one_empty_window() {
        let w = Workspace {
            windows: vec![WindowEntry {
                roots: Vec::new(),
                active: None,
                window: bounds(30.),
                ui: UiState::default(),
            }],
            ..Workspace::default()
        };
        let (windows, _) = w.into_windows();
        assert_eq!(windows.len(), 1);
        assert!(windows[0].projects.is_empty());
        assert_eq!(windows[0].window, bounds(30.));
    }

    #[test]
    fn a_parked_project_comes_back_with_its_shells_until_it_leaves_open_recent() {
        let mut project = Project::new(PathBuf::from("/n/a"));
        project.layout = Some(Layout::new(ItemKind::Terminal { session: Some(3) }));
        let mut parked = vec![project.clone(), Project::new(PathBuf::from("/n/b"))];
        let back = take_parked(&mut parked, Path::new("/n/a")).unwrap();
        assert_eq!(back, project);
        assert_eq!(take_parked(&mut parked, Path::new("/n/a")), None);

        let mut w = window(&["/n/c"], 0, 10.);
        w.recent = vec![PathBuf::from("/n/a")];
        assert_eq!(w.adopt_project(back.clone()), 1);
        assert_eq!(w.projects[1], back);
        assert!(
            w.recent.is_empty(),
            "an adopted project is open, not recent"
        );
        assert_eq!(
            w.adopt_project(back),
            1,
            "adopting twice focuses the first copy"
        );

        parked.insert(0, project);
        let orphaned = retain_parked(&mut parked, &[PathBuf::from("/n/b")]);
        assert_eq!(orphaned, [3]);
        assert_eq!(
            roots(&Workspace {
                projects: parked,
                ..Workspace::default()
            }),
            ["/n/b"]
        );
    }
}
