use std::collections::HashMap;
use std::path::{Path, PathBuf};

use athena_editor::{Buffer, EditorView, ServerEdit};
use athena_lsp::{EditError, FileChange, FileEvent, TextEdit, WorkspaceEdit, apply_text_edits};
use gpui::{Context, Entity};

use super::Shell;
use super::fileops;
use super::item::ItemView;
use super::lsp::document_key;

/// Closed files larger than this are not edited, as the editor would not open them either.
const MAX_FILE: u64 = 16 * 1024 * 1024;

/// Each open file's buffer version when a request went to the server, by document key.
pub(super) type AskedAt = HashMap<PathBuf, i64>;

/// What a workspace edit changed, for the message after a rename or a fix.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Applied {
    pub files: usize,
    pub edits: usize,
}

fn name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// A closed file's text, refusing what the editor would refuse: binary, huge or not UTF-8.
fn read_text(path: &Path) -> Result<Option<String>, String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let failed = |e: std::io::Error| format!("Could not read {}: {e}", name(path));
    // Non-blocking, so a FIFO without a writer is refused below instead of hanging here.
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(failed(e)),
    };
    let meta = file.metadata().map_err(failed)?;
    if !meta.is_file() {
        return Err(format!("{} is not a file", name(path)));
    }
    let too_large = || format!("{} is too large to edit", name(path));
    if meta.len() > MAX_FILE {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(failed)?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(too_large());
    }
    if bytes.contains(&0) {
        return Err(format!("{} is not a text file", name(path)));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| format!("{} is not UTF-8 text", name(path)))
}

/// Writes `text` the way saving in the editor does: atomically, keeping permissions, through a
/// symlink to its target and in place for a hard-linked file.
fn write_text(path: &Path, text: &str) -> Result<(), String> {
    Buffer::new(text, Some(path.to_path_buf()))
        .save()
        .map_err(|e| format!("Could not write {}: {e:#}", name(path)))
}

fn not_empty(path: &Path) -> String {
    format!(
        "The language server asked to replace {}, which is not empty; nothing was changed.",
        name(path)
    )
}

/// Whether `to` only re-cases `from` on a case-insensitive disk, where both name one file.
fn recases(from: &Path, to: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if from == to || from.to_string_lossy().to_lowercase() != to.to_string_lossy().to_lowercase() {
        return false;
    }
    match (
        std::fs::symlink_metadata(from),
        std::fs::symlink_metadata(to),
    ) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}

fn edit_error(path: &Path, e: EditError) -> String {
    match e {
        EditError::Overlap { line } => format!(
            "The language server's edits to {} overlap at line {}; nothing was changed.",
            name(path),
            line + 1
        ),
    }
}

fn server_edits(edits: &[TextEdit]) -> Vec<ServerEdit> {
    edits
        .iter()
        .map(|e| ServerEdit {
            start: (e.range.start.line, e.range.start.character),
            end: (e.range.end.line, e.range.end.character),
            text: e.text.clone(),
        })
        .collect()
}

/// A file open in an editor, as a dry run sees it.
pub(super) struct OpenText {
    pub text: String,
    pub version: Option<i64>,
    /// The language server has this file open, so an edit naming a version must match it.
    pub synced: bool,
    /// Its version when the request was sent, which an edit naming no version must match.
    pub asked: Option<i64>,
}

/// Files as a dry run sees them: a planned text, or gone.
#[derive(Default)]
struct Overlay {
    files: HashMap<PathBuf, Option<String>>,
    /// Folders renamed earlier in the edit, so files under the new name are read from the old.
    moved: Vec<(PathBuf, PathBuf)>,
}

impl Overlay {
    fn disk_path(&self, path: &Path) -> PathBuf {
        for (from, to) in self.moved.iter().rev() {
            if let Ok(rest) = path.strip_prefix(to) {
                return from.join(rest);
            }
        }
        path.to_path_buf()
    }

    fn text(
        &self,
        path: &Path,
        open: &impl Fn(&Path) -> Option<OpenText>,
    ) -> Result<Option<String>, String> {
        if let Some(text) = self.files.get(path) {
            return Ok(text.clone());
        }
        if let Some(file) = open(path) {
            return Ok(Some(file.text));
        }
        read_text(&self.disk_path(path))
    }

