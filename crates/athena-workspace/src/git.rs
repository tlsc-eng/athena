//! Git status, diff and blame through `/usr/bin/git`, with the parsers kept pure for tests.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
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

/// The branch's upstream and how far apart they are, from the last fetch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tracking {
    /// `origin/main`.
    pub upstream: String,
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub branch: Option<String>,
    pub entries: Vec<Entry>,
    pub tracking: Option<Tracking>,
}

/// Parses `git status --porcelain=v2 -z --branch` output.
pub fn parse_status(out: &[u8]) -> Status {
    let mut status = Status::default();
    let mut oid = None;
    let mut upstream = None;
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
                } else if let Some(up) = record.strip_prefix("# branch.upstream ") {
                    upstream = Some(up.to_string());
                } else if let Some(ab) = record.strip_prefix("# branch.ab ") {
                    let tracking = status.tracking.get_or_insert_default();
                    for part in ab.split(' ') {
                        if let Some(n) = part.strip_prefix('+') {
                            tracking.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            tracking.behind = n.parse().unwrap_or(0);
                        }
                    }
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
    // git leaves out branch.ab when the upstream is gone, and then there is nothing to sync.
    match (upstream, &mut status.tracking) {
        (Some(up), Some(tracking)) => tracking.upstream = up,
        _ => status.tracking = None,
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

/// A git that runs the clean and smudge filters config names, for commands the user asked for
/// that write file contents and would store or check out the wrong bytes without them.
fn filtering_git(root: &Path) -> Command {
    let mut cmd = Command::new(GIT);
    cmd.arg("-C")
        .arg(root)
        .arg("--no-optional-locks")
        // A repository's own config must not make opening it run a command.
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "log.showSignature=false",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        // File names like `app/[slug]/page.tsx` are paths, not glob patterns.
        .env("GIT_LITERAL_PATHSPECS", "1")
        .stdin(Stdio::null());
    cmd
}

/// A git whose filter drivers are all switched off, so reading a repository never runs a command
/// its config names; line-ending conversion is built in and still applies.
fn git(root: &Path) -> Command {
    let mut probe = filtering_git(root);
    probe.args(["config", "--list", "-z"]);
    // A config git cannot read also fails the real run, before any filter.
    let drivers = run(probe, None)
        .map(|out| filter_drivers(&out))
        .unwrap_or_default();
    let mut cmd = filtering_git(root);
    without_filters(&mut cmd, &drivers);
    cmd
}

/// Driver names in `git config --list -z` output, from its `filter.<name>.<key>` entries.
fn filter_drivers(out: &[u8]) -> Vec<String> {
    let mut names: Vec<String> = out
        .split(|&b| b == 0)
        .filter_map(|entry| {
            let key = String::from_utf8_lossy(entry.split(|&b| b == b'\n').next()?).into_owned();
            let (name, _) = key.strip_prefix("filter.")?.rsplit_once('.')?;
            Some(name.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Empties each driver's commands through `GIT_CONFIG_KEY_n`, which takes a name with `=` or
/// spaces as it is, after any such settings Athena itself was started with.
fn without_filters(cmd: &mut Command, drivers: &[String]) {
    let mut n: usize = std::env::var("GIT_CONFIG_COUNT")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    for name in drivers {
        for (key, value) in [
            ("clean", ""),
            ("smudge", ""),
            ("process", ""),
            ("required", "false"),
        ] {
            cmd.env(
                format!("GIT_CONFIG_KEY_{n}"),
                format!("filter.{name}.{key}"),
            )
            .env(format!("GIT_CONFIG_VALUE_{n}"), value);
            n += 1;
        }
    }
    cmd.env("GIT_CONFIG_COUNT", n.to_string());
}

fn run(cmd: Command, stdin: Option<&str>) -> Result<Vec<u8>> {
    run_within(cmd, stdin, TIMEOUT)
}

/// A git that talks to a remote: no prompt can wait on a terminal or dialog nobody sees.
fn remote_git(root: &Path) -> Command {
    let mut probe = filtering_git(root);
    probe.args(["config", "--get", "core.sshCommand"]);
    let ssh_configured = ["GIT_SSH_COMMAND", "GIT_SSH"]
        .iter()
        .any(|var| std::env::var_os(var).is_some_and(|v| !v.is_empty()))
        || run(probe, None).is_ok_and(|out| !out.trim_ascii().is_empty());
    let mut cmd = filtering_git(root);
    never_prompt(&mut cmd, ssh_configured);
    cmd
}

/// The user's own ssh command is left alone; without a terminal its prompts still fail at once.
fn never_prompt(cmd: &mut Command, ssh_configured: bool) {
    cmd.env("GIT_ASKPASS", "/usr/bin/true")
        .env("SSH_ASKPASS_REQUIRE", "never");
    if !ssh_configured {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
    unsafe {
        cmd.pre_exec(|| match libc::setsid() {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
}

/// Whether the child has exited and with status 0, left unreaped so its id cannot be reused
/// while its group is signalled.
fn exited(pid: libc::pid_t) -> Option<bool> {
    // SAFETY: an all-zero siginfo_t is a valid value for waitid to fill in.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: waits on our own child with WNOWAIT, so `Child::wait` still reaps it.
    let r = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if r == -1 {
        let interrupted = std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted;
        return (!interrupted).then_some(false);
    }
    (info.si_pid == pid).then_some(info.si_code == libc::CLD_EXITED && info.si_status == 0)
}

/// A remote op leads its own session, so its ssh and helpers go with it.
fn kill_all(child: &mut Child) {
    // SAFETY: the child is not reaped yet, so a group with its id can only be its own.
    unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
    let _ = child.kill();
}

/// Error from a run killed for taking longer than its time limit.
#[derive(Debug)]
pub struct TimedOut(Duration);

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "git did not finish within {} seconds", self.0.as_secs())
    }
}

impl std::error::Error for TimedOut {}

/// Error from a run stopped through its [`Cancel`].
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("git was stopped")
    }
}

impl std::error::Error for Cancelled {}

/// Stops a run from another thread: its git is killed and the run fails with [`Cancelled`].
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Cancels when dropped, so dropping the task that holds it stops the git it started.
    pub fn on_drop(&self) -> CancelOnDrop {
        CancelOnDrop(self.clone())
    }
}

/// See [`Cancel::on_drop`].
pub struct CancelOnDrop(Cancel);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn run_within(cmd: Command, stdin: Option<&str>, limit: Duration) -> Result<Vec<u8>> {
    run_until(cmd, stdin, limit, &Cancel::default())
}

fn run_until(
    mut cmd: Command,
    stdin: Option<&str>,
    limit: Duration,
    cancel: &Cancel,
) -> Result<Vec<u8>> {
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
    let pid = child.id() as libc::pid_t;
    let succeeded = loop {
        if let Some(succeeded) = exited(pid) {
            break succeeded;
        }
        if cancel.is_cancelled() {
            kill_all(&mut child);
            let _ = child.wait();
            return Err(Cancelled.into());
        }
        if Instant::now() >= deadline {
            kill_all(&mut child);
            let _ = child.wait();
            return Err(TimedOut(limit).into());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    // A process git started (a hook, a credential helper) can hold the pipes open after git exits.
    let collect = |pipe: mpsc::Receiver<Vec<u8>>| {
        pipe.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| TimedOut(limit))
    };
    let stdout = collect(stdout);
    let stderr = (!succeeded).then(|| collect(stderr));
    if stdout.is_err() || stderr.as_ref().is_some_and(Result::is_err) {
        kill_all(&mut child);
    }
    let status = child.wait()?;
    let stdout = stdout?;
    if let Some(stderr) = stderr {
        let stderr = stderr?;
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
    pub tracking: Option<Tracking>,
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
        tracking: parsed.tracking,
        took: started.elapsed(),
    })
}

/// Changed lines of the saved file against the index, as VS Code's quick diff compares them.
pub fn diff_hunks(root: &Path, path: &Path) -> Result<Vec<Hunk>> {
    let rel = path
        .strip_prefix(root)
        .context("file is outside the project")?;
    let mut cmd = git(root);
    cmd.args([
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "-U0",
        "--",
    ])
    .arg(rel);
    Ok(parse_hunks(&String::from_utf8_lossy(&run(cmd, None)?)))
}

/// Blame for a zero-based line; `contents` blames unsaved text instead of the file on disk.
pub fn blame_line(
    root: &Path,
    path: &Path,
    line: usize,
    contents: Option<&str>,
    cancel: &Cancel,
) -> Result<Option<Blame>> {
    let rel = path
        .strip_prefix(root)
        .context("file is outside the project")?;
    let n = line + 1;
    let mut cmd = git(root);
    cmd.args([
        "blame",
        "--porcelain",
        "--no-textconv",
        "-L",
        &format!("{n},{n}"),
    ]);
    if contents.is_some() {
        cmd.args(["--contents", "-"]);
    }
    cmd.arg("--").arg(rel);
    let out = run_until(cmd, contents, TIMEOUT, cancel)?;
    Ok(parse_blame(&String::from_utf8_lossy(&out)))
}

/// A commit that last changed some lines of a blamed file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameCommit {
    pub sha: String,
    pub author: String,
    /// Seconds since the epoch.
    pub time: i64,
    pub summary: String,
    pub uncommitted: bool,
    /// The file's path at this commit, from the repository's top.
    pub path: String,
    /// The file's path in the parent the lines came from; `None` for a root or boundary commit.
    pub previous: Option<String>,
    /// The parent commit the lines came from, with `previous`.
    pub parent: Option<String>,
}

/// Whole-file blame: the commits, and which of them each zero-based line came from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileBlame {
    pub commits: Vec<BlameCommit>,
    pub lines: Vec<usize>,
}

/// Parses `git blame --porcelain` for a whole file; a repeated commit carries only its header.
pub fn parse_file_blame(out: &str) -> FileBlame {
    let mut blame = FileBlame::default();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut current: Option<usize> = None;
    for line in out.lines() {
        if line.starts_with('\t') {
            if let Some(c) = current.take() {
                blame.lines.push(c);
            }
            continue;
        }
        if current.is_none() {
            let mut fields = line.split(' ');
            let sha = fields.next().unwrap_or_default();
            let numbered = fields.nth(1).is_some_and(|n| n.parse::<usize>().is_ok());
            if sha.len() < 7 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) || !numbered {
                continue;
            }
            let next = blame.commits.len();
            let i = *index.entry(sha.to_string()).or_insert(next);
            if i == next {
                blame.commits.push(BlameCommit {
                    sha: sha.to_string(),
                    author: String::new(),
                    time: 0,
                    summary: String::new(),
                    uncommitted: sha.bytes().all(|b| b == b'0'),
                    path: String::new(),
                    previous: None,
                    parent: None,
                });
            }
            current = Some(i);
            continue;
        }
        let Some(c) = current.and_then(|i| blame.commits.get_mut(i)) else {
            continue;
        };
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "author" => c.author = value.to_string(),
            "author-time" => c.time = value.parse().unwrap_or(0),
            "summary" => c.summary = value.to_string(),
            "filename" => c.path = value.to_string(),
            "previous" => {
                if let Some((sha, path)) = value.split_once(' ') {
                    c.parent = Some(sha.to_string());
                    c.previous = Some(path.to_string());
                }
            }
            _ => {}
        }
    }
    blame
}

