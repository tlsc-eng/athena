//! Copies of files as they were before a Claude Code session first edited them, so the session's
//! edits can be reviewed as one diff. Taken by the PreToolUse hook, read by the window.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
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
    /// No snapshot was taken (hooks off, file too large or not a regular file, another session).
    Unknown,
    /// The session created the file.
    Absent,
    Text(Vec<u8>),
}

pub fn store() -> Result<PathBuf> {
    Ok(athena_proto::data_dir()?.join("snapshots"))
}

/// Claude Code session ids are UUIDs; anything else must not become a directory name.
pub fn valid_session(session: &str) -> bool {
    !session.is_empty()
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
        return Before::Unknown;
    }
    if base.with_extension("new").exists() {
        return Before::Absent;
    }
    match fs::read(&base) {
        Ok(text) => Before::Text(text),
        Err(_) => Before::Unknown,
    }
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
    fn a_file_too_large_to_copy_stays_unknown_after_claude_shrinks_it() {
        let dir = temp("large");
        let (store, file) = (dir.join("store"), dir.join("big.log"));
        fs::File::create(&file)
            .unwrap()
            .set_len(MAX_FILE + 1)
            .unwrap();
        take(&store, "s-1", &file).unwrap();
        assert_eq!(read(&store, "s-1", &file), Before::Unknown);
        fs::write(&file, "edited by Claude\n").unwrap();
        take(&store, "s-1", &file).unwrap();
        assert_eq!(read(&store, "s-1", &file), Before::Unknown);
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
        assert_eq!(read(&store, "s-1", &fifo), Before::Unknown);
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
