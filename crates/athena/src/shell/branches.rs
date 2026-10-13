use std::path::PathBuf;

use athena_workspace::git::{self, Branch, Commit, NewWorktree, Stash, Switch, Underway, Worktree};
use gpui::{ClipboardItem, Context, PromptLevel, Window, actions};

use super::Shell;
use super::lsp::{CONFIRMED, confirm_buttons};
use super::palette::Mode;

actions!(
    athena,
    [
        GitMergeBranch,
        GitRebaseOnto,
        GitDeleteBranch,
        GitRenameBranch,
        GitAbortMergeOrRebase,
        GitContinueRebase,
        GitCreateTag,
        GitPushTags,
        GitShowLog
    ]
);

/// What choosing a row of the branch picker does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum BranchPick {
    Switch(String),
    /// A remote branch with no local one yet: make a local branch tracking it.
    Track(String),
    Create(String),
    Stash {
        untracked: bool,
    },
    /// The stash's commit, as its index shifts when another is pushed.
    PopStash(String),
    /// A branch checked out in another worktree is opened there, as git cannot switch to it.
    OpenWorktree(PathBuf),
    /// Lists the branches a new worktree can check out.
    PickWorktreeBranch,
    AddWorktree(NewWorktree),
    RemoveWorktree(PathBuf),
    Merge(String),
    Rebase(String),
    Delete(String),
    /// The current branch's new name.
    Rename(String),
    Tag(String),
    /// Copies a commit's id.
    CopyCommit(String),
}

/// One picker row: label, detail, the text the query is matched against, and what it does.
pub(super) struct BranchEntry {
    pub label: String,
    pub detail: Option<String>,
    pub key: String,
    pub pick: BranchPick,
}

/// The picker's rows for `query`, VS Code's order: create from the query, local, then remote;
/// creating a worktree, stashing and the stashes to pop come last.
pub(super) fn entries(
    branches: &[Branch],
    stashes: &[Stash],
    worktrees: &[Worktree],
    query: &str,
) -> Vec<BranchEntry> {
    let typed = query.trim();
    let mut out = Vec::new();
    if !typed.is_empty() && !branches.iter().any(|b| b.name == typed) {
        let label = format!("Create new branch “{typed}”");
        out.push(BranchEntry {
            key: label.clone(),
            label,
            detail: Some("from the current commit".into()),
            pick: BranchPick::Create(typed.to_string()),
        });
    }
    let local: Vec<&str> = branches
        .iter()
        .filter(|b| !b.remote)
        .map(|b| b.name.as_str())
        .collect();
    let current = branches
        .iter()
        .find(|b| b.current && !b.remote)
        .map(|b| b.name.as_str());
    let elsewhere = |name: &str| {
        worktrees
            .iter()
            .find(|w| w.branch.as_deref() == Some(name))
            .map(|w| w.path.clone())
    };
    let (mine, remote): (Vec<&Branch>, Vec<&Branch>) = branches.iter().partition(|b| !b.remote);
    for b in mine.into_iter().chain(remote) {
        let pick = if !b.remote {
            BranchPick::Switch(b.name.clone())
        } else {
            match b.name.split_once('/') {
                Some((_, name)) if local.contains(&name) => BranchPick::Switch(name.to_string()),
                _ => BranchPick::Track(b.name.clone()),
            }
        };
        let mut detail = vec![b.when.clone(), b.subject.clone()];
        let pick = match &pick {
            BranchPick::Switch(name) if Some(name.as_str()) != current => match elsewhere(name) {
                Some(path) => {
                    detail.insert(
                        0,
                        format!("in worktree {}", crate::actions::display_path(&path)),
                    );
                    BranchPick::OpenWorktree(path)
                }
                None => pick,
            },
            _ => pick,
        };
        if b.current {
            detail.insert(0, "current".into());
        }
        detail.retain(|d| !d.is_empty());
        out.push(BranchEntry {
            label: b.name.clone(),
            detail: Some(detail.join(" · ")),
            key: b.name.clone(),
            pick,
        });
    }
    out.push(BranchEntry {
        label: "Create worktree…".into(),
        detail: None,
        key: "Create worktree…".into(),
        pick: BranchPick::PickWorktreeBranch,
    });
    for (label, untracked) in [
        ("Stash changes", false),
        ("Stash changes (include untracked)", true),
    ] {
        out.push(BranchEntry {
            label: label.into(),
            detail: None,
            key: label.into(),
            pick: BranchPick::Stash { untracked },
        });
    }
    out.extend(stash_entries(stashes));
    out
}