/// Blames every line of a file; `contents` blames unsaved text instead of the file on disk.
pub fn blame_file(
    root: &Path,
    path: &Path,
    contents: Option<&str>,
    cancel: &Cancel,
) -> Result<FileBlame> {
    let rel = path
        .strip_prefix(root)
        .context("file is outside the project")?;
    let mut cmd = git(root);
    cmd.args(["blame", "--porcelain", "--no-textconv"]);
    if contents.is_some() {
        cmd.args(["--contents", "-"]);
    }
    cmd.arg("--").arg(rel);
    let out = run_until(cmd, contents, TIMEOUT, cancel)?;
    Ok(parse_file_blame(&String::from_utf8_lossy(&out)))
}

/// One commit in a file's history, newest first in [`file_log`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub sha: String,
    pub parents: Vec<String>,
    pub author: String,
    /// Seconds since the epoch.
    pub time: i64,
    pub subject: String,
    /// The file's path at this commit, from the repository's top.
    pub path: String,
    /// The file's path in the first parent; `None` when this commit added it.
    pub old_path: Option<String>,
}

/// The most commits the timeline lists.
pub const LOG_LIMIT: usize = 500;
const LOG_FORMAT: &str = "--format=%H%x00%P%x00%an%x00%at%x00%s";

/// Whether a `--name-status` field is a status letter rather than a path or commit id.
fn is_name_status(field: &str) -> bool {
    let mut chars = field.chars();
    chars.next().is_some_and(|c| "ACDMRTUXB".contains(c)) && chars.all(|c| c.is_ascii_digit())
}

