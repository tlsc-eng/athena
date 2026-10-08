//! Git status, diff and blame through `/usr/bin/git`, with the parsers kept pure for tests.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const GIT: &str = "/usr/bin/git";
/// A git run still going after this is killed, so a hung git cannot stall the views waiting on it.
const TIMEOUT: Duration = Duration::from_secs(30);

/// What changed about a file, as the tree, tabs and Changes list colour it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Ignored,
    Conflict,
}

impl FileStatus {
    /// Which status a folder shows when its files differ; higher wins.
    pub fn severity(self) -> u8 {
        match self {
            Self::Conflict => 6,
            Self::Deleted => 5,
            Self::Modified => 4,
            Self::Renamed | Self::Added => 3,
            Self::Untracked => 2,
            Self::Ignored => 1,
        }
    }

    /// The one-letter badge VS Code shows next to the file name.
    pub fn letter(self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Untracked => "U",
            Self::Ignored => "I",
            Self::Conflict => "!",
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            b'M' | b'T' => Self::Modified,
            b'A' | b'C' => Self::Added,
            b'D' => Self::Deleted,
            b'R' => Self::Renamed,
            _ => return None,
        })
    }
}

/// One record of `git status --porcelain=v2`, with paths relative to the repository root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    /// The old path of a rename or copy.
    pub orig: Option<String>,
    /// Staged change (index against HEAD).
    pub staged: Option<FileStatus>,
    /// Unstaged change (worktree against index), or Untracked/Ignored/Conflict.
    pub unstaged: Option<FileStatus>,
}

impl Entry {
    /// The status shown for the file: the worktree's if it has one, else the index's.
    pub fn status(&self) -> FileStatus {
        self.unstaged
            .or(self.staged)
            .unwrap_or(FileStatus::Modified)
    }

    /// `git status -uno` style entries for whole directories end in a slash.
    pub fn is_dir(&self) -> bool {
        self.path.ends_with('/')
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub branch: Option<String>,
    pub entries: Vec<Entry>,
}

/// Parses `git status --porcelain=v2 -z --branch` output.
pub fn parse_status(out: &[u8]) -> Status {
    let mut status = Status::default();
    let mut oid = None;
    let mut records = out.split(|&b| b == 0).map(String::from_utf8_lossy);
    while let Some(record) = records.next() {
        let bytes = record.as_bytes();
        if bytes.len() < 2 {
            continue;
        }
        match bytes[0] {
            b'#' => {
                if let Some(head) = record.strip_prefix("# branch.head ") {
                    status.branch = (head != "(detached)").then(|| head.to_string());
                } else if let Some(o) = record.strip_prefix("# branch.oid ") {
                    oid = (o != "(initial)").then(|| o.chars().take(7).collect::<String>());
                }
            }
            b'1' | b'2' | b'u' => {
                let fields = match bytes[0] {
                    b'1' => 9,
                    b'2' => 10,
                    _ => 11,
                };
                let parts: Vec<&str> = record.splitn(fields, ' ').collect();
                if parts.len() < fields {
                    continue;
                }
                let xy = parts[1].as_bytes();
                let path = parts[fields - 1].to_string();
                let (staged, unstaged) = if bytes[0] == b'u' {
                    (None, Some(FileStatus::Conflict))
                } else {
                    (
                        FileStatus::from_code(*xy.first().unwrap_or(&b'.')),
                        FileStatus::from_code(*xy.get(1).unwrap_or(&b'.')),
                    )
                };
                // A rename's old path is the next NUL-separated field, not a record of its own.
                let orig = (bytes[0] == b'2')
                    .then(|| records.next().map(|o| o.into_owned()))
                    .flatten();
                status.entries.push(Entry {
                    path,
                    orig,
                    staged,
                    unstaged,
                });
            }
            b'?' | b'!' => {
                let kind = if bytes[0] == b'?' {
                    FileStatus::Untracked
                } else {
                    FileStatus::Ignored
                };
                status.entries.push(Entry {
                    path: record[2..].to_string(),
                    orig: None,
                    staged: None,
                    unstaged: Some(kind),
                });
            }
            _ => {}
        }
    }
    if status.branch.is_none() {
        status.branch = oid;
    }
    status
}

/// Where the diff against HEAD changed lines, as zero-based lines of the new file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hunk {
    Added {
        start: usize,
        len: usize,
    },
    Modified {
        start: usize,
        len: usize,
    },
    /// Lines were removed just above zero-based line `before`.
    Removed {
        before: usize,
    },
}

/// Reads the `@@ -a,b +c,d @@` headers of a `-U0` unified diff.
pub fn parse_hunks(diff: &str) -> Vec<Hunk> {
    diff.lines()
        .filter_map(|line| {
            let header = line.strip_prefix("@@ -")?;
            let (ranges, _) = header.split_once(" @@")?;
            let (old, new) = ranges.split_once(" +")?;
            let count = |r: &str| -> Option<(usize, usize)> {
                Some(match r.split_once(',') {
                    Some((start, len)) => (start.parse().ok()?, len.parse().ok()?),
                    None => (r.parse().ok()?, 1),
                })
            };
            let (_, old_len) = count(old)?;
            let (new_start, new_len) = count(new)?;
            Some(match (old_len, new_len) {
                (_, 0) => Hunk::Removed { before: new_start },
                (0, len) => Hunk::Added {
                    start: new_start.saturating_sub(1),
                    len,
                },
                (_, len) => Hunk::Modified {
                    start: new_start.saturating_sub(1),
                    len,
                },
            })
        })
        .collect()
}

