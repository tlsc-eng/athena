use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result};

use crate::{Project, Workspace};

/// Missing file is an empty workspace; a corrupt one is an error so the caller can keep a copy.
pub fn load(path: &Path) -> Result<Workspace> {
    match fs::read(path) {
        Ok(bytes) => {
            let mut workspace: Workspace = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", path.display()))?;
            workspace.projects.iter_mut().for_each(Project::migrate);
            Ok(workspace)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Workspace::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Writes via temp file and rename so a crash never leaves a half-written workspace.
pub fn save(path: &Path, workspace: &Workspace) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(workspace)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{UiState, WindowMode, WindowState};

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
    fn missing_ui_state_loads_defaults() {
        let w: Workspace =
            serde_json::from_str(r#"{"projects":[],"active":null,"window":null}"#).unwrap();
        assert_eq!(w.ui, UiState::default());
        let ui: UiState = serde_json::from_str(r#"{"tree_width":320}"#).unwrap();
        assert_eq!(ui.tree_width, 320.);
        assert_eq!(ui.drawer_height, 240.);
        assert!(ui.tree_visible);
    }
}
