use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use athena_workspace::git::{Decorations, FileStatus, GitDirs};
use athena_workspace::watch::FolderWatcher;
use gpui::{Context, Task, Window};

use super::Shell;
use super::item::ItemView;

/// A batch this large (a build writing its output) re-lists the whole tree instead of path by path.
const INVALIDATE_ABOVE: usize = 256;

/// One file watcher per open project, feeding debounced batches back to the shell.
pub(super) struct WatchState {
    watchers: HashMap<PathBuf, FolderWatcher>,
    /// Roots whose watch could not start, not retried until Athena restarts.
    failed: Vec<PathBuf>,
    changes: async_channel::Sender<(PathBuf, Vec<PathBuf>)>,
    /// One per repository, by its common git folder, for branch and worktree changes.
    git_watchers: HashMap<PathBuf, FolderWatcher>,
    git_failed: Vec<PathBuf>,
    git_changes: async_channel::Sender<(PathBuf, Vec<PathBuf>)>,
    _task: Task<()>,
    _git_task: Task<()>,
}

impl Shell {
    pub(super) fn start_watching(window: &mut Window, cx: &mut Context<Self>) -> WatchState {
        let (changes, batches) = async_channel::unbounded::<(PathBuf, Vec<PathBuf>)>();
        let task = cx.spawn_in(window, async move |this, cx| {
            while let Ok((root, paths)) = batches.recv().await {
                if this
                    .update(cx, |this, cx| this.files_changed(&root, &paths, cx))
                    .is_err()
                {
                    return;
                }
            }
        });
        let (git_changes, git_batches) = async_channel::unbounded::<(PathBuf, Vec<PathBuf>)>();
        let git_task = cx.spawn_in(window, async move |this, cx| {
            while let Ok((common, paths)) = git_batches.recv().await {
                if this
                    .update(cx, |this, cx| this.git_dirs_changed(&common, &paths, cx))
                    .is_err()
                {
                    return;
                }
            }
        });
        WatchState {
            watchers: HashMap::new(),
            failed: Vec::new(),
            changes,
            git_watchers: HashMap::new(),
            git_failed: Vec::new(),
            git_changes,
            _task: task,
            _git_task: git_task,
        }
    }

    /// Watches every open project's folder and stops watching closed ones.
    pub(super) fn sync_watchers(&mut self) {
        self.sync_git_watchers();
        let roots: Vec<PathBuf> = self
            .workspace
            .projects
            .iter()
            .map(|p| p.root.clone())
            .collect();
        let watch = &mut self.watch;
        watch.watchers.retain(|root, _| roots.contains(root));
        watch.failed.retain(|root| roots.contains(root));
        for root in roots {
            if watch.watchers.contains_key(&root) || watch.failed.contains(&root) {
                continue;
            }
            let changes = watch.changes.clone();
            let tag = root.clone();
            match FolderWatcher::new(&root, move |paths| {
                let _ = changes.send_blocking((tag.clone(), paths));
            }) {
                Ok(watcher) => {
                    watch.watchers.insert(root, watcher);
                }
                Err(err) => {
                    tracing::warn!("not watching {} for changes: {err}", root.display());
                    watch.failed.push(root);
                }
            }
        }
    }

    fn sync_git_watchers(&mut self) {
        self.note_git_dirs();
        let commons: Vec<PathBuf> = self
            .git
            .dirs
            .values()
            .flatten()
            .map(|d| d.common_dir.clone())
            .collect();
        let watch = &mut self.watch;
        watch.git_watchers.retain(|c, _| commons.contains(c));
        watch.git_failed.retain(|c| commons.contains(c));
        for common in commons {
            if watch.git_watchers.contains_key(&common) || watch.git_failed.contains(&common) {
                continue;
            }
            let changes = watch.git_changes.clone();
            let tag = common.clone();
            let keep = common.clone();
            match FolderWatcher::filtered(
                &common,
                move |p| names_branch_or_worktree(&keep, p),
                move |paths| {
                    let _ = changes.send_blocking((tag.clone(), paths));
                },
            ) {
                Ok(watcher) => {
                    watch.git_watchers.insert(common, watcher);
                }
                Err(err) => {
                    tracing::warn!("not watching {} for branches: {err}", common.display());
                    watch.git_failed.push(common);
                }
            }
        }
    }

    /// A branch moved or a worktree came or went in the repository whose common git folder is `common`.
    fn git_dirs_changed(&mut self, common: &Path, paths: &[PathBuf], cx: &mut Context<Self>) {
        let everything = paths.iter().any(|p| p == common);
        let repos: Vec<(PathBuf, GitDirs)> = self
            .git
            .dirs
            .iter()
            .filter_map(|(root, dirs)| Some((root.clone(), dirs.clone()?)))
            .filter(|(_, dirs)| dirs.common_dir == common)
            .collect();
        let mut regrouped = false;
        for (root, dirs) in repos {
            if everything || paths.iter().any(|p| dirs.moves_worktrees(p)) {
                self.git.worktree_mains.remove(&root);
                regrouped = true;
            }
            if everything || paths.iter().any(|p| dirs.moves_branch(p)) {
                self.git_changed(&root, cx);
            }
        }
        if regrouped {
            self.note_worktree_mains();
            cx.notify();
        }
    }

