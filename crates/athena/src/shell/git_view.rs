use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use athena_editor::{EditorView, GutterMark};
use athena_ui::{ActiveTheme, Button, ButtonKind, InputEvent, TextInput, Theme, Tooltip};
use athena_workspace::DiffBase;
use athena_workspace::git::{self, Decorations, Entry, FileStatus, Hunk};
use gpui::{
    AnyElement, Context, Entity, Focusable, FontWeight, Hsla, PromptLevel, Subscription, Task,
    Window, div, prelude::*, px, uniform_list,
};

use super::Shell;
use super::item::ItemView;

/// How often the active project's status is re-read while the window is in front.
const POLL: Duration = Duration::from_secs(5);
/// Saves and focus changes come in bursts; one status run covers the burst.
const KICK_DELAY: Duration = Duration::from_millis(300);
/// The cursor rests this long on a line before it is blamed.
const BLAME_DELAY: Duration = Duration::from_millis(150);
/// A status run slower than this switches the project to cheaper untracked-file scanning.
const SLOW_STATUS: Duration = Duration::from_secs(1);
const ROW_HEIGHT: f32 = 24.;
/// VS Code's `git.autofetchPeriod`.
const AUTOFETCH_EVERY: Duration = Duration::from_secs(180);
/// VS Code's `git.autofetch` default; a settings file can pass its own value to `set_autofetch`.
const AUTOFETCH_DEFAULT: bool = false;

#[derive(Default)]
struct Repo {
    /// The project's path inside its repository; `None` while it is not in one.
    prefix: Option<String>,
    /// A status run has finished at least once, so a missing prefix means "not a repository".
    checked: bool,
    slow: bool,
    branch: Option<String>,
    tracking: Option<git::Tracking>,
    /// Conflicted files and the conflict blocks left in them.
    conflicts: (usize, usize),
    entries: Rc<Vec<(PathBuf, Entry)>>,
    decorations: Rc<Decorations>,
}

/// What a file's gutter marks were computed from, so unchanged files are not diffed again.
type Signature = (Option<FileStatus>, Option<SystemTime>, u64);

#[derive(Default)]
pub(super) struct GitState {
    repos: HashMap<PathBuf, Repo>,
    marks: HashMap<PathBuf, (Signature, Vec<GutterMark>)>,
    running: bool,
    again: bool,
    kick: Option<Task<()>>,
    poll: Option<Task<()>>,
    blame_on: bool,
    blame: Option<Task<()>>,
    commit_input: Option<Entity<TextInput>>,
    _commit_events: Option<Subscription>,
    amend: bool,
    /// The message typed before Amend filled in the last commit's, put back when it is turned off.
    before_amend: Option<String>,
    /// The last commit's body while amending; the box shows only its subject line.
    amend_body: Option<String>,
    /// The commit Amend filled the box from, so a commit made since is not rewritten with it.
    amend_head: Option<String>,
    committing: bool,
    /// Branches for the branch picker, newest first.
    pub(super) branches: Vec<git::Branch>,
    /// Stashes for the branch picker, newest first.
    pub(super) stashes: Vec<git::Stash>,
    /// The fetch, pull or push under way, as the status bar words it.
    pub(super) remote_busy: Option<&'static str>,
    autofetch: Option<Task<()>>,
    /// Editors followed for saves that resolve a file's last conflict.
    pub(super) conflict_watch: HashMap<gpui::EntityId, Subscription>,
}

/// A remote operation the status bar menu and the palette offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Remote {
    Fetch,
    Pull,
    Push,
}

impl Remote {
    fn busy(self) -> &'static str {
        match self {
            Self::Fetch => "Fetching…",
            Self::Pull => "Pulling…",
            Self::Push => "Pushing…",
        }
    }

    fn failed(self) -> &'static str {
        match self {
            Self::Fetch => "Fetch failed",
            Self::Pull => "Pull failed",
            Self::Push => "Push failed",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    Staged,
    Changes,
    Untracked,
}

impl Group {
    fn label(self) -> &'static str {
        match self {
            Self::Staged => "Staged Changes",
            Self::Changes => "Changes",
            Self::Untracked => "Untracked",
        }
    }
}

#[derive(Clone)]
enum Row {
    Header(Group, usize),
    File {
        group: Group,
        path: PathBuf,
        status: FileStatus,
        /// Paths relative to the project for `git add` / `git restore --staged`.
        targets: Vec<PathBuf>,
        /// An untracked folder listed whole, which is shown in the tree rather than opened.
        is_dir: bool,
    },
}

/// The tree and tab colour for a file's status, as VS Code's git decorations use them.
pub(super) fn status_color(status: FileStatus, t: &Theme) -> Hsla {
    match status {
        FileStatus::Modified => t.color.warning,
        FileStatus::Added | FileStatus::Renamed => t.color.success,
        FileStatus::Untracked => t.color.success.opacity(0.7),
        FileStatus::Deleted | FileStatus::Conflict => t.color.danger,
        FileStatus::Ignored => t.color.content_disabled,
    }
}

/// The trailing status letter for a tree row; folders get none.
pub(super) fn status_badge(
    status: Option<FileStatus>,
    is_dir: bool,
    t: &Theme,
) -> Option<AnyElement> {
    let status = status.filter(|s| !is_dir && *s != FileStatus::Ignored)?;
    Some(
        div()
            .flex_none()
            .text_color(status_color(status, t))
            .child(status.letter())
            .into_any_element(),
    )
}

fn marks_from(hunks: Vec<Hunk>) -> Vec<GutterMark> {
    hunks
        .into_iter()
        .map(|h| match h {
            Hunk::Added { start, len } => GutterMark::Added { start, len },
            Hunk::Modified { start, len } => GutterMark::Modified { start, len },
            Hunk::Removed { before } => GutterMark::Removed { before },
        })
        .collect()
}

