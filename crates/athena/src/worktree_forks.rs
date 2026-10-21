//! The commit each Claude worktree branched from, kept beside Athena's other per-app state so a
//! later "bring changes back" can squash exactly what the worktree added.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use athena_workspace::git::{self, NewWorktree};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fork {
    pub branch: String,
    pub fork_sha: String,
    pub main_root: PathBuf,
    /// Unix seconds.
    pub created: u64,
}

pub fn store() -> Result<PathBuf> {
    Ok(athena_proto::data_dir()?.join("worktrees"))
}

/// Branch names repeat across repositories, and sanitising can merge two names, so the file name
/// ends in a hash of the main checkout and the exact branch.
fn sidecar(store: &Path, main: &Path, branch: &str) -> PathBuf {
    let safe: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut hash = Sha256::new();
    hash.update(main.as_os_str().as_encoded_bytes());
    hash.update([0]);
    hash.update(branch.as_bytes());
    let tag: String = hash.finalize()[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    store.join(format!("{safe}-{tag}.json"))
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// `git worktree add -b branch` at `path` from the main checkout's HEAD, then notes the commit
/// the worktree started at.
pub fn create(store: &Path, main: &Path, path: &Path, branch: &str) -> Result<()> {
    git::add_worktree(main, path, &NewWorktree::Create(branch.to_string()))?;
    let fork = Fork {
        branch: branch.to_string(),
        fork_sha: git::head_sha(path)?,
        main_root: canonical(main),
        created: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    };
    write(store, &fork).context("the worktree was created, but its fork point was not recorded")
}

fn write(store: &Path, fork: &Fork) -> Result<()> {
    fs::create_dir_all(store)?;
    let file = sidecar(store, &fork.main_root, &fork.branch);
    let tmp = file.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(fork)?)?;
    fs::rename(&tmp, &file)?;
    Ok(())
}

/// Removes the worktree at `root` and the fork point recorded for its branch.
pub fn remove(store: &Path, main: &Path, root: &Path, force: bool) -> Result<()> {
    let branch = git::current_branch(root).ok().flatten();
    git::remove_worktree(main, root, force)?;
    if let Some(branch) = branch {
        let _ = fs::remove_file(sidecar(store, &canonical(main), &branch));
    }
    Ok(())
}

/// Where the linked worktree at `worktree` branched from; `None` for one Athena did not start
/// from the main checkout.
#[allow(dead_code)] // Called by "bring changes back", which is not built yet.
pub fn worktree_fork(worktree: &Path) -> Option<Fork> {
    fork_in(&store().ok()?, worktree)
}

fn fork_in(store: &Path, worktree: &Path) -> Option<Fork> {
    let main = git::main_worktree(worktree)?;
    let branch = git::current_branch(worktree).ok()??;
    let fork: Fork =
        serde_json::from_slice(&fs::read(sidecar(store, &main, &branch)).ok()?).ok()?;
    (fork.branch == branch && fork.main_root == main).then_some(fork)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("/usr/bin/git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=T", "-c", "user.email=t@x"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("athena-forks-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        sh(&dir, &["init", "-q", "-b", "main"]);
        sh(&dir, &["commit", "-q", "--allow-empty", "-m", "init"]);
        dir
    }

    #[test]
    fn a_new_worktree_records_the_commit_it_started_from_and_finishing_forgets_it() {
        if !git::available() {
            return;
        }
        let main = repo("fork");
        let store = main.with_extension("state");
        let tree = main.with_extension("wt");
        let _ = fs::remove_dir_all(&store);
        let _ = fs::remove_dir_all(&tree);
        let start = sh(&main, &["rev-parse", "HEAD"]);

        create(&store, &main, &tree, "claude/fix-login").unwrap();
        sh(&main, &["commit", "-q", "--allow-empty", "-m", "later"]);
        sh(&tree, &["commit", "-q", "--allow-empty", "-m", "work"]);

        let tree = tree.canonicalize().unwrap();
        let fork = fork_in(&store, &tree).unwrap();
        assert_eq!(fork.fork_sha, start);
        assert_eq!(fork.branch, "claude/fix-login");
        assert_eq!(fork.main_root, main);
        assert_eq!(fork_in(&store, &main), None);
        assert_eq!(fs::read_dir(&store).unwrap().count(), 1);

        remove(&store, &main, &tree, false).unwrap();
        assert_eq!(fs::read_dir(&store).unwrap().count(), 0);
        assert_eq!(fork_in(&store, &tree), None);
        fs::remove_dir_all(&main).unwrap();
        fs::remove_dir_all(&store).unwrap();
    }

    #[test]
    fn the_same_branch_name_in_two_repositories_keeps_two_records() {
        let store = Path::new("/s");
        assert_ne!(
            sidecar(store, Path::new("/a"), "claude/x"),
            sidecar(store, Path::new("/b"), "claude/x")
        );
        assert_ne!(
            sidecar(store, Path::new("/a"), "claude/x"),
            sidecar(store, Path::new("/a"), "claude-x")
        );
    }
}
