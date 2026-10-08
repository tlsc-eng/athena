use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, bail};
use athena_editor::{DiffEvent, DiffView, HunkActions};
use athena_workspace::git::{self, Rev};
use athena_workspace::{DiffBase, ItemKind};
use gpui::{AppContext as _, Context, Entity, Window};

use super::Shell;
use super::item::{ItemView, file_label};

/// Bigger files are not diffed; shaping and highlighting them would stall the window.
const MAX_DIFF_BYTES: usize = 20 * 1024 * 1024;
/// Copies kept by revert and discard are deleted after this long.
const KEEP_COPIES: Duration = Duration::from_secs(30 * 24 * 3600);

/// The tab title VS Code gives a diff.
pub(super) fn diff_title(path: &Path, base: &DiffBase) -> String {
    let name = file_label(path);
    match base {
        DiffBase::Head => format!("{name} (Index)"),
        DiffBase::Index => format!("{name} (Working Tree)"),
        DiffBase::Snapshot { .. } => format!("{name} (Claude's Edits)"),
    }
}

fn sides(base: &DiffBase) -> (&'static str, &'static str, HunkActions) {
    match base {
        DiffBase::Head => (
            "HEAD",
            "Index",
            HunkActions {
                unstage: true,
                ..HunkActions::default()
            },
        ),
        DiffBase::Index => (
            "Index",
            "Working Tree",
            HunkActions {
                stage: true,
                revert: true,
                ..HunkActions::default()
            },
        ),
        DiffBase::Snapshot { .. } => (
            "Before Claude",
            "Working Tree",
            HunkActions {
                revert: true,
                ..HunkActions::default()
            },
        ),
    }
}

/// Text for one side of a diff; a file that is not there is empty.
fn text(bytes: Option<Vec<u8>>) -> Result<String> {
    let bytes = bytes.unwrap_or_default();
    if bytes.len() > MAX_DIFF_BYTES {
        bail!("The file is larger than 20 MB.");
    }
    if bytes[..bytes.len().min(8000)].contains(&0) {
        bail!("This is a binary file; Athena shows differences in text files only.");
    }
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("This file is not UTF-8 text."))
}

fn read_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Both sides of a diff, read off the main thread.
fn load(
    root: &Path,
    path: &Path,
    orig: Option<PathBuf>,
    base: &DiffBase,
) -> Result<(String, String)> {
    let rel = || {
        path.strip_prefix(root)
            .map(Path::to_path_buf)
            .context("the file is outside the project")
    };
    let (old, new) = match base {
        DiffBase::Head => {
            let rel = rel()?;
            let old_rel = orig.unwrap_or_else(|| rel.clone());
            (
                git::show(root, Rev::Head, &old_rel)?,
                git::show(root, Rev::Index, &rel)?,
            )
        }
        DiffBase::Index => (git::show(root, Rev::Index, &rel()?)?, read_file(path)?),
        DiffBase::Snapshot { .. } => bail!("Claude's edits are not kept yet."),
    };
    Ok((text(old)?, text(new)?))
}

/// Where revert and discard keep the version they replace, one folder per action.
pub(super) fn backup_dir() -> Result<PathBuf> {
    let dir = athena_proto::data_dir()?.join("discarded");
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| now.duration_since(t).unwrap_or_default() > KEEP_COPIES);
            if old {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    Ok(dir.join(stamp.to_string()))
}

impl Shell {
    pub(super) fn open_diff(
        &mut self,
        path: PathBuf,
        base: DiffBase,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_kind(ItemKind::Diff { path, base }, window, cx);
    }

    pub(super) fn new_diff_view(
        &mut self,
        root: &Path,
        path: &Path,
        base: &DiffBase,
        cx: &mut Context<Self>,
    ) -> ItemView {
        let (old_label, new_label, actions) = sides(base);
        let title = diff_title(path, base);
        let view = cx
            .new(|cx| DiffView::new(path.to_path_buf(), title, old_label, new_label, actions, cx));
        let (root_key, base_key) = (root.to_path_buf(), base.clone());
        cx.subscribe(&view, move |this, view, event: &DiffEvent, cx| {
            this.diff_event(&root_key, &view, &base_key, event.clone(), cx)
        })
        .detach();
        self.load_diff(root, &view, base, cx);
        ItemView::Diff(view)
    }