fn signature(path: &Path, status: Option<FileStatus>) -> Signature {
    let meta = std::fs::metadata(path).ok();
    (
        status,
        meta.as_ref().and_then(|m| m.modified().ok()),
        meta.map_or(0, |m| m.len()),
    )
}

/// `path` spelled under `root`, for files opened by their resolved path (/private/tmp for /tmp).
pub(super) fn under_root(root: &Path, path: &Path) -> PathBuf {
    if path.starts_with(root) {
        return path.to_path_buf();
    }
    root.canonicalize()
        .ok()
        .and_then(|real| Some(root.join(path.strip_prefix(real).ok()?)))
        .unwrap_or_else(|| path.to_path_buf())
}

fn rel_to(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

impl Shell {
    /// Polls the active project's status every few seconds while the window is in front.
    pub(super) fn start_git(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| TextInput::new("Message (⌘↩ to commit)", cx));
        self.git._commit_events = Some(cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::SubmitBeside => this.commit(window, cx),
                InputEvent::Cancel => this.focus_active_item(window, cx),
                _ => {}
            },
        ));
        self.git.commit_input = Some(input);
        self.set_autofetch(AUTOFETCH_DEFAULT, window, cx);
        // The window is not active yet while it is being built, so the first run is kicked.
        self.git_kick(cx);
        self.git.poll = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let alive = this.update_in(cx, |this, window, cx| {
                    if window.is_window_active() {
                        this.refresh_git(cx);
                    }
                });
                if alive.is_err() {
                    return;
                }
                cx.background_executor().timer(POLL).await;
            }
        }));
    }

    /// Re-reads git state soon, folding a burst of saves or focus changes into one run.
    pub(super) fn git_kick(&mut self, cx: &mut Context<Self>) {
        if let Some(root) = self.workspace.active_project().map(|p| p.root.clone()) {
            self.git_repo(&root);
        }
        self.git.kick = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(KICK_DELAY).await;
            let _ = this.update(cx, |this, cx| this.refresh_git(cx));
        }));
    }

    /// Forgets a closed project's git state and gutter marks.
    pub(super) fn git_project_closed(&mut self, root: &Path) {
        self.git.repos.remove(root);
        self.git.marks.retain(|path, _| !path.starts_with(root));
    }

    /// The branch for the title bar, from the last status run.
    pub(super) fn cached_branch(&self, root: &Path) -> Option<String> {
        self.git.repos.get(root)?.branch.clone()
    }

    /// Whether a status run has found the project in a repository.
    pub(super) fn git_checked(&self, root: &Path) -> bool {
        self.git
            .repos
            .get(root)
            .is_some_and(|r| r.checked && r.prefix.is_some())
    }

    /// The branch's upstream and ahead/behind counts, from the last status run.
    pub(super) fn cached_tracking(&self, root: &Path) -> Option<git::Tracking> {
        self.git.repos.get(root)?.tracking.clone()
    }

    /// Fetches the active project every three minutes while the window is in front, as VS Code's
    /// `git.autofetch` does; off by default, also as there.
    pub(super) fn set_autofetch(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !on {
            self.git.autofetch = None;
            return;
        }
        if self.git.autofetch.is_some() {
            return;
        }
        self.git.autofetch = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(AUTOFETCH_EVERY).await;
                let alive = this.update_in(cx, |this, window, cx| {
                    if window.is_window_active() && this.git.remote_busy.is_none() {
                        this.git_remote(Remote::Fetch, true, window, cx);
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
        }));
    }

    /// Fetch, `pull --ff-only` or push for the active project; a branch without an upstream is
    /// published after the user agrees. `quiet` keeps a background fetch's failures out of sight.
    pub(super) fn git_remote(
        &mut self,
        op: Remote,
        quiet: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root() else {
            return;
        };
        if !git::available() || self.git.remote_busy.is_some() {
            return;
        }
        if op == Remote::Push && self.cached_tracking(&root).is_none() {
            return self.publish_branch(root, window, cx);
        }
        self.run_remote(root, op, None, quiet, cx);
    }

    fn run_remote(
        &mut self,
        root: PathBuf,
        op: Remote,
        publish: Option<(String, String)>,
        quiet: bool,
        cx: &mut Context<Self>,
    ) {
        self.git.remote_busy = Some(op.busy());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let task_root = root.clone();
            let done = cx
                .background_executor()
                .spawn(async move {
                    match op {
                        Remote::Fetch => git::fetch(&task_root),
                        Remote::Pull => git::pull(&task_root),
                        Remote::Push => git::push(
                            &task_root,
                            publish.as_ref().map(|(r, b)| (r.as_str(), b.as_str())),
                        ),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.git.remote_busy = None;
                match done {
                    Err(err) if quiet => tracing::debug!("background {op:?}: {err:#}"),
                    Err(err) => this.transient_notice(op.failed(), format!("{err:#}"), cx),
                    Ok(()) if op == Remote::Pull => {
                        this.transient_notice("Pulled", "The branch is up to date.", cx)
                    }
                    Ok(()) if op == Remote::Push => {
                        this.transient_notice("Pushed", "The remote branch is up to date.", cx)
                    }
                    Ok(()) => {}
                }
                this.tree.invalidate();
                this.git_kick(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// VS Code's "Publish Branch": asks before pushing a branch with no upstream to origin, or
    /// the only remote.
    fn publish_branch(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let task_root = root.clone();
            let found = cx
                .background_executor()
                .spawn(async move {
                    let branch = git::current_branch(&task_root)?;
                    let remotes = git::remotes(&task_root)?;
                    anyhow::Ok((branch, remotes))
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let (branch, remotes) = match found {
                    Ok(found) => found,
                    Err(err) => {
                        return this.transient_notice("Push failed", format!("{err:#}"), cx);
                    }
                };
                let Some(branch) = branch else {
                    return this.transient_notice(
                        "Push failed",
                        "HEAD is detached; switch to a branch to push it.",
                        cx,
                    );
                };
                let Some(remote) = publish_remote(&remotes) else {
                    return this.transient_notice(
                        "No remote to push to",
                        "Add one with git remote add, then push again.",
                        cx,
                    );
                };
                let message = format!("The branch “{branch}” has no upstream branch.");
                let detail = format!(
                    "Publish it to {remote}? Athena runs git push -u {remote} {branch}, so later \
                     pushes and pulls go there."
                );
                let answer = window.prompt(
                    PromptLevel::Info,
                    &message,
                    Some(&detail),
                    &["Publish Branch", "Cancel"],
                    cx,
                );
                cx.spawn(async move |this, cx| {
                    if answer.await == Ok(0) {
                        let _ = this.update(cx, |this, cx| {
                            let publish = Some((remote, branch));
                            this.run_remote(root, Remote::Push, publish, false, cx)
                        });
                    }
                })
                .detach();
            });
        })
        .detach();
    }

    /// `git stash push`, as VS Code's Stash and Stash (Include Untracked).
    pub(super) fn git_stash(&mut self, include_untracked: bool, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        self.git_background(
            "Could not stash",
            move || git::stash_push(&root, include_untracked, None),
            cx,
        );
    }

    /// `git stash pop` of the stash at `index`.
    pub(super) fn git_stash_pop(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        self.git_background(
            "Could not pop the stash",
            move || git::stash_pop(&root, index),
            cx,
        );
    }

    /// Runs a git write off the main thread, then reports its failure and re-reads the status.
    fn git_background(
        &mut self,
        failed: &'static str,
        job: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let done = cx.background_executor().spawn(async move { job() }).await;
            let _ = this.update(cx, |this, cx| {
                if let Err(err) = done {
                    this.transient_notice(failed, format!("{err:#}"), cx);
                }
                this.tree.invalidate();
                this.git_kick(cx);
            });
        })
        .detach();
    }

    /// A project's git state, seeded from .git/HEAD so the title bar has a branch before git answers.
    fn git_repo(&mut self, root: &Path) -> &mut Repo {
        self.git
            .repos
            .entry(root.to_path_buf())
            .or_insert_with(|| Repo {
                branch: athena_workspace::git_branch(root),
                ..Repo::default()
            })
    }

    /// The last status run's decorations for a project, whichever project is active.
    pub(super) fn git_decorations(&self, root: &Path) -> Option<Rc<Decorations>> {
        Some(self.git.repos.get(root)?.decorations.clone())
    }

    pub(super) fn git_status_for(&self, path: &Path) -> Option<FileStatus> {
        let root = &self.workspace.active_project()?.root;
        self.git
            .repos
            .get(root)?
            .decorations
            .get(&under_root(root, path))
    }

    /// A renamed file's old path, relative to the project, for diffing it against HEAD.
    pub(super) fn git_orig_path(&self, root: &Path, path: &Path) -> Option<PathBuf> {
        let repo = self.git.repos.get(root)?;
        let prefix = repo.prefix.as_deref()?;
        let (_, entry) = repo.entries.iter().find(|(p, _)| p == path)?;
        Some(PathBuf::from(entry.orig.as_ref()?.strip_prefix(prefix)?))
    }

    pub(super) fn refresh_git(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.workspace.active_project().map(|p| p.root.clone()) else {
            return;
        };
        if !git::available() {
            self.git_repo(&root).branch = athena_workspace::git_branch(&root);
            return;
        }
        // One run at a time; a request during a run starts another right after it.
        if self.git.running {
            self.git.again = true;
            return;
        }
        self.git.running = true;
        let repo = self.git_repo(&root);
        let (prefix, slow) = (repo.prefix.clone(), repo.slow);
        cx.spawn(async move |this, cx| {
            let task_root = root.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let prefix = match prefix {
                        Some(p) => p,
                        None => git::prefix(&task_root)?,
                    };
                    let snapshot = git::status(&task_root, &prefix, !slow)?;
                    let conflicts = super::conflicts::count_conflicts(&snapshot.entries);
                    anyhow::Ok((prefix, snapshot, conflicts))
                })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                // The project closed while git ran; recording it would bring its state back.
                _ if !this.workspace.projects.iter().any(|p| p.root == root) => {
                    this.git_finished(cx)
                }
                Ok((prefix, snapshot, conflicts)) => {
                    if let Some(repo) = this.git.repos.get_mut(&root)
                        && repo.conflicts != conflicts
                    {
                        repo.conflicts = conflicts;
                        cx.notify();
                    }
                    this.git_status_arrived(&root, prefix, snapshot, cx)
                }
                Err(err) if err.is::<git::TimedOut>() => {
                    tracing::warn!(root = %root.display(), "git status: {err:#}");
                    this.git_repo(&root).slow = true;
                    this.git_finished(cx);
                }
                Err(err) => {
                    tracing::debug!(root = %root.display(), "git status: {err:#}");
                    let repo = this.git_repo(&root);
                    repo.prefix = None;
                    repo.checked = true;
                    cx.notify();
                    this.git_finished(cx);
                }
            });
        })
        .detach();
    }

    fn git_status_arrived(
        &mut self,
        root: &Path,
        prefix: String,
        snapshot: git::Snapshot,
        cx: &mut Context<Self>,
    ) {
        let slow = snapshot.took > SLOW_STATUS;
        tracing::debug!(
            root = %root.display(),
            files = snapshot.entries.len(),
            elapsed_ms = snapshot.took.as_millis() as u64,
            slow,
            branch = ?snapshot.branch,
            "git status"
        );
        let repo = self.git_repo(root);
        if slow && !repo.slow {
            tracing::info!(root = %root.display(), "git status is slow; listing untracked folders whole");
        }
        repo.slow |= slow;
        repo.prefix = Some(prefix);
        let first = !std::mem::replace(&mut repo.checked, true);
        // Most polls find nothing new; skipping the redraw keeps an idle window idle.
        if first
            || *repo.entries != snapshot.entries
            || repo.branch != snapshot.branch
            || repo.tracking != snapshot.tracking
        {
            repo.branch = snapshot.branch;
            repo.tracking = snapshot.tracking;
            repo.decorations = Rc::new(Decorations::new(root, &snapshot.entries));
            repo.entries = Rc::new(snapshot.entries);
            cx.notify();
        }
        self.reload_diffs(root, None, cx);
        self.sync_gutters(root, cx);
    }

    fn git_finished(&mut self, cx: &mut Context<Self>) {
        self.git.running = false;
        if std::mem::take(&mut self.git.again) {
            self.refresh_git(cx);
        }
    }

    pub(super) fn editors_under(&self, root: &Path) -> Vec<Entity<EditorView>> {
        self.items
            .iter()
            .filter(|((r, _), _)| r == root)
            .filter_map(|(_, v)| match v {
                ItemView::Editor(e) => Some(e.clone()),
                _ => None,
            })
            .collect()
    }

    /// Diffs the open files whose status or saved text changed, then pushes every editor's marks.
    fn sync_gutters(&mut self, root: &Path, cx: &mut Context<Self>) {
        let open: std::collections::HashSet<PathBuf> = self
            .items
            .values()
            .filter_map(|v| match v {
                ItemView::Editor(e) => Some(e.read(cx).path().to_path_buf()),
                _ => None,
            })
            .collect();
        self.git.marks.retain(|path, _| open.contains(path));
        let editors = self.editors_under(root);
        let mut stale = Vec::new();
        for editor in &editors {
            let path = editor.read(cx).path().to_path_buf();
            let status = self.git_status_for(&path);
            let sig = signature(&path, status);
            match self.git.marks.get(&path) {
                Some((known, marks)) if *known == sig => {
                    let marks = marks.clone();
                    editor.update(cx, |e, cx| e.set_gutter_marks(marks, cx));
                }
                _ if matches!(
                    status,
                    None | Some(FileStatus::Untracked | FileStatus::Ignored)
                ) =>
                {
                    self.git.marks.insert(path, (sig, Vec::new()));
                    editor.update(cx, |e, cx| e.set_gutter_marks(Vec::new(), cx));
                }
                _ => stale.push((path.clone(), under_root(root, &path), sig)),
            }
        }
        stale.sort_by(|a, b| a.0.cmp(&b.0));
        stale.dedup_by(|a, b| a.0 == b.0);
        if stale.is_empty() {
            return self.git_finished(cx);
        }
        let root = root.to_path_buf();
        cx.spawn(async move |this, cx| {
            let task_root = root.clone();
            let diffs = cx
                .background_executor()
                .spawn(async move {
                    stale
                        .into_iter()
                        .map(|(path, git_path, sig)| {
                            let marks = git::diff_hunks(&task_root, &git_path)
                                .map(marks_from)
                                .unwrap_or_default();
                            (path, sig, marks)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                for (path, sig, marks) in diffs {
                    tracing::debug!(path = %path.display(), marks = marks.len(), "git diff");
                    for editor in this.editors_under(&root) {
                        if editor.read(cx).path() == path {
                            let marks = marks.clone();
                            editor.update(cx, |e, cx| e.set_gutter_marks(marks, cx));
                        }
                    }
                    this.git.marks.insert(path, (sig, marks));
                }
                this.git_finished(cx);
            });
        })
        .detach();
    }

    /// A new editor shows known marks at once and fresh ones after a quick status run.
    pub(super) fn git_opened(&mut self, editor: &Entity<EditorView>, cx: &mut Context<Self>) {
        self.watch_conflicts(editor, cx);
        let path = editor.read(cx).path().to_path_buf();
        if let Some((_, marks)) = self.git.marks.get(&path) {
            let marks = marks.clone();
            editor.update(cx, |e, cx| e.set_gutter_marks(marks, cx));
        }
        self.git_kick(cx);
    }

    pub(super) fn focused_editor(&self) -> Option<Entity<EditorView>> {
        let project = self.workspace.active_project()?;
        let item = project.layout.as_ref()?.focused_pane()?.active_item()?;
        match self.items.get(&(project.root.clone(), item.id))? {
            ItemView::Editor(e) => Some(e.clone()),
            _ => None,
        }
    }

    /// Cmd+Alt+Shift+G: turns the faded author/date caption on the cursor's line on or off.
    pub(super) fn toggle_blame(&mut self, cx: &mut Context<Self>) {
        self.git.blame_on = !self.git.blame_on;
        let editors: Vec<_> = self
            .items
            .values()
            .filter_map(|v| match v {
                ItemView::Editor(e) => Some(e.clone()),
                _ => None,
            })
            .collect();
        if self.git.blame_on {
            if let Some(editor) = self.focused_editor() {
                let line = editor.read(cx).cursor_line();
                self.git_cursor_moved(&editor, line as u32, cx);
            }
        } else {
            self.git.blame = None;
            for editor in editors {
                editor.update(cx, |e, cx| e.set_blame(None, cx));
            }
        }
        let (title, body) = if self.git.blame_on {
            (
                "Inline blame is on",
                "The cursor's line shows who changed it last.",
            )
        } else {
            ("Inline blame is off", "")
        };
        self.transient_notice(title, body, cx);
    }

    pub(super) fn git_cursor_moved(
        &mut self,
        editor: &Entity<EditorView>,
        line: u32,
        cx: &mut Context<Self>,
    ) {
        if !self.git.blame_on || !git::available() {
            return;
        }
        let Some(root) = self
            .items
            .iter()
            .find(|(_, v)| matches!(v, ItemView::Editor(e) if e == editor))
            .map(|((r, _), _)| r.clone())
        else {
            return;
        };
        let weak = editor.downgrade();
        let line = line as usize;
        self.git.blame = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(BLAME_DELAY).await;
            // Read after the pause, so typing does not copy the whole buffer per keystroke.
            let Ok((path, contents)) = weak.read_with(cx, |e, _| {
                let contents = if e.is_dirty() { e.text() } else { None };
                (under_root(&root, e.path()), contents)
            }) else {
                return;
            };
            let found = cx
                .background_executor()
                .spawn(async move { git::blame_line(&root, &path, line, contents.as_deref()) })
                .await;
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            let caption = match found {
                Ok(Some(blame)) => Some((line, blame.caption(now))),
                Ok(None) => None,
                Err(err) => {
                    tracing::debug!("git blame: {err:#}");
                    None
                }
            };
            if let Some(editor) = weak.upgrade() {
                let _ = editor.update(cx, |e, cx| e.set_blame(caption, cx));
            }
        }));
    }

    pub(super) fn git_stage(&mut self, targets: Vec<PathBuf>, stage: bool, cx: &mut Context<Self>) {
        let Some(root) = self.workspace.active_project().map(|p| p.root.clone()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn(async move {
                    if stage {
                        git::stage(&root, &targets)
                    } else {
                        git::unstage(&root, &targets)
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(err) = done {
                    let title = if stage {
                        "Could not stage"
                    } else {
                        "Could not unstage"
                    };
                    this.transient_notice(title, format!("{err:#}"), cx);
                }
                this.refresh_git(cx);
            });
        })
        .detach();
    }

    fn change_rows(&self) -> Option<Vec<Row>> {
        let root = &self.workspace.active_project()?.root;
        let repo = self.git.repos.get(root)?;
        Some(change_rows(root, repo.prefix.as_deref()?, &repo.entries))
    }

    pub(super) fn render_changes_count(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let root = &self.workspace.active_project()?.root;
        let repo = self.git.repos.get(root)?;
        repo.prefix.as_ref()?;
        let n = repo
            .entries
            .iter()
            .filter(|(_, e)| e.unstaged != Some(FileStatus::Ignored))
            .count();
        let text = match n {
            0 => "No changes".to_string(),
            1 => "1 changed file".to_string(),
            n => format!("{n} changed files"),
        };
        let branch = repo.branch.clone().map(|b| format!("{b} · "));
        let conflicts = super::conflicts::conflicts_label(repo.conflicts);
        let t = cx.theme();
        Some(
            div()
                .flex()
                .gap(px(6.))
                .text_color(t.color.content_muted)
                .child(format!("{}{text}", branch.unwrap_or_default()))
                .children(
                    conflicts.map(|c| div().text_color(t.color.danger).child(format!("· {c}"))),
                )
                .into_any_element(),
        )
    }

    /// Cmd+Enter in the message box or the Commit button: commits what is staged, offering to
    /// stage everything first when nothing is, as VS Code does.
    pub(super) fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.git.committing {
            return;
        }
        let Some(input) = self.git.commit_input.clone() else {
            return;
        };
        let Some(rows) = self.change_rows() else {
            return;
        };
        let message = input.read(cx).text().trim().to_string();
        let amend = self.git.amend;
        if message.is_empty() {
            window.focus(&input.focus_handle(cx));
            return self.transient_notice("Type a commit message", "Then press ⌘↩ or Commit.", cx);
        }
        let staged = rows.iter().any(|r| {
            matches!(
                r,
                Row::File {
                    group: Group::Staged,
                    ..
                }
            )
        });
        let unstaged: Vec<PathBuf> = rows
            .iter()
            .filter_map(|r| match r {
                Row::File {
                    group: Group::Changes | Group::Untracked,
                    status,
                    targets,
                    ..
                } if *status != FileStatus::Conflict => Some(targets.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        if staged || amend {
            return self.run_commit(message, amend, Vec::new(), cx);
        }
        if unstaged.is_empty() {
            return self.transient_notice("Nothing to commit", "There are no changes.", cx);
        }
        let detail = format!(
            "Stage all {} changed files and commit them directly?",
            unstaged.len()
        );
        let answer = window.prompt(
            PromptLevel::Info,
            "There are no staged changes to commit.",
            Some(&detail),
            &["Stage All and Commit", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update(cx, |this, cx| this.run_commit(message, false, unstaged, cx));
            }
        })
        .detach();
    }

    fn run_commit(
        &mut self,
        message: String,
        amend: bool,
        stage_first: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root() else {
            return;
        };
        let message = match self.git.amend_body.as_deref().filter(|_| amend) {
            Some(body) => with_body(&message, body),
            None => message,
        };
        let amend_head = self.git.amend_head.clone().filter(|_| amend);
        self.git.committing = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let task_message = message.clone();
            let done = cx
                .background_executor()
                .spawn(async move {
                    if let Some(head) = amend_head
                        && git::last_commit(&root)?.0 != head
                    {
                        anyhow::bail!(
                            "The last commit changed since Amend filled in its message. Turn \
                             Amend off and on again to amend the new one."
                        );
                    }
                    if !stage_first.is_empty() {
                        git::stage(&root, &stage_first)?;
                    }
                    git::commit(&root, &task_message, amend)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.git.committing = false;
                match done {
                    Ok(()) => {
                        this.git.amend = false;
                        this.git.before_amend = None;
                        this.git.amend_body = None;
                        this.git.amend_head = None;
                        if let Some(input) = this.git.commit_input.clone() {
                            input.update(cx, |i, cx| i.set_text("", cx));
                        }
                        let title = if amend {
                            "Amended the last commit"
                        } else {
                            "Committed"
                        };
                        let subject = message.lines().next().unwrap_or_default().to_string();
                        this.transient_notice(title, subject, cx);
                    }
                    Err(err) => this.transient_notice("Commit failed", format!("{err:#}"), cx),
                }
                this.refresh_git(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Amend fills in the last commit's message, as VS Code's "Commit (Amend)" edits it.
    fn toggle_amend(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.git.commit_input.clone() else {
            return;
        };
        self.git.amend = !self.git.amend;
        cx.notify();
        if !self.git.amend {
            self.git.amend_body = None;
            self.git.amend_head = None;
            if let Some(before) = self.git.before_amend.take() {
                input.update(cx, |i, cx| i.set_text(before, cx));
            }
            return;
        }
        let Some(root) = self.active_root() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let last = cx
                .background_executor()
                .spawn(async move { git::last_commit(&root) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.git.amend {
                    return;
                }
                let Ok((head, last)) = last else {
                    return;
                };
                this.git.amend_head = Some(head);
                let typed = input.read(cx).text().to_string();
                this.git.before_amend = Some(typed);
                // The box holds one line, so the body waits aside and is sent back with it.
                let (subject, body) = last.split_once('\n').unwrap_or((&last, ""));
                let body = body.trim_matches('\n');
                this.git.amend_body = (!body.is_empty()).then(|| body.to_string());
                input.update(cx, |i, cx| i.set_text(subject.trim().to_string(), cx));
            });
        })
        .detach();
    }

    /// Discards unstaged changes (a copy is kept) or moves untracked files to the Trash, after
    /// asking.
    fn discard(
        &mut self,
        targets: Vec<PathBuf>,
        untracked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root() else {
            return;
        };
        let what = match targets.as_slice() {
            [one] => one.display().to_string(),
            many => format!("{} files", many.len()),
        };
        let (message, detail, button) = if untracked {
            (
                format!("Move {what} to the Trash?"),
                "You can put it back from the Trash in Finder.".to_string(),
                "Move to Trash",
            )
        } else {
            (
                format!("Discard changes in {what}?"),
                "The file goes back to its staged or committed version. Git cannot undo this; \
                 Athena keeps a copy of your version in Application Support/athena/discarded \
                 for 30 days."
                    .to_string(),
                "Discard Changes",
            )
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            Some(&detail),
            &[button, "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let task_root = root.clone();
            let done = cx
                .background_executor()
                .spawn(async move {
                    let backup = super::review::backup_dir()?;
                    for rel in &targets {
                        if untracked {
                            super::fileops::trash(&task_root.join(rel))?;
                        } else {
                            git::discard(&task_root, rel, &backup)?;
                        }
                    }
                    anyhow::Ok(())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(err) = done {
                    this.transient_notice("Could not discard", format!("{err:#}"), cx);
                }
                this.tree.invalidate();
                this.git_kick(cx);
            });
        })
        .detach();
    }

    fn render_commit_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let input = self.git.commit_input.clone()?;
        let root = &self.workspace.active_project()?.root;
        let branch = self.git.repos.get(root)?.branch.clone();
        let t = cx.theme().clone();
        let amend = self.git.amend;
        let label = match (self.git.committing, amend) {
            (true, _) => "Committing…",
            (false, true) => "Amend",
            (false, false) => "Commit",
        };
        Some(
            div()
                .flex_none()
                .h(px(40.))
                .px(px(12.))
                .flex()
                .items_center()
                .gap(px(8.))
                .border_b_1()
                .border_color(t.color.border)
                .text_size(t.typography.caption)
                .child(
                    div()
                        .id("git-branch")
                        .h(px(26.))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .flex_none()
                        .rounded(t.shape.radius_control)
                        .border_1()
                        .border_color(t.color.border)
                        .cursor_pointer()
                        .text_color(t.color.content_secondary)
                        .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                        .tooltip(|_, cx| Tooltip::view("Switch branch", cx))
                        .on_click(cx.listener(|this, _, window, cx| this.open_branches(window, cx)))
                        .child(format!(
                            "⎇ {}",
                            branch.unwrap_or_else(|| "no branch".into())
                        )),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h(px(26.))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .rounded(t.shape.radius_control)
                        .border_1()
                        .border_color(t.color.border)
                        .bg(t.color.surface)
                        .text_size(t.typography.body)
                        .child(input),
                )
                .child(
                    div()
                        .id("git-amend")
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(6.))
                        .cursor_pointer()
                        .text_color(if amend {
                            t.color.content
                        } else {
                            t.color.content_muted
                        })
                        .tooltip(|_, cx| Tooltip::view("Rewrite the last commit", cx))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_amend(cx)))
                        .child(
                            div()
                                .size(px(12.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(2.))
                                .border_1()
                                .border_color(if amend {
                                    t.color.accent
                                } else {
                                    t.color.border_strong
                                })
                                .when(amend, |el| el.bg(t.color.accent))
                                .text_color(t.color.content_on_accent)
                                .text_size(px(9.))
                                .when(amend, |el| el.child("✓")),
                        )
                        .child("Amend"),
                )
                .child(
                    Button::new("git-commit", label, ButtonKind::Primary)
                        .on_click(cx.listener(|this, _, window, cx| this.commit(window, cx))),
                )
                .into_any_element(),
        )
    }

    /// The Changes tab: a commit box above the staged, unstaged and untracked files.
    pub(super) fn render_changes(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let message = |text: &str| {
            div()
                .flex_1()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(t.typography.caption)
                .text_color(t.color.content_muted)
                .child(text.to_string())
                .into_any_element()
        };
        if !git::available() {
            return message("Git needs the Xcode Command Line Tools (xcode-select --install).");
        }
        let root = self.workspace.active_project().map(|p| p.root.clone());
        let known = root.as_ref().and_then(|r| self.git.repos.get(r));
        let Some(rows) = self.change_rows() else {
            return match known {
                Some(repo) if repo.checked => message("This project is not in a git repository."),
                _ => message("Reading git status…"),
            };
        };
        let body = if rows.is_empty() {
            message("No changes since the last commit.")
        } else {
            self.render_change_list(rows, root, cx)
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .children(self.render_commit_bar(cx))
            .child(div().flex_1().min_h_0().flex().child(body))
            .into_any_element()
    }

    fn render_change_list(
        &self,
        rows: Vec<Row>,
        root: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let rows = Rc::new(rows);
        uniform_list(
            "git-changes",
            rows.len(),
            cx.processor(move |_this, range: Range<usize>, _window, cx| {
                range
                    .map(|i| match &rows[i] {
                        Row::Header(group, count) => {
                            let group = *group;
                            let targets: Vec<PathBuf> = rows
                                .iter()
                                .filter_map(|r| match r {
                                    Row::File {
                                        group: g,
                                        targets,
                                        status,
                                        ..
                                    } if *g == group && *status != FileStatus::Conflict => {
                                        Some(targets.clone())
                                    }
                                    _ => None,
                                })
                                .flatten()
                                .collect();
                            let stage = group != Group::Staged;
                            let name = format!("changes-header-{i}");
                            let discard = targets.clone();
                            div()
                                .id(("git-header", i))
                                .group(name.clone())
                                .w_full()
                                .h(px(ROW_HEIGHT))
                                .px(px(12.))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .text_size(t.typography.caption)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(t.color.content_secondary)
                                .child(group.label())
                                .child(
                                    div()
                                        .px(px(6.))
                                        .rounded(t.shape.radius_control)
                                        .bg(t.color.surface_active)
                                        .text_color(t.color.content_muted)
                                        .child(count.to_string()),
                                )
                                .child(div().flex_1())
                                .when(group != Group::Staged, |el| {
                                    el.child(row_button(
                                        ("git-header-discard", i),
                                        "Discard All",
                                        name.clone(),
                                        &t,
                                        cx.listener(move |this, _, window, cx| {
                                            let untracked = group == Group::Untracked;
                                            this.discard(discard.clone(), untracked, window, cx)
                                        }),
                                    ))
                                })
                                .child(row_button(
                                    ("git-header-action", i),
                                    if stage { "Stage All" } else { "Unstage All" },
                                    name,
                                    &t,
                                    cx.listener(move |this, _, _, cx| {
                                        this.git_stage(targets.clone(), stage, cx)
                                    }),
                                ))
                                .into_any_element()
                        }
                        Row::File {
                            group,
                            path,
                            status,
                            targets,
                            is_dir,
                        } => {
                            let rel = root
                                .as_ref()
                                .map_or(path.as_path(), |r| path.strip_prefix(r).unwrap_or(path));
                            let name = rel
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let dir = rel
                                .parent()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default();
                            let color = status_color(*status, &t);
                            let deleted = *status == FileStatus::Deleted;
                            let conflict = *status == FileStatus::Conflict;
                            let group = *group;
                            let stage = group != Group::Staged;
                            let open = path.clone();
                            let file = path.clone();
                            let is_dir = *is_dir;
                            let targets = targets.clone();
                            let discard = targets.clone();
                            let hover = format!("changes-row-{i}");
                            let base = if group == Group::Staged {
                                DiffBase::Head
                            } else {
                                DiffBase::Index
                            };
                            div()
                                .id(("git-file", i))
                                .group(hover.clone())
                                .w_full()
                                .h(px(ROW_HEIGHT))
                                .pl(px(24.))
                                .pr(px(12.))
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .text_size(t.typography.caption)
                                .cursor_pointer()
                                .hover(|s| s.bg(t.color.surface_hover))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if is_dir {
                                        this.reveal_in_tree(&open, cx);
                                    } else if conflict {
                                        this.open_file(open.clone(), window, cx);
                                    } else {
                                        this.open_diff(open.clone(), base.clone(), window, cx);
                                    }
                                }))
                                .child(athena_ui::file_icon(path, is_dir, cx))
                                .child(
                                    div()
                                        .flex_none()
                                        .text_color(color)
                                        .when(deleted, |el| el.line_through())
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_color(t.color.content_muted)
                                        .child(dir),
                                )
                                .when(!is_dir && !deleted, |el| {
                                    el.child(row_button(
                                        ("git-file-open", i),
                                        "Open File",
                                        hover.clone(),
                                        &t,
                                        cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.open_file(file.clone(), window, cx)
                                        }),
                                    ))
                                })
                                .when(group != Group::Staged && !conflict, |el| {
                                    el.child(row_button(
                                        ("git-file-discard", i),
                                        "Discard",
                                        hover.clone(),
                                        &t,
                                        cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            let untracked = group == Group::Untracked;
                                            this.discard(discard.clone(), untracked, window, cx)
                                        }),
                                    ))
                                })
                                .child(row_button(
                                    ("git-file-action", i),
                                    if stage { "Stage" } else { "Unstage" },
                                    hover,
                                    &t,
                                    cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.git_stage(targets.clone(), stage, cx)
                                    }),
                                ))
                                .child(
                                    div()
                                        .w(px(12.))
                                        .flex_none()
                                        .text_color(color)
                                        .child(status.letter()),
                                )
                                .into_any_element()
                        }
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .size_full()
        .into_any_element()
    }
}

