use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use athena_editor::{EditorView, GutterMark};
use athena_ui::{ActiveTheme, Theme};
use athena_workspace::git::{self, Decorations, Entry, FileStatus, Hunk};
use gpui::{
    AnyElement, Context, Entity, FontWeight, Hsla, Task, Window, div, prelude::*, px, uniform_list,
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

#[derive(Default)]
struct Repo {
    /// The project's path inside its repository; `None` while it is not in one.
    prefix: Option<String>,
    /// A status run has finished at least once, so a missing prefix means "not a repository".
    checked: bool,
    slow: bool,
    branch: Option<String>,
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
fn under_root(root: &Path, path: &Path) -> PathBuf {
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

    /// The branch for the title bar, from the last status run.
    pub(super) fn cached_branch(&self, root: &Path) -> Option<String> {
        self.git.repos.get(root)?.branch.clone()
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

    fn refresh_git(&mut self, cx: &mut Context<Self>) {
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
                    anyhow::Ok((prefix, snapshot))
                })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok((prefix, snapshot)) => this.git_status_arrived(&root, prefix, snapshot, cx),
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
        if first || *repo.entries != snapshot.entries || repo.branch != snapshot.branch {
            repo.branch = snapshot.branch;
            repo.decorations = Rc::new(Decorations::new(root, &snapshot.entries));
            repo.entries = Rc::new(snapshot.entries);
            cx.notify();
        }
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

    fn git_stage(&mut self, targets: Vec<PathBuf>, stage: bool, cx: &mut Context<Self>) {
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
        Some(
            div()
                .text_color(cx.theme().color.content_muted)
                .child(format!("{}{text}", branch.unwrap_or_default()))
                .into_any_element(),
        )
    }

    /// The Changes tab: staged, unstaged and untracked files with stage and unstage buttons.
    pub(super) fn render_changes(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let message = |text: &str| {
            div()
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
        if rows.is_empty() {
            return message("No changes since the last commit.");
        }
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
                                        group: g, targets, ..
                                    } if *g == group => Some(targets.clone()),
                                    _ => None,
                                })
                                .flatten()
                                .collect();
                            let stage = group != Group::Staged;
                            let name = format!("changes-header-{i}");
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
                            let stage = *group != Group::Staged;
                            let open = path.clone();
                            let is_dir = *is_dir;
                            let targets = targets.clone();
                            let hover = format!("changes-row-{i}");
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
                                .when(!deleted, |el| el.cursor_pointer())
                                .hover(|s| s.bg(t.color.surface_hover))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if is_dir {
                                        this.reveal_in_tree(&open, cx);
                                    } else if !deleted {
                                        this.pending_open = Some(open.clone());
                                        cx.notify();
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