/// One row per stash, newest first, each popping it.
pub(super) fn stash_entries(stashes: &[Stash]) -> Vec<BranchEntry> {
    stashes
        .iter()
        .map(|s| {
            let label = format!("Pop stash@{{{}}}: {}", s.index, s.message);
            BranchEntry {
                key: label.clone(),
                label,
                detail: Some(s.when.clone()),
                pick: BranchPick::PopStash(s.commit.clone()),
            }
        })
        .collect()
}

/// Rows for merging into, rebasing onto or deleting: every branch the mode can use but the current.
pub(super) fn other_branch_entries(
    branches: &[Branch],
    worktrees: &[Worktree],
    mode: Mode,
) -> Vec<BranchEntry> {
    let checked_out = |name: &str| worktrees.iter().any(|w| w.branch.as_deref() == Some(name));
    branches
        .iter()
        .filter(|b| !b.current)
        .filter(|b| mode != Mode::DeleteBranch || (!b.remote && !checked_out(&b.name)))
        .map(|b| {
            let pick = match mode {
                Mode::MergeBranch => BranchPick::Merge(b.name.clone()),
                Mode::RebaseOnto => BranchPick::Rebase(b.name.clone()),
                _ => BranchPick::Delete(b.name.clone()),
            };
            let detail: Vec<&str> = [b.when.as_str(), b.subject.as_str()]
                .into_iter()
                .filter(|d| !d.is_empty())
                .collect();
            BranchEntry {
                label: b.name.clone(),
                detail: Some(detail.join(" · ")),
                key: b.name.clone(),
                pick,
            }
        })
        .collect()
}

/// The one row a typed name makes: renaming the current branch, or tagging HEAD.
pub(super) fn named_entries(mode: Mode, current: Option<&str>, query: &str) -> Vec<BranchEntry> {
    let name = query.trim();
    if name.is_empty() || name.starts_with('-') || name.contains(char::is_whitespace) {
        return Vec::new();
    }
    let (label, pick) = match (mode, current) {
        (Mode::RenameBranch, Some(current)) if current != name => (
            format!("Rename “{current}” to “{name}”"),
            BranchPick::Rename(name.to_string()),
        ),
        (Mode::NewTag, _) => (
            format!("Create tag “{name}” at the current commit"),
            BranchPick::Tag(name.to_string()),
        ),
        _ => return Vec::new(),
    };
    vec![BranchEntry {
        key: label.clone(),
        label,
        detail: None,
        pick,
    }]
}

pub(super) fn log_entries(commits: &[Commit], now: i64) -> Vec<BranchEntry> {
    commits
        .iter()
        .map(|c| {
            let short: String = c.sha.chars().take(7).collect();
            let mut detail = vec![
                short.clone(),
                c.author.clone(),
                git::relative_time(now - c.time),
            ];
            if !c.refs.is_empty() {
                detail.push(c.refs.clone());
            }
            BranchEntry {
                label: c.subject.clone(),
                detail: Some(detail.join(" · ")),
                key: format!("{} {short} {} {}", c.subject, c.author, c.refs),
                pick: BranchPick::CopyCommit(c.sha.clone()),
            }
        })
        .collect()
}