/// Binds the git remote and stash commands on the shell's root element.
pub(super) fn bind_git_actions(el: gpui::Div, cx: &mut Context<Shell>) -> gpui::Div {
    use crate::actions::{GitFetch, GitPopStash, GitPull, GitPush, GitStash};
    el.on_action(
        cx.listener(|this, _: &GitFetch, w, cx| this.git_remote(Remote::Fetch, false, w, cx)),
    )
    .on_action(cx.listener(|this, _: &GitPull, w, cx| this.git_remote(Remote::Pull, false, w, cx)))
    .on_action(cx.listener(|this, _: &GitPush, w, cx| this.git_remote(Remote::Push, false, w, cx)))
    .on_action(cx.listener(|this, _: &GitStash, _, cx| this.git_stash(false, cx)))
    .on_action(cx.listener(
        |this, _: &crate::actions::GitStashIncludeUntracked, _, cx| this.git_stash(true, cx),
    ))
    .on_action(cx.listener(|this, _: &GitPopStash, w, cx| this.open_stashes(w, cx)))
}

/// Where Publish Branch pushes: origin when there is one, else the only remote.
fn publish_remote(remotes: &[String]) -> Option<String> {
    match remotes {
        [only] => Some(only.clone()),
        many => many.iter().find(|r| *r == "origin").cloned(),
    }
}

