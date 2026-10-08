use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;

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