    fn exists(&self, path: &Path, open: &impl Fn(&Path) -> Option<OpenText>) -> bool {
        match self.files.get(path) {
            Some(text) => text.is_some(),
            None => open(path).is_some() || std::fs::symlink_metadata(self.disk_path(path)).is_ok(),
        }
    }
}

/// Plays the whole edit against the open files and the disk without changing anything, so a bad
/// step refuses it before the first file is touched.
fn check_edit(
    edit: &WorkspaceEdit,
    open: impl Fn(&Path) -> Option<OpenText>,
) -> Result<(), String> {
    let mut overlay = Overlay::default();
    for change in &edit.changes {
        match change {
            FileChange::Edit {
                path,
                version,
                edits,
            } => {
                let stale = open(path).is_some_and(|file| match version {
                    Some(version) => file.synced && file.version != Some(*version),
                    None => file.asked.is_some_and(|asked| file.version != Some(asked)),
                });
                if stale {
                    return Err(format!(
                        "{} changed while the language server worked; nothing was changed.",
                        name(path)
                    ));
                }
                let text = overlay
                    .text(path, &open)?
                    .ok_or_else(|| format!("{} does not exist", name(path)))?;
                let new = apply_text_edits(&text, edits).map_err(|e| edit_error(path, e))?;
                overlay.files.insert(path.clone(), Some(new));
            }
            FileChange::Create {
                path,
                overwrite,
                ignore_if_exists,
            } => {
                if overlay.exists(path, &open) {
                    if *ignore_if_exists {
                        continue;
                    }
                    if !*overwrite {
                        return Err(format!("{} already exists", name(path)));
                    }
                    if open(path).is_some() {
                        return Err(format!(
                            "{} is open; Athena does not replace an open file.",
                            name(path)
                        ));
                    }
                    if overlay
                        .text(path, &open)?
                        .is_some_and(|text| !text.is_empty())
                    {
                        return Err(not_empty(path));
                    }
                }
                overlay.files.insert(path.clone(), Some(String::new()));
            }
            FileChange::Rename {
                from,
                to,
                ignore_if_exists,
                ..
            } => {
                let recased = !overlay.files.contains_key(to)
                    && recases(&overlay.disk_path(from), &overlay.disk_path(to));
                if overlay.exists(to, &open) && !recased {
                    if *ignore_if_exists {
                        continue;
                    }
                    // Overwriting could lose a file the user cannot get back, so it is refused.
                    return Err(format!("{} already exists", name(to)));
                }
                if overlay.disk_path(from).is_dir() && !overlay.files.contains_key(from) {
                    overlay.moved.push((from.clone(), to.clone()));
                    continue;
                }
                let text = overlay
                    .text(from, &open)?
                    .ok_or_else(|| format!("{} does not exist", name(from)))?;
                overlay.files.insert(from.clone(), None);
                overlay.files.insert(to.clone(), Some(text));
            }
            FileChange::Delete {
                path,
                ignore_if_not_exists,
            } => {
                if *ignore_if_not_exists && !overlay.exists(path, &open) {
                    continue;
                }
                return Err(format!(
                    "The language server asked to delete {}; Athena does not delete files for it, so nothing was changed.",
                    name(path)
                ));
            }
        }
    }
    Ok(())
}

/// What applying an edit needs from the editors: send edits to an open file (false when the
/// file is not open), and follow a renamed file with its tabs.
trait Editors {
    fn edit_open(&mut self, path: &Path, edits: &[TextEdit]) -> bool;
    fn renamed(&mut self, from: &Path, to: &Path);
}