/// A commit message from the one-line box plus the body kept aside while amending.
fn with_body(subject: &str, body: &str) -> String {
    format!("{}\n\n{body}", subject.trim_end())
}

fn change_rows(root: &Path, prefix: &str, entries: &[(PathBuf, Entry)]) -> Vec<Row> {
    let mut groups: [(Group, Vec<Row>); 3] = [
        (Group::Staged, Vec::new()),
        (Group::Changes, Vec::new()),
        (Group::Untracked, Vec::new()),
    ];
    for (path, entry) in entries {
        let rel = rel_to(root, path);
        let mut staged_targets = vec![rel.clone()];
        // Unstaging a rename has to put the old path back in the index too.
        if let Some(orig) = entry.orig.as_ref().and_then(|o| o.strip_prefix(prefix)) {
            staged_targets.push(PathBuf::from(orig));
        }
        let mut row = |group: Group, status: FileStatus, targets: Vec<PathBuf>| {
            let slot = &mut groups.iter_mut().find(|(g, _)| *g == group).unwrap().1;
            slot.push(Row::File {
                group,
                path: path.clone(),
                status,
                targets,
                is_dir: entry.is_dir(),
            });
        };
        match entry.unstaged {
            Some(FileStatus::Ignored) => continue,
            Some(FileStatus::Untracked) => row(Group::Untracked, FileStatus::Untracked, vec![rel]),
            Some(FileStatus::Conflict) => row(Group::Changes, FileStatus::Conflict, vec![rel]),
            Some(status) => {
                if let Some(staged) = entry.staged {
                    row(Group::Staged, staged, staged_targets);
                }
                row(Group::Changes, status, vec![rel]);
            }
            None => row(Group::Staged, entry.status(), staged_targets),
        }
    }
    let mut rows = Vec::new();
    for (group, files) in groups {
        if !files.is_empty() {
            rows.push(Row::Header(group, files.len()));
            rows.extend(files);
        }
    }
    rows
}

