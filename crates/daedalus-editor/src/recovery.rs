use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError, TryLockError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ropey::Rope;

/// Unsaved text by buffer id, kept outside gpui so a panic hook or the quit path can reach it.
static DIRTY: Mutex<BTreeMap<u64, (PathBuf, Rope)>> = Mutex::new(BTreeMap::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// Marks a session folder whose files a launch has already offered.
const ANNOUNCED: &str = ".announced";
/// Announced session folders kept on disk, newest first.
const KEEP_SESSIONS: usize = 10;

pub(crate) fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Records buffer `id`'s text while it has unsaved edits; `None` once it is clean or gone.
pub(crate) fn note(id: u64, dirty: Option<(&Path, &Rope)>) {
    let mut map = DIRTY.lock().unwrap_or_else(PoisonError::into_inner);
    match dirty {
        Some((path, rope)) => match map.get_mut(&id) {
            Some(entry) if entry.0 == path => entry.1 = rope.clone(),
            _ => {
                map.insert(id, (path.to_path_buf(), rope.clone()));
            }
        },
        None => {
            map.remove(&id);
        }
    }
}

/// Writes every buffer with unsaved edits under `root/<millis>/`, mirroring each file's absolute
/// path, and returns how many were written.
/// Gives up rather than wait long on the list, which a panic may have struck mid-update.
pub fn write_dirty(root: &Path) -> io::Result<usize> {
    let mut tries = 0;
    let map = loop {
        match DIRTY.try_lock() {
            Ok(map) => break map,
            Err(TryLockError::Poisoned(p)) => break p.into_inner(),
            Err(TryLockError::WouldBlock) if tries < 50 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(TryLockError::WouldBlock) => return Ok(0),
        }
    };
    if map.is_empty() {
        return Ok(0);
    }
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let session = root.join(millis.to_string());
    let mut written = 0;
    let mut failed = None;
    for (path, rope) in map.values() {
        let dest = session.join(path.strip_prefix("/").unwrap_or(path));
        let result = (|| {
            fs::create_dir_all(dest.parent().unwrap_or(&session))?;
            rope.write_to(fs::File::create(&dest)?)
        })();
        match result {
            Ok(()) => written += 1,
            Err(err) => failed = Some(err),
        }
    }
    match failed {
        Some(err) if written == 0 => Err(err),
        _ => Ok(written),
    }
}

/// Recovery copies no launch has offered yet, oldest session first; marks them offered and
/// prunes old sessions that were offered before.
pub fn take_unannounced(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut sessions: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    sessions.sort();
    let mut files = Vec::new();
    let mut announced = Vec::new();
    for session in &sessions {
        let marker = session.join(ANNOUNCED);
        if marker.exists() {
            announced.push(session);
            continue;
        }
        collect_files(session, &mut files);
        let _ = fs::write(&marker, b"");
    }
    let stale = sessions.len().saturating_sub(KEEP_SESSIONS);
    for session in announced.into_iter().take(stale) {
        let _ = fs::remove_dir_all(session);
    }
    files
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_files(&path, out);
        } else if path.file_name().is_some_and(|n| n != ANNOUNCED) {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_buffers_are_written_once_and_offered_once() {
        let root = std::env::temp_dir().join(format!("athena-recovery-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let id = next_id();
        let other = next_id();
        note(
            id,
            Some((
                Path::new("/p/src/main.rs"),
                &Rope::from_str("fn main() {}\n"),
            )),
        );
        note(other, Some((Path::new("/p/b.txt"), &Rope::from_str("b"))));
        note(other, None);
        assert!(write_dirty(&root).unwrap() >= 1);
        note(id, None);

        let offered = take_unannounced(&root);
        let mine: Vec<&PathBuf> = offered
            .iter()
            .filter(|p| p.ends_with("p/src/main.rs") || p.ends_with("p/b.txt"))
            .collect();
        assert_eq!(mine.len(), 1, "{offered:?}");
        assert_eq!(fs::read_to_string(mine[0]).unwrap(), "fn main() {}\n");
        assert!(take_unannounced(&root).is_empty(), "offered only once");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn only_announced_sessions_past_the_limit_are_pruned() {
        let root = std::env::temp_dir().join(format!("athena-prune-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for i in 0..KEEP_SESSIONS + 3 {
            let session = root.join(format!("{:013}", 1_000 + i));
            fs::create_dir_all(&session).unwrap();
            fs::write(session.join("f"), "x").unwrap();
            if i > 0 {
                fs::write(session.join(ANNOUNCED), "").unwrap();
            }
        }
        assert_eq!(take_unannounced(&root).len(), 1);
        let left = fs::read_dir(&root).unwrap().count();
        assert_eq!(left, KEEP_SESSIONS);
        assert!(
            root.join(format!("{:013}", 1_000)).exists(),
            "the new one stays"
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