/// Applies one checked change; files changed on disk are added to `on_disk`.
fn apply_change(
    change: &FileChange,
    editors: &mut impl Editors,
    applied: &mut Applied,
    on_disk: &mut Vec<(PathBuf, FileEvent)>,
) -> Result<(), String> {
    match change {
        FileChange::Edit { path, edits, .. } => {
            if edits.is_empty() {
                return Ok(());
            }
            applied.files += 1;
            applied.edits += edits.len();
            if editors.edit_open(path, edits) {
                return Ok(());
            }
            let text = read_text(path)?.ok_or_else(|| format!("{} does not exist", name(path)))?;
            let new = apply_text_edits(&text, edits).map_err(|e| edit_error(path, e))?;
            write_text(path, &new)?;
            on_disk.push((path.clone(), FileEvent::Changed));
        }
        FileChange::Create {
            path,
            overwrite,
            ignore_if_exists,
        } => {
            let exists = std::fs::symlink_metadata(path).is_ok();
            if exists && (*ignore_if_exists || !*overwrite) {
                return Ok(());
            }
            if exists && std::fs::metadata(path).is_ok_and(|m| m.len() > 0) {
                return Err(not_empty(path));
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
            }
            match exists {
                true => write_text(path, "")?,
                false => fileops::create_file(path).map_err(|e| format!("{e:#}"))?,
            }
            applied.files += 1;
            on_disk.push((path.clone(), FileEvent::Created));
        }
        FileChange::Rename {
            from,
            to,
            ignore_if_exists,
            ..
        } => {
            if *ignore_if_exists && std::fs::symlink_metadata(to).is_ok() && !recases(from, to) {
                return Ok(());
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
            }
            fileops::rename(from, to).map_err(|e| format!("{e:#}"))?;
            editors.renamed(from, to);
            applied.files += 1;
            on_disk.push((from.clone(), FileEvent::Deleted));
            on_disk.push((to.clone(), FileEvent::Created));
        }
        // The check lets through only deletes of files that are already gone.
        FileChange::Delete { .. } => {}
    }
    Ok(())
}

/// Applies every change of an edit [`check_edit`] passed, in order; returns what changed on disk
/// alongside the result, so callers can report even a partial edit.
fn apply_checked(
    edit: &WorkspaceEdit,
    editors: &mut impl Editors,
) -> (Result<Applied, String>, Vec<(PathBuf, FileEvent)>) {
    let mut on_disk = Vec::new();
    let mut applied = Applied::default();
    let total = edit.changes.len();
    for (step, change) in edit.changes.iter().enumerate() {
        if let Err(why) = apply_change(change, editors, &mut applied, &mut on_disk) {
            let why = match step {
                0 => why,
                _ => format!("{why} (after {step} of {total} changes were made)"),
            };
            return (Err(why), on_disk);
        }
    }
    (Ok(applied), on_disk)
}

struct ShellEditors<'a, 'b> {
    shell: &'a mut Shell,
    cx: &'a mut Context<'b, Shell>,
}

impl Editors for ShellEditors<'_, '_> {
    fn edit_open(&mut self, path: &Path, edits: &[TextEdit]) -> bool {
        let Some(editor) = self.shell.editor_holding(path, self.cx) else {
            return false;
        };
        let edits = server_edits(edits);
        editor.update(self.cx, |e, cx| e.apply_server_edits(&edits, cx));
        true
    }

    fn renamed(&mut self, from: &Path, to: &Path) {
        let (from, to) = (
            self.shell.project_spelling(from),
            self.shell.project_spelling(to),
        );
        self.shell.retarget_items(&from, &to, self.cx);
    }
}

impl Shell {
    /// The editor whose buffer holds `path`; tabs on one file share a buffer, so one is enough.
    fn editor_holding(&self, path: &Path, cx: &Context<Self>) -> Option<Entity<EditorView>> {
        self.editors_showing(&document_key(path), cx)
            .into_iter()
            .next()
    }

    /// Sends every open file's pending change, then notes each one's version, so an edit the
    /// server answers with can be refused if a file it changes is edited meanwhile.
    pub(super) fn versions_for_request(&mut self, cx: &Context<Self>) -> AskedAt {
        let editors: Vec<Entity<EditorView>> = self
            .items
            .values()
            .filter_map(|view| match view {
                ItemView::Editor(e) => Some(e.clone()),
                _ => None,
            })
            .collect();
        let mut asked = AskedAt::new();
        for editor in editors {
            let doc = document_key(editor.read(cx).path());
            self.flush_change(&doc, &editor, cx);
            if let Some(version) = editor.read(cx).version() {
                asked.insert(doc, version as i64);
            }
        }
        asked
    }

    /// Applies an edit the server sent on its own; see [`Self::apply_requested_edit`].
    pub(super) fn apply_workspace_edit(
        &mut self,
        edit: &WorkspaceEdit,
        cx: &mut Context<Self>,
    ) -> Result<Applied, String> {
        self.apply_requested_edit(edit, &AskedAt::new(), cx)
    }

