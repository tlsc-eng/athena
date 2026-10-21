//! The commit each Claude worktree branched from, kept beside Athena's other per-app state so a
//! later "bring changes back" can squash exactly what the worktree added.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
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

/// Keyed by the worktree's folder, so switching branches or detaching HEAD inside it does not
/// lose the record.
fn sidecar(store: &Path, worktree: &Path) -> PathBuf {
    let name = worktree
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let digest = Sha256::digest(worktree.as_os_str().as_encoded_bytes());
    let tag: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    store.join(format!("{safe}-{tag}.json"))
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// `git worktree add -b branch` at `path` from the main checkout's HEAD, then notes the commit
/// the worktree started at. The outer error means no worktree was made; the inner one that it was
/// made but its fork point could not be saved.
pub fn create(
    store: Option<&Path>,
    main: &Path,
    path: &Path,
    branch: &str,
) -> Result<Option<anyhow::Error>> {
    git::add_worktree(main, path, &NewWorktree::Create(branch.to_string()))?;
    let saved = match store {
        Some(store) => record(store, main, path, branch),
        None => Err(anyhow::anyhow!("Athena's data folder is not available")),
    };
    Ok(saved.err())
}

fn record(store: &Path, main: &Path, path: &Path, branch: &str) -> Result<()> {
    let fork = Fork {
        branch: branch.to_string(),
        fork_sha: git::head_sha(path)?,
        main_root: canonical(main),
        created: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    };
    fs::create_dir_all(store)?;
    let file = sidecar(store, &canonical(path));
    let tmp = file.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&fork)?)?;
    fs::rename(&tmp, &file)?;
    Ok(())
}

/// Removes the worktree at `worktree`, run from `repo`, and the fork point recorded for it.
pub fn remove(store: Option<&Path>, repo: &Path, worktree: &Path, force: bool) -> Result<()> {
    // Once the folder is gone it can no longer be resolved to the path the record is keyed by.
    let file = store.map(|s| sidecar(s, &canonical(worktree)));
    git::remove_worktree(repo, worktree, force)?;
    if let Some(file) = file {
        let _ = fs::remove_file(file);
    }
    Ok(())
}

/// Where the worktree at `worktree` branched from; `None` for one Athena did not start from the
/// main checkout.
pub fn worktree_fork(worktree: &Path) -> Option<Fork> {
    fork_in(&store().ok()?, worktree)
}

fn fork_in(store: &Path, worktree: &Path) -> Option<Fork> {
    serde_json::from_slice(&fs::read(sidecar(store, &canonical(worktree))).ok()?).ok()
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

        assert!(
            create(Some(&store), &main, &tree, "claude/fix-login")
                .unwrap()
                .is_none()
        );
        sh(&main, &["commit", "-q", "--allow-empty", "-m", "later"]);
        sh(&tree, &["commit", "-q", "--allow-empty", "-m", "work"]);

        let tree = tree.canonicalize().unwrap();
        let fork = fork_in(&store, &tree).unwrap();
        assert_eq!(fork.fork_sha, start);
        assert_eq!(fork.branch, "claude/fix-login");
        assert_eq!(fork.main_root, main);
        assert_eq!(fork_in(&store, &main), None);
        assert_eq!(fs::read_dir(&store).unwrap().count(), 1);

        sh(&tree, &["switch", "-q", "--detach"]);
        assert_eq!(fork_in(&store, &tree).unwrap().fork_sha, start);

        remove(Some(&store), &main, &tree, false).unwrap();
        assert_eq!(fs::read_dir(&store).unwrap().count(), 0);
        assert_eq!(fork_in(&store, &tree), None);

        let gone = main.with_extension("gone");
        let _ = fs::remove_dir_all(&gone);
        assert!(
            create(Some(&store), &main, &gone, "claude/gone")
                .unwrap()
                .is_none()
        );
        let gone = gone.canonicalize().unwrap();
        assert_eq!(fs::read_dir(&store).unwrap().count(), 1);
        fs::remove_dir_all(&gone).unwrap();
        remove(Some(&store), &main, &gone, false).unwrap();
        assert_eq!(fs::read_dir(&store).unwrap().count(), 0);
        fs::remove_dir_all(&main).unwrap();
        fs::remove_dir_all(&store).unwrap();
    }

    #[test]
    fn worktree_folders_with_the_same_name_keep_separate_records() {
        let store = Path::new("/s");
        let (a, b) = (
            sidecar(store, Path::new("/a/repo-x")),
            sidecar(store, Path::new("/b/repo-x")),
        );
        assert_ne!(a, b);
        assert!(a.to_string_lossy().starts_with("/s/repo-x-"));
    }

    #[test]
    fn a_fork_point_that_cannot_be_saved_leaves_the_new_worktree_in_place() {
        if !git::available() {
            return;
        }
        let main = repo("unsaved");
        let tree = main.with_extension("wt");
        let _ = fs::remove_dir_all(&tree);
        let blocked = main.join("not-a-dir");
        fs::write(&blocked, "").unwrap();

        let unsaved = create(Some(&blocked), &main, &tree, "claude/x").unwrap();
        assert!(unsaved.is_some() && tree.is_dir());
        assert_eq!(fork_in(&blocked, &tree), None);
        remove(Some(&blocked), &main, &tree, false).unwrap();
        fs::remove_dir_all(&main).unwrap();
    }
}