/// Parses `git log -z --name-status` written with `LOG_FORMAT`; `path` is the file's current
/// path from the repository's top, followed back through renames.
pub fn parse_log(out: &[u8], path: &str) -> Vec<LogEntry> {
    let text = String::from_utf8_lossy(out);
    // Only NUL separates fields, and no subject or name can hold one, so none can fake a record.
    let mut fields = text
        .split('\0')
        .map(|f| f.trim_start_matches('\n'))
        .peekable();
    let mut tracking = path.to_string();
    let mut entries = Vec::new();
    while let Some(sha) = fields.next() {
        if sha.is_empty() {
            continue;
        }
        let (Some(parents), Some(author), Some(time), Some(subject)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            break;
        };
        let mut names = Vec::new();
        while let Some(status) = fields.next_if(|f| is_name_status(f)) {
            let paths = if status.starts_with(['R', 'C']) { 2 } else { 1 };
            names.push(status);
            names.extend(fields.by_ref().take(paths));
        }
        if sha.len() < 7 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let mut name = names.into_iter().map(str::to_string);
        let mut name = || name.next().unwrap_or_default();
        // A merge has no name-status of its own; the file keeps the name it has after it.
        let (now, before) = match name().as_str() {
            s if s.starts_with('R') || s.starts_with('C') => {
                let old = name();
                (name(), Some(old))
            }
            "A" => (name(), None),
            "" => (tracking.clone(), Some(tracking.clone())),
            _ => {
                let p = name();
                (p.clone(), Some(p))
            }
        };
        let parents: Vec<String> = parents
            .split(' ')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        let old_path = before.filter(|_| !parents.is_empty());
        if let Some(old) = &old_path {
            tracking = old.clone();
        }
        entries.push(LogEntry {
            sha: sha.to_string(),
            parents,
            author: author.to_string(),
            time: time.parse().unwrap_or(0),
            subject: subject.to_string(),
            path: now,
            old_path,
        });
    }
    entries
}

/// The commits that changed a file, newest first, following it through renames.
pub fn file_log(root: &Path, path: &Path, cancel: &Cancel) -> Result<Vec<LogEntry>> {
    let rel = path
        .strip_prefix(root)
        .context("file is outside the project")?;
    let top = format!("{}{}", prefix(root)?, rel.display());
    let mut cmd = git(root);
    cmd.args(["log", "--follow", "-z", "--name-status", "-M", LOG_FORMAT])
        .arg(format!("--max-count={LOG_LIMIT}"))
        .arg("--")
        .arg(rel);
    Ok(parse_log(&run_until(cmd, None, TIMEOUT, cancel)?, &top))
}

/// Whether `rev` is a commit id, optionally naming its first parent with a trailing `^`.
fn is_commit_id(rev: &str) -> bool {
    let id = rev.strip_suffix('^').unwrap_or(rev);
    (4..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A file's contents at commit `rev` (`<sha>` or `<sha>^`) with `top` named from the
/// repository's top, as checkout would write them; `None` when the commit has no such file.
pub fn show_at(root: &Path, rev: &str, top: &str) -> Result<Option<Vec<u8>>> {
    if !is_commit_id(rev) {
        bail!("{rev} is not a commit id");
    }
    let spec = format!("{rev}:{top}");
    let mut exists = git(root);
    exists.args(["cat-file", "-e", &spec]);
    match run(exists, None) {
        Ok(_) => {}
        Err(e) if e.is::<TimedOut>() => return Err(e),
        Err(_) => return Ok(None),
    }
    let mut cmd = git(root);
    cmd.args(["cat-file", "--filters", &spec]);
    run(cmd, None).map(Some)
}

/// Commands that rewrite the index or worktree can take minutes on a big tree, and a kill
/// halfway leaves `index.lock` behind.
const WRITE_TIMEOUT: Duration = Duration::from_secs(600);

/// `git add` for the given paths.
pub fn stage(root: &Path, paths: &[PathBuf]) -> Result<()> {
    let mut cmd = filtering_git(root);
    cmd.args(["add", "--"]).args(paths);
    run_within(cmd, None, WRITE_TIMEOUT).map(drop)
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
    run_within(cmd, None, WRITE_TIMEOUT).map(drop)
}

/// Which stored version of a file to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rev {
    Head,
    Index,
}

/// A file's contents at HEAD or in the index as checkout would write them, line endings applied
/// but no filter driver run; `None` when it is not there (new, or no commits yet).
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
    cmd.args(["cat-file", "--filters", &spec]);
    run(cmd, None).map(Some)
}

/// Refuses a file that goes through a filter driver: its hunks were read without the driver, so
/// only git itself, staging or restoring the whole file, can store or write it correctly.
pub fn refuse_filtered(root: &Path, rel: &Path) -> Result<()> {
    let mut cmd = git(root);
    cmd.args(["check-attr", "-z", "filter", "--"]).arg(rel);
    let out = run(cmd, None)?;
    let driver = out.split(|&b| b == 0).nth(2).unwrap_or_default();
    match driver {
        b"" | b"unspecified" | b"unset" | b"set" => Ok(()),
        name => bail!(
            "{} goes through the \"{}\" filter, so Athena stages and reverts it only as a whole \
             file.",
            rel.display(),
            String::from_utf8_lossy(name)
        ),
    }
}

/// Puts `contents` in the index as `rel`, or takes `rel` out of it for `None`, leaving the
/// worktree alone; staging one hunk does this. Refuses when the index no longer holds `expected`.
pub fn write_index(root: &Path, rel: &Path, expected: &str, contents: Option<&str>) -> Result<()> {
    refuse_filtered(root, rel)?;
    let now = show(root, Rev::Index, rel)?.unwrap_or_default();
    if now != expected.as_bytes() {
        bail!(
            "{} changed in the index since the diff was shown",
            rel.display()
        );
    }
    let Some(contents) = contents else {
        let mut remove = git(root);
        remove
            .args(["update-index", "--force-remove", "--"])
            .arg(rel);
        return run(remove, None).map(drop);
    };
    let mut hash = git(root);
    // `--path` applies the file's line-ending conversion.
    hash.args(["hash-object", "-w", "--stdin", "--path"])
        .arg(rel);
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
    // Unlike the other paths here, a `--cacheinfo` path is relative to the repository's top.
    let top = format!("{}{}", prefix(root)?, rel.display());
    let mut update = git(root);
    update
        .args(["update-index", "--add", "--cacheinfo"])
        .arg(format!("{mode},{sha},{top}"));
    run(update, None).map(drop)
}

/// Pre-commit hooks can run a test suite; the usual limit would kill them.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(600);