/// A small text button shown while its row is hovered.
fn row_button(
    id: (&'static str, usize),
    label: &'static str,
    group: String,
    t: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .px(px(6.))
        .rounded(t.shape.radius_control)
        .invisible()
        .group_hover(group, |s| s.visible())
        .cursor_pointer()
        .text_color(t.color.content_muted)
        .hover(|s| s.bg(t.color.surface_active).text_color(t.color.content))
        .on_click(on_click)
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishing_prefers_origin_and_asks_nothing_of_many_others() {
        let list = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(
            publish_remote(&list(&["upstream"])).as_deref(),
            Some("upstream")
        );
        assert_eq!(
            publish_remote(&list(&["fork", "origin"])).as_deref(),
            Some("origin")
        );
        assert_eq!(publish_remote(&list(&["a", "b"])), None);
        assert_eq!(publish_remote(&[]), None);
    }

    #[test]
    fn amending_sends_the_kept_body_back_after_the_subject() {
        let last = "Fix the parser\n\nIt dropped the last token.\nSecond line.";
        let (subject, body) = last.split_once('\n').unwrap();
        assert_eq!(
            with_body(subject, body.trim_matches('\n')),
            "Fix the parser\n\nIt dropped the last token.\nSecond line."
        );
    }

    #[test]
    fn an_untracked_folder_row_is_marked_as_a_folder() {
        let root = Path::new("/p");
        let entry = |path: &str| Entry {
            path: path.into(),
            orig: None,
            staged: None,
            unstaged: Some(FileStatus::Untracked),
        };
        let rows = change_rows(
            root,
            "",
            &[
                (root.join("notes"), entry("notes/")),
                (root.join("todo.md"), entry("todo.md")),
            ],
        );
        let dirs: Vec<(PathBuf, bool)> = rows
            .iter()
            .filter_map(|r| match r {
                Row::File { path, is_dir, .. } => Some((path.clone(), *is_dir)),
                Row::Header(..) => None,
            })
            .collect();
        assert_eq!(
            dirs,
            vec![(root.join("notes"), true), (root.join("todo.md"), false)]
        );
    }
}
