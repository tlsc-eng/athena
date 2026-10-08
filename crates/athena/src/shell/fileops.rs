// Wired up by the context menu in Phase C; until then only the tests call these.
#![cfg_attr(not(test), allow(dead_code))]

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// Creates an empty file, failing if anything already exists at `path`.
pub(super) fn create_file(path: &Path) -> Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map(drop)
        .with_context(|| describe("create", path))
}

/// Creates one directory; the parent must exist.
pub(super) fn create_dir(path: &Path) -> Result<()> {
    fs::create_dir(path).with_context(|| describe("create", path))
}

/// Renames without overwriting, except for a case-only rename of the same file.
pub(super) fn rename(from: &Path, to: &Path) -> Result<()> {
    if let Ok(existing) = fs::symlink_metadata(to) {
        let source = fs::symlink_metadata(from).with_context(|| describe("rename", from))?;
        if (existing.dev(), existing.ino()) != (source.dev(), source.ino()) {
            bail!(
                "could not rename {}: {} already exists",
                name(from),
                name(to)
            );
        }
    }
    fs::rename(from, to).with_context(|| describe("rename", from))
}

/// Moves `path` to the Trash so Finder's Put Back can restore it.
pub(super) fn trash(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_err() {
        return Err(io::Error::from(io::ErrorKind::NotFound))
            .with_context(|| describe("trash", path));
    }
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSFileManager, NSString, NSURL};
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        NSFileManager::defaultManager()
            .trashItemAtURL_resultingItemURL_error(&url, None)
            .map_err(|e| anyhow::anyhow!("{}", e.localizedDescription()))
            .with_context(|| describe("trash", path))
    }
    #[cfg(not(target_os = "macos"))]
    {
        bail!(
            "{}: the Trash is only available on macOS",
            describe("trash", path)
        )
    }
}

fn name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn describe(verb: &str, path: &Path) -> String {
    format!("could not {verb} {}", name(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("athena-fileops-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn create_file_refuses_to_overwrite() {
        let dir = scratch("create-file");
        let file = dir.join("a.txt");
        create_file(&file).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"");
        fs::write(&file, "keep").unwrap();
        assert!(create_file(&file).is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn create_dir_needs_a_parent_and_a_free_name() {
        let dir = scratch("create-dir");
        create_dir(&dir.join("src")).unwrap();
        assert!(dir.join("src").is_dir());
        assert!(create_dir(&dir.join("src")).is_err());
        assert!(create_dir(&dir.join("missing/child")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rename_never_overwrites_another_file() {
        let dir = scratch("rename");
        let (a, b) = (dir.join("a.rs"), dir.join("b.rs"));
        fs::write(&a, "a").unwrap();
        fs::write(&b, "b").unwrap();
        let err = rename(&a, &b).unwrap_err();
        assert!(err.to_string().contains("b.rs already exists"), "{err}");
        assert_eq!(fs::read_to_string(&b).unwrap(), "b");

        rename(&a, &dir.join("c.rs")).unwrap();
        assert!(!a.exists());
        assert_eq!(fs::read_to_string(dir.join("c.rs")).unwrap(), "a");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rename_allows_changing_only_the_case() {
        let dir = scratch("rename-case");
        let from = dir.join("readme.md");
        fs::write(&from, "x").unwrap();
        rename(&from, &dir.join("README.md")).unwrap();
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["README.md"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn trash_reports_a_missing_path() {
        let dir = scratch("trash-missing");
        assert!(trash(&dir.join("nope")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[ignore = "moves a file into the real user Trash"]
    fn trash_moves_the_file_out_of_place() {
        let dir = scratch("trash");
        let file = dir.join("athena-trash-test.txt");
        fs::write(&file, "x").unwrap();
        trash(&file).unwrap();
        assert!(!file.exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
