use std::fs;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// `~/Library/Application Support/athena`, created owner-only on first use.
pub fn data_dir() -> Result<PathBuf> {
    let home = std::env::home_dir().context("no home directory")?;
    let dir = home.join("Library/Application Support/athena");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("mux.sock"))
}

pub fn lock_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("mux.lock"))
}

pub fn log_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("mux.log"))
}

pub fn app_log_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("app.log"))
}

/// Where the Athena window listens for `athena <folder>`.
pub fn app_socket_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("app.sock"))
}

/// Where copies of unsaved files go when Athena quits or crashes without saving them.
pub fn recovery_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("recovery"))
}

/// Where the window tells athena-mux which port its Claude Code IDE server listens on.
pub fn ide_env_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("ide.env"))
}

/// The variables in an ide.env file that shells may get; malformed lines and other keys are dropped.
pub fn read_ide_env(path: &Path) -> Vec<(String, String)> {
    let mut text = String::new();
    let read = fs::File::open(path).and_then(|f| f.take(4096).read_to_string(&mut text));
    if read.is_err() {
        return Vec::new();
    }
    text.lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, value)| match *key {
            "CLAUDE_CODE_SSE_PORT" => value.parse::<u16>().is_ok_and(|p| p != 0),
            "ENABLE_IDE_INTEGRATION" => *value == "true",
            _ => false,
        })
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ide_env_keeps_only_a_valid_port_and_the_enable_flag() {
        let dir = std::env::temp_dir().join(format!("athena-ide-env-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ide.env");
        fs::write(
            &path,
            "CLAUDE_CODE_SSE_PORT=51234\nENABLE_IDE_INTEGRATION=true\nPATH=/evil\nnonsense\n",
        )
        .unwrap();
        assert_eq!(
            read_ide_env(&path),
            [
                ("CLAUDE_CODE_SSE_PORT".to_string(), "51234".to_string()),
                ("ENABLE_IDE_INTEGRATION".to_string(), "true".to_string()),
            ]
        );
        fs::write(
            &path,
            "CLAUDE_CODE_SSE_PORT=0\nCLAUDE_CODE_SSE_PORT=1; rm -rf /\n",
        )
        .unwrap();
        assert!(read_ide_env(&path).is_empty());
        assert!(read_ide_env(&dir.join("missing")).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
