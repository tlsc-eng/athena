use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use athena_editor::{BlameCommit, EditorEvent, EditorView, GitGutterEvent, GutterBlame};
use athena_workspace::DiffBase;
use athena_workspace::git::{self, FileBlame, Rev};
use gpui::{Context, Entity, EntityId, Subscription, Task};

use super::Shell;
use super::git_view::under_root;
use super::item::{ItemView, file_label};

/// Typing pauses this long before the blame gutter is worked out again for the unsaved text.
const REBLAME_DELAY: Duration = Duration::from_millis(500);

/// The blame gutters shown, and the editors followed for them and for quick diff peeks.
#[derive(Default)]
pub(super) struct GutterState {
    blames: HashMap<EntityId, Blamed>,
    watch: HashMap<EntityId, [Subscription; 2]>,
}

struct Blamed {
    /// The blame the gutter shows, to find a clicked commit's paths.
    blame: Option<FileBlame>,
    task: Option<Task<()>>,
}

/// How recent `time` is among a file's commits: 1 for the newest down to 0 for the oldest.
fn heat(time: i64, oldest: i64, newest: i64) -> f32 {
    if newest <= oldest {
        return 1.;
    }
    (time - oldest) as f32 / (newest - oldest) as f32
}

/// The gutter's wording of a file's blame, as of `now` (seconds since the epoch).
fn gutter_blame(blame: &FileBlame, now: i64) -> GutterBlame {
    let times = blame
        .commits
        .iter()
        .filter(|c| !c.uncommitted)
        .map(|c| c.time);
    let (oldest, newest) = (times.clone().min().unwrap_or(0), times.max().unwrap_or(0));
    let commits = blame
        .commits
        .iter()
        .map(|c| match c.uncommitted {
            true => BlameCommit {
                sha: c.sha.clone(),
                author: "You".into(),
                age: "Not committed yet".into(),
                summary: "Uncommitted changes".into(),
                heat: 1.,
            },
            false => BlameCommit {
                sha: c.sha.clone(),
                author: c.author.clone(),
                age: git::relative_time(now - c.time),
                summary: c.summary.clone(),
                heat: heat(c.time, oldest, newest),
            },
        })
        .collect();
    GutterBlame {
        commits,
        lines: blame.lines.clone(),
    }
}

