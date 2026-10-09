use std::path::{Path, PathBuf};

use athena_workspace::git::{self, Branch, NewWorktree, Worktree};
use gpui::{Context, PromptLevel, Window, actions};

use super::Shell;
use super::branches::{BranchEntry, BranchPick};
use super::item::ItemView;
use super::lsp::{CONFIRMED, confirm_buttons};
use super::palette::Mode;
use crate::actions::display_path;

actions!(
    athena,
    [GitOpenWorktree, GitCreateWorktree, GitDeleteWorktree]
);

/// Where a worktree for `branch` goes: beside the main worktree, as `<repo>-<branch>`, with a
/// number added while that folder is taken.
pub(super) fn worktree_path(main: &Path, branch: &str, taken: impl Fn(&Path) -> bool) -> PathBuf {
    let slug: String = branch
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches(|c| c == '-' || c == '.');
    let repo = main
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "worktree".into());
    let parent = main.parent().unwrap_or(main);
    let base = parent.join(format!("{repo}-{slug}"));
    let mut path = base.clone();
    let mut n = 2;
    while taken(&path) {
        path = PathBuf::from(format!("{}-{n}", base.display()));
        n += 1;
    }
    path
}

fn worktree_label(w: &Worktree) -> String {
    w.path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| w.path.display().to_string())
}

fn worktree_detail(w: &Worktree, main: bool) -> String {
    let mut detail = vec![match &w.branch {
        Some(branch) => branch.clone(),
        None => format!("detached at {}", w.head.chars().take(7).collect::<String>()),
    }];
    if main {
        detail.push("main worktree".into());
    }
    if w.locked {
        detail.push("locked".into());
    }
    detail.push(display_path(&w.path));
    detail.join(" · ")
}

/// Rows of "Open worktree…": every worktree with a folder, then creating one.
pub(super) fn open_entries(worktrees: &[Worktree]) -> Vec<BranchEntry> {
    let mut out: Vec<BranchEntry> = worktrees
        .iter()
        .enumerate()
        .filter(|(_, w)| !w.bare && !w.prunable)
        .map(|(i, w)| BranchEntry {
            label: worktree_label(w),
            detail: Some(worktree_detail(w, i == 0)),
            key: worktree_label(w),
            pick: BranchPick::OpenWorktree(w.path.clone()),
        })
        .collect();
    out.push(BranchEntry {
        label: "Create worktree…".into(),
        detail: None,
        key: "Create worktree…".into(),
        pick: BranchPick::PickWorktreeBranch,
    });
    out
}

/// Rows of "Delete worktree…": the linked worktrees, never the main one.
pub(super) fn remove_entries(worktrees: &[Worktree]) -> Vec<BranchEntry> {
    worktrees
        .iter()
        .skip(1)
        .filter(|w| !w.bare)
        .map(|w| BranchEntry {
            label: worktree_label(w),
            detail: Some(worktree_detail(w, false)),
            key: worktree_label(w),
            pick: BranchPick::RemoveWorktree(w.path.clone()),
        })
        .collect()
}

/// Rows of "Create worktree…": a new branch named by the query, then the local branches no
/// worktree has checked out, then remote branches with no local one.
pub(super) fn new_entries(
    branches: &[Branch],
    worktrees: &[Worktree],
    query: &str,
) -> Vec<BranchEntry> {
    let typed = query.trim();
    let mut out = Vec::new();
    if !typed.is_empty() && !branches.iter().any(|b| b.name == typed) {
        let label = format!("Create worktree with new branch “{typed}”");
        out.push(BranchEntry {
            key: label.clone(),
            label,
            detail: Some("from the current commit".into()),
            pick: BranchPick::AddWorktree(NewWorktree::Create(typed.to_string())),
        });
    }
    let checked_out = |name: &str| worktrees.iter().any(|w| w.branch.as_deref() == Some(name));
    let local: Vec<&str> = branches
        .iter()
        .filter(|b| !b.remote)
        .map(|b| b.name.as_str())
        .collect();
    let (mine, remote): (Vec<&Branch>, Vec<&Branch>) = branches.iter().partition(|b| !b.remote);
    for b in mine.into_iter().chain(remote) {
        let pick = if !b.remote {
            if checked_out(&b.name) {
                continue;
            }
            NewWorktree::Existing(b.name.clone())
        } else {
            match b.name.split_once('/') {
                Some((_, name)) if !local.contains(&name) => NewWorktree::Track(b.name.clone()),
                _ => continue,
            }
        };
        let detail: Vec<String> = [b.when.clone(), b.subject.clone()]
            .into_iter()
            .filter(|d| !d.is_empty())
            .collect();
        out.push(BranchEntry {
            label: b.name.clone(),
            detail: Some(detail.join(" · ")),
            key: b.name.clone(),
            pick: BranchPick::AddWorktree(pick),
        });
    }
    out
}

