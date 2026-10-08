use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::Serialize;

/// Where Claude Code looks for IDEs; it scans this directory whatever config profile it runs with.
pub fn default_dir() -> Option<PathBuf> {
    std::env::home_dir().map(|home| home.join(".claude/ide"))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Contents<'a> {
    pid: u32,
    workspace_folders: &'a [PathBuf],
    ide_name: &'static str,
    transport: &'static str,
    running_in_windows: bool,
    auth_token: &'a str,
}

/// `<port>.lock`, which tells Claude Code where Athena listens and which folders it has open.
pub struct Lock {
    path: PathBuf,
    token: String,
}

impl Lock {
    pub fn create(dir: &Path, port: u16, token: &str, folders: &[PathBuf]) -> io::Result<Self> {
        if !dir.is_dir() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let lock = Self {
            path: dir.join(format!("{port}.lock")),
            token: token.to_string(),
        };
        lock.update(folders)?;
        Ok(lock)
    }

    pub fn update(&self, folders: &[PathBuf]) -> io::Result<()> {
        let contents = Contents {
            pid: std::process::id(),
            workspace_folders: folders,
            ide_name: "Athena",
            transport: "ws",
            running_in_windows: false,
            auth_token: &self.token,
        };
        let json = serde_json::to_vec(&contents).map_err(io::Error::other)?;
        write_private(&self.path, &json)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn remove(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Replaces `path` in one step with an owner-only file, so a reader never sees half of it.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    // Not ending in `.lock`, so Claude Code never parses (and deletes) a half-written copy.
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    drop(file);
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// A fresh 128-bit token as lowercase hex.
pub fn token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Compares in time that does not depend on where the strings differ.
pub fn tokens_match(given: &[u8], expected: &[u8]) -> bool {
    let mut diff = given.len() ^ expected.len();
    for (i, e) in expected.iter().enumerate() {
        diff |= usize::from(given.get(i).copied().unwrap_or(0) ^ e);
    }
    diff == 0
}

/// Publishes the port for athena-mux to give new shells.
pub fn write_env(path: &Path, port: u16) -> io::Result<()> {
    let text = format!("CLAUDE_CODE_SSE_PORT={port}\nENABLE_IDE_INTEGRATION=true\n");
    write_private(path, text.as_bytes())
}

/// The port an earlier run published, to listen on it again so older shells still point here.
pub fn previous_port(env: &Path) -> Option<u16> {
    athena_proto::read_ide_env(env)
        .into_iter()
        .find(|(key, _)| key == "CLAUDE_CODE_SSE_PORT")
        .and_then(|(_, port)| port.parse().ok())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("athena-ide-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_lock_is_owner_only_json_in_an_owner_only_folder_and_follows_the_folders() {
        let base = temp("lock");
        let dir = base.join("ide");
        let lock = Lock::create(&dir, 51234, "abc", &[PathBuf::from("/p/one")]).unwrap();
        assert_eq!(lock.path(), dir.join("51234.lock"));
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(lock.path()).unwrap().mode() & 0o777, 0o600);
        let read = || -> serde_json::Value {
            serde_json::from_slice(&fs::read(lock.path()).unwrap()).unwrap()
        };
        let json = read();
        assert_eq!(json["pid"], std::process::id());
        assert_eq!(json["ideName"], "Athena");
        assert_eq!(json["transport"], "ws");
        assert_eq!(json["runningInWindows"], false);
        assert_eq!(json["authToken"], "abc");
        assert_eq!(json["workspaceFolders"], serde_json::json!(["/p/one"]));

        lock.update(&[PathBuf::from("/p/one"), PathBuf::from("/p/two")])
            .unwrap();
        assert_eq!(
            read()["workspaceFolders"],
            serde_json::json!(["/p/one", "/p/two"])
        );
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["51234.lock"], "no temporary file is left behind");

        lock.remove();
        assert!(!lock.path().exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn tokens_are_fresh_hex_and_compare_only_when_equal() {
        let (a, b) = (token().unwrap(), token().unwrap());
        assert_eq!(a.len(), 32);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(a, b);
        assert!(tokens_match(a.as_bytes(), a.as_bytes()));
        assert!(!tokens_match(b.as_bytes(), a.as_bytes()));
        assert!(!tokens_match(&a.as_bytes()[..31], a.as_bytes()));
        assert!(!tokens_match(format!("{a}0").as_bytes(), a.as_bytes()));
        assert!(!tokens_match(b"", a.as_bytes()));
    }

    #[test]
    fn the_env_file_round_trips_its_port() {
        let dir = temp("env");
        fs::create_dir_all(&dir).unwrap();
        let env = dir.join("ide.env");
        assert_eq!(previous_port(&env), None);
        write_env(&env, 40404).unwrap();
        assert_eq!(previous_port(&env), Some(40404));
        assert_eq!(fs::metadata(&env).unwrap().mode() & 0o777, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }
}
