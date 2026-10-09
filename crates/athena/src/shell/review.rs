use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, bail};
use athena_editor::{DiffEvent, DiffView, HunkActions};
use athena_workspace::git::{self, Rev};
use athena_workspace::{DiffBase, ItemKind};
use gpui::{AppContext as _, Context, Entity, EntityId, Window};

use super::Shell;
use super::item::{ItemView, file_label};
use super::notices::ToastAction;
use crate::snapshots::{self, Before};

/// Bigger files are not diffed; shaping and highlighting them would stall the window.
pub(super) const MAX_DIFF_BYTES: usize = 20 * 1024 * 1024;
/// Copies kept by revert and discard are deleted after this long.
const KEEP_COPIES: Duration = Duration::from_secs(30 * 24 * 3600);

/// Toasts for Claude's edits, one per file, so a burst of edits replaces rather than stacks.
#[derive(Default)]
pub(super) struct ReviewState {
    edit_toasts: HashMap<PathBuf, u64>,
    loads: Loads,
    pub(super) timeline: super::timeline::TimelineState,
    /// The file "Select for Compare" picked, the left side of the next Compare with Selected.
    pub(super) compare_with: Option<PathBuf>,
}

/// One load per diff at a time; a reload asked for meanwhile runs once after it, so a late
/// result never replaces a newer one.
#[derive(Default)]
struct Loads {
    /// Views with a load running, and whether another was asked for since it started.
    running: HashMap<EntityId, bool>,
}

impl Loads {
    /// Whether to start a load now.
    fn start(&mut self, view: EntityId) -> bool {
        match self.running.get_mut(&view) {
            Some(again) => {
                *again = true;
                false
            }
            None => {
                self.running.insert(view, false);
                true
            }
        }
    }

    /// Whether to load again because a reload was asked for while this one ran.
    fn finish(&mut self, view: EntityId) -> bool {
        self.running.remove(&view).unwrap_or(false)
    }
}

/// The tab title VS Code gives a diff.
pub(super) fn diff_title(path: &Path, base: &DiffBase) -> String {
    let name = file_label(path);
    match base {
        DiffBase::Head => format!("{name} (Index)"),
        DiffBase::Index => format!("{name} (Working Tree)"),
        DiffBase::Snapshot { .. } => format!("{name} (Claude's Edits)"),
        DiffBase::Proposal { .. } => format!("{name} (Claude's Proposal)"),
        DiffBase::SearchReplace => format!("{name} (Replace Preview)"),
        DiffBase::Conflict => format!("{name} (Current ↔ Incoming)"),
        DiffBase::Commit { rev, .. } => format!("{name} ({0}^ ↔ {0})", short_rev(rev)),
        DiffBase::Revision { rev, .. } => format!("{name} ({} ↔ Working Tree)", short_rev(rev)),
        DiffBase::Files { other } => format!("{} ↔ {name}", file_label(other)),
    }
}

/// The 7-character id git shows for a commit.
pub(super) fn short_rev(rev: &str) -> &str {
    &rev[..rev.len().min(7)]
}

fn sides(base: &DiffBase) -> (String, String, HunkActions) {
    let (old, new, actions) = match base {
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
        DiffBase::Proposal { .. } => ("On Disk", "Claude's Proposal", HunkActions::default()),
        DiffBase::SearchReplace => ("Current", "After Replace", HunkActions::default()),
        DiffBase::Conflict => (
            "Current Changes",
            "Incoming Changes",
            HunkActions::default(),
        ),
        DiffBase::Commit { rev, .. } => {
            let short = short_rev(rev);
            return (format!("{short}^"), short.into(), HunkActions::default());
        }
        DiffBase::Revision { rev, .. } => {
            let short = short_rev(rev).to_string();
            return (short, "Working Tree".into(), HunkActions::default());
        }
        DiffBase::Files { other } => {
            return (file_label(other), String::new(), HunkActions::default());
        }
    };
    (old.into(), new.into(), actions)
}