    /// Something outside Athena (or a save) changed files under `root`.
    fn files_changed(&mut self, root: &Path, paths: &[PathBuf], cx: &mut Context<Self>) {
        let everything = paths.iter().any(|p| p == root);
        if everything || paths.len() > INVALIDATE_ABOVE {
            self.tree.invalidate();
        } else {
            for path in paths {
                self.tree.forget(path);
            }
        }
        let active = self.workspace.active_project().map(|p| p.root.as_path()) == Some(root);
        let decorations = self.git_decorations(root);
        // FSEvents reports real paths, while decorations are keyed under the project's own root.
        let real = root.canonicalize().ok();
        let in_root = |p: &PathBuf| match real.as_ref().and_then(|r| p.strip_prefix(r).ok()) {
            Some(rel) if !p.starts_with(root) => root.join(rel),
            _ => p.clone(),
        };
        if paths
            .iter()
            .any(|p| affects_git(root, &in_root(p), decorations.as_deref()))
        {
            if active {
                self.git_kick(cx);
            } else {
                self.throttled_summary(root, cx);
            }
        }
        let changed: HashSet<&Path> = paths.iter().map(PathBuf::as_path).collect();
        for view in self.items.values() {
            let touched = |file: &Path| {
                everything || changed.contains(file) || changed.contains(real_path(file).as_path())
            };
            match view {
                ItemView::Editor(editor) if touched(editor.read(cx).path()) => {
                    editor.update(cx, |v, cx| v.check_disk(cx))
                }
                ItemView::Image(image) if touched(image.read(cx).path()) => {
                    image.update(cx, |v, cx| v.reload_if_changed(cx))
                }
                ItemView::Large(large) if touched(large.read(cx).path()) => {
                    large.update(cx, |v, cx| v.reload_if_changed(cx))
                }
                ItemView::Doc(doc) if touched(doc.read(cx).path()) => {
                    doc.update(cx, |v, cx| v.refresh(cx))
                }
                _ => {}
            }
        }
        cx.notify();
    }
}

/// The path FSEvents reports for `file`: links and `/tmp`-style folders resolved, and for a
/// deleted file its folder resolved instead.
fn real_path(file: &Path) -> PathBuf {
    file.canonicalize()
        .ok()
        .or_else(|| {
            let real_dir = file.parent()?.canonicalize().ok()?;
            Some(real_dir.join(file.file_name()?))
        })
        .unwrap_or_else(|| file.to_path_buf())
}

/// Whether a change can alter `git status`: anything in the work tree that is not already known to
/// be ignored. Branches and refs inside `.git` reach the shell through the repository's own
/// watcher, and `git status` itself rewrites the index, so `.git` is left out.
fn affects_git(root: &Path, path: &Path, decorations: Option<&Decorations>) -> bool {
    !path.starts_with(root.join(".git"))
        && decorations.is_none_or(|d| d.get(path) != Some(FileStatus::Ignored))
}

/// The paths under a common git folder that name branches or worktrees, not objects or indexes.
fn names_branch_or_worktree(common: &Path, path: &Path) -> bool {
    let Ok(inside) = path.strip_prefix(common) else {
        return false;
    };
    let mut parts = inside.components();
    match parts.next().map(|c| c.as_os_str().to_string_lossy()) {
        Some(first) if first == "refs" => true,
        Some(first) if first == "HEAD" || first == "packed-refs" => parts.next().is_none(),
        Some(first) if first == "worktrees" => {
            let rest: Vec<_> = parts.collect();
            rest.len() <= 1 || (rest.len() == 2 && rest[1].as_os_str() == "HEAD")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_symlinked_file_is_matched_by_the_path_its_target_changes_under() {
        let dir = std::env::temp_dir().join(format!("athena-watch-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("AGENTS.md");
        std::fs::write(&target, "x").unwrap();
        let link = dir.join("CLAUDE.md");
        std::os::unix::fs::symlink("AGENTS.md", &link).unwrap();
        assert_eq!(real_path(&link), target.canonicalize().unwrap());
        std::fs::remove_file(&target).unwrap();
        let gone = dir.join("gone.rs");
        assert_eq!(
            real_path(&gone),
            dir.canonicalize().unwrap().join("gone.rs")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn git_internals_are_left_to_the_repository_watcher() {
        let root = Path::new("/p");
        let affects = |p: &str| affects_git(root, Path::new(p), None);
        assert!(affects("/p/src/main.rs"));
        assert!(!affects("/p/.git/HEAD"));
        assert!(!affects("/p/.git/index"));
        assert!(affects("/p/.gitignore"));
    }

    #[test]
    fn the_repository_watcher_sees_branches_and_worktrees_but_not_objects() {
        let common = Path::new("/r/.git");
        let names = |p: &str| names_branch_or_worktree(common, Path::new(p));
        assert!(names("/r/.git/HEAD"));
        assert!(names("/r/.git/packed-refs"));
        assert!(names("/r/.git/refs/heads/feat/x"));
        assert!(names("/r/.git/worktrees"));
        assert!(names("/r/.git/worktrees/feat"));
        assert!(names("/r/.git/worktrees/feat/HEAD"));
        assert!(!names("/r/.git/worktrees/feat/index"));
        assert!(!names("/r/.git/index"));
        assert!(!names("/r/.git/objects/ab/cdef"));
        assert!(!names("/r/.git/HEAD.lock"));
        assert!(!names("/r/src/main.rs"));
    }

    #[test]
    fn changes_under_ignored_folders_do_not_refresh_git() {
        let root = Path::new("/p");
        let entry = |path: &str, status| athena_workspace::git::Entry {
            path: path.into(),
            orig: None,
            staged: None,
            unstaged: Some(status),
        };
        let decorations = Decorations::new(
            root,
            &[
                (root.join("target"), entry("target/", FileStatus::Ignored)),
                (root.join("notes"), entry("notes/", FileStatus::Untracked)),
            ],
        );
        let affects = |p: &str| affects_git(root, Path::new(p), Some(&decorations));
        assert!(!affects("/p/target/debug/build/out.o"));
        assert!(!affects("/p/target"));
        assert!(affects("/p/notes/new.md"));
        assert!(affects("/p/src/main.rs"));
    }
}