/// Who last changed a line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blame {
    pub author: String,
    /// Seconds since the epoch.
    pub time: i64,
    pub summary: String,
    pub uncommitted: bool,
}

/// Parses the first entry of `git blame --porcelain`.
pub fn parse_blame(out: &str) -> Option<Blame> {
    let mut lines = out.lines();
    let sha = lines.next()?.split(' ').next()?;
    if sha.len() < 7 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut blame = Blame {
        author: String::new(),
        time: 0,
        summary: String::new(),
        uncommitted: sha.bytes().all(|b| b == b'0'),
    };
    for line in lines {
        if line.starts_with('\t') {
            break;
        }
        if let Some(author) = line.strip_prefix("author ") {
            blame.author = author.to_string();
        } else if let Some(time) = line.strip_prefix("author-time ") {
            blame.time = time.parse().unwrap_or(0);
        } else if let Some(summary) = line.strip_prefix("summary ") {
            blame.summary = summary.to_string();
        }
    }
    Some(blame)
}

impl Blame {
    /// "Ann · 3 days ago · Fix the parser", or the VS Code wording for unsaved and uncommitted lines.
    pub fn caption(&self, now: i64) -> String {
        if self.uncommitted {
            return "You · Not committed yet".into();
        }
        format!(
            "{} · {} · {}",
            self.author,
            relative_time(now - self.time),
            self.summary
        )
    }
}

/// "just now", "5 minutes ago", "3 days ago", "2 years ago".
pub fn relative_time(seconds: i64) -> String {
    let s = seconds.max(0);
    let (n, unit) = match s {
        0..60 => return "just now".into(),
        60..3_600 => (s / 60, "minute"),
        3_600..86_400 => (s / 3_600, "hour"),
        86_400..604_800 => (s / 86_400, "day"),
        604_800..2_592_000 => (s / 604_800, "week"),
        2_592_000..31_536_000 => (s / 2_592_000, "month"),
        _ => (s / 31_536_000, "year"),
    };
    let plural = if n == 1 { "" } else { "s" };
    format!("{n} {unit}{plural} ago")
}

/// Whether running `/usr/bin/git` will work; without the developer tools it opens an install dialog.
pub fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let Ok(out) = Command::new("/usr/bin/xcode-select").arg("-p").output() else {
            return false;
        };
        let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
        out.status.success() && Path::new(&dir).join("usr/bin/git").exists()
    })
}

fn git(root: &Path) -> Command {
    let mut cmd = Command::new(GIT);
    cmd.arg("-C")
        .arg(root)
        .arg("--no-optional-locks")
        // A repository's own config must not make opening it run a command.
        .args(["-c", "core.fsmonitor=false"])
        .env("GIT_TERMINAL_PROMPT", "0")
        // File names like `app/[slug]/page.tsx` are paths, not glob patterns.
        .env("GIT_LITERAL_PATHSPECS", "1")
        .stdin(Stdio::null());
    cmd
}

fn run(cmd: Command, stdin: Option<&str>) -> Result<Vec<u8>> {
    run_within(cmd, stdin, TIMEOUT)
}

/// Error from a run killed for taking longer than its time limit.
#[derive(Debug)]
pub struct TimedOut;

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "git did not finish within {} seconds", TIMEOUT.as_secs())
    }
}

impl std::error::Error for TimedOut {}

fn run_within(mut cmd: Command, stdin: Option<&str>, limit: Duration) -> Result<Vec<u8>> {
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run git")?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let text = text.to_owned();
        // A failed write shows up as git's own error below; a git that never reads hits the limit.
        std::thread::spawn(move || pipe.write_all(text.as_bytes()));
    }
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut out = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut out);
            }
            let _ = tx.send(out);
        });
        rx
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(TimedOut.into());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    // A process git started (a hook, a credential helper) can hold the pipes open after git exits.
    let collect = |pipe: mpsc::Receiver<Vec<u8>>| {
        pipe.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| TimedOut)
    };
    let stdout = collect(stdout)?;
    if !status.success() {
        let stderr = collect(stderr)?;
        // `git commit` with nothing staged explains itself on stdout.
        let said = [stderr, stdout]
            .into_iter()
            .map(|b| String::from_utf8_lossy(&b).trim().to_string())
            .find(|s| !s.is_empty());
        bail!(
            "{}",
            said.unwrap_or_else(|| format!("git failed ({status})"))
        );
    }
    Ok(stdout)
}

/// The project root's path inside its repository ("" at the top, "sub/dir/" below it).
pub fn prefix(root: &Path) -> Result<String> {
    let mut cmd = git(root);
    cmd.args(["rev-parse", "--show-prefix"]);
    Ok(String::from_utf8_lossy(&run(cmd, None)?).trim().to_string())
}