    fn load_diff(
        &mut self,
        root: &Path,
        view: &Entity<DiffView>,
        base: &DiffBase,
        cx: &mut Context<Self>,
    ) {
        let path = view.read(cx).path().to_path_buf();
        let orig = self.git_orig_path(root, &path);
        let (root, base) = (root.to_path_buf(), base.clone());
        let weak = view.downgrade();
        cx.spawn(async move |_, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { load(&root, &path, orig, &base) })
                .await;
            let Some(view) = weak.upgrade() else {
                return;
            };
            let _ = view.update(cx, |v, cx| match loaded {
                Ok((old, new)) => v.set_texts(old, new, cx),
                Err(err) => {
                    tracing::debug!(path = %v.path().display(), "diff: {err:#}");
                    v.set_error(format!("{err:#}"), cx)
                }
            });
        })
        .detach();
    }

    /// Re-reads every open diff of a project, after git status or a Claude edit moved things on.
    pub(super) fn reload_diffs(
        &mut self,
        root: &Path,
        only: Option<&Path>,
        cx: &mut Context<Self>,
    ) {
        let diffs: Vec<(Entity<DiffView>, DiffBase)> = self
            .workspace
            .projects
            .iter()
            .filter(|p| p.root == root)
            .filter_map(|p| p.layout.as_ref())
            .flat_map(|l| l.items())
            .filter_map(
                |item| match (&item.kind, self.items.get(&(root.to_path_buf(), item.id))) {
                    (ItemKind::Diff { path, base }, Some(ItemView::Diff(view)))
                        if only.is_none_or(|o| o == path) =>
                    {
                        Some((view.clone(), base.clone()))
                    }
                    _ => None,
                },
            )
            .collect();
        for (view, base) in diffs {
            self.load_diff(root, &view, &base, cx);
        }
    }

    fn diff_event(
        &mut self,
        root: &Path,
        view: &Entity<DiffView>,
        base: &DiffBase,
        event: DiffEvent,
        cx: &mut Context<Self>,
    ) {
        let path = view.read(cx).path().to_path_buf();
        if event == DiffEvent::OpenFile {
            self.pending_open = Some(path);
            return cx.notify();
        }
        let (dir, rel) = match (base, path.strip_prefix(root)) {
            (_, Ok(rel)) => (root.to_path_buf(), rel.to_path_buf()),
            (DiffBase::Snapshot { .. }, Err(_)) => match (path.parent(), path.file_name()) {
                (Some(dir), Some(name)) => (dir.to_path_buf(), PathBuf::from(name)),
                _ => return,
            },
            (_, Err(_)) => return,
        };
        let (failure, job): (&str, Box<dyn FnOnce() -> Result<()> + Send>) = match event {
            DiffEvent::Stage(contents) => (
                "Could not stage the change",
                Box::new(move || git::write_index(&dir, &rel, &contents)),
            ),
            DiffEvent::Unstage(contents) => (
                "Could not unstage the change",
                Box::new(move || git::write_index(&dir, &rel, &contents)),
            ),
            DiffEvent::Revert { contents, expected } => (
                "Could not revert the change",
                Box::new(move || {
                    git::revert_file(&dir, &rel, &expected, &contents, &backup_dir()?)
                }),
            ),
            DiffEvent::OpenFile => return,
        };
        let root = root.to_path_buf();
        cx.spawn(async move |this, cx| {
            let done = cx.background_executor().spawn(async move { job() }).await;
            let _ = this.update(cx, |this, cx| {
                if let Err(err) = done {
                    this.transient_notice(failure, format!("{err:#}"), cx);
                }
                this.reload_diffs(&root, Some(&path), cx);
                this.git_kick(cx);
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_and_oversized_sides_are_refused_and_missing_ones_are_empty() {
        assert_eq!(text(None).unwrap(), "");
        assert_eq!(text(Some(b"a\n".to_vec())).unwrap(), "a\n");
        assert!(text(Some(vec![0x89, b'P', 0, 1])).is_err());
        assert!(text(Some(vec![0xff, 0xfe, b'a'])).is_err());
    }

    #[test]
    fn sides_come_from_head_the_index_and_the_disk() {
        if !git::available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("athena-review-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let sh = |args: &[&str]| {
            let ok = std::process::Command::new("/usr/bin/git")
                .arg("-C")
                .arg(&dir)
                .args([
                    "-c",
                    "user.name=T",
                    "-c",
                    "user.email=t@x",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        std::fs::write(dir.join("old.txt"), "one\n").unwrap();
        sh(&["init", "-q"]);
        sh(&["add", "-A"]);
        sh(&["commit", "-qm", "init"]);
        sh(&["mv", "old.txt", "new.txt"]);
        std::fs::write(dir.join("new.txt"), "one\ntwo\n").unwrap();
        let path = dir.join("new.txt");
        let staged = load(&dir, &path, Some(PathBuf::from("old.txt")), &DiffBase::Head).unwrap();
        assert_eq!(staged, ("one\n".to_string(), "one\n".to_string()));
        let unstaged = load(&dir, &path, None, &DiffBase::Index).unwrap();
        assert_eq!(unstaged, ("one\n".to_string(), "one\ntwo\n".to_string()));
        std::fs::write(dir.join("fresh.txt"), "new\n").unwrap();
        let untracked = load(&dir, &dir.join("fresh.txt"), None, &DiffBase::Index).unwrap();
        assert_eq!(untracked, (String::new(), "new\n".to_string()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn diff_tabs_are_titled_like_vs_code() {
        let p = Path::new("/r/main.go");
        assert_eq!(diff_title(p, &DiffBase::Index), "main.go (Working Tree)");
        assert_eq!(diff_title(p, &DiffBase::Head), "main.go (Index)");
        let (_, _, actions) = sides(&DiffBase::Head);
        assert!(actions.unstage && !actions.stage && !actions.revert);
    }
}