impl Shell {
    /// Lists the worktrees and branches in the background, then opens the palette in `mode`.
    pub(super) fn open_worktrees(
        &mut self,
        mode: Mode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root() else {
            return;
        };
        if !git::available() {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let listed = cx
                .background_executor()
                .spawn(async move { anyhow::Ok((git::worktrees(&root)?, git::branches(&root)?)) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match listed {
                Ok((worktrees, _)) if mode == Mode::RemoveWorktree && worktrees.len() < 2 => this
                    .transient_notice(
                        "No worktrees to delete",
                        "This repository has only its main worktree.",
                        cx,
                    ),
                Ok((worktrees, branches)) => {
                    this.git.worktrees = worktrees;
                    this.git.branches = branches;
                    this.open_palette(mode, window, cx);
                }
                Err(err) => {
                    this.transient_notice("Could not list worktrees", format!("{err:#}"), cx)
                }
            });
        })
        .detach();
    }

    /// Opens a worktree as a project, grouped under its main repository in the rail.
    pub(super) fn open_worktree(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !path.is_dir() {
            return self.transient_notice(
                "Worktree folder is missing",
                format!("{} no longer exists.", display_path(&path)),
                cx,
            );
        }
        self.open_folder(path, cx);
    }

    /// `git worktree add` beside the main worktree, then opens the new worktree.
    pub(super) fn add_worktree(&mut self, how: NewWorktree, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        let main = self
            .git
            .worktrees
            .first()
            .filter(|w| !w.bare)
            .map(|w| w.path.clone())
            .unwrap_or_else(|| root.clone());
        let branch = match &how {
            NewWorktree::Existing(b) | NewWorktree::Create(b) => b.clone(),
            NewWorktree::Track(remote) => remote
                .split_once('/')
                .map_or(remote.clone(), |(_, b)| b.to_string()),
        };
        let path = worktree_path(&main, &branch, |p| p.exists());
        cx.spawn(async move |this, cx| {
            let target = path.clone();
            let done = cx
                .background_executor()
                .spawn(async move { git::add_worktree(&root, &target, &how) })
                .await;
            let _ = this.update(cx, |this, cx| match done {
                Ok(()) => {
                    this.transient_notice(
                        "Worktree created",
                        format!("{branch} is checked out in {}.", display_path(&path)),
                        cx,
                    );
                    this.open_folder(path, cx);
                }
                Err(err) => this.transient_notice(
                    format!("Could not create a worktree for {branch}"),
                    format!("{err:#}"),
                    cx,
                ),
            });
        })
        .detach();
    }