/// A status run's result, with entry paths made absolute under `root`.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub branch: Option<String>,
    pub entries: Vec<(PathBuf, Entry)>,
    /// How long git took, to switch huge repositories to cheaper untracked scanning.
    pub took: Duration,
}

/// Status of everything under `root`; `all_untracked` lists files inside untracked folders too.
pub fn status(root: &Path, prefix: &str, all_untracked: bool) -> Result<Snapshot> {
    let started = Instant::now();
    let mut cmd = git(root);
    cmd.args([
        "status",
        "--porcelain=v2",
        "-z",
        "--branch",
        "--ignored=matching",
        if all_untracked {
            "--untracked-files=all"
        } else {
            "--untracked-files=normal"
        },
        "--",
        ".",
    ]);
    let parsed = parse_status(&run(cmd, None)?);
    let entries = parsed
        .entries
        .into_iter()
        .filter_map(|e| {
            let rel = e.path.strip_prefix(prefix)?.trim_end_matches('/');
            Some((root.join(rel), e))
        })
        .collect();
    Ok(Snapshot {
        branch: parsed.branch,
        entries,
        took: started.elapsed(),
    })
}

/// Changed lines of the saved file against HEAD.
pub fn diff_hunks(root: &Path, path: &Path) -> Result<Vec<Hunk>> {
    let rel = path
        .strip_prefix(root)
        .context("file is outside the project")?;
    let mut cmd = git(root);
    cmd.args(["diff", "HEAD", "--no-color", "--no-ext-diff", "-U0", "--"])
        .arg(rel);
    Ok(parse_hunks(&String::from_utf8_lossy(&run(cmd, None)?)))
}

/// Blame for a zero-based line; `contents` blames unsaved text instead of the file on disk.
pub fn blame_line(
    root: &Path,
    path: &Path,
    line: usize,
    contents: Option<&str>,
) -> Result<Option<Blame>> {
    let rel = path
        .strip_prefix(root)
        .context("file is outside the project")?;
    let n = line + 1;
    let mut cmd = git(root);
    cmd.args(["blame", "--porcelain", "-L", &format!("{n},{n}")]);
    if contents.is_some() {
        cmd.args(["--contents", "-"]);
    }
    cmd.arg("--").arg(rel);
    Ok(parse_blame(&String::from_utf8_lossy(&run(cmd, contents)?)))
}

/// `git add` for the given paths.
pub fn stage(root: &Path, paths: &[PathBuf]) -> Result<()> {
    let mut cmd = git(root);
    cmd.args(["add", "--"]).args(paths);
    run(cmd, None).map(drop)
}

/// Takes the given paths out of the index again, keeping the worktree.
pub fn unstage(root: &Path, paths: &[PathBuf]) -> Result<()> {
    let mut head = git(root);
    head.args(["rev-parse", "-q", "--verify", "HEAD"]);
    let mut cmd = git(root);
    match run(head, None) {
        Ok(_) => cmd.args(["restore", "--staged", "--"]),
        Err(e) if e.is::<TimedOut>() => return Err(e),
        // Before the first commit there is no HEAD to restore from; on a born branch this would
        // stage the files' deletion instead.
        Err(_) => cmd.args(["rm", "--cached", "-q", "--"]),
    };
    cmd.args(paths);
    run(cmd, None).map(drop)
}

/// Which stored version of a file to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rev {
    Head,
    Index,
}

/// A file's contents at HEAD or in the index; `None` when it is not there (new, or no commits yet).
pub fn show(root: &Path, rev: Rev, rel: &Path) -> Result<Option<Vec<u8>>> {
    let spec = match rev {
        Rev::Head => format!("HEAD:./{}", rel.display()),
        Rev::Index => format!(":./{}", rel.display()),
    };
    let mut exists = git(root);
    exists.args(["cat-file", "-e", &spec]);
    match run(exists, None) {
        Ok(_) => {}
        Err(e) if e.is::<TimedOut>() => return Err(e),
        Err(_) => return Ok(None),
    }
    let mut cmd = git(root);
    cmd.args(["cat-file", "blob", &spec]);
    run(cmd, None).map(Some)
}

/// Puts `contents` in the index as `rel`, leaving the worktree alone; staging one hunk does this.
pub fn write_index(root: &Path, rel: &Path, contents: &str) -> Result<()> {
    let mut hash = git(root);
    hash.args(["hash-object", "-w", "--stdin"]);
    let sha = String::from_utf8_lossy(&run(hash, Some(contents))?)
        .trim()
        .to_string();
    let mut ls = git(root);
    ls.args(["ls-files", "-s", "--"]).arg(rel);
    let listed = String::from_utf8_lossy(&run(ls, None)?).into_owned();
    let mode = match listed.split_whitespace().next() {
        Some(mode) => mode.to_string(),
        None => {
            let exec = std::fs::metadata(root.join(rel)).is_ok_and(|m| {
                std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0
            });
            if exec { "100755" } else { "100644" }.to_string()
        }
    };
    let mut update = git(root);
    update
        .args(["update-index", "--add", "--cacheinfo"])
        .arg(format!("{mode},{sha},{}", rel.display()));
    run(update, None).map(drop)
}