/// Text for one side of a diff; a file that is not there is empty.
pub(super) fn text(bytes: Option<Vec<u8>>) -> Result<String> {
    let bytes = bytes.unwrap_or_default();
    if bytes.len() > MAX_DIFF_BYTES {
        bail!("The file is larger than 20 MB.");
    }
    if bytes[..bytes.len().min(8000)].contains(&0) {
        bail!("This is a binary file; Athena shows differences in text files only.");
    }
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("This file is not UTF-8 text."))
}

pub(super) fn read_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Shown above a review of Claude's edits that falls back to the index.
const SKIPPED_NOTE: &str = "No copy from before Claude's edits was kept because the file was over \
                            20 MB or not a regular file. Showing changes against the index.";

/// Both sides of a diff, read off the main thread.
#[derive(Debug, PartialEq, Eq)]
struct Sides {
    old: String,
    new: String,
    /// A review of Claude's edits whose baseline was skipped, compared with the index instead.
    against_index: bool,
}

fn load(root: &Path, path: &Path, orig: Option<PathBuf>, base: &DiffBase) -> Result<Sides> {
    load_with(root, path, orig, base, snapshots::store)
}

fn load_with(
    root: &Path,
    path: &Path,
    orig: Option<PathBuf>,
    base: &DiffBase,
    store: impl FnOnce() -> Result<PathBuf>,
) -> Result<Sides> {
    let rel = || {
        path.strip_prefix(root)
            .map(Path::to_path_buf)
            .context("the file is outside the project")
    };
    let mut against_index = false;
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
        DiffBase::Snapshot { session } => {
            let old = match snapshots::read(&store()?, session, path) {
                Before::Text(t) => Some(t),
                Before::Absent => None,
                Before::Skipped => {
                    against_index = true;
                    git::show(root, Rev::Index, &rel()?)?
                }
                Before::Unknown => bail!(
                    "No copy of the file from before Claude's edits was kept. Enable Claude Code \
                     hooks for this project to keep one from the next session on."
                ),
            };
            (old, read_file(path)?)
        }
        DiffBase::Proposal { .. } => bail!("Claude's proposed change is no longer waiting."),
        DiffBase::SearchReplace => bail!("A replace preview comes from the search."),
        DiffBase::Commit { rev, old, new } => {
            let before = match old {
                Some(old) => git::show_at(root, &format!("{rev}^"), old)?,
                None => None,
            };
            (before, git::show_at(root, rev, new)?)
        }
        DiffBase::Revision { rev, at } => (git::show_at(root, rev, at)?, read_file(path)?),
        DiffBase::Files { other } => (read_file(other)?, read_file(path)?),
        DiffBase::Conflict => {
            let both = text(read_file(path)?)?;
            return Ok(Sides {
                old: athena_editor::resolve_all(&both, athena_editor::Resolution::Current),
                new: athena_editor::resolve_all(&both, athena_editor::Resolution::Incoming),
                against_index: false,
            });
        }
    };
    Ok(Sides {
        old: text(old)?,
        new: text(new)?,
        against_index,
    })
}

/// Stages one hunk; staging the last hunk of a deleted file stages the deletion.
fn stage_hunk(root: &Path, rel: &Path, expected: &str, contents: &str) -> Result<()> {
    let deleted = contents.is_empty() && std::fs::symlink_metadata(root.join(rel)).is_err();
    git::write_index(root, rel, expected, (!deleted).then_some(contents))
}

