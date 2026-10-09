use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

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

/// Moves `from` to `to`, first sending what is at `to` to the Trash when `replace` is set; across
/// volumes it copies, then trashes the original.
pub(super) fn move_to(
    from: &Path,
    to: &Path,
    replace: bool,
    trash: impl Fn(&Path) -> Result<()>,
) -> Result<()> {
    if replace && fs::symlink_metadata(to).is_ok() {
        if encloses(to, from) {
            bail!("cannot replace {} with something inside it", name(to));
        }
        trash(to)?;
    }
    match rename(from, to) {
        Err(err) if crosses_volumes(&err) => move_by_copy(from, to, &trash),
        result => result,
    }
}

fn move_by_copy(from: &Path, to: &Path, trash: impl Fn(&Path) -> Result<()>) -> Result<()> {
    copy(from, to)?;
    trash(from)
}

fn crosses_volumes(err: &anyhow::Error) -> bool {
    err.downcast_ref::<io::Error>()
        .is_some_and(|e| e.kind() == io::ErrorKind::CrossesDevices)
}

/// Whether `outer` is `inner` or a folder holding it, however either path is spelled.
fn encloses(outer: &Path, inner: &Path) -> bool {
    let Ok(outer) = fs::symlink_metadata(outer) else {
        return false;
    };
    let same = |m: fs::Metadata| (m.dev(), m.ino()) == (outer.dev(), outer.ino());
    if fs::symlink_metadata(inner).is_ok_and(same) {
        return true;
    }
    let Some(parent) = inner.parent() else {
        return false;
    };
    let parent = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    parent.ancestors().any(|a| fs::metadata(a).is_ok_and(same))
}

/// Where `from` lands when dropped on folder `dir`: `None` when it is already there, an error
/// when `dir` is `from` itself or inside it, or when it would land on a folder holding `from`.
pub(super) fn drop_destination(from: &Path, dir: &Path) -> Result<Option<PathBuf>> {
    if dir.starts_with(from) {
        bail!("cannot move {} into itself", name(from));
    }
    if from.parent() == Some(dir) {
        return Ok(None);
    }
    let Some(file_name) = from.file_name() else {
        bail!("{} has no name to move", from.display());
    };
    let dest = dir.join(file_name);
    if from.starts_with(&dest) {
        bail!("cannot replace {} with something inside it", name(&dest));
    }
    Ok(Some(dest))
}

/// A name in `dir` for a copy of `name` that nothing has yet: "a copy.txt", "a copy 2.txt", …
pub(super) fn free_copy_name(dir: &Path, name: &str) -> PathBuf {
    let (stem, ext) = match name.rfind('.').filter(|&i| i > 0) {
        Some(i) => name.split_at(i),
        None => (name, ""),
    };
    (1..)
        .map(|n| match n {
            1 => format!("{stem} copy{ext}"),
            n => format!("{stem} copy {n}{ext}"),
        })
        .map(|candidate| dir.join(candidate))
        .find(|path| fs::symlink_metadata(path).is_err())
        .expect("some copy name is free")
}