/// Pre-commit hooks can run a test suite; the usual limit would kill them.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(600);

/// Commits the index with `message`, or rewrites the last commit with it when `amend`.
pub fn commit(root: &Path, message: &str, amend: bool) -> Result<()> {
    let mut cmd = git(root);
    cmd.args(["commit", "--quiet", "--file", "-"]);
    if amend {
        cmd.arg("--amend");
    }
    run_within(cmd, Some(message), COMMIT_TIMEOUT).map(drop)
}

/// The last commit's full message, for the amend box.
pub fn last_message(root: &Path) -> Result<String> {
    let mut cmd = git(root);
    cmd.args(["log", "-1", "--format=%B"]);
    Ok(String::from_utf8_lossy(&run(cmd, None)?)
        .trim_end()
        .to_string())
}

/// Copies a file into `backup` (keeping its relative path) before it is overwritten or deleted.
fn keep_copy(root: &Path, rel: &Path, backup: &Path) -> Result<()> {
    let from = root.join(rel);
    if !from.is_file() {
        return Ok(());
    }
    let to = backup.join(rel);
    std::fs::create_dir_all(to.parent().unwrap_or(backup))?;
    std::fs::copy(&from, &to).with_context(|| format!("keep a copy of {}", rel.display()))?;
    Ok(())
}

/// Throws away a tracked file's unstaged changes, keeping a copy of it in `backup` first.
pub fn discard(root: &Path, rel: &Path, backup: &Path) -> Result<()> {
    keep_copy(root, rel, backup)?;
    let mut cmd = git(root);
    cmd.args(["restore", "--worktree", "--"]).arg(rel);
    run(cmd, None).map(drop)
}