/// Unstages one hunk; unstaging the last hunk of a newly added file takes it out of the index.
fn unstage_hunk(
    root: &Path,
    rel: &Path,
    head_rel: &Path,
    expected: &str,
    contents: &str,
) -> Result<()> {
    let added = contents.is_empty() && git::show(root, Rev::Head, head_rel)?.is_none();
    git::write_index(root, rel, expected, (!added).then_some(contents))
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
        if let DiffBase::Proposal { id } = base {
            return self.new_proposal_view(path, id, cx);
        }
        let (old_label, mut new_label, actions) = sides(base);
        if new_label.is_empty() {
            new_label = file_label(path);
        }
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
        if *base == DiffBase::SearchReplace {
            return self.load_replace_preview(root, view, cx);
        }
        let id = view.entity_id();
        if matches!(base, DiffBase::Proposal { .. }) || !self.review.loads.start(id) {
            return;
        }
        let path = view.read(cx).path().to_path_buf();
        let orig = self.git_orig_path(root, &path);
        let (root, base) = (root.to_path_buf(), base.clone());
        let (label, _, _) = sides(&base);
        let weak = view.downgrade();
        cx.spawn(async move |this, cx| {
            let (task_root, task_base) = (root.clone(), base.clone());
            let loaded = cx
                .background_executor()
                .spawn(async move { load(&task_root, &path, orig, &task_base) })
                .await;
            let view = weak.upgrade();
            if let Some(view) = &view {
                let _ = view.update(cx, |v, cx| match loaded {
                    Ok(sides) => {
                        let (old_label, note) = match sides.against_index {
                            true => ("Index".to_string(), Some(SKIPPED_NOTE)),
                            false => (label, None),
                        };
                        v.set_old_label(old_label, note, cx);
                        v.set_texts(sides.old, sides.new, cx)
                    }
                    Err(err) => {
                        tracing::debug!(path = %v.path().display(), "diff: {err:#}");
                        v.set_error(format!("{err:#}"), cx)
                    }
                });
            }
            let _ = this.update(cx, |this, cx| {
                if this.review.loads.finish(id)
                    && let Some(view) = view
                {
                    this.load_diff(&root, &view, &base, cx);
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
                    // A commit's versions never change, so status runs need not read them again.
                    (ItemKind::Diff { path, base }, Some(ItemView::Diff(view)))
                        if only.is_none_or(|o| o == path)
                            && !matches!(base, DiffBase::Commit { .. }) =>
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
        let head_rel = self
            .git_orig_path(root, &path)
            .unwrap_or_else(|| rel.clone());
        let (failure, job): (&str, Box<dyn FnOnce() -> Result<()> + Send>) = match event {
            DiffEvent::Stage { contents, expected } => (
                "Could not stage the change",
                Box::new(move || stage_hunk(&dir, &rel, &expected, &contents)),
            ),
            DiffEvent::Unstage { contents, expected } => (
                "Could not unstage the change",
                Box::new(move || unstage_hunk(&dir, &rel, &head_rel, &expected, &contents)),
            ),
            DiffEvent::Revert { contents, expected } => (
                "Could not revert the change",
                Box::new(move || {
                    git::revert_file(&dir, &rel, &expected, &contents, &backup_dir()?)
                }),
            ),
            DiffEvent::OpenFile | DiffEvent::Accept | DiffEvent::Reject => return,
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

    /// A Claude Code hook reported an edit: refresh its diffs and offer a review of the session's
    /// changes to the file.
    pub(super) fn claude_edited(&mut self, path: PathBuf, session: String, cx: &mut Context<Self>) {
        let Some(root) = self
            .workspace
            .projects
            .iter()
            .map(|p| p.root.clone())
            .find(|r| path.starts_with(r))
        else {
            tracing::debug!(path = %path.display(), "Claude edited a file outside the open projects");
            return;
        };
        self.reload_diffs(&root, Some(&path), cx);
        let kept = snapshots::store()
            .map(|store| snapshots::read(&store, &session, &path) != Before::Unknown)
            .unwrap_or(false);
        tracing::debug!(path = %path.display(), kept, "Claude edited a file");
        let base = if kept {
            DiffBase::Snapshot { session }
        } else {
            DiffBase::Index
        };
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let open = path.clone();
        let action = ToastAction {
            label: "Review diff",
            run: Rc::new(move |this: &mut Shell, window, cx| {
                if let Some(i) = this.workspace.projects.iter().position(|p| p.root == root) {
                    this.switch_to(i, cx);
                }
                this.open_diff(open.clone(), base.clone(), window, cx);
            }),
        };
        if let Some(old) = self.review.edit_toasts.remove(&path) {
            self.dismiss_toast(old, cx);
        }
        let title = format!("Claude edited {}", file_label(&path));
        let id = self.action_toast(title, rel, action, cx);
        self.review.edit_toasts.insert(path, id);
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
        let pair = |s: Sides| (s.old, s.new);
        let staged = load(&dir, &path, Some(PathBuf::from("old.txt")), &DiffBase::Head).unwrap();
        assert_eq!(pair(staged), ("one\n".to_string(), "one\n".to_string()));
        let unstaged = load(&dir, &path, None, &DiffBase::Index).unwrap();
        assert_eq!(
            pair(unstaged),
            ("one\n".to_string(), "one\ntwo\n".to_string())
        );
        std::fs::write(dir.join("fresh.txt"), "new\n").unwrap();
        let untracked = load(&dir, &dir.join("fresh.txt"), None, &DiffBase::Index).unwrap();
        assert_eq!(pair(untracked), (String::new(), "new\n".to_string()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_last_hunk_of_a_deleted_or_new_file_moves_the_index_entry() {
        if !git::available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("athena-review-hunk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let git_out = |args: &[&str]| {
            let out = std::process::Command::new("/usr/bin/git")
                .arg("-C")
                .arg(&dir)
                .args(["-c", "user.name=T", "-c", "user.email=t@x"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        std::fs::write(dir.join("gone.txt"), "a\nb\n").unwrap();
        std::fs::write(dir.join("empty.txt"), "x\n").unwrap();
        git_out(&["init", "-q"]);
        git_out(&["add", "-A"]);
        git_out(&["commit", "-qm", "init"]);

        std::fs::remove_file(dir.join("gone.txt")).unwrap();
        let Sides { old, new, .. } =
            load(&dir, &dir.join("gone.txt"), None, &DiffBase::Index).unwrap();
        assert_eq!((old.as_str(), new.as_str()), ("a\nb\n", ""));
        stage_hunk(&dir, Path::new("gone.txt"), &old, &new).unwrap();
        assert_eq!(git_out(&["ls-files", "--", "gone.txt"]), "");
        assert_eq!(
            git_out(&["status", "--porcelain", "--", "gone.txt"]),
            "D  gone.txt\n"
        );

        // A file emptied on disk stays tracked, as an empty file.
        std::fs::write(dir.join("empty.txt"), "").unwrap();
        stage_hunk(&dir, Path::new("empty.txt"), "x\n", "").unwrap();
        assert_eq!(
            git_out(&["status", "--porcelain", "--", "empty.txt"]),
            "M  empty.txt\n"
        );

        std::fs::write(dir.join("new.txt"), "n\n").unwrap();
        git_out(&["add", "new.txt"]);
        let rel = Path::new("new.txt");
        let Sides { old, new, .. } =
            load(&dir, &dir.join("new.txt"), None, &DiffBase::Head).unwrap();
        assert_eq!((old.as_str(), new.as_str()), ("", "n\n"));
        unstage_hunk(&dir, rel, rel, &new, &old).unwrap();
        assert_eq!(
            git_out(&["status", "--porcelain", "--", "new.txt"]),
            "?? new.txt\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_review_whose_baseline_was_skipped_compares_with_the_index() {
        if !git::available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("athena-review-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        let dir = dir.canonicalize().unwrap();
        let (repo, store) = (dir.join("repo"), dir.join("store"));
        let ok = std::process::Command::new("/usr/bin/git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let fifo = repo.join("pipe");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: c_path is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        snapshots::take(&store, "s-1", &fifo).unwrap();
        std::fs::remove_file(&fifo).unwrap();
        std::fs::write(&fifo, "now a file\n").unwrap();
        let base = DiffBase::Snapshot {
            session: "s-1".into(),
        };
        let sides = load_with(&repo, &fifo, None, &base, || Ok(store.clone())).unwrap();
        assert_eq!(
            sides,
            Sides {
                old: String::new(),
                new: "now a file\n".into(),
                against_index: true,
            }
        );
        let other = DiffBase::Snapshot {
            session: "s-2".into(),
        };
        let err = load_with(&repo, &fifo, None, &other, || Ok(store.clone())).unwrap_err();
        assert!(err.to_string().contains("hooks"), "{err:#}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_reload_asked_for_during_a_load_runs_once_after_it() {
        let mut loads = Loads::default();
        let (a, b) = (EntityId::from(1u64), EntityId::from(2u64));
        assert!(loads.start(a));
        assert!(!loads.start(a));
        assert!(!loads.start(a));
        assert!(loads.start(b));
        assert!(loads.finish(a));
        assert!(loads.start(a));
        assert!(!loads.finish(a));
        assert!(!loads.finish(b));
        assert!(loads.running.is_empty());
    }

    #[test]
    fn diff_tabs_are_titled_like_vs_code() {
        let p = Path::new("/r/main.go");
        assert_eq!(diff_title(p, &DiffBase::Index), "main.go (Working Tree)");
        assert_eq!(diff_title(p, &DiffBase::Head), "main.go (Index)");
        let (_, _, actions) = sides(&DiffBase::Head);
        assert!(actions.unstage && !actions.stage && !actions.revert);
        let commit = DiffBase::Commit {
            rev: "0123456789abcdef".into(),
            old: None,
            new: "main.go".into(),
        };
        assert_eq!(diff_title(p, &commit), "main.go (0123456^ ↔ 0123456)");
        let files = DiffBase::Files {
            other: "/r/old.go".into(),
        };
        assert_eq!(diff_title(p, &files), "old.go ↔ main.go");
        assert_eq!(sides(&files).0, "old.go");
    }

    #[test]
    fn commit_revision_and_file_sides_load_through_renames() {
        if !git::available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("athena-review-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let dir = dir.canonicalize().unwrap();
        let git_out = |args: &[&str]| {
            let out = std::process::Command::new("/usr/bin/git")
                .arg("-C")
                .arg(&dir)
                .args(["-c", "user.name=T", "-c", "user.email=t@x"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        std::fs::write(dir.join("sub/a.txt"), "one\n").unwrap();
        git_out(&["init", "-q"]);
        git_out(&["add", "-A"]);
        git_out(&["commit", "-qm", "init"]);
        git_out(&["mv", "sub/a.txt", "sub/b.txt"]);
        std::fs::write(dir.join("sub/b.txt"), "one\ntwo\n").unwrap();
        git_out(&["add", "-A"]);
        git_out(&["commit", "-qm", "move"]);
        let rev = git_out(&["rev-parse", "HEAD"]);
        let sub = dir.join("sub");
        let path = sub.join("b.txt");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let pair = |base: DiffBase| {
            let s = load(&sub, &path, None, &base).unwrap();
            (s.old, s.new)
        };
        let commit = DiffBase::Commit {
            rev: rev.clone(),
            old: Some("sub/a.txt".into()),
            new: "sub/b.txt".into(),
        };
        assert_eq!(pair(commit), ("one\n".into(), "one\ntwo\n".into()));
        let revision = DiffBase::Revision {
            rev,
            at: "sub/b.txt".into(),
        };
        assert_eq!(
            pair(revision),
            ("one\ntwo\n".into(), "one\ntwo\nthree\n".into())
        );
        std::fs::write(sub.join("c.txt"), "c\n").unwrap();
        let files = DiffBase::Files {
            other: sub.join("c.txt"),
        };
        assert_eq!(pair(files), ("c\n".into(), "one\ntwo\nthree\n".into()));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