/// The diff a blamed commit opens for `path`: what it did to the file, or the uncommitted changes.
fn commit_diff(blame: &FileBlame, sha: &str) -> Option<DiffBase> {
    let commit = blame.commits.iter().find(|c| c.sha == sha)?;
    if commit.uncommitted {
        return Some(DiffBase::Index);
    }
    Some(DiffBase::Commit {
        rev: commit.sha.clone(),
        old: commit.previous.clone(),
        new: commit.path.clone(),
    })
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

impl Shell {
    fn editor_root(&self, editor: &Entity<EditorView>) -> Option<PathBuf> {
        self.items
            .iter()
            .find(|(_, v)| matches!(v, ItemView::Editor(e) if e == editor))
            .map(|((r, _), _)| r.clone())
    }

    /// Follows an editor's saves, edits and gutter clicks, once per editor.
    pub(super) fn watch_git_gutter(&mut self, editor: &Entity<EditorView>, cx: &mut Context<Self>) {
        let id = editor.entity_id();
        if self.git.gutters.watch.contains_key(&id) {
            return;
        }
        let edits = cx.subscribe(editor, |this, editor, event: &EditorEvent, cx| {
            if !this.git.gutters.blames.contains_key(&editor.entity_id()) {
                return;
            }
            match event {
                EditorEvent::Saved => this.blame_file(&editor, Duration::ZERO, cx),
                EditorEvent::Edited { .. } => this.blame_file(&editor, REBLAME_DELAY, cx),
                _ => {}
            }
        });
        let clicks = cx.subscribe(editor, |this, editor, event: &GitGutterEvent, cx| {
            this.git_gutter_event(&editor, event.clone(), cx)
        });
        self.git.gutters.watch.insert(id, [edits, clicks]);
    }

    /// Forgets editors that closed.
    pub(super) fn prune_git_gutters(&mut self) {
        let open: std::collections::HashSet<EntityId> = self
            .items
            .values()
            .filter_map(|v| match v {
                ItemView::Editor(e) => Some(e.entity_id()),
                _ => None,
            })
            .collect();
        self.git.gutters.watch.retain(|id, _| open.contains(id));
        self.git.gutters.blames.retain(|id, _| open.contains(id));
    }

    /// "Git: Toggle file blame" on the focused editor.
    pub(super) fn toggle_file_blame(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.focused_editor() else {
            return self.transient_notice(
                "Open a file first",
                "File blame shows who last changed each line.",
                cx,
            );
        };
        if self
            .git
            .gutters
            .blames
            .remove(&editor.entity_id())
            .is_some()
        {
            editor.update(cx, |e, cx| e.set_file_blame(None, cx));
            return;
        }
        self.watch_git_gutter(&editor, cx);
        self.blame_file(&editor, Duration::ZERO, cx);
    }

    fn blame_file(&mut self, editor: &Entity<EditorView>, delay: Duration, cx: &mut Context<Self>) {
        let Some(root) = self.editor_root(editor) else {
            return;
        };
        let id = editor.entity_id();
        let weak = editor.downgrade();
        let task = cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let Ok((path, contents)) = weak.read_with(cx, |e, _| {
                let contents = if e.is_dirty() { e.text() } else { None };
                (under_root(&root, e.path()), contents)
            }) else {
                return;
            };
            let name = file_label(&path);
            let found = cx
                .background_executor()
                .spawn(async move { git::blame_file(&root, &path, contents.as_deref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let Some(editor) = weak.upgrade() else {
                    return;
                };
                let Some(entry) = this.git.gutters.blames.get_mut(&id) else {
                    return;
                };
                entry.task = None;
                match found {
                    Ok(blame) if !blame.lines.is_empty() => {
                        let shown = gutter_blame(&blame, now());
                        entry.blame = Some(blame);
                        editor.update(cx, |e, cx| e.set_file_blame(Some(shown), cx));
                    }
                    other => {
                        if let Err(err) = other {
                            tracing::debug!("git blame: {err:#}");
                        }
                        this.git.gutters.blames.remove(&id);
                        editor.update(cx, |e, cx| e.set_file_blame(None, cx));
                        let body = format!("{name} has no committed lines to blame.");
                        this.transient_notice("No blame for this file", body, cx);
                    }
                }
            });
        });
        let entry = self.git.gutters.blames.entry(id).or_insert(Blamed {
            blame: None,
            task: None,
        });
        entry.task = Some(task);
    }

    fn git_gutter_event(
        &mut self,
        editor: &Entity<EditorView>,
        event: GitGutterEvent,
        cx: &mut Context<Self>,
    ) {
        let path = editor.read(cx).path().to_path_buf();
        match event {
            GitGutterEvent::OpenCommit { sha } => {
                let base = self
                    .git
                    .gutters
                    .blames
                    .get(&editor.entity_id())
                    .and_then(|b| commit_diff(b.blame.as_ref()?, &sha));
                if let Some(base) = base {
                    self.open_diff_soon(path, base, cx);
                }
            }
            GitGutterEvent::StageChange { contents, expected } => {
                self.stage_from_peek(editor, &path, contents, expected, cx)
            }
            GitGutterEvent::PeekChange { line } => self.peek_change(editor, &path, line, cx),
        }
    }

    /// Reads the file's index version for the quick diff peek the editor asked for.
    fn peek_change(
        &mut self,
        editor: &Entity<EditorView>,
        path: &Path,
        line: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.editor_root(editor) else {
            return;
        };
        let path = under_root(&root, path);
        let Ok(rel) = path.strip_prefix(&root).map(Path::to_path_buf) else {
            return;
        };
        let weak = editor.downgrade();
        cx.spawn(async move |this, cx| {
            let base = cx
                .background_executor()
                .spawn(async move { super::review::text(git::show(&root, Rev::Index, &rel)?) })
                .await;
            match base {
                Ok(base) => {
                    let _ = weak.update(cx, |e, cx| e.show_change_peek(Some(base), line, cx));
                }
                Err(err) => {
                    let _ = this.update(cx, |this, cx| {
                        this.transient_notice("Can't show this change", format!("{err:#}"), cx)
                    });
                }
            }
        })
        .detach();
    }

    /// Opens a diff from an event handler, which has no window to open it in.
    pub(super) fn open_diff_soon(&mut self, path: PathBuf, base: DiffBase, cx: &mut Context<Self>) {
        let Some(window) = cx.active_window() else {
            return;
        };
        let shell = cx.entity();
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                shell.update(cx, |this, cx| this.open_diff(path, base, window, cx))
            });
        });
    }

    fn stage_from_peek(
        &mut self,
        editor: &Entity<EditorView>,
        path: &Path,
        contents: String,
        expected: String,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.editor_root(editor) else {
            return;
        };
        let shown = path.to_path_buf();
        let path = under_root(&root, path);
        let Ok(rel) = path.strip_prefix(&root).map(Path::to_path_buf) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn(async move { git::write_index(&root, &rel, &expected, Some(&contents)) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(err) = done {
                    this.transient_notice("Could not stage the change", format!("{err:#}"), cx);
                }
                this.forget_gutter_marks(&shown);
                this.git_kick(cx);
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athena_workspace::git::BlameCommit as Commit;

    fn commit(sha: &str, time: i64, uncommitted: bool) -> Commit {
        Commit {
            sha: sha.into(),
            author: "Ann".into(),
            time,
            summary: "s".into(),
            uncommitted,
            path: "src/b.rs".into(),
            previous: Some("src/a.rs".into()),
        }
    }

    #[test]
    fn newer_commits_run_hotter_and_uncommitted_lines_are_yours() {
        let blame = FileBlame {
            commits: vec![
                commit("aaaaaaa", 100, false),
                commit("bbbbbbb", 300, false),
                commit("0000000", 999, true),
                commit("ccccccc", 200, false),
            ],
            lines: vec![0, 1, 2, 3],
        };
        let shown = gutter_blame(&blame, 300 + 2 * 86_400);
        let heats: Vec<f32> = shown.commits.iter().map(|c| c.heat).collect();
        assert_eq!(heats, [0., 1., 1., 0.5]);
        assert_eq!(shown.commits[1].age, "2 days ago");
        assert_eq!(shown.commits[2].author, "You");
        assert_eq!(heat(5, 5, 5), 1.);
    }

    #[test]
    fn a_blamed_commit_opens_its_change_through_a_rename() {
        let blame = FileBlame {
            commits: vec![commit("aaaaaaa", 1, false), commit("0000000", 2, true)],
            lines: vec![0, 1],
        };
        assert_eq!(
            commit_diff(&blame, "aaaaaaa"),
            Some(DiffBase::Commit {
                rev: "aaaaaaa".into(),
                old: Some("src/a.rs".into()),
                new: "src/b.rs".into(),
            })
        );
        assert_eq!(commit_diff(&blame, "0000000"), Some(DiffBase::Index));
        assert_eq!(commit_diff(&blame, "fffffff"), None);
    }
}
