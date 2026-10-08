use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// Changes closer together than this arrive as one batch.
pub const QUIET: Duration = Duration::from_millis(200);
/// A steady stream of changes (a build writing files) is still reported this often.
pub const MAX_WAIT: Duration = Duration::from_secs(1);

/// Watches one folder and everything under it; dropping this stops the watch and its thread.
pub struct FolderWatcher {
    _inner: RecommendedWatcher,
}

impl FolderWatcher {
    /// Calls `on_change` from a background thread with the paths that changed under `root`, in
    /// debounced batches; a batch holding `root` itself means "assume anything changed".
    pub fn new(
        root: &Path,
        on_change: impl Fn(Vec<PathBuf>) + Send + 'static,
    ) -> notify::Result<Self> {
        let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
        let everything = root.to_path_buf();
        let mut inner =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let paths = match event {
                    Ok(event) if !event.need_rescan() => event.paths,
                    _ => vec![everything.clone()],
                };
                let _ = tx.send(paths);
            })?;
        inner.watch(root, RecursiveMode::Recursive)?;
        std::thread::Builder::new()
            .name("athena-watch".into())
            .spawn(move || debounce(&rx, QUIET, MAX_WAIT, on_change))?;
        Ok(Self { _inner: inner })
    }
}

/// Gathers paths until `quiet` passes with nothing new (or `max_wait` since the first), hands
/// them over sorted and deduplicated, and returns once the sender is gone.
pub fn debounce(
    rx: &Receiver<Vec<PathBuf>>,
    quiet: Duration,
    max_wait: Duration,
    on_change: impl Fn(Vec<PathBuf>),
) {
    while let Ok(first) = rx.recv() {
        let started = Instant::now();
        let mut batch: BTreeSet<PathBuf> = first.into_iter().collect();
        let mut open = true;
        while open {
            let left = max_wait.saturating_sub(started.elapsed());
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(quiet.min(left)) {
                Ok(more) => batch.extend(more),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => open = false,
            }
        }
        on_change(batch.into_iter().collect());
        if !open {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Batches = Arc<Mutex<Vec<Vec<PathBuf>>>>;

    fn collect() -> (Batches, impl Fn(Vec<PathBuf>)) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        (seen, move |batch| sink.lock().unwrap().push(batch))
    }

    #[test]
    fn a_burst_arrives_as_one_sorted_deduplicated_batch() {
        let (tx, rx) = mpsc::channel();
        tx.send(vec!["/p/b".into(), "/p/a".into()]).unwrap();
        tx.send(vec!["/p/a".into()]).unwrap();
        drop(tx);
        let (seen, sink) = collect();
        debounce(&rx, Duration::from_millis(50), Duration::from_secs(5), sink);
        let seen = seen.lock().unwrap();
        assert_eq!(*seen, vec![vec![PathBuf::from("/p/a"), "/p/b".into()]]);
    }

    #[test]
    fn a_pause_longer_than_the_quiet_time_starts_a_new_batch() {
        let (tx, rx) = mpsc::channel();
        let sender = std::thread::spawn(move || {
            tx.send(vec!["/p/a".into()]).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            tx.send(vec!["/p/b".into()]).unwrap();
        });
        let (seen, sink) = collect();
        debounce(&rx, Duration::from_millis(50), Duration::from_secs(5), sink);
        sender.join().unwrap();
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_steady_stream_is_still_reported_by_the_deadline() {
        let (tx, rx) = mpsc::channel();
        let sender = std::thread::spawn(move || {
            for i in 0..30 {
                tx.send(vec![PathBuf::from(format!("/p/{i}"))]).unwrap();
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let (seen, sink) = collect();
        debounce(
            &rx,
            Duration::from_millis(100),
            Duration::from_millis(200),
            sink,
        );
        sender.join().unwrap();
        assert!(
            seen.lock().unwrap().len() >= 2,
            "flushed before the stream ended"
        );
    }

    #[test]
    fn writing_a_file_in_a_watched_folder_is_reported() {
        let dir = std::env::temp_dir().join(format!("athena-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let (tx, rx) = mpsc::channel();
        let watcher = FolderWatcher::new(&dir, move |batch| {
            let _ = tx.send(batch);
        })
        .unwrap();
        // FSEvents may report nothing for changes made right after the stream starts.
        std::thread::sleep(Duration::from_millis(300));
        let file = dir.join("new.txt");
        std::fs::write(&file, "x").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut found = false;
        while !found && Instant::now() < deadline {
            if let Ok(batch) = rx.recv_timeout(Duration::from_millis(200)) {
                found = batch.iter().any(|p| p == &file || p == &dir);
            }
        }
        drop(watcher);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(found, "no change reported for {}", file.display());
    }
}