/// Copies a file or a whole folder, never overwriting anything.
pub(super) fn copy(from: &Path, to: &Path) -> Result<()> {
    fn walk(from: &Path, to: &Path) -> io::Result<()> {
        let meta = fs::symlink_metadata(from)?;
        if meta.file_type().is_symlink() {
            return std::os::unix::fs::symlink(fs::read_link(from)?, to);
        }
        if !meta.is_dir() {
            OpenOptions::new().write(true).create_new(true).open(to)?;
            fs::copy(from, to)?;
            return Ok(());
        }
        fs::create_dir(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            walk(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    }
    walk(from, to).with_context(|| describe("copy", from))
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
    fn a_folder_cannot_be_dropped_into_itself_or_its_own_subfolder() {
        let src = Path::new("/p/src");
        assert!(drop_destination(src, src).is_err());
        let err = drop_destination(src, Path::new("/p/src/nested")).unwrap_err();
        assert!(err.to_string().contains("into itself"), "{err}");
        assert_eq!(drop_destination(src, Path::new("/p")).unwrap(), None);
        assert_eq!(
            drop_destination(src, Path::new("/p/srcs")).unwrap(),
            Some(PathBuf::from("/p/srcs/src"))
        );
        assert_eq!(
            drop_destination(Path::new("/p/src/a.rs"), Path::new("/p")).unwrap(),
            Some(PathBuf::from("/p/a.rs"))
        );
    }

    #[test]
    fn dropping_a_folder_where_its_namesake_parent_sits_never_trashes_that_parent() {
        let dir = scratch("replace-parent");
        let from = dir.join("pkg/pkg");
        fs::create_dir_all(&from).unwrap();
        fs::write(from.join("lib.rs"), "keep").unwrap();
        let trashed = std::cell::RefCell::new(Vec::new());
        let trash = |path: &Path| {
            trashed.borrow_mut().push(path.to_path_buf());
            Ok(())
        };
        let dropped = drop_destination(&from, &dir).and_then(|dest| match dest {
            Some(dest) => move_to(&from, &dest, true, trash),
            None => Ok(()),
        });
        assert!(dropped.is_err());
        assert!(move_to(&from, &dir.join("pkg"), true, trash).is_err());
        assert_eq!(trashed.borrow().as_slice(), &[] as &[PathBuf]);
        assert_eq!(fs::read_to_string(from.join("lib.rs")).unwrap(), "keep");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_move_across_volumes_copies_then_trashes_the_original() {
        let exdev = anyhow::Error::from(io::Error::from_raw_os_error(libc::EXDEV));
        assert!(crosses_volumes(&exdev.context("could not rename a")));
        let missing = anyhow::Error::from(io::Error::from(io::ErrorKind::NotFound));
        assert!(!crosses_volumes(&missing));

        let dir = scratch("move-by-copy");
        fs::create_dir_all(dir.join("src/inner")).unwrap();
        fs::write(dir.join("src/inner/a.rs"), "a").unwrap();
        let trashed = std::cell::RefCell::new(Vec::new());
        let trash = |path: &Path| {
            trashed.borrow_mut().push(path.to_path_buf());
            Ok(())
        };
        move_by_copy(&dir.join("src"), &dir.join("dst"), trash).unwrap();
        assert_eq!(fs::read_to_string(dir.join("dst/inner/a.rs")).unwrap(), "a");
        assert_eq!(trashed.borrow().as_slice(), [dir.join("src")]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_copy_takes_the_first_free_copy_name() {
        let dir = scratch("copy-name");
        assert_eq!(free_copy_name(&dir, "a.rs"), dir.join("a copy.rs"));
        fs::write(dir.join("a copy.rs"), "").unwrap();
        assert_eq!(free_copy_name(&dir, "a.rs"), dir.join("a copy 2.rs"));
        assert_eq!(free_copy_name(&dir, ".env"), dir.join(".env copy"));
        assert_eq!(free_copy_name(&dir, "src"), dir.join("src copy"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn copy_takes_a_whole_folder_and_never_overwrites() {
        let dir = scratch("copy");
        fs::create_dir_all(dir.join("src/inner")).unwrap();
        fs::write(dir.join("src/inner/a.rs"), "a").unwrap();
        copy(&dir.join("src"), &dir.join("dst")).unwrap();
        assert_eq!(fs::read_to_string(dir.join("dst/inner/a.rs")).unwrap(), "a");
        fs::write(dir.join("b.rs"), "new").unwrap();
        assert!(copy(&dir.join("b.rs"), &dir.join("dst/inner/a.rs")).is_err());
        assert_eq!(fs::read_to_string(dir.join("dst/inner/a.rs")).unwrap(), "a");
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