/// Commits the index with `message`, or rewrites the last commit with it when `amend`.
pub fn commit(root: &Path, message: &str, amend: bool) -> Result<()> {
    // Its hooks may stage files, and a git they start inherits these settings.
    let mut cmd = filtering_git(root);
    cmd.args(["commit", "--quiet", "--file", "-"]);
    if amend {
        cmd.arg("--amend");
    }
    run_within(cmd, Some(message), COMMIT_TIMEOUT).map(drop)
}

/// The last commit's id and full message, for the amend box.
pub fn last_commit(root: &Path) -> Result<(String, String)> {
    let mut cmd = git(root);
    cmd.args(["log", "-1", "--format=%H%n%B"]);
    let out = String::from_utf8_lossy(&run(cmd, None)?).into_owned();
    let (id, message) = out.split_once('\n').unwrap_or((&out, ""));
    Ok((id.to_string(), message.trim_end().to_string()))
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
    let mut cmd = filtering_git(root);
    cmd.args(["restore", "--worktree", "--"]).arg(rel);
    run_within(cmd, None, WRITE_TIMEOUT).map(drop)
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
    let mut cmd = filtering_git(root);
    cmd.arg("switch");
    match how {
        Switch::Existing => cmd.arg("--no-guess"),
        Switch::Track => cmd.arg("--track"),
        Switch::Create => cmd.arg("-c"),
    };
    cmd.arg(branch);
    run_within(cmd, None, WRITE_TIMEOUT).map(drop)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Switch {
    Existing,
    Track,
    Create,
}

/// Fetches, pulls and pushes wait on the network and on hooks; a hung one still ends.
const REMOTE_TIMEOUT: Duration = Duration::from_secs(300);

/// `git fetch` from the branch's remote, or origin.
pub fn fetch(root: &Path) -> Result<()> {
    let mut cmd = remote_git(root);
    cmd.arg("fetch");
    run_within(cmd, None, REMOTE_TIMEOUT).map(drop)
}

/// `git pull --ff-only`: git refuses, and says why, when the branches have diverged.
pub fn pull(root: &Path) -> Result<()> {
    let mut cmd = remote_git(root);
    cmd.args(["pull", "--ff-only"]);
    run_within(cmd, None, REMOTE_TIMEOUT).map(drop)
}

/// `git push`, or with `publish` the branch to that remote, setting it as the upstream.
pub fn push(root: &Path, publish: Option<(&str, &str)>) -> Result<()> {
    let mut cmd = remote_git(root);
    cmd.arg("push");
    if let Some((remote, branch)) = publish {
        if remote.starts_with('-') || branch.starts_with('-') {
            bail!("a remote or branch name cannot start with a dash");
        }
        cmd.args(["-u", remote, branch]);
    }
    run_within(cmd, None, REMOTE_TIMEOUT).map(drop)
}

/// The checked-out branch's name; `None` on a detached HEAD.
pub fn current_branch(root: &Path) -> Result<Option<String>> {
    let mut cmd = git(root);
    cmd.args(["symbolic-ref", "-q", "--short", "HEAD"]);
    match run(cmd, None) {
        Ok(out) => Ok(Some(String::from_utf8_lossy(&out).trim().to_string())),
        Err(e) if e.is::<TimedOut>() => Err(e),
        Err(_) => Ok(None),
    }
}

pub fn remotes(root: &Path) -> Result<Vec<String>> {
    let mut cmd = git(root);
    cmd.arg("remote");
    Ok(String::from_utf8_lossy(&run(cmd, None)?)
        .lines()
        .map(str::to_string)
        .filter(|l| !l.is_empty())
        .collect())
}

/// One `git stash list` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stash {
    pub index: usize,
    /// The stash's commit, which keeps naming it as newer stashes renumber the list.
    pub commit: String,
    /// "WIP on main: 1a2b3c Fix the parser", or the message it was stashed with.
    pub message: String,
    /// "3 days ago".
    pub when: String,
}

