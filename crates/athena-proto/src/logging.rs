use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriterExt};

/// A log larger than this is moved to `<name>.log.1` at startup.
const MAX_LOG: u64 = 5 * 1024 * 1024;

/// Sends `tracing` events and `log` records to `path`, filtered by the `env` variable (default `info`).
pub fn init(path: &Path, env: &str) -> io::Result<()> {
    if fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG) {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
    let file = Mutex::new(
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?,
    );
    // A spawned daemon's stderr already is its log file, so only tee to a real terminal.
    let writer = if cfg!(debug_assertions) && io::stderr().is_terminal() {
        BoxMakeWriter::new(file.and(io::stderr))
    } else {
        BoxMakeWriter::new(file)
    };
    let filter = EnvFilter::try_from_env(env).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .try_init()
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_log_is_rotated_and_reopened_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("athena-log-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("app.log");
        fs::write(&path, vec![b'x'; MAX_LOG as usize + 1]).unwrap();

        init(&path, "ATHENA_TEST_LOG").unwrap();
        tracing::info!("hello from the test");

        assert_eq!(
            fs::metadata(dir.join("app.log.1")).unwrap().len(),
            MAX_LOG + 1
        );
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("hello from the test"), "{text}");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }
}
