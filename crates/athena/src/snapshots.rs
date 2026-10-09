//! Copies of files as they were before a Claude Code session first edited them, so the session's
//! edits can be reviewed as one diff. Taken by the PreToolUse hook, read by the window.

use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, bail};
use sha2::{Digest, Sha256};

/// Sessions untouched for this long are deleted.
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// Past this, the oldest sessions go until the store fits again.
const MAX_BYTES: u64 = 200 * 1024 * 1024;
/// Bigger files are not copied; the diff view would not show them either.
const MAX_FILE: u64 = 20 * 1024 * 1024;

/// What a session's snapshot says about a file before its first edit.
#[derive(Debug, PartialEq, Eq)]
pub enum Before {
    /// No snapshot was taken (hooks off, or the file was first edited in another session).
    Unknown,
    /// The hook saw the file but kept no copy: it was too large or not a regular file.
    Skipped,
    /// The session created the file.
    Absent,
    Text(Vec<u8>),
}

pub fn store() -> Result<PathBuf> {
    Ok(athena_proto::data_dir()?.join("snapshots"))
}

/// Claude Code session ids are UUIDs; anything else must not become a directory name, nor a
/// word on a command line that reads as a flag.
pub fn valid_session(session: &str) -> bool {
    session.starts_with(|c: char| c.is_ascii_alphanumeric())
        && session.len() <= 128
        && session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn key(path: &Path) -> String {
    let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// Copies `path` into the session's snapshot unless the session already has one of it.
pub fn take(store: &Path, session: &str, path: &Path) -> Result<()> {
    if !valid_session(session) || !path.is_absolute() {
        bail!("bad snapshot request");
    }
    let dir = store.join(session);
    let fresh = !dir.exists();
    let base = dir.join(key(path));
    let marker = base.with_extension("new");
    // Without a skip marker, a later edit would snapshot the file as Claude already changed it.
    let skip = base.with_extension("skip");
    if base.exists() || marker.exists() || skip.exists() {
        return Ok(());
    }
    fs::create_dir_all(&dir)?;
    let mut sidecar = fs::File::create(base.with_extension("path"))?;
    sidecar.write_all(path.as_os_str().as_encoded_bytes())?;
    match fs::metadata(path) {
        // Copying a FIFO or device would block the hook or never end.
        Ok(meta) if meta.len() > MAX_FILE || !meta.is_file() => {
            fs::File::create(&skip)?;
        }
        Ok(_) => {
            let tmp = base.with_extension("tmp");
            fs::copy(path, &tmp)?;
            fs::rename(&tmp, &base)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::File::create(&marker)?;
        }
        Err(e) => return Err(e.into()),
    }
    if fresh {
        prune(store, session, SystemTime::now(), MAX_AGE, MAX_BYTES);
    }
    Ok(())
}

pub fn read(store: &Path, session: &str, path: &Path) -> Before {
    if !valid_session(session) {
        return Before::Unknown;
    }
    let base = store.join(session).join(key(path));
    if base.with_extension("skip").exists() {
        return Before::Skipped;
    }
    if base.with_extension("new").exists() {
        return Before::Absent;
    }
    match fs::read(&base) {
        Ok(text) => Before::Text(text),
        Err(_) => Before::Unknown,
    }
}

/// A session in the store and the files it kept a copy of before their first edit.
#[derive(Debug, PartialEq, Eq)]
pub struct Stored {
    pub session: String,
    pub modified: SystemTime,
    pub files: Vec<PathBuf>,
}

/// The files `session` snapshotted, by path.
pub fn files(store: &Path, session: &str) -> Vec<PathBuf> {
    if !valid_session(session) {
        return Vec::new();
    }
    let mut files: Vec<PathBuf> = fs::read_dir(store.join(session))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "path"))
        .filter_map(|e| fs::read(e.path()).ok())
        .map(|bytes| PathBuf::from(std::ffi::OsString::from_vec(bytes)))
        .collect();
    files.sort();
    files
}

/// Sessions that snapshotted a file under `root`, newest first, each with only those files.
pub fn sessions_under(store: &Path, root: &Path) -> Vec<Stored> {
    let mut out: Vec<Stored> = fs::read_dir(store)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let session = e.file_name().to_str()?.to_string();
            let modified = e.metadata().and_then(|m| m.modified()).ok()?;
            let files: Vec<PathBuf> = files(store, &session)
                .into_iter()
                .filter(|f| f.starts_with(root))
                .collect();
            (!files.is_empty()).then_some(Stored {
                session,
                modified,
                files,
            })
        })
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out
}

