use std::path::PathBuf;

use athena_workspace::git::{self, Branch, NewWorktree, Stash, Switch, Worktree};
use gpui::{Context, Window};

use super::Shell;
use super::palette::Mode;

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
            BranchPick::Switch(name) if !b.current => match elsewhere(name) {
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

impl Shell {
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
            BranchPick::AddWorktree(how) => return self.add_worktree(how, cx),
            BranchPick::RemoveWorktree(path) => {
                return self.remove_worktree(path, false, window, cx);
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
    }
}