/// Writes `contents` over a file that still holds `expected`, keeping a copy in `backup` first.
pub fn revert_file(
    root: &Path,
    rel: &Path,
    expected: &str,
    contents: &str,
    backup: &Path,
) -> Result<()> {
    let path = root.join(rel);
    let now = std::fs::read(&path).unwrap_or_default();
    if now != expected.as_bytes() {
        bail!("{} changed since the diff was shown", rel.display());
    }
    keep_copy(root, rel, backup)?;
    let tmp = path.with_file_name(format!(
        ".{}.athena-revert",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::write(&tmp, contents)?;
    if let Ok(meta) = std::fs::metadata(&path) {
        std::fs::set_permissions(&tmp, meta.permissions())?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// A local or remote-tracking branch, newest commit first in `branches`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    /// `main`, or `origin/main` for a remote one.
    pub name: String,
    pub remote: bool,
    pub current: bool,
    /// "3 days ago".
    pub when: String,
    pub subject: String,
}

/// Parses `for-each-ref` records of refname, short name, HEAD marker, date and subject.
pub fn parse_branches(out: &str) -> Vec<Branch> {
    out.lines()
        .filter_map(|line| {
            let mut f = line.split('\0');
            let full = f.next()?;
            let name = f.next()?.to_string();
            let current = f.next()? == "*";
            let when = f.next().unwrap_or_default().to_string();
            let subject = f.next().unwrap_or_default().to_string();
            let remote = full.starts_with("refs/remotes/");
            // `origin/HEAD` points at another branch; listing it twice helps nobody.
            (!(remote && full.ends_with("/HEAD"))).then_some(Branch {
                name,
                remote,
                current,
                when,
                subject,
            })
        })
        .collect()
}

pub fn branches(root: &Path) -> Result<Vec<Branch>> {
    let mut cmd = git(root);
    cmd.args([
        "for-each-ref",
        "--sort=-committerdate",
        "--format=%(refname)%00%(refname:short)%00%(HEAD)%00%(committerdate:relative)%00%(subject)",
        "refs/heads",
        "refs/remotes",
    ]);
    Ok(parse_branches(&String::from_utf8_lossy(&run(cmd, None)?)))
}

/// `git switch`: to a local branch, to a new local branch tracking a remote one, or to a new
/// branch made at HEAD. Git itself refuses when local changes would be overwritten.
pub fn switch(root: &Path, branch: &str, how: Switch) -> Result<()> {
    if branch.starts_with('-') {
        bail!("a branch name cannot start with a dash");
    }
    let mut cmd = git(root);
    cmd.arg("switch");
    match how {
        Switch::Existing => cmd.arg("--no-guess"),
        Switch::Track => cmd.arg("--track"),
        Switch::Create => cmd.arg("-c"),
    };
    cmd.arg(branch);
    run(cmd, None).map(drop)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Switch {
    Existing,
    Track,
    Create,
}

/// Per-file statuses plus every folder's most severe one, for colouring a file tree.
#[derive(Clone, Debug, Default)]
pub struct Decorations {
    files: HashMap<PathBuf, FileStatus>,
    dirs: HashMap<PathBuf, FileStatus>,
    /// Untracked or ignored folders listed whole, whose files are not listed one by one.
    whole: HashMap<PathBuf, FileStatus>,
}

impl Decorations {
    pub fn new(root: &Path, entries: &[(PathBuf, Entry)]) -> Self {
        let mut out = Self::default();
        for (path, entry) in entries {
            let status = entry.status();
            if entry.is_dir() {
                out.whole.insert(path.clone(), status);
            } else {
                out.files.insert(path.clone(), status);
            }
            if status == FileStatus::Ignored {
                continue;
            }
            for dir in path.ancestors().skip(1) {
                if !dir.starts_with(root) || dir == root {
                    break;
                }
                let slot = out.dirs.entry(dir.to_path_buf()).or_insert(status);
                if status.severity() > slot.severity() {
                    *slot = status;
                }
            }
        }
        out
    }

    pub fn get(&self, path: &Path) -> Option<FileStatus> {
        if let Some(s) = self.files.get(path).or_else(|| self.dirs.get(path)) {
            return Some(*s);
        }
        path.ancestors().find_map(|a| self.whole.get(a)).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.whole.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nul(records: &[&str]) -> Vec<u8> {
        let mut out = records.join("\0").into_bytes();
        out.push(0);
        out
    }

    #[test]
    fn status_reads_every_record_kind() {
        let out = nul(&[
            "# branch.oid 0930e963f1c8991bb9873ea31854b682b2ec228a",
            "# branch.head feat/x",
            "1 .M N... 100644 100644 100644 de98 de98 src/main.rs",
            "1 A. N... 000000 100644 100644 0000 abcd new file.rs",
            "2 R. N... 100644 100644 100644 587b 587b R100 to.txt",
            "from.txt",
            "u UU N... 100644 100644 100644 100644 a b c conflict.rs",
            "? notes/todo.md",
            "! target/",
        ]);
        let status = parse_status(&out);
        assert_eq!(status.branch.as_deref(), Some("feat/x"));
        let e = &status.entries;
        assert_eq!(e.len(), 6);
        assert_eq!(e[0].path, "src/main.rs");
        assert_eq!(e[0].unstaged, Some(FileStatus::Modified));
        assert_eq!(e[0].staged, None);
        assert_eq!(e[1].path, "new file.rs");
        assert_eq!(e[1].status(), FileStatus::Added);
        assert_eq!(e[2].path, "to.txt");
        assert_eq!(e[2].orig.as_deref(), Some("from.txt"));
        assert_eq!(e[2].status(), FileStatus::Renamed);
        assert_eq!(e[3].status(), FileStatus::Conflict);
        assert_eq!(e[4].status(), FileStatus::Untracked);
        assert_eq!(e[4].path, "notes/todo.md");
        assert_eq!(e[5].status(), FileStatus::Ignored);
        assert!(e[5].is_dir());
    }

    #[test]
    fn detached_head_shows_the_short_commit() {
        let out = nul(&["# branch.oid 0123456789abcdef", "# branch.head (detached)"]);
        assert_eq!(parse_status(&out).branch.as_deref(), Some("0123456"));
    }

    #[test]
    fn hunk_headers_become_marks() {
        let diff = "diff --git a/f b/f\n\
                    @@ -0,0 +1,3 @@\n+a\n+b\n+c\n\
                    @@ -5 +5 @@ fn main() {\n-x\n+y\n\
                    @@ -7,2 +7,0 @@\n-p\n-q\n\
                    @@ -10,1 +9,2 @@\n-r\n+s\n+t\n";
        assert_eq!(
            parse_hunks(diff),
            vec![
                Hunk::Added { start: 0, len: 3 },
                Hunk::Modified { start: 4, len: 1 },
                Hunk::Removed { before: 7 },
                Hunk::Modified { start: 8, len: 2 },
            ]
        );
    }

    #[test]
    fn malformed_status_and_hunk_headers_do_not_panic() {
        assert_eq!(
            parse_hunks("@@ -0,0 +0,2 @@\n+a\n+b\n"),
            vec![Hunk::Added { start: 0, len: 2 }]
        );
        let status = parse_status(b"1  N... 100644 100644 100644 a b x.rs\0");
        assert!(status.entries.len() <= 1);
    }

    #[test]
    fn blame_porcelain_gives_author_time_and_summary() {
        let out = "0930e963f1c8991bb9873ea31854b682b2ec228a 1 1 1\n\
                   author Ann Lee\nauthor-mail <ann@x>\nauthor-time 1700000000\n\
                   author-tz +0000\nsummary Fix the parser\nfilename f.txt\n\ta\n";
        let blame = parse_blame(out).unwrap();
        assert_eq!(blame.author, "Ann Lee");
        assert_eq!(blame.time, 1_700_000_000);
        assert!(!blame.uncommitted);
        assert_eq!(
            blame.caption(1_700_000_000 + 3 * 86_400),
            "Ann Lee · 3 days ago · Fix the parser"
        );
        let mine = parse_blame(
            "0000000000000000000000000000000000000000 2 2 1\nauthor Not Committed Yet\n\tB\n",
        )
        .unwrap();
        assert!(mine.uncommitted);
        assert_eq!(mine.caption(0), "You · Not committed yet");
    }

    #[test]
    fn relative_times_read_like_vs_code() {
        assert_eq!(relative_time(5), "just now");
        assert_eq!(relative_time(60), "1 minute ago");
        assert_eq!(relative_time(7_200), "2 hours ago");
        assert_eq!(relative_time(86_400 * 8), "1 week ago");
        assert_eq!(relative_time(31_536_000 * 2), "2 years ago");
    }

    #[test]
    fn folders_take_their_most_severe_file() {
        let root = Path::new("/r");
        let entry = |path: &str, s: FileStatus| {
            (
                root.join(path.trim_end_matches('/')),
                Entry {
                    path: path.into(),
                    orig: None,
                    staged: None,
                    unstaged: Some(s),
                },
            )
        };
        let d = Decorations::new(
            root,
            &[
                entry("a/b/new.rs", FileStatus::Untracked),
                entry("a/c.rs", FileStatus::Modified),
                entry("a/b/x/", FileStatus::Untracked),
                entry("target/", FileStatus::Ignored),
            ],
        );
        assert_eq!(d.get(&root.join("a")), Some(FileStatus::Modified));
        assert_eq!(d.get(&root.join("a/b")), Some(FileStatus::Untracked));
        assert_eq!(
            d.get(&root.join("a/b/x/deep.rs")),
            Some(FileStatus::Untracked)
        );
        assert_eq!(d.get(&root.join("target/debug")), Some(FileStatus::Ignored));
        assert_eq!(d.get(&root.join("a/d.rs")), None);
        assert_eq!(d.get(root), None);
    }

    fn repo_git(dir: &Path, args: &[&str]) {
        let out = Command::new(GIT)
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    #[test]
    fn output_held_open_by_a_leftover_process_hits_the_time_limit() {
        let started = Instant::now();
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "/bin/sleep 5 & echo started"]);
        let err = run_within(cmd, None, Duration::from_millis(300)).unwrap_err();
        assert!(err.is::<TimedOut>());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn a_hung_command_is_killed_at_the_time_limit() {
        let started = Instant::now();
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("30");
        let err = run_within(cmd, None, Duration::from_millis(200)).unwrap_err();
        assert!(err.is::<TimedOut>());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn brackets_are_paths_and_the_repository_fsmonitor_never_runs() {
        if !available() {
            eprintln!("git is not installed; skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("athena-git-lit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("app/[slug]")).unwrap();
        std::fs::create_dir_all(dir.join("app/s")).unwrap();
        let dir = dir.canonicalize().unwrap();
        std::fs::write(dir.join("app/[slug]/page.tsx"), "a\n").unwrap();
        std::fs::write(dir.join("app/s/page.tsx"), "b\n").unwrap();
        repo_git(&dir, &["init", "-q"]);
        let marker = dir.join("fsmonitor-ran");
        let hook = dir.join("fsmonitor.sh");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        repo_git(&dir, &["config", "core.fsmonitor", hook.to_str().unwrap()]);

        stage(&dir, &[PathBuf::from("app/[slug]/page.tsx")]).unwrap();
        let snap = status(&dir, &prefix(&dir).unwrap(), true).unwrap();
        let of = |p: &str| {
            let (_, e) = snap
                .entries
                .iter()
                .find(|(path, _)| *path == dir.join(p))
                .unwrap();
            e.status()
        };
        assert_eq!(of("app/[slug]/page.tsx"), FileStatus::Added);
        assert_eq!(of("app/s/page.tsx"), FileStatus::Untracked);
        assert!(
            !marker.exists(),
            "status ran the repository's fsmonitor hook"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_real_repository_round_trips_status_diff_and_blame() {
        if !available() {
            eprintln!("git is not installed; skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("athena-git-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let dir = dir.canonicalize().unwrap();
        std::fs::write(dir.join("sub/f.txt"), "a\nb\nc\n").unwrap();
        std::fs::write(dir.join("old.txt"), "x\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "ign/\n").unwrap();
        repo_git(&dir, &["init", "-q"]);
        repo_git(&dir, &["add", "-A"]);
        repo_git(&dir, &["commit", "-qm", "init"]);
        repo_git(&dir, &["mv", "old.txt", "new.txt"]);
        std::fs::write(dir.join("sub/f.txt"), "a\nB\nc\nd\n").unwrap();
        std::fs::write(dir.join("sub/u.txt"), "u\n").unwrap();
        std::fs::create_dir_all(dir.join("ign")).unwrap();
        std::fs::write(dir.join("ign/i"), "i\n").unwrap();

        let snap = status(&dir, &prefix(&dir).unwrap(), true).unwrap();
        assert_eq!(snap.branch.as_deref(), Some("main"));
        let find = |p: &str| snap.entries.iter().find(|(path, _)| *path == dir.join(p));
        let (_, renamed) = find("new.txt").unwrap();
        assert_eq!(renamed.orig.as_deref(), Some("old.txt"));
        assert_eq!(renamed.staged, Some(FileStatus::Renamed));
        assert_eq!(find("sub/f.txt").unwrap().1.status(), FileStatus::Modified);
        assert_eq!(find("sub/u.txt").unwrap().1.status(), FileStatus::Untracked);
        assert_eq!(find("ign").unwrap().1.status(), FileStatus::Ignored);

        // A project opened on a subfolder sees paths relative to itself.
        let sub = dir.join("sub");
        let pre = prefix(&sub).unwrap();
        assert_eq!(pre, "sub/");
        let nested = status(&sub, &pre, true).unwrap();
        let paths: Vec<_> = nested.entries.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(paths, vec![sub.join("f.txt"), sub.join("u.txt")]);

        assert_eq!(
            diff_hunks(&sub, &sub.join("f.txt")).unwrap(),
            vec![
                Hunk::Modified { start: 1, len: 1 },
                Hunk::Added { start: 3, len: 1 }
            ]
        );
        let first = blame_line(&sub, &sub.join("f.txt"), 0, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            (first.author.as_str(), first.summary.as_str()),
            ("Test", "init")
        );
        let changed = blame_line(&sub, &sub.join("f.txt"), 1, None)
            .unwrap()
            .unwrap();
        assert!(changed.uncommitted);
        let unsaved = blame_line(&sub, &sub.join("f.txt"), 0, Some("z\nB\n"))
            .unwrap()
            .unwrap();
        assert!(unsaved.uncommitted);

        stage(&sub, &[PathBuf::from("u.txt")]).unwrap();
        let staged = status(&sub, &pre, true).unwrap();
        let u = staged
            .entries
            .iter()
            .find(|(p, _)| *p == sub.join("u.txt"))
            .unwrap();
        assert_eq!(u.1.staged, Some(FileStatus::Added));
        unstage(&sub, &[PathBuf::from("u.txt")]).unwrap();
        let back = status(&sub, &pre, true).unwrap();
        let u = back
            .entries
            .iter()
            .find(|(p, _)| *p == sub.join("u.txt"))
            .unwrap();
        assert_eq!(u.1.status(), FileStatus::Untracked);

        // A failed restore on a born branch is reported, not retried as `rm --cached`.
        stage(&sub, &[PathBuf::from("f.txt")]).unwrap();
        let err = unstage(&sub, &[PathBuf::from("f.txt"), PathBuf::from("gone.txt")]).unwrap_err();
        assert!(err.to_string().contains("known to git"), "{err:#}");
        let still = status(&sub, &pre, true).unwrap();
        let f = still
            .entries
            .iter()
            .find(|(p, _)| *p == sub.join("f.txt"))
            .unwrap();
        assert_eq!(f.1.staged, Some(FileStatus::Modified));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unstaging_before_the_first_commit_untracks_the_file() {
        if !available() {
            eprintln!("git is not installed; skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("athena-git-unborn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        repo_git(&dir, &["init", "-q"]);
        stage(&dir, &[PathBuf::from("a.txt")]).unwrap();
        unstage(&dir, &[PathBuf::from("a.txt")]).unwrap();
        let snap = status(&dir, "", true).unwrap();
        let (_, a) = snap
            .entries
            .iter()
            .find(|(p, _)| *p == dir.join("a.txt"))
            .unwrap();
        assert_eq!(a.status(), FileStatus::Untracked);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A repository with one commit of `f.txt`, set up so our own git runs can commit in it.
    fn committed_repo(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("athena-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        std::fs::write(dir.join("f.txt"), contents).unwrap();
        repo_git(&dir, &["init", "-q"]);
        for (key, value) in [
            ("user.name", "Test"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "/dev/null"),
        ] {
            repo_git(&dir, &["config", key, value]);
        }
        repo_git(&dir, &["add", "-A"]);
        repo_git(&dir, &["commit", "-qm", "init"]);
        dir
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let mut cmd = git(dir);
        cmd.args(args);
        String::from_utf8(run(cmd, None).unwrap()).unwrap()
    }

    #[test]
    fn staging_one_hunk_leaves_the_others_unstaged() {
        if !available() {
            return;
        }
        let dir = committed_repo("stage-hunk", "a\nb\nc\nd\n");
        std::fs::write(dir.join("f.txt"), "a\nB\nc\nD\n").unwrap();
        write_index(&dir, Path::new("f.txt"), "a\nB\nc\nd\n").unwrap();
        let staged = git_out(&dir, &["diff", "--cached", "--no-color"]);
        let unstaged = git_out(&dir, &["diff", "--no-color"]);
        assert!(staged.contains("+B") && !staged.contains("+D"), "{staged}");
        assert!(
            unstaged.contains("+D") && !unstaged.contains("+B"),
            "{unstaged}"
        );
        let index = show(&dir, Rev::Index, Path::new("f.txt")).unwrap().unwrap();
        assert_eq!(index, b"a\nB\nc\nd\n");
        let head = show(&dir, Rev::Head, Path::new("f.txt")).unwrap().unwrap();
        assert_eq!(head, b"a\nb\nc\nd\n");
        assert_eq!(show(&dir, Rev::Head, Path::new("new.txt")).unwrap(), None);

        // Unstaging writes HEAD's text back to the index.
        write_index(&dir, Path::new("f.txt"), "a\nb\nc\nd\n").unwrap();
        assert!(git_out(&dir, &["diff", "--cached"]).is_empty());

        // A hunk of an untracked file stages it as added.
        std::fs::write(dir.join("new.txt"), "x\ny\n").unwrap();
        write_index(&dir, Path::new("new.txt"), "x\n").unwrap();
        let snap = status(&dir, "", true).unwrap();
        let (_, new) = snap
            .entries
            .iter()
            .find(|(p, _)| *p == dir.join("new.txt"))
            .unwrap();
        assert_eq!(new.staged, Some(FileStatus::Added));
        assert_eq!(new.unstaged, Some(FileStatus::Modified));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reverting_a_hunk_keeps_a_copy_and_refuses_a_file_that_moved_on() {
        if !available() {
            return;
        }
        let dir = committed_repo("revert-hunk", "a\nb\n");
        let backup = dir.join("backup");
        std::fs::write(dir.join("f.txt"), "a\nB\nc\n").unwrap();
        let err = revert_file(&dir, Path::new("f.txt"), "stale", "a\nb\nc\n", &backup);
        assert!(err.is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "a\nB\nc\n"
        );
        revert_file(&dir, Path::new("f.txt"), "a\nB\nc\n", "a\nb\nc\n", &backup).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "a\nb\nc\n"
        );
        assert_eq!(
            std::fs::read_to_string(backup.join("f.txt")).unwrap(),
            "a\nB\nc\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn commit_then_amend_rewrites_the_last_commit() {
        if !available() {
            return;
        }
        let dir = committed_repo("commit", "a\n");
        std::fs::write(dir.join("f.txt"), "b\n").unwrap();
        stage(&dir, &[PathBuf::from("f.txt")]).unwrap();
        commit(&dir, "Change a to b\n\nWith a body.", false).unwrap();
        assert_eq!(last_message(&dir).unwrap(), "Change a to b\n\nWith a body.");
        assert_eq!(git_out(&dir, &["rev-list", "--count", "HEAD"]).trim(), "2");
        std::fs::write(dir.join("f.txt"), "c\n").unwrap();
        stage(&dir, &[PathBuf::from("f.txt")]).unwrap();
        commit(&dir, "Change a to c", true).unwrap();
        assert_eq!(last_message(&dir).unwrap(), "Change a to c");
        assert_eq!(git_out(&dir, &["rev-list", "--count", "HEAD"]).trim(), "2");
        assert_eq!(
            show(&dir, Rev::Head, Path::new("f.txt")).unwrap().unwrap(),
            b"c\n"
        );
        let nothing = commit(&dir, "Empty", false).unwrap_err();
        assert!(nothing.to_string().contains("nothing"), "{nothing:#}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn discarding_restores_the_index_version_and_keeps_a_copy() {
        if !available() {
            return;
        }
        let dir = committed_repo("discard", "a\n");
        let backup = dir.join("backup");
        std::fs::write(dir.join("f.txt"), "mine\n").unwrap();
        discard(&dir, Path::new("f.txt"), &backup).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "a\n");
        assert_eq!(
            std::fs::read_to_string(backup.join("f.txt")).unwrap(),
            "mine\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn switching_branches_is_refused_over_conflicting_local_changes() {
        if !available() {
            return;
        }
        let dir = committed_repo("switch", "a\n");
        switch(&dir, "feat", Switch::Create).unwrap();
        std::fs::write(dir.join("f.txt"), "feat\n").unwrap();
        stage(&dir, &[PathBuf::from("f.txt")]).unwrap();
        commit(&dir, "On feat", false).unwrap();
        switch(&dir, "main", Switch::Existing).unwrap();
        let list = branches(&dir).unwrap();
        let names: Vec<(&str, bool)> = list.iter().map(|b| (b.name.as_str(), b.current)).collect();
        assert!(
            names.contains(&("main", true)) && names.contains(&("feat", false)),
            "{names:?}"
        );

        std::fs::write(dir.join("f.txt"), "dirty\n").unwrap();
        let err = switch(&dir, "feat", Switch::Existing).unwrap_err();
        assert!(err.to_string().contains("overwritten"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "dirty\n"
        );
        assert_eq!(git_out(&dir, &["branch", "--show-current"]).trim(), "main");
        assert!(switch(&dir, "-f", Switch::Create).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn branch_records_skip_the_remote_head_alias() {
        let out = "refs/heads/main\0main\0*\x002 days ago\0Fix\n\
                   refs/remotes/origin/HEAD\0origin\0 \x001 day ago\0x\n\
                   refs/remotes/origin/dev\0origin/dev\0 \x001 day ago\0Dev work\n";
        let list = parse_branches(out);
        assert_eq!(list.len(), 2);
        assert!(list[0].current && !list[0].remote);
        assert_eq!(list[1].name, "origin/dev");
        assert!(list[1].remote);
        assert_eq!(list[1].subject, "Dev work");
    }
}