/// Puts `path` back as it was before `session` first edited it, deleting it if the session
/// created it; the current file is first copied into `backup` under its path below `root`.
pub fn revert(store: &Path, session: &str, root: &Path, path: &Path, backup: &Path) -> Result<()> {
    let before = read(store, session, path);
    if matches!(before, Before::Unknown | Before::Skipped) {
        bail!(
            "no copy of {} from before the session was kept",
            path.display()
        );
    }
    let rel = path
        .strip_prefix(root)
        .ok()
        .or_else(|| path.file_name().map(Path::new))
        .context("the file has no name")?;
    if path.is_file() {
        let to = backup.join(rel);
        fs::create_dir_all(to.parent().unwrap_or(backup))?;
        fs::copy(path, &to).with_context(|| format!("keep a copy of {}", rel.display()))?;
    }
    match before {
        Before::Text(bytes) => write_through(path, &bytes)?,
        _ => match fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        },
    }
    Ok(())
}

/// Replaces the file `path` names, through any links, by a fully written copy of `bytes`.
fn write_through(path: &Path, bytes: &[u8]) -> Result<()> {
    static TEMPS: AtomicU64 = AtomicU64::new(0);
    let target = link_target(path);
    let dir = target.parent().context("the file has no folder")?;
    fs::create_dir_all(dir)?;
    let name = target.file_name().context("the file has no name")?;
    let tmp = dir.join(format!(
        ".{}.{}.{}.athena-revert",
        name.to_string_lossy(),
        std::process::id(),
        TEMPS.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        out.write_all(bytes)?;
        out.sync_all()?;
        if let Ok(meta) = fs::metadata(&target) {
            fs::set_permissions(&tmp, meta.permissions())?;
        }
        fs::rename(&tmp, &target)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.with_context(|| format!("write {}", target.display()))
}

/// Where `path` leads once its links are followed, even to a file that no longer exists.
fn link_target(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    let mut at = path.to_path_buf();
    for _ in 0..40 {
        match fs::read_link(&at) {
            Ok(next) => {
                at = at
                    .parent()
                    .map_or_else(|| next.clone(), |dir| dir.join(&next))
            }
            Err(_) => break,
        }
    }
    at
}

/// Deletes sessions older than `max_age`, then the oldest others until the store is under
/// `max_bytes`; `keep` is the session being written and always stays.
pub fn prune(store: &Path, keep: &str, now: SystemTime, max_age: Duration, max_bytes: u64) {
    let Ok(entries) = fs::read_dir(store) else {
        return;
    };
    let mut sessions: Vec<(SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_name() != keep && e.path().is_dir())
        .map(|e| {
            let path = e.path();
            let modified = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (modified, size(&path), path)
        })
        .collect();
    sessions.sort();
    let mut total: u64 = sessions.iter().map(|s| s.1).sum::<u64>() + size(&store.join(keep));
    for (modified, bytes, path) in sessions {
        let old = now.duration_since(modified).unwrap_or_default() > max_age;
        if (old || total > max_bytes) && fs::remove_dir_all(&path).is_ok() {
            total -= bytes;
        }
    }
}

fn size(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("athena-snap-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_first_edit_is_kept_and_later_ones_do_not_replace_it() {
        let dir = temp("first");
        let (store, file) = (dir.join("store"), dir.join("a.go"));
        fs::write(&file, "before\n").unwrap();
        take(&store, "s-1", &file).unwrap();
        fs::write(&file, "after one edit\n").unwrap();
        take(&store, "s-1", &file).unwrap();
        assert_eq!(
            read(&store, "s-1", &file),
            Before::Text(b"before\n".to_vec())
        );
        assert_eq!(read(&store, "s-2", &file), Before::Unknown);

        let created = dir.join("new.go");
        take(&store, "s-1", &created).unwrap();
        fs::write(&created, "x").unwrap();
        take(&store, "s-1", &created).unwrap();
        assert_eq!(read(&store, "s-1", &created), Before::Absent);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn session_ids_cannot_escape_the_store() {
        let dir = temp("escape");
        let file = dir.join("a");
        fs::write(&file, "x").unwrap();
        for bad in ["", "..", "../x", "a/b", "a b"] {
            assert!(take(&dir.join("store"), bad, &file).is_err(), "{bad:?}");
        }
        assert!(take(&dir.join("store"), "ok", Path::new("relative")).is_err());
        assert!(valid_session("0b6c1e3a-1f2d-4c1b-9d55-1f0d2c3b4a5e"));
        assert!(!valid_session("--dangerously-skip-permissions"));
        assert!(!valid_session("_x"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pruning_drops_old_sessions_then_the_oldest_until_it_fits() {
        let dir = temp("prune");
        for (name, bytes) in [("old", 10), ("mid", 600), ("new", 600), ("live", 600)] {
            fs::create_dir_all(dir.join(name)).unwrap();
            fs::write(dir.join(name).join("f"), vec![0u8; bytes]).unwrap();
        }
        let day = Duration::from_secs(86_400);
        let later = |d: u32| SystemTime::now() + day * d;
        // Everything is younger than the age limit; only size matters here.
        prune(&dir, "live", later(0), day * 7, 1_300);
        let left = |n: &str| dir.join(n).exists();
        assert!(!left("old") && !left("mid"), "the oldest go first");
        assert!(left("new") && left("live"));
        prune(&dir, "live", later(30), day * 7, u64::MAX);
        assert!(
            !left("new") && left("live"),
            "the live session is never pruned"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_file_too_large_to_copy_stays_skipped_after_claude_shrinks_it() {
        let dir = temp("large");
        let (store, file) = (dir.join("store"), dir.join("big.log"));
        fs::File::create(&file)
            .unwrap()
            .set_len(MAX_FILE + 1)
            .unwrap();
        take(&store, "s-1", &file).unwrap();
        assert_eq!(read(&store, "s-1", &file), Before::Skipped);
        fs::write(&file, "edited by Claude\n").unwrap();
        take(&store, "s-1", &file).unwrap();
        assert_eq!(read(&store, "s-1", &file), Before::Skipped);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_session_lists_the_files_it_touched_under_a_root_and_reverts_them() {
        let dir = temp("list");
        let (store, root) = (dir.join("store"), dir.join("app"));
        fs::create_dir_all(root.join("src")).unwrap();
        let (edited, created, outside) = (
            root.join("src/a.go"),
            root.join("new.go"),
            dir.join("elsewhere.txt"),
        );
        fs::write(&edited, "before\n").unwrap();
        fs::write(&outside, "x").unwrap();
        for f in [&edited, &created, &outside] {
            take(&store, "s-1", f).unwrap();
        }
        take(&store, "s-2", &outside).unwrap();
        fs::write(&edited, "after\n").unwrap();
        fs::write(&created, "made by Claude\n").unwrap();

        let listed = sessions_under(&store, &root);
        assert_eq!(listed.len(), 1, "s-2 touched nothing under the root");
        assert_eq!(listed[0].session, "s-1");
        assert_eq!(listed[0].files, [created.clone(), edited.clone()]);
        assert_eq!(files(&store, "s-1").len(), 3);
        assert!(files(&store, "../x").is_empty());

        let backup = dir.join("backup");
        revert(&store, "s-1", &root, &edited, &backup).unwrap();
        assert_eq!(fs::read_to_string(&edited).unwrap(), "before\n");
        assert_eq!(
            fs::read_to_string(backup.join("src/a.go")).unwrap(),
            "after\n"
        );
        revert(&store, "s-1", &root, &created, &backup).unwrap();
        assert!(!created.exists(), "a file the session created is removed");
        assert_eq!(
            fs::read_to_string(backup.join("new.go")).unwrap(),
            "made by Claude\n"
        );
        assert!(revert(&store, "s-9", &root, &edited, &backup).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reverting_writes_through_a_link_and_recreates_a_removed_folder() {
        let dir = temp("link");
        let (store, root, real) = (dir.join("store"), dir.join("app"), dir.join("real"));
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("a.go"), "before\n").unwrap();
        let link = root.join("a.go");
        std::os::unix::fs::symlink("../real/a.go", &link).unwrap();
        take(&store, "s-1", &link).unwrap();
        fs::write(&link, "after\n").unwrap();
        let gone = root.join("gone/b.go");
        fs::create_dir_all(gone.parent().unwrap()).unwrap();
        fs::write(&gone, "kept\n").unwrap();
        take(&store, "s-1", &gone).unwrap();
        fs::remove_dir_all(gone.parent().unwrap()).unwrap();

        let backup = dir.join("backup");
        revert(&store, "s-1", &root, &link, &backup).unwrap();
        assert!(
            fs::symlink_metadata(&link).unwrap().is_symlink(),
            "the link stays"
        );
        assert_eq!(fs::read_to_string(real.join("a.go")).unwrap(), "before\n");
        revert(&store, "s-1", &root, &gone, &backup).unwrap();
        assert_eq!(fs::read_to_string(&gone).unwrap(), "kept\n");
        for folder in [&root, &real, &gone.parent().unwrap().to_path_buf()] {
            let names: Vec<_> = fs::read_dir(folder)
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect();
            assert!(
                names
                    .iter()
                    .all(|n| !n.to_string_lossy().contains("athena-revert")),
                "{names:?}"
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_fifo_is_skipped_without_blocking() {
        let dir = temp("fifo");
        let (store, fifo) = (dir.join("store"), dir.join("pipe"));
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: c_path is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let (thread_store, thread_fifo) = (store.clone(), fifo.clone());
        std::thread::spawn(move || {
            let _ = tx.send(take(&thread_store, "s-1", &thread_fifo).is_ok());
        });
        let taken = rx.recv_timeout(Duration::from_secs(5));
        if taken.is_err() {
            // Unblock the stuck reader so the test process can exit.
            let _ = fs::OpenOptions::new().write(true).open(&fifo);
        }
        assert_eq!(taken, Ok(true), "take must not block on a FIFO");
        assert_eq!(read(&store, "s-1", &fifo), Before::Skipped);
        assert!(
            store
                .join("s-1")
                .join(key(&fifo))
                .with_extension("skip")
                .exists()
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
