use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::{Layout, Workspace};

/// Missing file is an empty workspace; a corrupt one is an error so the caller can keep a copy.
pub fn load(path: &Path) -> Result<Workspace> {
    match fs::read(path) {
        Ok(bytes) => {
            let mut workspace: Workspace = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", path.display()))?;
            for project in &mut workspace.projects {
                project.migrate();
                project.layout = project.layout.take().and_then(Layout::validated);
            }
            Ok(workspace)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Workspace::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Corrupt copies kept beside the workspace file, newest first.
const KEEP_CORRUPT: usize = 5;

/// Writes via temp file and rename so a crash never leaves a half-written workspace.
pub fn save(path: &Path, workspace: &Workspace) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut out = fs::File::create(&tmp).with_context(|| format!("write {}", tmp.display()))?;
    out.write_all(&serde_json::to_vec_pretty(workspace)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    // Without this, a power loss after the rename can leave an empty file under the real name.
    out.sync_all()?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// Whether a [`load`] error means the file holds something other than a workspace, rather than
/// that it could not be read right now.
pub fn is_corrupt(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.is::<serde_json::Error>())
}

/// Moves a corrupt workspace file to `<name>.corrupt-<millis>`, keeping the newest few such copies.
pub fn set_aside(path: &Path) -> Result<PathBuf> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let name = path
        .file_name()
        .context("workspace path has no file name")?
        .to_string_lossy()
        .into_owned();
    let prefix = format!("{name}.corrupt-");
    let aside = path.with_file_name(format!("{prefix}{millis}"));
    fs::rename(path, &aside).with_context(|| format!("rename to {}", aside.display()))?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut copies: Vec<PathBuf> = fs::read_dir(dir)?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .map(|e| e.path())
        .collect();
    copies.sort();
    for old in copies.iter().rev().skip(KEEP_CORRUPT) {
        let _ = fs::remove_file(old);
    }
    Ok(aside)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ItemId, ItemKind, PaneId, UiState, ViewState, WindowMode, WindowState};

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("athena-persist-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("workspace.json");
        assert_eq!(load(&path).unwrap(), Workspace::default());

        let mut w = Workspace::default();
        w.add_project("/n/a".into());
        w.window = Some(WindowState {
            x: 10.,
            y: 20.,
            width: 1200.,
            height: 800.,
            mode: WindowMode::Windowed,
        });
        w.ui = UiState {
            tree_width: 300.,
            drawer_height: 180.,
            tree_visible: false,
            font_zoom: -2,
            zoom_level: 3,
        };
        save(&path, &w).unwrap();
        assert_eq!(load(&path).unwrap(), w);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::write(&path, b"{not json").unwrap();
        assert!(load(&path).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_unparsable_files_count_as_corrupt_and_old_copies_are_pruned() {
        let dir = std::env::temp_dir().join(format!("athena-corrupt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("workspace.json");

        fs::write(&path, b"{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads any file, so the unreadable case only holds for a normal user.
        if fs::read(&path).is_err() {
            assert!(!is_corrupt(&load(&path).unwrap_err()));
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        for i in 0..7 {
            fs::write(
                dir.join(format!("workspace.json.corrupt-{:013}", 100 + i)),
                "x",
            )
            .unwrap();
        }
        fs::write(&path, b"{not json").unwrap();
        let err = load(&path).unwrap_err();
        assert!(is_corrupt(&err), "{err:#}");
        let aside = set_aside(&path).unwrap();
        assert!(!path.exists());
        assert_eq!(fs::read(&aside).unwrap(), b"{not json");
        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left.len(), KEEP_CORRUPT, "{left:?}");
        assert_eq!(
            left.last().map(String::as_str),
            aside.file_name().unwrap().to_str()
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn layouts_with_empty_panes_or_stale_ids_are_repaired_on_load() {
        let dir = std::env::temp_dir().join(format!("athena-repair-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("workspace.json");
        let leaf = |id: u64, items: &str, active: usize| {
            format!(r#"{{"Leaf":{{"id":{id},"items":[{items}],"active":{active}}}}}"#)
        };
        let term = |id: u64| format!(r#"{{"id":{id},"kind":{{"Terminal":{{"session":null}}}}}}"#);
        let split = format!(
            r#"{{"Split":{{"axis":"Horizontal","ratio":0.5,"first":{},"second":{}}}}}"#,
            leaf(1, "", 0),
            leaf(4, &term(9), 3)
        );
        let json = format!(
            r#"{{"projects":[
                {{"root":"/a","layout":{{"tree":{split},"focused":1,"next_id":2}}}},
                {{"root":"/b","layout":{{"tree":{},"focused":1,"next_id":3}}}}
            ],"active":0,"window":null}}"#,
            leaf(1, "", 0)
        );
        fs::write(&path, json).unwrap();
        let w = load(&path).unwrap();
        let mut layout = w.projects[0].layout.clone().unwrap();
        let panes = layout.panes();
        assert_eq!(panes.len(), 1);
        assert_eq!((panes[0].id, panes[0].active), (PaneId(4), 0));
        assert_eq!(layout.focused, PaneId(4));
        let added = layout
            .add_item(PaneId(4), ItemKind::Terminal { session: None })
            .unwrap();
        assert!(added.0 > 9, "new ids never repeat old ones");
        assert_eq!(
            w.projects[1].layout, None,
            "a layout with only an empty pane is dropped"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn editor_view_state_survives_a_round_trip_and_old_files_load_without_it() {
        let dir = std::env::temp_dir().join(format!("athena-view-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("workspace.json");
        let old = r#"{"projects":[{"root":"/a","layout":{"tree":{"Leaf":{"id":1,
            "items":[{"id":2,"kind":{"Editor":{"path":"/a/x.rs"}}}],"active":0}},
            "focused":1,"next_id":3}}],"active":0,"window":null}"#;
        fs::write(&path, old).unwrap();
        let mut w = load(&path).unwrap();
        let layout = w.projects[0].layout.as_mut().unwrap();
        assert_eq!(layout.items().next().unwrap().view, None);

        let state = ViewState {
            cursor: (41, 7),
            scroll_top: Some(30),
            folds: vec![3, 12],
            wrap: Some(true),
        };
        layout.item_mut(ItemId(2)).unwrap().view = Some(state.clone());
        save(&path, &w).unwrap();
        let again = load(&path).unwrap();
        let item = again.projects[0]
            .layout
            .as_ref()
            .unwrap()
            .items()
            .next()
            .unwrap();
        assert_eq!(item.view.as_ref(), Some(&state));

        let partial: ViewState = serde_json::from_str(r#"{"cursor":[5,2]}"#).unwrap();
        assert_eq!((partial.cursor, partial.scroll_top), ((5, 2), None));
        assert!(partial.folds.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_ui_state_loads_defaults() {
        let w: Workspace =
            serde_json::from_str(r#"{"projects":[],"active":null,"window":null}"#).unwrap();
        assert_eq!(w.ui, UiState::default());
        let ui: UiState = serde_json::from_str(r#"{"tree_width":320}"#).unwrap();
        assert_eq!(ui.tree_width, 320.);
        assert_eq!(ui.drawer_height, 240.);
        assert!(ui.tree_visible);
        assert_eq!(ui.font_zoom, 0);
    }
}
