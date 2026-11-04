use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ide::lock::{token, tokens_match, write_private};

const CODE_TTL: Duration = Duration::from_secs(5 * 60);
const FILE: &str = "devices.json";

/// A one-time code a phone presents to pair; `Debug` hides it.
pub struct PairingCode {
    code: Option<String>,
    expires: Instant,
}

impl PairingCode {
    pub fn new(now: Instant) -> io::Result<Self> {
        Ok(Self {
            code: Some(token()?),
            expires: now + CODE_TTL,
        })
    }

    /// The code to show in the pairing modal; empty once it has been used.
    pub fn code(&self) -> &str {
        self.code.as_deref().unwrap_or("")
    }

    /// When the code stops working, for the pairing modal's countdown.
    pub fn expires(&self) -> Instant {
        self.expires
    }

    /// Spends the code on the first attempt, right or wrong, so a guess cannot be retried.
    pub fn take(&mut self, given: &str, now: Instant) -> bool {
        let Some(code) = self.code.take() else {
            return false;
        };
        let matches = tokens_match(given.as_bytes(), code.as_bytes());
        matches && now < self.expires
    }
}

impl fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingCode")
            .field("expires", &self.expires)
            .finish_non_exhaustive()
    }
}

/// A paired phone; only the hash of its token is kept.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub token_sha256: String,
    pub login: String,
    pub node: String,
    pub created_ms: u64,
    pub last_seen_ms: u64,
}

#[derive(Debug)]
pub struct Devices {
    dir: PathBuf,
    devices: Vec<Device>,
}

impl Devices {
    /// Reads `devices.json` from `dir`; an unreadable or corrupt file counts as no devices.
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(FILE);
        let devices = match fs::read(&path) {
            // SAFETY: geteuid has no preconditions and cannot fail.
            Ok(_) if !trusted(dir, &path, unsafe { libc::geteuid() }) => Vec::new(),
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                tracing::warn!("ignoring corrupt {}: {err}", path.display());
                Vec::new()
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(err) => {
                tracing::warn!("cannot read {}: {err}", path.display());
                Vec::new()
            }
        };
        Self {
            dir: dir.to_path_buf(),
            devices,
        }
    }

    pub fn list(&self) -> &[Device] {
        &self.devices
    }

    /// Pairs a new device and returns it with its token, which is never stored.
    pub fn issue(&mut self, name: &str, login: &str, node: &str) -> io::Result<(Device, String)> {
        let secret = format!("{}{}", token()?, token()?);
        let now = now_ms();
        let device = Device {
            id: token()?,
            name: name.to_string(),
            token_sha256: hash(&secret),
            login: login.to_string(),
            node: node.to_string(),
            created_ms: now,
            last_seen_ms: now,
        };
        self.devices.push(device.clone());
        if let Err(err) = self.save() {
            self.devices.pop();
            return Err(err);
        }
        Ok((device, secret))
    }

    /// Checks every stored hash, so timing does not reveal which device matched.
    pub fn verify(&self, presented: &str) -> Option<&Device> {
        let hashed = hash(presented);
        let mut found = None;
        for device in &self.devices {
            if tokens_match(hashed.as_bytes(), device.token_sha256.as_bytes()) {
                found = Some(device);
            }
        }
        found
    }

    /// Returns whether a device with `id` existed. The device stays revoked in memory even if
    /// saving fails, so a revoked phone loses access at once.
    pub fn revoke(&mut self, id: &str) -> io::Result<bool> {
        let Some(at) = self.devices.iter().position(|d| d.id == id) else {
            return Ok(false);
        };
        self.devices.remove(at);
        self.save()?;
        Ok(true)
    }

    fn save(&self) -> io::Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))?;
        let json = serde_json::to_vec_pretty(&self.devices).map_err(io::Error::other)?;
        write_private(&self.dir.join(FILE), &json)
    }
}

fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// A store someone else owns, or one we cannot lock down, cannot be trusted to hold only our tokens.
fn trusted(dir: &Path, path: &Path, euid: u32) -> bool {
    let (Ok(dir_meta), Ok(meta)) = (fs::metadata(dir), fs::metadata(path)) else {
        tracing::warn!("cannot inspect {}", path.display());
        return false;
    };
    if dir_meta.uid() != euid || meta.uid() != euid {
        tracing::warn!("ignoring {}: not owned by this user", path.display());
        return false;
    }
    if meta.mode() & 0o077 != 0
        && let Err(err) = fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    {
        tracing::warn!("ignoring {}: cannot restrict it: {err}", path.display());
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("daedalus-devices-{}-{n}", std::process::id()))
                .join("companion");
            let _ = fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            if let Some(root) = self.0.parent() {
                let _ = fs::remove_dir_all(root);
            }
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().mode() & 0o777
    }

    #[test]
    fn a_pairing_code_works_once_and_a_wrong_guess_spends_it() {
        let now = Instant::now();
        let mut code = PairingCode::new(now).unwrap();
        let right = code.code().to_string();
        assert_eq!(right.len(), 32);
        assert!(code.take(&right, now));
        assert!(!code.take(&right, now));

        let mut code = PairingCode::new(now).unwrap();
        let right = code.code().to_string();
        assert!(!code.take("not-the-code", now));
        assert!(!code.take(&right, now));
    }

    #[test]
    fn a_pairing_code_expires_after_five_minutes() {
        let now = Instant::now();
        let mut code = PairingCode::new(now).unwrap();
        let right = code.code().to_string();
        assert!(!code.take(&right, now + CODE_TTL));

        let mut code = PairingCode::new(now).unwrap();
        let right = code.code().to_string();
        assert!(code.take(&right, now + CODE_TTL - Duration::from_secs(1)));
    }

    #[test]
    fn debug_output_never_shows_the_pairing_code() {
        let code = PairingCode::new(Instant::now()).unwrap();
        assert!(!format!("{code:?}").contains(code.code()));
    }

    #[test]
    fn an_issued_token_verifies_and_a_wrong_one_does_not() {
        let tmp = TempDir::new();
        let mut devices = Devices::load(&tmp.0);
        let (device, secret) = devices.issue("Phone", "me@example.com", "mac").unwrap();
        let (other, _) = devices.issue("iPad", "me@example.com", "mac").unwrap();
        assert_eq!(secret.len(), 64);
        assert_eq!(devices.verify(&secret).map(|d| &d.id), Some(&device.id));
        assert_ne!(device.id, other.id);
        assert!(devices.verify("wrong").is_none());
        assert!(devices.verify("").is_none());
        assert!(devices.verify(&secret[..63]).is_none());
    }

    #[test]
    fn the_file_holds_the_hash_and_never_the_token() {
        let tmp = TempDir::new();
        let mut devices = Devices::load(&tmp.0);
        let (device, secret) = devices.issue("Phone", "me@example.com", "mac").unwrap();
        let text = fs::read_to_string(tmp.0.join(FILE)).unwrap();
        assert!(!text.contains(&secret));
        assert!(text.contains(&device.token_sha256));
        assert_eq!(device.token_sha256, hash(&secret));
    }

    #[test]
    fn devices_survive_a_reload_and_revoking_removes_them_for_good() {
        let tmp = TempDir::new();
        let mut devices = Devices::load(&tmp.0);
        let (device, secret) = devices.issue("Phone", "me@example.com", "mac").unwrap();

        let mut again = Devices::load(&tmp.0);
        assert_eq!(again.list().len(), 1);
        assert!(again.verify(&secret).is_some());

        assert!(again.revoke(&device.id).unwrap());
        assert!(!again.revoke(&device.id).unwrap());
        assert!(again.verify(&secret).is_none());
        assert!(Devices::load(&tmp.0).list().is_empty());
    }

    #[test]
    fn the_folder_is_owner_only_and_the_file_is_written_owner_only_without_leftovers() {
        let tmp = TempDir::new();
        let mut devices = Devices::load(&tmp.0);
        devices.issue("Phone", "me@example.com", "mac").unwrap();
        assert_eq!(mode(&tmp.0), 0o700);
        assert_eq!(mode(&tmp.0.join(FILE)), 0o600);
        let names: Vec<_> = fs::read_dir(&tmp.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [FILE]);
    }

    #[test]
    fn a_failed_save_still_leaves_the_device_revoked() {
        let tmp = TempDir::new();
        let mut devices = Devices::load(&tmp.0);
        let (device, secret) = devices.issue("Phone", "me@example.com", "mac").unwrap();
        fs::remove_dir_all(&tmp.0).unwrap();
        fs::write(&tmp.0, b"").unwrap();

        assert!(devices.revoke(&device.id).is_err());
        assert!(devices.verify(&secret).is_none());
    }

    #[test]
    fn a_store_owned_by_someone_else_loads_as_empty() {
        let tmp = TempDir::new();
        let mut devices = Devices::load(&tmp.0);
        devices.issue("Phone", "me@example.com", "mac").unwrap();
        let path = tmp.0.join(FILE);
        let euid = fs::metadata(&path).unwrap().uid();

        assert!(trusted(&tmp.0, &path, euid));
        assert!(!trusted(&tmp.0, &path, euid + 1));
    }

    #[test]
    fn a_corrupt_or_missing_file_loads_as_empty() {
        let tmp = TempDir::new();
        assert!(Devices::load(&tmp.0).list().is_empty());
        fs::create_dir_all(&tmp.0).unwrap();
        fs::write(tmp.0.join(FILE), b"{not json").unwrap();
        assert!(Devices::load(&tmp.0).list().is_empty());
        fs::write(tmp.0.join(FILE), [0xff, 0xfe, 0x00]).unwrap();
        assert!(Devices::load(&tmp.0).list().is_empty());
    }

    #[test]
    fn a_file_left_readable_by_others_is_locked_down_on_load() {
        let tmp = TempDir::new();
        let mut devices = Devices::load(&tmp.0);
        let (_, secret) = devices.issue("Phone", "me@example.com", "mac").unwrap();
        let path = tmp.0.join(FILE);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let loaded = Devices::load(&tmp.0);
        assert!(loaded.verify(&secret).is_some());
        assert_eq!(mode(&path), 0o600);
    }
}
