use std::fs;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// `~/Library/Application Support/daedalus`, created owner-only on first use.
pub fn data_dir() -> Result<PathBuf> {
    let home = std::env::home_dir().context("no home directory")?;
    data_dir_in(&home)
}

fn data_dir_in(home: &Path) -> Result<PathBuf> {
    let dir = home.join("Library/Application Support/daedalus");
    if !dir.exists() {
        adopt_legacy(&home.join("Library/Application Support/athena"), &dir);
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// Athena's data directory, renamed in place so a running Athena session daemon keeps its socket, lock and shell paths.
fn adopt_legacy(legacy: &Path, dir: &Path) {
    let is_real_dir = fs::symlink_metadata(legacy).is_ok_and(|m| m.file_type().is_dir());
    if !is_real_dir || fs::rename(legacy, dir).is_err() {
        return;
    }
    if std::os::unix::fs::symlink("daedalus", legacy).is_err() {
        let _ = fs::remove_dir(legacy);
        let _ = std::os::unix::fs::symlink("daedalus", legacy);
    }
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

/// Where the Daedalus window listens for `daedalus <folder>`.
pub fn app_socket_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("app.sock"))
}

/// Where copies of unsaved files go when Daedalus quits or crashes without saving them.
pub fn recovery_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("recovery"))
}

/// Where the window tells daedalus-mux which port its Claude Code IDE server listens on.
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

    fn short_home(tag: &str) -> PathBuf {
        let home = PathBuf::from(format!("/tmp/dd-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join("Library/Application Support")).unwrap();
        home
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn legacy_dir_is_adopted_and_stays_reachable_through_the_symlink() {
        use std::os::unix::net::{UnixListener, UnixStream};

        let home = short_home("adopt");
        let support = home.join("Library/Application Support");
        let legacy = support.join("athena");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("settings.json"), "{}").unwrap();
        let _listener = UnixListener::bind(legacy.join("mux.sock")).unwrap();

        let dir = data_dir_in(&home).unwrap();

        assert_eq!(dir, support.join("daedalus"));
        assert!(dir.join("settings.json").is_file());
        assert_eq!(fs::read_link(&legacy).unwrap(), Path::new("daedalus"));
        assert_eq!(mode(&dir), 0o700);
        UnixStream::connect(dir.join("mux.sock")).unwrap();
        UnixStream::connect(legacy.join("mux.sock")).unwrap();
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn fresh_home_gets_only_the_daedalus_dir() {
        let home = short_home("fresh");
        let support = home.join("Library/Application Support");

        let dir = data_dir_in(&home).unwrap();

        assert_eq!(dir, support.join("daedalus"));
        assert_eq!(mode(&dir), 0o700);
        assert!(fs::symlink_metadata(support.join("athena")).is_err());
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn second_call_leaves_the_adopted_layout_alone() {
        let home = short_home("twice");
        let legacy = home.join("Library/Application Support").join("athena");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("settings.json"), "{}").unwrap();

        let first = data_dir_in(&home).unwrap();
        let second = data_dir_in(&home).unwrap();

        assert_eq!(first, second);
        assert!(second.join("settings.json").is_file());
        assert_eq!(fs::read_link(&legacy).unwrap(), Path::new("daedalus"));
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn legacy_symlink_is_never_followed() {
        let home = short_home("link");
        let support = home.join("Library/Application Support");
        let elsewhere = home.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("keep"), "x").unwrap();
        std::os::unix::fs::symlink(&elsewhere, support.join("athena")).unwrap();

        let dir = data_dir_in(&home).unwrap();

        assert!(!dir.join("keep").exists());
        assert_eq!(fs::read_link(support.join("athena")).unwrap(), elsewhere);
        assert!(elsewhere.join("keep").is_file());
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn failed_rename_leaves_the_legacy_dir_untouched() {
        let home = short_home("fail");
        let support = home.join("Library/Application Support");
        let legacy = support.join("athena");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("settings.json"), "{}").unwrap();
        fs::set_permissions(&support, fs::Permissions::from_mode(0o500)).unwrap();

        adopt_legacy(&legacy, &support.join("daedalus"));

        fs::set_permissions(&support, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(fs::symlink_metadata(&legacy).unwrap().is_dir());
        assert!(legacy.join("settings.json").is_file());
        assert!(!support.join("daedalus").exists());
        fs::remove_dir_all(&home).unwrap();
    }
}
