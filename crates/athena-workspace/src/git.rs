//! Git status, diff and blame through `/usr/bin/git`, with the parsers kept pure for tests.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const GIT: &str = "/usr/bin/git";

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
                        FileStatus::from_code(xy[0]),
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
                    start: new_start - 1,
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

fn run(mut cmd: Command, stdin: Option<&str>) -> Result<Vec<u8>> {
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run git")?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A failed write shows up as git's own error below.
        let _ = pipe.write_all(text.as_bytes());
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
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
    let mut cmd = git(root);
    cmd.args(["restore", "--staged", "--"]).args(paths);
    match run(cmd, None) {
        // Before the first commit there is no HEAD to restore from.
        Err(_) => {
            let mut cmd = git(root);
            cmd.args(["rm", "--cached", "-q", "--"]).args(paths);
            run(cmd, None).map(drop)
        }
        ok => ok.map(drop),
    }
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
        let ok = Command::new(GIT)
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
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed");
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
        let dir = std::env::temp_dir().join(format!("athena-git-{}", std::process::id()));
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
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