    /// `git worktree remove`; one with changes is deleted only after the user agrees.
    pub(super) fn remove_worktree(
        &mut self,
        path: PathBuf,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root() else {
            return;
        };
        let roots: Vec<PathBuf> = self
            .workspace
            .projects
            .iter()
            .map(|p| p.root.clone())
            .collect();
        let cwds: Vec<PathBuf> = self
            .items
            .values()
            .filter_map(|item| match item {
                ItemView::Terminal(v) => v.read(cx).foreground()?.1,
                _ => None,
            })
            .collect();
        if let Some((title, body)) = worktree_in_use(&path, &roots, &cwds) {
            return self.transient_notice(title, body, cx);
        }
        cx.spawn_in(window, async move |this, cx| {
            let target = path.clone();
            let done = cx
                .background_executor()
                .spawn(async move { git::remove_worktree(&root, &target, force) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match done {
                Ok(()) => {
                    this.transient_notice("Worktree deleted", display_path(&path), cx);
                    this.git_kick(cx);
                }
                Err(err) if err.is::<git::Dirty>() => {
                    let message = format!(
                        "{} has uncommitted or untracked changes.",
                        display_path(&path)
                    );
                    let answer = window.prompt(
                        PromptLevel::Warning,
                        &message,
                        Some("Deleting the worktree discards them. Its branch is kept."),
                        &confirm_buttons("Cancel", "Delete Anyway"),
                        cx,
                    );
                    cx.spawn_in(window, async move |this, cx| {
                        if answer.await == Ok(CONFIRMED) {
                            let _ = this.update_in(cx, |this, window, cx| {
                                this.remove_worktree(path, true, window, cx)
                            });
                        }
                    })
                    .detach();
                }
                Err(err) => {
                    this.transient_notice("Could not delete the worktree", format!("{err:#}"), cx)
                }
            });
        })
        .detach();
    }

    /// The main worktree of the project at `root` when it is a linked worktree, cached.
    pub(super) fn worktree_main(&self, root: &Path) -> Option<PathBuf> {
        self.git.worktree_mains.get(root).cloned().flatten()
    }

    /// Reads which open projects are linked worktrees, for those not read yet.
    pub(super) fn note_worktree_mains(&mut self) {
        for project in &self.workspace.projects {
            if !self.git.worktree_mains.contains_key(&project.root) {
                let main = git::main_worktree(&project.root);
                self.git.worktree_mains.insert(project.root.clone(), main);
            }
        }
    }

    /// Orders the rail so each linked worktree follows its main repository, when that is open.
    pub(super) fn group_worktrees(&mut self) {
        self.note_worktree_mains();
        let roots: Vec<PathBuf> = self
            .workspace
            .projects
            .iter()
            .map(|p| p.root.clone())
            .collect();
        let order = rail_order(&roots, |root| self.worktree_main(root));
        if order.iter().copied().eq(0..roots.len()) {
            return;
        }
        let active = self.workspace.active.and_then(|a| roots.get(a).cloned());
        let mut projects: Vec<Option<athena_workspace::Project>> =
            std::mem::take(&mut self.workspace.projects)
                .into_iter()
                .map(Some)
                .collect();
        self.workspace.projects = order.iter().filter_map(|&i| projects[i].take()).collect();
        self.workspace.active = active.and_then(|a| roots_index(&self.workspace.projects, &a));
    }
}

/// Project indices in rail order: each linked worktree right after its main repository when
/// that is open, everything else where it was.
fn rail_order(roots: &[PathBuf], main_of: impl Fn(&Path) -> Option<PathBuf>) -> Vec<usize> {
    let main_of = |root: &Path| main_of(root).filter(|m| roots.contains(m));
    let mut order = Vec::with_capacity(roots.len());
    for (i, root) in roots.iter().enumerate() {
        if main_of(root).is_some() {
            continue;
        }
        order.push(i);
        order.extend((0..roots.len()).filter(|&j| main_of(&roots[j]).as_ref() == Some(root)));
    }
    order
}

/// Why the worktree at `path` cannot be deleted now: a project open in it, or a terminal there.
fn worktree_in_use(
    path: &Path,
    roots: &[PathBuf],
    cwds: &[PathBuf],
) -> Option<(&'static str, &'static str)> {
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let path = real(path);
    if roots.iter().any(|r| real(r).starts_with(&path)) {
        return Some((
            "The worktree is open",
            "Close its project first, then delete the worktree.",
        ));
    }
    if cwds.iter().any(|c| real(c).starts_with(&path)) {
        return Some((
            "A terminal is in the worktree",
            "Leave the folder or close the terminal first, then delete the worktree.",
        ));
    }
    None
}

fn roots_index(projects: &[athena_workspace::Project], root: &Path) -> Option<usize> {
    projects.iter().position(|p| p.root == root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worktree(path: &str, branch: Option<&str>) -> Worktree {
        Worktree {
            path: PathBuf::from(path),
            branch: branch.map(String::from),
            head: "0123456789".into(),
            bare: false,
            locked: false,
            prunable: false,
        }
    }

    fn branch(name: &str, remote: bool) -> Branch {
        Branch {
            name: name.into(),
            remote,
            current: false,
            when: "1 day ago".into(),
            subject: "Work".into(),
        }
    }

    #[test]
    fn a_worktree_with_a_project_or_terminal_inside_it_is_in_use() {
        let dir = std::env::temp_dir().join(format!("athena-wt-use-{}", std::process::id()));
        let (wt, sub, other) = (
            dir.join("repo-feat"),
            dir.join("repo-feat/src"),
            dir.join("b"),
        );
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let real = |p: &Path| p.canonicalize().unwrap();
        let others = std::slice::from_ref(&other);
        assert_eq!(worktree_in_use(&wt, others, others), None);
        let open = worktree_in_use(&wt, &[other.clone(), real(&sub)], &[]).unwrap();
        assert_eq!(open.0, "The worktree is open");
        let shell = worktree_in_use(&real(&wt), &[], std::slice::from_ref(&sub)).unwrap();
        assert_eq!(shell.0, "A terminal is in the worktree");
        // A sibling whose name only starts the same is not inside it.
        assert_eq!(
            worktree_in_use(&dir.join("repo"), std::slice::from_ref(&wt), &[]),
            None
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn worktrees_follow_their_open_main_repository_in_the_rail() {
        let roots: Vec<PathBuf> = ["/a-feat", "/b", "/a", "/c-x", "/a-fix"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let main_of = |root: &Path| match root.to_str()? {
            "/a-feat" | "/a-fix" => Some(PathBuf::from("/a")),
            "/c-x" => Some(PathBuf::from("/c")),
            _ => None,
        };
        assert_eq!(rail_order(&roots, main_of), [1, 2, 0, 4, 3]);
    }

    #[test]
    fn new_worktrees_go_beside_the_main_one_named_after_the_branch() {
        let main = Path::new("/code/athena");
        assert_eq!(
            worktree_path(main, "feat/x y", |_| false),
            PathBuf::from("/code/athena-feat-x-y")
        );
        let taken = |p: &Path| p == Path::new("/code/athena-dev");
        assert_eq!(
            worktree_path(main, "dev", taken),
            PathBuf::from("/code/athena-dev-2")
        );
    }

    #[test]
    fn creating_offers_a_new_branch_and_branches_no_worktree_has() {
        let branches = [
            branch("main", false),
            branch("free", false),
            branch("origin/free", true),
            branch("origin/remote-only", true),
        ];
        let worktrees = [worktree("/r", Some("main"))];
        let picks: Vec<BranchPick> = new_entries(&branches, &worktrees, "new")
            .into_iter()
            .map(|e| e.pick)
            .collect();
        assert_eq!(
            picks,
            [
                BranchPick::AddWorktree(NewWorktree::Create("new".into())),
                BranchPick::AddWorktree(NewWorktree::Existing("free".into())),
                BranchPick::AddWorktree(NewWorktree::Track("origin/remote-only".into())),
            ]
        );
        assert!(
            new_entries(&branches, &worktrees, "free")
                .iter()
                .all(|e| !matches!(e.pick, BranchPick::AddWorktree(NewWorktree::Create(_))))
        );
    }

    #[test]
    fn the_main_worktree_opens_but_is_never_offered_for_deletion() {
        let mut gone = worktree("/r-gone", None);
        gone.prunable = true;
        let list = [
            worktree("/r", Some("main")),
            worktree("/r-feat", Some("feat")),
            gone,
        ];
        let open = open_entries(&list);
        assert_eq!(open.len(), 3);
        assert_eq!(open[0].detail.as_deref(), Some("main · main worktree · /r"));
        assert_eq!(open[2].pick, BranchPick::PickWorktreeBranch);
        let remove: Vec<BranchPick> = remove_entries(&list).into_iter().map(|e| e.pick).collect();
        assert_eq!(
            remove,
            [
                BranchPick::RemoveWorktree("/r-feat".into()),
                BranchPick::RemoveWorktree("/r-gone".into()),
            ]
        );
        assert_eq!(
            worktree_detail(&list[2], false),
            "detached at 0123456 · /r-gone"
        );
    }
}
