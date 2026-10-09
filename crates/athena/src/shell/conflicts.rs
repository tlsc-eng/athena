use std::path::{Path, PathBuf};
use std::rc::Rc;

use athena_editor::{CompareMergeConflicts, EditorEvent, EditorView};
use athena_workspace::DiffBase;
use athena_workspace::git::{Entry, FileStatus};
use gpui::{Context, Entity, InteractiveElement};

use super::Shell;
use super::notices::ToastAction;

/// Conflicted files and the conflicts left in them, read from disk.
pub(super) fn count_conflicts(entries: &[(PathBuf, Entry)]) -> (usize, usize) {
    let blocks: Vec<usize> = entries
        .iter()
        .filter(|(_, e)| e.unstaged == Some(FileStatus::Conflict))
        .map(|(path, _)| {
            std::fs::read_to_string(path)
                .map(|t| athena_editor::count_merge_conflicts(&t))
                .unwrap_or(0)
        })
        .collect();
    (blocks.len(), blocks.iter().sum())
}

/// "2 conflicts in 1 file" for the Changes tab, or nothing when there are none.
pub(super) fn conflicts_label((files, blocks): (usize, usize)) -> Option<String> {
    if files == 0 {
        return None;
    }
    let plural = |n: usize, word: &str| {
        if n == 1 {
            format!("1 {word}")
        } else {
            format!("{n} {word}s")
        }
    };
    Some(format!(
        "{} in {}",
        plural(blocks, "conflict"),
        plural(files, "file")
    ))
}

impl Shell {
    /// Offers to stage a conflicted file once a save leaves no conflict markers in it.
    pub(super) fn watch_conflicts(&mut self, editor: &Entity<EditorView>, cx: &mut Context<Self>) {
        let id = editor.entity_id();
        if self.git.conflict_watch.contains_key(&id) {
            return;
        }
        let subscription = cx.subscribe(editor, |this, editor, event, cx| {
            if matches!(event, EditorEvent::Saved) {
                this.conflicts_saved(&editor, cx);
            }
        });
        self.git.conflict_watch.insert(id, subscription);
    }

    fn conflicts_saved(&mut self, editor: &Entity<EditorView>, cx: &mut Context<Self>) {
        let path = editor.read(cx).path().to_path_buf();
        if self.git_status_for(&path) != Some(FileStatus::Conflict)
            || editor.read(cx).merge_conflict_count() > 0
        {
            return;
        }
        let Some(root) = self.active_root() else {
            return;
        };
        let rel = rel_in(&root, &path);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let run = Rc::new(
            move |this: &mut Shell, _: &mut gpui::Window, cx: &mut Context<Shell>| {
                this.git_stage(vec![rel.clone()], true, cx)
            },
        );
        self.action_toast(
            "All conflicts resolved",
            format!("Stage {name} to mark it resolved."),
            ToastAction {
                label: "Stage",
                run,
            },
            cx,
        );
    }
}

fn rel_in(root: &Path, path: &Path) -> PathBuf {
    let path = super::git_view::under_root(root, path);
    path.strip_prefix(root).unwrap_or(&path).to_path_buf()
}

/// Binds Compare Changes from a conflict's actions row.
pub(super) fn bind_conflict_actions(el: gpui::Div, cx: &mut Context<Shell>) -> gpui::Div {
    el.on_action(
        cx.listener(|this, action: &CompareMergeConflicts, window, cx| {
            this.open_diff(action.path.clone(), DiffBase::Conflict, window, cx)
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_changes_tab_counts_blocks_and_files() {
        assert_eq!(conflicts_label((0, 0)), None);
        assert_eq!(
            conflicts_label((1, 1)).as_deref(),
            Some("1 conflict in 1 file")
        );
        assert_eq!(
            conflicts_label((2, 3)).as_deref(),
            Some("3 conflicts in 2 files")
        );
    }

    #[test]
    fn conflicted_files_are_read_for_their_blocks() {
        let dir = std::env::temp_dir().join(format!("athena-conflicts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let block = "<<<<<<< HEAD\na\n=======\nb\n>>>>>>> x\n";
        std::fs::write(dir.join("c.txt"), format!("{block}mid\n{block}")).unwrap();
        std::fs::write(dir.join("m.txt"), block).unwrap();
        std::fs::write(dir.join("s.txt"), format!("<<<<<<< stray\n{block}")).unwrap();
        let entry = |path: &str, status: FileStatus| {
            (
                dir.join(path),
                Entry {
                    path: path.into(),
                    orig: None,
                    staged: None,
                    unstaged: Some(status),
                },
            )
        };
        let entries = [
            entry("c.txt", FileStatus::Conflict),
            entry("m.txt", FileStatus::Modified),
        ];
        assert_eq!(count_conflicts(&entries), (1, 2));
        let stray = [entry("s.txt", FileStatus::Conflict)];
        assert_eq!(count_conflicts(&stray), (1, 1));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