/// Parses `stash list` records of the reflog selector, commit, subject and relative date.
pub fn parse_stashes(out: &str) -> Vec<Stash> {
    out.lines()
        .filter_map(|line| {
            let mut f = line.split('\0');
            let index = f
                .next()?
                .strip_prefix("stash@{")?
                .strip_suffix('}')?
                .parse()
                .ok()?;
            Some(Stash {
                index,
                commit: f.next()?.to_string(),
                message: f.next().unwrap_or_default().to_string(),
                when: f.next().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

pub fn stashes(root: &Path) -> Result<Vec<Stash>> {
    let mut cmd = git(root);
    cmd.args(["stash", "list", "--format=%gd%x00%H%x00%s%x00%cr"]);
    Ok(parse_stashes(&String::from_utf8_lossy(&run(cmd, None)?)))
}

/// `git stash push`, with untracked files too when asked, as VS Code's two Stash commands.
pub fn stash_push(root: &Path, include_untracked: bool, message: Option<&str>) -> Result<()> {
    let mut cmd = filtering_git(root);
    // With literal pathspecs, git's own clean-up of stashed untracked files matches nothing.
    cmd.env_remove("GIT_LITERAL_PATHSPECS")
        .args(["stash", "push"]);
    if include_untracked {
        cmd.arg("--include-untracked");
    }
    if let Some(message) = message.filter(|m| !m.trim().is_empty()) {
        cmd.args(["--message", message]);
    }
    run_within(cmd, None, WRITE_TIMEOUT).map(drop)
}

/// Pops the stash whose commit is `commit`; git keeps the stash when applying it conflicts.
pub fn stash_pop(root: &Path, commit: &str) -> Result<()> {
    // pop takes only stash@{n}, so the index is looked up now, not when the list was shown.
    let mut list = git(root);
    list.args(["stash", "list", "--format=%H"]);
    let out = String::from_utf8_lossy(&run(list, None)?).into_owned();
    let Some(index) = out.lines().position(|h| h == commit) else {
        bail!("that stash is no longer in the stash list");
    };
    let mut cmd = filtering_git(root);
    cmd.args(["stash", "pop", &format!("stash@{{{index}}}")]);
    run_within(cmd, None, WRITE_TIMEOUT).map(drop)
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

    /// A repository whose origin is over ssh, with `ssh_command` as its core.sshCommand.
    fn ssh_remote_repo(name: &str, ssh_command: Option<&str>) -> PathBuf {
        let dir = committed_repo(name, "x\n");
        repo_git(
            &dir,
            &["remote", "add", "origin", "athena-test.invalid:x.git"],
        );
        if let Some(ssh) = ssh_command {
            repo_git(&dir, &["config", "core.sshCommand", ssh]);
        }
        dir
    }

    fn script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_users_ssh_that_asks_on_the_terminal_fails_at_once() {
        if !available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("athena-git-tty-{}", std::process::id()));
        let fake = dir.with_extension("ssh");
        let ran = dir.with_extension("ran");
        let _ = std::fs::remove_file(&ran);
        script(
            &fake,
            &format!(
                "touch '{}'; read answer < /dev/tty || exit 1",
                ran.display()
            ),
        );
        let repo = ssh_remote_repo("tty", fake.to_str());
        let mut cmd = remote_git(&repo);
        cmd.env_remove("GIT_SSH_COMMAND")
            .env_remove("GIT_SSH")
            .arg("fetch");
        let started = Instant::now();
        let err = run_within(cmd, None, Duration::from_secs(20)).unwrap_err();
        assert!(!err.is::<TimedOut>(), "the prompt waited on a terminal");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(ran.exists(), "the configured ssh command was not used");
        for path in [&fake, &ran] {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn ssh_runs_in_batch_mode_unless_the_user_configured_it() {
        if !available() {
            return;
        }
        let bin = std::env::temp_dir().join(format!("athena-git-bin-{}", std::process::id()));
        std::fs::create_dir_all(&bin).unwrap();
        let said = bin.join("args");
        script(
            &bin.join("ssh"),
            &format!("echo \"$@\" > '{}'; exit 1", said.display()),
        );
        let repo = ssh_remote_repo("batch", None);
        let mut cmd = git(&repo);
        never_prompt(&mut cmd, false);
        cmd.env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .arg("fetch");
        run_within(cmd, None, Duration::from_secs(20)).unwrap_err();
        let args = std::fs::read_to_string(&said).unwrap();
        assert!(args.contains("BatchMode=yes"), "{args}");
        let _ = std::fs::remove_dir_all(bin);
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn a_remote_op_past_its_limit_takes_its_ssh_down_too() {
        if !available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("athena-git-hang-{}", std::process::id()));
        let fake = dir.with_extension("ssh");
        let marker = dir.with_extension("left");
        let _ = std::fs::remove_file(&marker);
        script(
            &fake,
            &format!("(sleep 2; touch '{}') & sleep 30", marker.display()),
        );
        let repo = ssh_remote_repo("hang", fake.to_str());
        let mut cmd = remote_git(&repo);
        cmd.env_remove("GIT_SSH_COMMAND")
            .env_remove("GIT_SSH")
            .arg("fetch");
        let started = Instant::now();
        let err = run_within(cmd, None, Duration::from_millis(500)).unwrap_err();
        assert!(err.is::<TimedOut>());
        assert!(started.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(2500));
        assert!(!marker.exists(), "ssh's child outlived the timeout");
        let _ = std::fs::remove_file(fake);
        let _ = std::fs::remove_dir_all(repo);
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
        let none = Cancel::default();
        let first = blame_line(&sub, &sub.join("f.txt"), 0, None, &none)
            .unwrap()
            .unwrap();
        assert_eq!(
            (first.author.as_str(), first.summary.as_str()),
            ("Test", "init")
        );
        let changed = blame_line(&sub, &sub.join("f.txt"), 1, None, &none)
            .unwrap()
            .unwrap();
        assert!(changed.uncommitted);
        let unsaved = blame_line(&sub, &sub.join("f.txt"), 0, Some("z\nB\n"), &none)
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
        write_index(
            &dir,
            Path::new("f.txt"),
            "a\nb\nc\nd\n",
            Some("a\nB\nc\nd\n"),
        )
        .unwrap();
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
        write_index(
            &dir,
            Path::new("f.txt"),
            "a\nB\nc\nd\n",
            Some("a\nb\nc\nd\n"),
        )
        .unwrap();
        assert!(git_out(&dir, &["diff", "--cached"]).is_empty());

        // A hunk of an untracked file stages it as added.
        std::fs::write(dir.join("new.txt"), "x\ny\n").unwrap();
        write_index(&dir, Path::new("new.txt"), "", Some("x\n")).unwrap();
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
    fn a_hunk_staged_from_a_subfolder_project_lands_on_that_file() {
        if !available() {
            return;
        }
        let dir = committed_repo("stage-sub", "top\n");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/f.txt"), "a\nb\n").unwrap();
        stage(&dir, &[PathBuf::from("sub/f.txt")]).unwrap();
        commit(&dir, "Add sub", false).unwrap();
        let sub = dir.join("sub");
        std::fs::write(sub.join("f.txt"), "a\nB\nc\n").unwrap();
        write_index(&sub, Path::new("f.txt"), "a\nb\n", Some("a\nB\n")).unwrap();
        let listed = git_out(&dir, &["ls-files"]);
        assert_eq!(listed, "f.txt\nsub/f.txt\n");
        assert_eq!(git_out(&dir, &["show", ":sub/f.txt"]), "a\nB\n");
        assert_eq!(git_out(&dir, &["show", ":f.txt"]), "top\n");

        // Taking an entry out also resolves the path under the subfolder.
        write_index(&sub, Path::new("f.txt"), "a\nB\n", None).unwrap();
        assert_eq!(git_out(&dir, &["ls-files"]), "f.txt\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn staging_against_an_index_that_moved_on_is_refused() {
        if !available() {
            return;
        }
        let dir = committed_repo("stage-stale", "a\nb\n");
        std::fs::write(dir.join("f.txt"), "A\nB\n").unwrap();
        // Someone staged the file in a terminal after the diff was read.
        stage(&dir, &[PathBuf::from("f.txt")]).unwrap();
        let err = write_index(&dir, Path::new("f.txt"), "a\nb\n", Some("A\nb\n")).unwrap_err();
        assert!(err.to_string().contains("changed in the index"), "{err:#}");
        assert_eq!(git_out(&dir, &["show", ":f.txt"]), "A\nB\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hunks_keep_line_endings_and_refuse_files_behind_a_filter() {
        if !available() {
            return;
        }
        let dir = committed_repo("stage-eol", "seed\n");
        std::fs::write(
            dir.join(".gitattributes"),
            "*.txt text eol=crlf\n*.dat filter=caps\n",
        )
        .unwrap();
        // The index keeps lower case, checkout writes upper case; git-lfs works the same way.
        repo_git(&dir, &["config", "filter.caps.clean", "tr A-Z a-z"]);
        repo_git(&dir, &["config", "filter.caps.smudge", "tr a-z A-Z"]);
        std::fs::write(dir.join("f.txt"), "a\r\nb\r\nc\r\nd\r\n").unwrap();
        std::fs::write(dir.join("g.dat"), "ONE\nTWO\n").unwrap();
        stage(&dir, &[PathBuf::from(".")]).unwrap();
        commit(&dir, "Add text and data", false).unwrap();
        assert_eq!(
            git_out(&dir, &["cat-file", "blob", ":f.txt"]),
            "a\nb\nc\nd\n"
        );
        assert_eq!(git_out(&dir, &["cat-file", "blob", ":g.dat"]), "one\ntwo\n");

        let index = show(&dir, Rev::Index, Path::new("f.txt")).unwrap().unwrap();
        assert_eq!(index, b"a\r\nb\r\nc\r\nd\r\n");
        std::fs::write(dir.join("f.txt"), "a\r\nB\r\nc\r\nD\r\n").unwrap();
        write_index(
            &dir,
            Path::new("f.txt"),
            "a\r\nb\r\nc\r\nd\r\n",
            Some("a\r\nB\r\nc\r\nd\r\n"),
        )
        .unwrap();
        assert_eq!(
            git_out(&dir, &["cat-file", "blob", ":f.txt"]),
            "a\nB\nc\nd\n"
        );
        let unstaged = git_out(&dir, &["diff", "--no-color", "--", "f.txt"]);
        assert!(
            unstaged.contains("+D") && !unstaged.contains("+B"),
            "{unstaged}"
        );

        assert_eq!(
            show(&dir, Rev::Head, Path::new("g.dat")).unwrap().unwrap(),
            b"one\ntwo\n",
            "showing a stored version runs no filter driver"
        );
        std::fs::write(dir.join("g.dat"), "ONE\nTHREE\n").unwrap();
        let err =
            write_index(&dir, Path::new("g.dat"), "one\ntwo\n", Some("one\nthree\n")).unwrap_err();
        assert!(err.to_string().contains("\"caps\" filter"), "{err:#}");
        assert_eq!(git_out(&dir, &["cat-file", "blob", ":g.dat"]), "one\ntwo\n");
        refuse_filtered(&dir, Path::new("f.txt")).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reading_a_repository_never_runs_its_filters_or_textconv() {
        if !available() {
            return;
        }
        let dir = committed_repo("no-filters", "a\nb\n");
        let ran = dir.with_extension("ran");
        let _ = std::fs::remove_dir_all(&ran);
        std::fs::create_dir_all(&ran).unwrap();
        std::fs::write(dir.join("e.crlf"), "x\r\ny\r\n").unwrap();
        std::fs::write(dir.join("g.dat"), "g\n").unwrap();
        // Info attributes come from the repository's own folder, which --attr-source leaves in force.
        std::fs::write(
            dir.join(".git/info/attributes"),
            "*.txt filter=evil diff=evil\n*.dat filter=proc\n*.crlf text eol=crlf\n",
        )
        .unwrap();
        repo_git(&dir, &["add", "-A"]);
        repo_git(&dir, &["commit", "-qm", "more"]);
        let touch = |what: &str| format!("touch '{}'; cat", ran.join(what).display());
        repo_git(&dir, &["config", "filter.evil.clean", &touch("clean")]);
        repo_git(&dir, &["config", "filter.evil.smudge", &touch("smudge")]);
        repo_git(&dir, &["config", "filter.evil.required", "true"]);
        repo_git(&dir, &["config", "diff.evil.textconv", &touch("textconv")]);
        repo_git(&dir, &["config", "filter.proc.process", &touch("process")]);
        std::fs::write(dir.join("f.txt"), "a\nB\n").unwrap();
        std::fs::write(dir.join("g.dat"), "h\n").unwrap();
        std::fs::write(dir.join("e.crlf"), "x\r\nY\r\n").unwrap();

        let none = Cancel::default();
        let f = dir.join("f.txt");
        let snap = status(&dir, "", true).unwrap();
        assert!(
            snap.entries.iter().any(|(p, _)| *p == f),
            "{:?}",
            snap.entries
        );
        assert_eq!(
            diff_hunks(&dir, &f).unwrap(),
            vec![Hunk::Modified { start: 1, len: 1 }]
        );
        assert!(!diff_hunks(&dir, &dir.join("g.dat")).unwrap().is_empty());
        blame_line(&dir, &f, 0, None, &none).unwrap().unwrap();
        blame_file(&dir, &f, None, &none).unwrap();
        blame_file(&dir, &f, Some("a\nb\nc\n"), &none).unwrap();
        let head = git_out(&dir, &["rev-parse", "HEAD"]).trim().to_string();
        assert_eq!(show_at(&dir, &head, "f.txt").unwrap().unwrap(), b"a\nb\n");
        assert_eq!(file_log(&dir, &f, &none).unwrap().len(), 1);
        for path in ["f.txt", "g.dat"] {
            show(&dir, Rev::Head, Path::new(path)).unwrap().unwrap();
            show(&dir, Rev::Index, Path::new(path)).unwrap().unwrap();
        }
        let err = write_index(&dir, Path::new("f.txt"), "a\nb\n", Some("a\nB\n")).unwrap_err();
        assert!(err.to_string().contains("\"evil\" filter"), "{err:#}");
        write_index(&dir, Path::new("e.crlf"), "x\r\ny\r\n", Some("x\r\nY\r\n")).unwrap();
        assert_eq!(git_out(&dir, &["cat-file", "blob", ":e.crlf"]), "x\nY\n");
        let listed: Vec<_> = std::fs::read_dir(&ran).unwrap().flatten().collect();
        assert!(listed.is_empty(), "a filter or textconv ran: {listed:?}");

        // Staging a whole file is git's own add, filters and all.
        stage(&dir, &[PathBuf::from("f.txt")]).unwrap();
        assert!(ran.join("clean").exists());
        for d in [dir, ran] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn driver_names_come_whole_from_the_config_listing() {
        let out =
            b"core.bare\nfalse\0filter.lfs.clean\ngit-lfs clean -- %f\0diff.x.textconv\ncat\0\
                    filter.lfs.process\ngit-lfs filter-process\0\
                    filter.a.b=c.smudge\ncat\0filter.x.required\0";
        assert_eq!(filter_drivers(out), ["a.b=c", "lfs", "x"]);
        let mut cmd = Command::new("/usr/bin/env");
        without_filters(&mut cmd, &["a.b=c".to_string()]);
        let env: HashMap<_, _> = cmd
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .collect();
        let base: usize = std::env::var("GIT_CONFIG_COUNT")
            .ok()
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        assert_eq!(env["GIT_CONFIG_COUNT"], (base + 4).to_string());
        assert_eq!(env[&format!("GIT_CONFIG_KEY_{base}")], "filter.a.b=c.clean");
        assert_eq!(env[&format!("GIT_CONFIG_VALUE_{}", base + 3)], "false");
    }

    #[test]
    fn a_cancelled_run_kills_its_command_at_once() {
        let cancel = Cancel::default();
        let guard = cancel.on_drop();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(guard);
        });
        let started = Instant::now();
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("30");
        let err = run_until(cmd, None, Duration::from_secs(20), &cancel).unwrap_err();
        assert!(err.is::<Cancelled>(), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(2));
        stopper.join().unwrap();
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
        assert_eq!(
            last_commit(&dir).unwrap().1,
            "Change a to b\n\nWith a body."
        );
        assert_eq!(git_out(&dir, &["rev-list", "--count", "HEAD"]).trim(), "2");
        std::fs::write(dir.join("f.txt"), "c\n").unwrap();
        stage(&dir, &[PathBuf::from("f.txt")]).unwrap();
        commit(&dir, "Change a to c", true).unwrap();
        assert_eq!(last_commit(&dir).unwrap().1, "Change a to c");
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
    fn status_reads_the_upstream_and_how_far_apart_it_is() {
        let out = nul(&[
            "# branch.oid 0930e963f1c8991bb9873ea31854b682b2ec228a",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -3",
        ]);
        let tracking = parse_status(&out).tracking.unwrap();
        assert_eq!(tracking.upstream, "origin/main");
        assert_eq!((tracking.ahead, tracking.behind), (2, 3));
        let none = nul(&["# branch.oid 0930e963", "# branch.head feat"]);
        assert_eq!(parse_status(&none).tracking, None);
        let gone = nul(&[
            "# branch.oid 0930e963",
            "# branch.head feat",
            "# branch.upstream origin/feat",
        ]);
        assert_eq!(
            parse_status(&gone).tracking,
            None,
            "the upstream was deleted"
        );
    }

    #[test]
    fn a_branch_whose_upstream_was_deleted_has_nothing_to_sync() {
        if !available() {
            return;
        }
        let a = committed_repo("gone", "one\n");
        let bare = a.with_extension("gone.git");
        let _ = std::fs::remove_dir_all(&bare);
        repo_git(&a, &["init", "--bare", "-q", bare.to_str().unwrap()]);
        repo_git(&a, &["remote", "add", "origin", bare.to_str().unwrap()]);
        repo_git(&a, &["push", "-q", "-u", "origin", "main"]);
        assert!(status(&a, "", true).unwrap().tracking.is_some());
        repo_git(&bare, &["update-ref", "-d", "refs/heads/main"]);
        repo_git(&a, &["fetch", "-q", "--prune"]);
        assert_eq!(status(&a, "", true).unwrap().tracking, None);
        for dir in [a, bare] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn stash_records_parse() {
        let out = "stash@{0}\0aaa\0On main: mine\x002 minutes ago\nstash@{1}\0bbb\0WIP on main: 1a2b Fix\x001 day ago\nnoise\n";
        let list = parse_stashes(out);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].index, 1);
        assert_eq!(list[1].commit, "bbb");
        assert_eq!(list[0].message, "On main: mine");
        assert_eq!(list[1].when, "1 day ago");
    }

    fn configure(dir: &Path) {
        for (key, value) in [
            ("user.name", "Test"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "/dev/null"),
        ] {
            repo_git(dir, &["config", key, value]);
        }
    }

    fn commit_file(dir: &Path, name: &str, contents: &str, message: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
        stage(dir, &[PathBuf::from(name)]).unwrap();
        commit(dir, message, false).unwrap();
    }

    fn tracking(dir: &Path) -> Option<Tracking> {
        status(dir, "", true).unwrap().tracking
    }

    #[test]
    fn fetch_pull_and_push_against_a_local_bare_remote() {
        if !available() {
            return;
        }
        let a = committed_repo("remote-a", "one\n");
        let parent = a.parent().unwrap().to_path_buf();
        let tag = format!("{}", std::process::id());
        let bare = parent.join(format!("athena-git-remote-{tag}.git"));
        let b = parent.join(format!("athena-git-remote-b-{tag}"));
        let _ = std::fs::remove_dir_all(&bare);
        let _ = std::fs::remove_dir_all(&b);
        repo_git(&parent, &["init", "--bare", "-q", bare.to_str().unwrap()]);
        repo_git(&a, &["remote", "add", "origin", bare.to_str().unwrap()]);
        assert_eq!(remotes(&a).unwrap(), ["origin"]);
        assert_eq!(current_branch(&a).unwrap().as_deref(), Some("main"));
        assert_eq!(tracking(&a), None);

        let err = push(&a, None).unwrap_err();
        assert!(err.to_string().contains("upstream"), "{err:#}");
        push(&a, Some(("origin", "main"))).unwrap();
        let t = tracking(&a).unwrap();
        assert_eq!(
            (t.upstream.as_str(), t.ahead, t.behind),
            ("origin/main", 0, 0)
        );

        repo_git(
            &parent,
            &["clone", "-q", bare.to_str().unwrap(), b.to_str().unwrap()],
        );
        configure(&b);
        commit_file(&b, "f.txt", "two\n", "Two");
        push(&b, None).unwrap();

        assert_eq!(tracking(&a).unwrap().behind, 0, "nothing fetched yet");
        fetch(&a).unwrap();
        assert_eq!(tracking(&a).unwrap().behind, 1);
        pull(&a).unwrap();
        assert_eq!(std::fs::read_to_string(a.join("f.txt")).unwrap(), "two\n");
        assert_eq!(tracking(&a).unwrap().behind, 0);

        commit_file(&a, "a.txt", "a\n", "Mine");
        commit_file(&b, "b.txt", "b\n", "Theirs");
        push(&b, None).unwrap();
        fetch(&a).unwrap();
        let t = tracking(&a).unwrap();
        assert_eq!((t.ahead, t.behind), (1, 1));
        let err = pull(&a).unwrap_err();
        assert!(err.to_string().contains("fast-forward"), "{err:#}");
        assert!(
            !a.join("b.txt").exists(),
            "a refused pull leaves the worktree alone"
        );

        for dir in [a, b, bare] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn stashing_and_popping_round_trips_changes() {
        if !available() {
            return;
        }
        let dir = committed_repo("stash", "a\n");
        std::fs::write(dir.join("f.txt"), "mine\n").unwrap();
        std::fs::write(dir.join("u.txt"), "u\n").unwrap();
        stash_push(&dir, false, Some("mine")).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "a\n");
        assert!(
            dir.join("u.txt").exists(),
            "untracked files stay without the option"
        );
        stash_push(&dir, true, None).unwrap();
        assert!(!dir.join("u.txt").exists());
        let list = stashes(&dir).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[1].message.ends_with("mine"), "{list:?}");
        // A stash pushed after the list was read renumbers "mine" to stash@{2}.
        std::fs::write(dir.join("f.txt"), "later\n").unwrap();
        stash_push(&dir, false, Some("later")).unwrap();
        stash_pop(&dir, &list[1].commit).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "mine\n"
        );
        let left = stashes(&dir).unwrap();
        assert_eq!(left.len(), 2);
        assert!(
            left.iter().all(|s| !s.message.ends_with("mine")),
            "{left:?}"
        );
        assert!(stash_pop(&dir, &list[1].commit).is_err());
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

    #[test]
    fn log_records_follow_renames_back_and_keep_non_ascii_authors() {
        let out = "aaaaaaa1\0bbbbbbb2 ccccccc3\0Zoë\x001700000000\0Merge branch 'x'\0\
                   bbbbbbb2\0ddddddd4\0Ann\x001600000000\0Edit\0\nM\0src/new.rs\0\
                   ddddddd4\0eeeeeee5\0Ann\x001500000000\0Move\0\nR097\0src/old.rs\0src/new.rs\0\
                   eeeeeee5\0\0李\x001400000000\0Start\0\nA\0src/old.rs\0";
        let log = parse_log(out.as_bytes(), "src/new.rs");
        assert_eq!(log.len(), 4);
        assert_eq!(log[0].author, "Zoë");
        assert_eq!(log[0].parents.len(), 2);
        assert_eq!(
            (log[0].path.as_str(), log[0].old_path.as_deref()),
            ("src/new.rs", Some("src/new.rs")),
            "a merge keeps the name the file has after it"
        );
        assert_eq!(log[1].subject, "Edit");
        assert_eq!(
            (log[2].path.as_str(), log[2].old_path.as_deref()),
            ("src/new.rs", Some("src/old.rs"))
        );
        assert_eq!(
            (log[3].path.as_str(), log[3].old_path.as_deref()),
            ("src/old.rs", None)
        );
        assert_eq!((log[3].author.as_str(), log[3].time), ("李", 1_400_000_000));
        assert!(parse_log(b"not-a-sha\0\0a\x001\0s\0", "f").is_empty());
    }

    #[test]
    fn a_subject_cannot_forge_a_timeline_record() {
        let forged = "Fix\x1eccccccc3 Eve 1 Fake";
        let out = format!(
            "aaaaaaa1\0bbbbbbb2\0Ann\x001700000000\0{forged}\0\nM\0f.rs\0\
             bbbbbbb2\0\0Ann\x001600000000\0Start\0\nA\0f.rs\0"
        );
        let log = parse_log(out.as_bytes(), "f.rs");
        let subjects: Vec<_> = log.iter().map(|e| e.subject.as_str()).collect();
        assert_eq!(subjects, [forged, "Start"]);
        assert_eq!(
            (log[0].path.as_str(), log[0].old_path.as_deref()),
            ("f.rs", Some("f.rs"))
        );
    }

    #[test]
    fn whole_file_blame_shares_commits_between_their_lines() {
        let out = "1111111111111111111111111111111111111111 1 1 2\n\
                   author Zoë Ünal\nauthor-time 1700000000\nsummary Fix\n\
                   previous 2222222222222222222222222222222222222222 old.rs\nfilename new.rs\n\ta\n\
                   1111111111111111111111111111111111111111 2 2\n\tb\n\
                   3333333333333333333333333333333333333333 1 3 1\n\
                   author Ann\nauthor-time 1600000000\nsummary Start\nboundary\nfilename old.rs\n\tc\n\
                   0000000000000000000000000000000000000000 4 4 1\n\
                   author Not Committed Yet\nauthor-time 1800000000\nsummary Version\nfilename new.rs\n\td\n\
                   1111111111111111111111111111111111111111 3 5 1\nfilename new.rs\n\te\n";
        let blame = parse_file_blame(out);
        assert_eq!(blame.lines, vec![0, 0, 1, 2, 0]);
        let fix = &blame.commits[0];
        assert_eq!(fix.author, "Zoë Ünal");
        assert_eq!(
            (fix.path.as_str(), fix.previous.as_deref()),
            ("new.rs", Some("old.rs"))
        );
        assert_eq!(
            fix.parent.as_deref(),
            Some("2222222222222222222222222222222222222222")
        );
        assert_eq!(blame.commits[1].previous, None);
        assert_eq!(blame.commits[1].parent, None);
        assert!(blame.commits[2].uncommitted && !fix.uncommitted);
    }

    #[test]
    fn history_blame_and_old_versions_come_from_the_repository_top() {
        if !available() {
            eprintln!("git is not installed; skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("athena-git-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let dir = dir.canonicalize().unwrap();
        let sub = dir.join("sub");
        std::fs::write(sub.join("old.txt"), "a\nb\n").unwrap();
        repo_git(&dir, &["init", "-q"]);
        repo_git(&dir, &["add", "-A"]);
        repo_git(&dir, &["commit", "-qm", "first"]);
        repo_git(&dir, &["mv", "sub/old.txt", "sub/new.txt"]);
        repo_git(&dir, &["commit", "-qm", "rename"]);
        std::fs::write(sub.join("new.txt"), "a\nB\nc\n").unwrap();
        repo_git(&dir, &["commit", "-qam", "edit"]);

        let none = Cancel::default();
        let log = file_log(&sub, &sub.join("new.txt"), &none).unwrap();
        let subjects: Vec<_> = log.iter().map(|e| e.subject.as_str()).collect();
        assert_eq!(subjects, ["edit", "rename", "first"]);
        assert_eq!(log[1].old_path.as_deref(), Some("sub/old.txt"));
        assert_eq!(log[2].path, "sub/old.txt");
        let parent = format!("{}^", log[0].sha);
        assert_eq!(
            show_at(&sub, &parent, &log[0].old_path.clone().unwrap()).unwrap(),
            Some(b"a\nb\n".to_vec())
        );
        assert_eq!(
            show_at(&sub, &format!("{}^", log[2].sha), "sub/old.txt").unwrap(),
            None
        );
        assert!(show_at(&sub, "--output=x", "sub/old.txt").is_err());

        let blame = blame_file(&sub, &sub.join("new.txt"), Some("a\nB\nc\nd\n"), &none).unwrap();
        assert_eq!(blame.lines.len(), 4);
        let at = |line: usize| &blame.commits[blame.lines[line]];
        assert_eq!(
            (at(0).summary.as_str(), at(0).path.as_str()),
            ("first", "sub/old.txt")
        );
        assert_eq!(at(1).summary, "edit");
        assert_eq!(at(1).parent.as_deref(), Some(log[1].sha.as_str()));
        assert!(at(3).uncommitted);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
