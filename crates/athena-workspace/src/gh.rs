//! The GitHub CLI: a branch's latest CI run, opening a new pull request in the browser and a pull
//! request's checks. `gh` never prompts here; without it, or signed out, the features hide.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;

use crate::git::{Cancel, TimedOut, new_session, run_output};

/// Reading runs and checks waits on GitHub's API; a hung request still ends.
const QUERY_LIMIT: Duration = Duration::from_secs(20);
/// `pr create --web` looks up the repository and the pushed branch before opening the browser.
const WEB_LIMIT: Duration = Duration::from_secs(60);

/// Whether the GitHub features can run, and the `gh` they run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gh {
    Missing,
    SignedOut,
    Ready(PathBuf),
}

impl Gh {
    /// One line saying what is missing, for a palette entry or notice; `None` when ready.
    pub fn explain(&self) -> Option<&'static str> {
        match self {
            Self::Missing => Some("Needs gh: brew install gh"),
            Self::SignedOut => Some("Needs gh auth login"),
            Self::Ready(_) => None,
        }
    }
}

/// Where `gh` is on `path`, a PATH value.
pub fn locate(path: &OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .map(|dir| dir.join("gh"))
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Whether `program` is signed in, as `gh auth status` says.
pub fn check(program: Option<PathBuf>) -> Gh {
    let Some(program) = program else {
        return Gh::Missing;
    };
    let mut cmd = gh(&program, Path::new("/"));
    cmd.args(["auth", "status"]);
    match run_output(cmd, None, QUERY_LIMIT, &Cancel::default()) {
        Ok(out) if out.status.success() => Gh::Ready(program),
        _ => Gh::SignedOut,
    }
}

/// A `gh` that cannot prompt or page: no terminal, prompts disabled, its own session.
fn gh(program: &Path, dir: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.current_dir(dir)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1")
        .env("GH_SPINNER_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .env("NO_COLOR", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    new_session(&mut cmd);
    cmd
}

/// Runs `gh` in `root`; an exit code in `fine` besides 0 still returns what it printed, with
/// its stderr.
fn run(
    program: &Path,
    root: &Path,
    args: &[&str],
    limit: Duration,
    fine: &[i32],
) -> Result<(Vec<u8>, String)> {
    let mut cmd = gh(program, root);
    cmd.args(args);
    let out = run_output(cmd, None, limit, &Cancel::default()).map_err(|err| {
        match err.downcast_ref::<TimedOut>() {
            Some(_) => anyhow!("gh did not finish within {} seconds", limit.as_secs()),
            None => err,
        }
    })?;
    let said = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if out.status.success() || out.status.code().is_some_and(|c| fine.contains(&c)) {
        return Ok((out.stdout, said));
    }
    if said.is_empty() {
        bail!("gh failed ({})", out.status);
    }
    bail!("{said}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CiState {
    Pending,
    Passed,
    Failed,
    /// Cancelled, skipped or neutral: finished without a verdict.
    Neutral,
}

/// A branch's most recent workflow run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CiRun {
    pub state: CiState,
    pub url: String,
    pub workflow: String,
    pub title: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunRecord {
    status: String,
    #[serde(default)]
    conclusion: String,
    url: String,
    #[serde(default)]
    workflow_name: String,
    #[serde(default)]
    display_title: String,
}

/// Reads `gh run list --json status,conclusion,url,workflowName,displayTitle`, newest first.
pub fn parse_runs(json: &[u8]) -> Result<Option<CiRun>> {
    let runs: Vec<RunRecord> = serde_json::from_slice(json)?;
    Ok(runs.into_iter().next().map(|r| CiRun {
        state: match (r.status.as_str(), r.conclusion.as_str()) {
            ("completed", "success") => CiState::Passed,
            ("completed", "failure" | "timed_out" | "startup_failure" | "action_required") => {
                CiState::Failed
            }
            ("completed", _) => CiState::Neutral,
            _ => CiState::Pending,
        },
        url: r.url,
        workflow: r.workflow_name,
        title: r.display_title,
    }))
}

/// The latest workflow run on `branch`; `None` when it has none.
pub fn latest_run(program: &Path, root: &Path, branch: &str) -> Result<Option<CiRun>> {
    let branch = format!("--branch={branch}");
    let args = [
        "run",
        "list",
        &branch,
        "--limit=1",
        "--json=status,conclusion,url,workflowName,displayTitle",
    ];
    parse_runs(&run(program, root, &args, QUERY_LIMIT, &[])?.0)
}

/// `gh pr create --web`: opens GitHub's new pull request page for the current branch, which
/// must already be pushed, since gh cannot ask where to push it.
pub fn create_pr_web(program: &Path, root: &Path) -> Result<()> {
    run(program, root, &["pr", "create", "--web"], WEB_LIMIT, &[]).map(drop)
}

/// One check on the current branch's pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub workflow: String,
    pub state: CiState,
    pub link: String,
}

#[derive(Deserialize)]
struct CheckRecord {
    name: String,
    #[serde(default)]
    workflow: String,
    #[serde(default)]
    bucket: String,
    #[serde(default)]
    link: String,
}

/// Reads `gh pr checks --json name,workflow,bucket,link`, failures first.
pub fn parse_checks(json: &[u8]) -> Result<Vec<Check>> {
    let records: Vec<CheckRecord> = serde_json::from_slice(json)?;
    let mut checks: Vec<Check> = records
        .into_iter()
        .map(|r| Check {
            state: match r.bucket.as_str() {
                "pass" => CiState::Passed,
                "fail" => CiState::Failed,
                "pending" => CiState::Pending,
                _ => CiState::Neutral,
            },
            name: r.name,
            workflow: r.workflow,
            link: r.link,
        })
        .collect();
    let rank = |s: CiState| match s {
        CiState::Failed => 0,
        CiState::Pending => 1,
        CiState::Passed => 2,
        CiState::Neutral => 3,
    };
    checks.sort_by_key(|c| rank(c.state));
    Ok(checks)
}

/// The checks of the current branch's pull request.
pub fn pr_checks(program: &Path, root: &Path) -> Result<Vec<Check>> {
    // gh exits 8 while checks are pending and 1 when one failed, having printed them all.
    let (out, said) = run(
        program,
        root,
        &["pr", "checks", "--json=name,workflow,bucket,link"],
        QUERY_LIMIT,
        &[1, 8],
    )?;
    match parse_checks(&out) {
        Err(_) if !said.is_empty() => bail!("{said}"),
        parsed => parsed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder holding a fake `gh` whose body is `body`, to put on a PATH.
    fn fake_gh(name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("athena-gh-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gh = dir.join("gh");
        std::fs::write(&gh, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    fn path_with(dir: &Path) -> std::ffi::OsString {
        std::env::join_paths([Path::new("/nonexistent"), dir]).unwrap()
    }

    #[test]
    fn without_gh_on_the_path_the_features_are_missing() {
        let empty = fake_gh("missing", "exit 0");
        std::fs::remove_file(empty.join("gh")).unwrap();
        assert_eq!(locate(&path_with(&empty)), None);
        let found = check(locate(&path_with(&empty)));
        assert_eq!(found, Gh::Missing);
        assert!(found.explain().unwrap().contains("brew install gh"));
        std::fs::remove_dir_all(empty).unwrap();
    }

    #[test]
    fn a_signed_out_gh_is_not_ready() {
        let dir = fake_gh(
            "signedout",
            "[ \"$1 $2\" = \"auth status\" ] && { echo 'You are not logged in' >&2; exit 1; }\nexit 0",
        );
        let found = check(locate(&path_with(&dir)));
        assert_eq!(found, Gh::SignedOut);
        assert!(found.explain().unwrap().contains("gh auth login"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_signed_in_gh_reads_runs_and_checks_and_opens_new_pull_requests() {
        let dir = fake_gh(
            "ready",
            r#"here=$(dirname "$0")
[ -t 0 ] && echo tty >> "$here/log"
echo "$GH_PROMPT_DISABLED $* in $(pwd -P)" >> "$here/log"
case "$1 $2" in
  "auth status") exit 0 ;;
  "run list") echo '[{"status":"completed","conclusion":"failure","url":"https://github.com/o/r/actions/runs/1","workflowName":"CI","displayTitle":"Fix it"}]' ;;
  "pr create") exit 0 ;;
  "pr checks") echo '[{"name":"lint","workflow":"CI","bucket":"pass","link":"https://x/1"},{"name":"test","workflow":"CI","bucket":"pending","link":"https://x/2"}]'; exit 8 ;;
esac"#,
        );
        let root = dir.canonicalize().unwrap();
        let Gh::Ready(program) = check(locate(&path_with(&dir))) else {
            panic!("gh should be ready");
        };
        let run = latest_run(&program, &root, "feat/x").unwrap().unwrap();
        assert_eq!(run.state, CiState::Failed);
        assert_eq!(run.url, "https://github.com/o/r/actions/runs/1");
        assert_eq!(
            (run.workflow.as_str(), run.title.as_str()),
            ("CI", "Fix it")
        );
        create_pr_web(&program, &root).unwrap();
        let checks = pr_checks(&program, &root).unwrap();
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["test", "lint"]);
        assert_eq!(checks[0].state, CiState::Pending);

        let log = std::fs::read_to_string(dir.join("log")).unwrap();
        let root = root.display();
        assert_eq!(
            log.lines().collect::<Vec<_>>(),
            [
                "1 auth status in /".to_string(),
                format!("1 run list --branch=feat/x --limit=1 --json=status,conclusion,url,workflowName,displayTitle in {root}"),
                format!("1 pr create --web in {root}"),
                format!("1 pr checks --json=name,workflow,bucket,link in {root}"),
            ]
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failing_gh_says_why() {
        let dir = fake_gh(
            "fails",
            "echo 'no pull requests found for branch \"x\"' >&2\nexit 1",
        );
        let program = dir.join("gh");
        let err = pr_checks(&program, &dir).unwrap_err();
        assert_eq!(err.to_string(), "no pull requests found for branch \"x\"");
        let err = create_pr_web(&program, &dir).unwrap_err();
        assert!(err.to_string().contains("no pull requests"), "{err:#}");
        assert!(latest_run(&program, &dir, "x").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn runs_read_as_pending_passed_failed_or_neutral() {
        let state = |status: &str, conclusion: &str| {
            let json =
                format!(r#"[{{"status":"{status}","conclusion":"{conclusion}","url":"u"}}]"#);
            parse_runs(json.as_bytes()).unwrap().unwrap().state
        };
        assert_eq!(state("in_progress", ""), CiState::Pending);
        assert_eq!(state("queued", ""), CiState::Pending);
        assert_eq!(state("completed", "success"), CiState::Passed);
        assert_eq!(state("completed", "timed_out"), CiState::Failed);
        assert_eq!(state("completed", "cancelled"), CiState::Neutral);
        assert_eq!(parse_runs(b"[]").unwrap(), None);
        assert!(parse_runs(b"not json").is_err());
    }
}
