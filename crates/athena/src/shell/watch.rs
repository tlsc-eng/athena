use std::collections::HashMap;
use std::path::{Path, PathBuf};

use athena_workspace::watch::FolderWatcher;
use gpui::{Context, Task, Window};

use super::Shell;
use super::item::ItemView;

/// One file watcher per open project, feeding debounced batches back to the shell.
pub(super) struct WatchState {
    watchers: HashMap<PathBuf, FolderWatcher>,
    /// Roots whose watch could not start, not retried until Athena restarts.
    failed: Vec<PathBuf>,
    changes: async_channel::Sender<(PathBuf, Vec<PathBuf>)>,
    _task: Task<()>,
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
        WatchState {
            watchers: HashMap::new(),
            failed: Vec::new(),
            changes,
            _task: task,
        }
    }

    /// Watches every open project's folder and stops watching closed ones.
    pub(super) fn sync_watchers(&mut self) {
        let roots: Vec<PathBuf> = self
            .workspace
            .projects
            .iter()
            .map(|p| p.root.clone())
            .collect();
        let watch = &mut self.watch;
        watch.watchers.retain(|root, _| roots.contains(root));
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

    /// Something outside Athena (or a save) changed files under `root`.
    fn files_changed(&mut self, root: &Path, paths: &[PathBuf], cx: &mut Context<Self>) {
        let everything = paths.iter().any(|p| p == root);
        if everything {
            self.tree.invalidate();
        } else {
            for path in paths {
                self.tree.forget(path);
            }
        }
        let active = self.workspace.active_project().map(|p| p.root.as_path()) == Some(root);
        if active && paths.iter().any(|p| affects_git(root, p)) {
            self.git_kick(cx);
        }
        for view in self.items.values() {
            let touched = |file: &Path| everything || paths.iter().any(|p| p == file);
            match view {
                ItemView::Editor(editor) if touched(editor.read(cx).path()) => {
                    editor.update(cx, |v, cx| v.check_disk(cx))
                }
                ItemView::Image(image) if touched(image.read(cx).path()) => {
                    image.update(cx, |v, cx| v.reload_if_changed(cx))
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

/// Whether a change can alter `git status`: anything in the work tree, but inside `.git` only the
/// branch and refs, since `git status` itself rewrites the index and would kick itself forever.
fn affects_git(root: &Path, path: &Path) -> bool {
    match path.strip_prefix(root.join(".git")) {
        Ok(inside) => inside == Path::new("HEAD") || inside.starts_with("refs"),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_internals_other_than_head_and_refs_do_not_refresh_git() {
        let root = Path::new("/p");
        assert!(affects_git(root, Path::new("/p/src/main.rs")));
        assert!(affects_git(root, Path::new("/p/.git/HEAD")));
        assert!(affects_git(root, Path::new("/p/.git/refs/heads/main")));
        assert!(!affects_git(root, Path::new("/p/.git/index")));
        assert!(!affects_git(root, Path::new("/p/.git/index.lock")));
        assert!(affects_git(root, Path::new("/p/.gitignore")));
    }
}