    /// Applies a server's workspace edit: open files through their editor, one undo step each,
    /// closed files written as a save would. Checked whole first; a disk error stops midway.
    pub(super) fn apply_requested_edit(
        &mut self,
        edit: &WorkspaceEdit,
        asked: &AskedAt,
        cx: &mut Context<Self>,
    ) -> Result<Applied, String> {
        let open = |path: &Path| {
            let editor = self.editor_holding(path, cx)?;
            let e = editor.read(cx);
            let doc = document_key(path);
            Some(OpenText {
                text: e.text()?,
                version: e.version().map(|v| v as i64),
                synced: self.lsp_knows(&doc),
                asked: asked.get(&doc).copied(),
            })
        };
        check_edit(edit, open)?;
        let mut editors = ShellEditors { shell: self, cx };
        let (result, on_disk) = apply_checked(edit, &mut editors);
        self.after_disk_changes(&on_disk, cx);
        if let Ok(applied) = &result {
            tracing::info!(
                files = applied.files,
                edits = applied.edits,
                "applied a workspace edit"
            );
        }
        result
    }

    /// Servers hear about files changed behind their back; the tree and git status refresh.
    fn after_disk_changes(&mut self, changes: &[(PathBuf, FileEvent)], cx: &mut Context<Self>) {
        if changes.is_empty() {
            return;
        }
        for client in self.all_clients() {
            client.did_change_watched_files(changes);
        }
        self.tree.invalidate();
        self.git_kick(cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athena_lsp::{Position, Range};

    fn edit(line: u32, start: u32, end: u32, text: &str) -> TextEdit {
        TextEdit {
            range: Range {
                start: Position {
                    line,
                    character: start,
                },
                end: Position {
                    line,
                    character: end,
                },
            },
            text: text.into(),
        }
    }

    fn change(path: &Path, version: Option<i64>, edits: Vec<TextEdit>) -> FileChange {
        FileChange::Edit {
            path: path.to_path_buf(),
            version,
            edits,
        }
    }

    /// Editors as plain texts, the version each server last saw beside them.
    #[derive(Default)]
    struct Fake {
        open: HashMap<PathBuf, (String, i64)>,
        asked: AskedAt,
        renamed: Vec<(PathBuf, PathBuf)>,
    }

    impl Fake {
        fn view(&self) -> impl Fn(&Path) -> Option<OpenText> + '_ {
            |path| {
                let (text, version) = self.open.get(path)?;
                Some(OpenText {
                    text: text.clone(),
                    version: Some(*version),
                    synced: true,
                    asked: self.asked.get(path).copied(),
                })
            }
        }
    }

    impl Editors for Fake {
        fn edit_open(&mut self, path: &Path, edits: &[TextEdit]) -> bool {
            let Some((text, version)) = self.open.get_mut(path) else {
                return false;
            };
            *text = apply_text_edits(text, edits).unwrap();
            *version += 1;
            true
        }

        fn renamed(&mut self, from: &Path, to: &Path) {
            self.renamed.push((from.to_path_buf(), to.to_path_buf()));
        }
    }

    fn run(edit: &WorkspaceEdit, fake: &mut Fake) -> Result<Applied, String> {
        check_edit(edit, fake.view())?;
        apply_checked(edit, fake).0
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("athena-edits-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_rename_reaches_open_and_closed_files_in_utf16_columns() {
        let dir = temp("multi");
        let closed = dir.join("closed.go");
        std::fs::write(&closed, "// héllo 😀 wörld\r\nx := count\r\n").unwrap();
        let open = dir.join("open.go");
        let mut fake = Fake::default();
        fake.open
            .insert(open.clone(), ("count++ // 😀\n".to_string(), 4));
        let edit = WorkspaceEdit {
            changes: vec![
                change(
                    &closed,
                    None,
                    vec![edit(0, 12, 17, "earth"), edit(1, 5, 10, "total")],
                ),
                change(&open, Some(4), vec![edit(0, 0, 5, "total")]),
            ],
        };
        let applied = run(&edit, &mut fake).unwrap();
        assert_eq!(applied, Applied { files: 2, edits: 3 });
        assert_eq!(
            std::fs::read_to_string(&closed).unwrap(),
            "// héllo 😀 earth\r\nx := total\r\n",
            "CRLF line endings survive"
        );
        assert_eq!(fake.open[&open].0, "total++ // 😀\n");
        let (_, on_disk) = apply_checked(&WorkspaceEdit::default(), &mut fake);
        assert!(on_disk.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_edit_that_cannot_apply_anywhere_changes_nothing() {
        let dir = temp("refuse");
        let first = dir.join("a.go");
        let second = dir.join("b.go");
        std::fs::write(&first, "alpha\n").unwrap();
        std::fs::write(&second, "beta gamma\n").unwrap();
        let overlapping = WorkspaceEdit {
            changes: vec![
                change(&first, None, vec![edit(0, 0, 5, "ALPHA")]),
                change(&second, None, vec![edit(0, 0, 6, "x"), edit(0, 3, 8, "y")]),
            ],
        };
        let why = run(&overlapping, &mut Fake::default()).unwrap_err();
        assert!(why.contains("overlap"), "{why}");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "alpha\n");

        let mut fake = Fake::default();
        fake.open.insert(second.clone(), ("beta gamma\n".into(), 9));
        let stale = WorkspaceEdit {
            changes: vec![
                change(&first, None, vec![edit(0, 0, 5, "ALPHA")]),
                change(&second, Some(8), vec![edit(0, 0, 4, "BETA")]),
            ],
        };
        let why = run(&stale, &mut fake).unwrap_err();
        assert!(why.contains("changed while"), "{why}");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "alpha\n");
        assert_eq!(fake.open[&second].0, "beta gamma\n");

        let delete = WorkspaceEdit {
            changes: vec![
                change(&first, None, vec![edit(0, 0, 5, "ALPHA")]),
                FileChange::Delete {
                    path: second.clone(),
                    ignore_if_not_exists: true,
                },
            ],
        };
        let why = run(&delete, &mut Fake::default()).unwrap_err();
        assert!(why.contains("does not delete"), "{why}");
        assert!(second.exists() && std::fs::read_to_string(&first).unwrap() == "alpha\n");
        let gone = WorkspaceEdit {
            changes: vec![FileChange::Delete {
                path: dir.join("never.go"),
                ignore_if_not_exists: true,
            }],
        };
        assert!(
            run(&gone, &mut Fake::default()).is_ok(),
            "a missing file may be deleted"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_edit_naming_no_version_is_refused_once_its_file_changed_since_the_request() {
        let dir = temp("asked");
        let closed = dir.join("a.go");
        let open = dir.join("b.go");
        std::fs::write(&closed, "alpha\n").unwrap();
        let mut fake = Fake::default();
        fake.open.insert(open.clone(), ("beta x\n".into(), 6));
        fake.asked.insert(open.clone(), 5);
        let edit = WorkspaceEdit {
            changes: vec![
                change(&closed, None, vec![edit(0, 0, 5, "ALPHA")]),
                change(&open, None, vec![edit(0, 0, 4, "BETA")]),
            ],
        };
        let why = run(&edit, &mut fake).unwrap_err();
        assert!(why.contains("changed while"), "{why}");
        assert_eq!(std::fs::read_to_string(&closed).unwrap(), "alpha\n");
        assert_eq!(fake.open[&open].0, "beta x\n");

        fake.asked.insert(open.clone(), 6);
        run(&edit, &mut fake).unwrap();
        assert_eq!(fake.open[&open].0, "BETA x\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn creating_over_a_file_only_ever_replaces_an_empty_one() {
        let dir = temp("overwrite");
        let full = dir.join("full.go");
        let empty = dir.join("empty.go");
        std::fs::write(&full, "package keep\n").unwrap();
        std::fs::write(&empty, "").unwrap();
        let create = |path: &Path| FileChange::Create {
            path: path.to_path_buf(),
            overwrite: true,
            ignore_if_exists: false,
        };
        let why = run(
            &WorkspaceEdit {
                changes: vec![create(&full)],
            },
            &mut Fake::default(),
        )
        .unwrap_err();
        assert!(why.contains("not empty"), "{why}");
        assert_eq!(std::fs::read_to_string(&full).unwrap(), "package keep\n");

        let filled_then_replaced = WorkspaceEdit {
            changes: vec![
                change(&empty, None, vec![edit(0, 0, 0, "package x\n")]),
                create(&empty),
            ],
        };
        assert!(run(&filled_then_replaced, &mut Fake::default()).is_err());
        assert_eq!(std::fs::read_to_string(&empty).unwrap(), "");

        let mut applied = Applied::default();
        let why = apply_change(
            &create(&full),
            &mut Fake::default(),
            &mut applied,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(why.contains("not empty"), "the applier refuses too: {why}");
        assert_eq!(std::fs::read_to_string(&full).unwrap(), "package keep\n");

        run(
            &WorkspaceEdit {
                changes: vec![create(&empty)],
            },
            &mut Fake::default(),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&empty).unwrap(), "");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_case_only_rename_goes_through_on_a_case_insensitive_disk() {
        let dir = temp("recase");
        let lower = dir.join("util.go");
        let upper = dir.join("Util.go");
        std::fs::write(&lower, "package util\n").unwrap();
        if !upper.exists() {
            return std::fs::remove_dir_all(&dir).unwrap();
        }
        let edit = WorkspaceEdit {
            changes: vec![FileChange::Rename {
                from: lower.clone(),
                to: upper.clone(),
                overwrite: false,
                ignore_if_exists: false,
            }],
        };
        let mut fake = Fake::default();
        run(&edit, &mut fake).unwrap();
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["Util.go"]);
        assert_eq!(fake.renamed, [(lower.clone(), upper.clone())]);

        let other = dir.join("other.go");
        std::fs::write(&other, "package other\n").unwrap();
        let clobber = WorkspaceEdit {
            changes: vec![FileChange::Rename {
                from: other.clone(),
                to: dir.join("UTIL.go"),
                overwrite: false,
                ignore_if_exists: false,
            }],
        };
        assert!(
            run(&clobber, &mut Fake::default()).is_err(),
            "another file is still refused"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn files_are_created_and_renamed_in_order_and_never_overwritten() {
        let dir = temp("ops");
        let old = dir.join("old.go");
        std::fs::write(&old, "package old\n").unwrap();
        let new = dir.join("pkg/new.go");
        let made = dir.join("gen/made.go");
        let edit = WorkspaceEdit {
            changes: vec![
                FileChange::Rename {
                    from: old.clone(),
                    to: new.clone(),
                    overwrite: false,
                    ignore_if_exists: false,
                },
                change(&new, None, vec![edit(0, 8, 11, "pkg")]),
                FileChange::Create {
                    path: made.clone(),
                    overwrite: false,
                    ignore_if_exists: false,
                },
                change(&made, None, vec![edit(0, 0, 0, "package gen\n")]),
            ],
        };
        let mut fake = Fake::default();
        run(&edit, &mut fake).unwrap();
        assert!(!old.exists());
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "package pkg\n");
        assert_eq!(std::fs::read_to_string(&made).unwrap(), "package gen\n");
        assert_eq!(fake.renamed, [(old.clone(), new.clone())]);

        std::fs::write(&old, "keep me\n").unwrap();
        let clobber = WorkspaceEdit {
            changes: vec![FileChange::Rename {
                from: new.clone(),
                to: old.clone(),
                overwrite: true,
                ignore_if_exists: false,
            }],
        };
        assert!(run(&clobber, &mut Fake::default()).is_err());
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "keep me\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn closed_files_are_read_only_when_they_are_regular_files() {
        let dir = temp("special");
        let fifo = dir.join("pipe.go");
        let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) }, 0);
        let edit = WorkspaceEdit {
            changes: vec![change(&fifo, None, vec![edit(0, 0, 0, "x")])],
        };
        let why = run(&edit, &mut Fake::default()).unwrap_err();
        assert!(why.contains("not a file"), "{why}");
        assert_eq!(read_text(&dir.join("gone.go")), Ok(None));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_closed_file_behind_a_symlink_is_written_through_it() {
        let dir = temp("link");
        let real = dir.join("real.go");
        let link = dir.join("link.go");
        std::fs::write(&real, "var a = 1\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let edit = WorkspaceEdit {
            changes: vec![change(&link, None, vec![edit(0, 4, 5, "b")])],
        };
        run(&edit, &mut Fake::default()).unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "var b = 1\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