impl Shell {
    /// Lists branches or history in the background, then opens the palette in `mode` on them.
    pub(super) fn open_branch_mode(
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
                .spawn(async move {
                    let commits = match mode {
                        Mode::Log => git::repo_log(&root, 500)?,
                        _ => Vec::new(),
                    };
                    let worktrees = git::worktrees(&root).unwrap_or_default();
                    anyhow::Ok((git::branches(&root)?, worktrees, commits))
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match listed {
                Ok((branches, worktrees, commits)) => {
                    this.git.branches = branches;
                    this.git.worktrees = worktrees;
                    this.git.commits = commits;
                    this.open_palette(mode, window, cx);
                }
                Err(err) => {
                    this.transient_notice("Could not read the repository", format!("{err:#}"), cx)
                }
            });
        })
        .detach();
    }

    /// Abort Merge or Abort Rebase, whichever is under way.
    pub(super) fn abort_underway(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        let Some(what) = git::underway(&root) else {
            return self.transient_notice(
                "Nothing to abort",
                "No merge or rebase is under way.",
                cx,
            );
        };
        self.run_git_write(
            format!("Could not abort the {}", what.name()),
            move || git::abort(&root, what),
            cx,
        );
    }

    pub(super) fn continue_rebase(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        if git::underway(&root) != Some(Underway::Rebase) {
            return self.transient_notice(
                "No rebase under way",
                "There is nothing to continue.",
                cx,
            );
        }
        self.run_merge_like(
            "continue the rebase".into(),
            move || git::continue_rebase(&root),
            cx,
        );
    }

    pub(super) fn push_tags(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        self.run_git_write(
            "Could not push tags".into(),
            move || git::push_tags(&root),
            cx,
        );
    }

    /// Runs a git command that changes the repository, then refreshes; failures become a notice.
    fn run_git_write(
        &mut self,
        failed: String,
        op: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let done = cx.background_executor().spawn(async move { op() }).await;
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

    /// A merge, rebase or rebase step: stopping on conflicts is reported as that, not a failure.
    fn run_merge_like(
        &mut self,
        what: String,
        op: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let done = cx.background_executor().spawn(async move { op() }).await;
            let _ = this.update(cx, |this, cx| {
                match (done, git::underway(&root)) {
                    (Ok(()), _) => {}
                    (Err(_), Some(stopped)) => this.transient_notice(
                        format!("The {} stopped on conflicts", stopped.name()),
                        match stopped {
                            Underway::Merge => "Resolve them in Changes, then commit. Git: Abort merge or rebase gives up.",
                            Underway::Rebase => "Resolve and stage them in Changes, then run Git: Continue rebase, or Git: Abort merge or rebase.",
                        },
                        cx,
                    ),
                    (Err(err), None) => {
                        this.transient_notice(format!("Could not {what}"), format!("{err:#}"), cx)
                    }
                }
                this.tree.invalidate();
                this.git_kick(cx);
            });
        })
        .detach();
    }

    fn delete_branch(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let (repo, branch) = (root.clone(), name.clone());
            let unique = cx
                .background_executor()
                .spawn(async move { git::unique_commits(&repo, &branch) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let unique = match unique {
                    Ok(n) => n,
                    Err(err) => {
                        return this.transient_notice(format!("Could not delete {name}"), format!("{err:#}"), cx);
                    }
                };
                if unique == 0 {
                    let (repo, branch) = (root.clone(), name.clone());
                    return this.run_git_write(
                        format!("Could not delete {name}"),
                        move || git::delete_branch(&repo, &branch, false),
                        cx,
                    );
                }
                let s = if unique == 1 { "" } else { "s" };
                let answer = window.prompt(
                    PromptLevel::Warning,
                    &format!("Delete branch {name}?"),
                    Some(&format!("Its {unique} commit{s} are on no other branch or remote, and would be lost.")),
                    &confirm_buttons("Cancel", "Delete Anyway"),
                    cx,
                );
                cx.spawn_in(window, async move |this, cx| {
                    if answer.await == Ok(CONFIRMED) {
                        let _ = this.update(cx, |this, cx| {
                            this.run_git_write(
                                format!("Could not delete {name}"),
                                move || git::delete_branch(&root, &name, true),
                                cx,
                            )
                        });
                    }
                })
                .detach();
            });
        })
        .detach();
    }

    /// The branch picker: lists branches in the background, then opens the palette on them.
    pub(super) fn open_branches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        if !git::available() {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let listed = cx
                .background_executor()
                .spawn(async move {
                    let worktrees = git::worktrees(&root).unwrap_or_default();
                    anyhow::Ok((git::branches(&root)?, git::stashes(&root)?, worktrees))
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match listed {
                Ok((branches, stashes, worktrees)) => {
                    this.git.branches = branches;
                    this.git.stashes = stashes;
                    this.git.worktrees = worktrees;
                    this.open_palette(Mode::Branches, window, cx);
                }
                Err(err) => {
                    this.transient_notice("Could not list branches", format!("{err:#}"), cx)
                }
            });
        })
        .detach();
    }

    /// VS Code's "Pop Stash…": lists the stashes, newest first, to pick one.
    pub(super) fn open_stashes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        if !git::available() {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let listed = cx
                .background_executor()
                .spawn(async move { git::stashes(&root) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match listed {
                Ok(stashes) if stashes.is_empty() => {
                    this.transient_notice("No stashes", "There is nothing to pop.", cx)
                }
                Ok(stashes) => {
                    this.git.stashes = stashes;
                    this.open_palette(Mode::Stashes, window, cx);
                }
                Err(err) => this.transient_notice("Could not list stashes", format!("{err:#}"), cx),
            });
        })
        .detach();
    }

    pub(super) fn run_branch(
        &mut self,
        pick: BranchPick,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root() else {
            return;
        };
        let (name, how) = match pick {
            BranchPick::Switch(name) => (name, Switch::Existing),
            BranchPick::Track(name) => (name, Switch::Track),
            BranchPick::Create(name) => (name, Switch::Create),
            BranchPick::Stash { untracked } => return self.git_stash(untracked, cx),
            BranchPick::PopStash(commit) => return self.git_stash_pop(commit, cx),
            BranchPick::OpenWorktree(path) => return self.open_worktree(path, cx),
            BranchPick::PickWorktreeBranch => {
                return self.open_worktrees(Mode::NewWorktree, window, cx);
            }
            BranchPick::AddWorktree(how) => return self.add_worktree(how, window, cx),
            BranchPick::RemoveWorktree(path) => {
                return self.remove_worktree(path, false, window, cx);
            }
            BranchPick::Merge(name) => {
                let what = format!("merge {name}");
                return self.run_merge_like(what, move || git::merge(&root, &name), cx);
            }
            BranchPick::Rebase(name) => {
                let what = format!("rebase onto {name}");
                return self.run_merge_like(what, move || git::rebase(&root, &name), cx);
            }
            BranchPick::Delete(name) => return self.delete_branch(name, window, cx),
            BranchPick::Rename(to) => {
                let Some(from) = self.cached_branch(&root) else {
                    return;
                };
                return self.run_git_write(
                    format!("Could not rename {from}"),
                    move || git::rename_branch(&root, &from, &to),
                    cx,
                );
            }
            BranchPick::Tag(name) => {
                return self.run_git_write(
                    format!("Could not create tag {name}"),
                    move || git::create_tag(&root, &name),
                    cx,
                );
            }
            BranchPick::CopyCommit(sha) => {
                cx.write_to_clipboard(ClipboardItem::new_string(sha.clone()));
                let short: String = sha.chars().take(7).collect();
                return self.transient_notice("Commit id copied", short, cx);
            }
        };
        cx.spawn(async move |this, cx| {
            let task_name = name.clone();
            let done = cx
                .background_executor()
                .spawn(async move { git::switch(&root, &task_name, how) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(err) = done {
                    this.transient_notice(
                        format!("Could not switch to {name}"),
                        format!("{err:#}"),
                        cx,
                    );
                }
                this.tree.invalidate();
                this.git_kick(cx);
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(name: &str, remote: bool, current: bool) -> Branch {
        Branch {
            name: name.into(),
            remote,
            current,
            when: "2 days ago".into(),
            subject: "Work".into(),
        }
    }

    #[test]
    fn merging_rebasing_and_deleting_offer_every_usable_branch_but_the_current() {
        let list = [
            branch("main", false, true),
            branch("feat", false, false),
            branch("busy", false, false),
            branch("origin/feat", true, false),
        ];
        let busy = Worktree {
            path: "/w/busy".into(),
            branch: Some("busy".into()),
            head: String::new(),
            bare: false,
            locked: false,
            prunable: false,
        };
        let picks = |mode| -> Vec<BranchPick> {
            other_branch_entries(&list, std::slice::from_ref(&busy), mode)
                .into_iter()
                .map(|e| e.pick)
                .collect()
        };
        assert_eq!(
            picks(Mode::MergeBranch),
            [
                BranchPick::Merge("feat".into()),
                BranchPick::Merge("busy".into()),
                BranchPick::Merge("origin/feat".into()),
            ]
        );
        assert_eq!(
            picks(Mode::DeleteBranch),
            [BranchPick::Delete("feat".into())]
        );
    }

    #[test]
    fn a_typed_name_renames_the_branch_or_tags_the_commit() {
        let rename = named_entries(Mode::RenameBranch, Some("main"), " trunk ");
        assert_eq!(rename[0].pick, BranchPick::Rename("trunk".into()));
        assert!(named_entries(Mode::RenameBranch, Some("main"), "main").is_empty());
        assert!(named_entries(Mode::RenameBranch, None, "trunk").is_empty());
        assert!(named_entries(Mode::NewTag, None, "-f").is_empty());
        assert!(named_entries(Mode::NewTag, None, "v 1").is_empty());
        assert_eq!(
            named_entries(Mode::NewTag, None, "v1")[0].pick,
            BranchPick::Tag("v1".into())
        );
    }

    #[test]
    fn typing_a_new_name_offers_to_create_it_first() {
        let list = [
            branch("main", false, true),
            branch("origin/dev", true, false),
        ];
        let rows = entries(&list, &[], &[], "feat/x");
        assert_eq!(rows[0].pick, BranchPick::Create("feat/x".into()));
        assert_eq!(rows[1].pick, BranchPick::Switch("main".into()));
        assert_eq!(
            rows[1].detail.as_deref(),
            Some("current · 2 days ago · Work")
        );
        assert_eq!(rows[2].pick, BranchPick::Track("origin/dev".into()));
        assert!(
            entries(&list, &[], &[], "main")
                .iter()
                .all(|e| !matches!(e.pick, BranchPick::Create(_)))
        );
    }

    #[test]
    fn a_remote_branch_with_a_local_twin_switches_to_the_local_one() {
        let list = [
            branch("dev", false, false),
            branch("origin/dev", true, false),
        ];
        let rows = entries(&list, &[], &[], "");
        assert_eq!(rows[1].pick, BranchPick::Switch("dev".into()));
    }

    #[test]
    fn stashing_and_each_stash_follow_the_branches() {
        let stash = Stash {
            index: 0,
            commit: "c0ffee".into(),
            message: "On main: wip".into(),
            when: "1 hour ago".into(),
        };
        let rows = entries(
            &[branch("main", false, true)],
            std::slice::from_ref(&stash),
            &[],
            "",
        );
        let picks: Vec<&BranchPick> = rows.iter().map(|r| &r.pick).collect();
        assert_eq!(
            picks,
            [
                &BranchPick::Switch("main".into()),
                &BranchPick::PickWorktreeBranch,
                &BranchPick::Stash { untracked: false },
                &BranchPick::Stash { untracked: true },
                &BranchPick::PopStash("c0ffee".into()),
            ]
        );
        assert_eq!(rows[4].label, "Pop stash@{0}: On main: wip");
    }

    #[test]
    fn a_branch_checked_out_in_another_worktree_opens_that_worktree() {
        let list = [
            branch("main", false, true),
            branch("feat", false, false),
            branch("origin/feat", true, false),
            branch("origin/main", true, false),
        ];
        let worktrees = [
            Worktree {
                path: "/r".into(),
                branch: Some("main".into()),
                head: String::new(),
                bare: false,
                locked: false,
                prunable: false,
            },
            Worktree {
                path: "/r-feat".into(),
                branch: Some("feat".into()),
                head: String::new(),
                bare: false,
                locked: false,
                prunable: false,
            },
        ];
        let rows = entries(&list, &[], &worktrees, "");
        assert_eq!(rows[0].pick, BranchPick::Switch("main".into()));
        assert_eq!(rows[1].pick, BranchPick::OpenWorktree("/r-feat".into()));
        assert_eq!(
            rows[1].detail.as_deref(),
            Some("in worktree /r-feat · 2 days ago · Work")
        );
        assert_eq!(rows[2].pick, BranchPick::OpenWorktree("/r-feat".into()));
        assert_eq!(rows[3].pick, BranchPick::Switch("main".into()));
        assert_eq!(rows[3].detail.as_deref(), Some("2 days ago · Work"));
    }
}
