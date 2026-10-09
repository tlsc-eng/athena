use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use athena_workspace::gh::{self, CiRun, CiState, Gh};
use gpui::{Context, InteractiveElement, Task, Window, actions};

use super::Shell;

actions!(athena, [CreatePullRequest, ViewPullRequestChecks]);

/// The CI dot is re-read this often while the window is in front.
const CI_EVERY: Duration = Duration::from_secs(120);

#[derive(Default)]
pub(super) struct GithubState {
    /// `None` until the first check has finished.
    gh: Option<Gh>,
    checking: bool,
    /// Each project's branch and the latest run on it.
    ci: HashMap<PathBuf, (String, Option<CiRun>)>,
    asking: HashSet<PathBuf>,
    poll: Option<Task<()>>,
    /// The checks the palette lists, failures first.
    pub(super) checks: Vec<gh::Check>,
}

/// What runs once `gh` is found ready.
type AfterCheck = fn(&mut Shell, PathBuf, &mut Window, &mut Context<Shell>);

/// `gh` on the login shell's PATH, or where Homebrew puts it.
fn find_gh() -> Option<PathBuf> {
    athena_lsp::find_program("gh")
        .or_else(|| gh::locate(OsStr::new("/opt/homebrew/bin:/usr/local/bin")))
}

/// How a run or check reads in a tooltip or a palette row.
pub(super) fn state_word(state: CiState) -> &'static str {
    match state {
        CiState::Pending => "in progress",
        CiState::Passed => "passed",
        CiState::Failed => "failed",
        CiState::Neutral => "cancelled or skipped",
    }
}

/// The CI dot's tooltip: the workflow, how it went and the commit it ran on.
pub(super) fn ci_tooltip(run: &CiRun) -> String {
    let workflow = if run.workflow.is_empty() {
        "CI"
    } else {
        &run.workflow
    };
    let mut tip = format!("{workflow} {}", state_word(run.state));
    if !run.title.is_empty() {
        tip.push_str(&format!(": {}", run.title));
    }
    tip.push_str(" · Click to open the run");
    tip
}

impl Shell {
    /// Finds `gh` and whether it is signed in, then keeps the CI dot current every two minutes
    /// while the window is in front.
    pub(super) fn start_github(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.check_gh(None, window, cx);
        self.git.github.poll = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(CI_EVERY).await;
                let alive = this.update_in(cx, |this, window, cx| {
                    if window.is_window_active()
                        && let Some(root) = this.active_root()
                    {
                        this.refresh_ci(root, cx);
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
        }));
    }

    /// Checks for `gh` in the background, then runs `then` with it if it is ready, or says
    /// what is missing.
    fn check_gh(&mut self, then: Option<AfterCheck>, window: &mut Window, cx: &mut Context<Self>) {
        if self.git.github.checking && then.is_none() {
            return;
        }
        self.git.github.checking = true;
        cx.spawn_in(window, async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move { gh::check(find_gh()) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.git.github.checking = false;
                let first = this.git.github.gh.is_none();
                this.git.github.gh = Some(found.clone());
                match (found, then) {
                    (Gh::Ready(program), Some(then)) => then(this, program, window, cx),
                    (gh, Some(_)) => {
                        let why = gh.explain().unwrap_or_default();
                        this.transient_notice(
                            "GitHub CLI unavailable",
                            format!("{why}, then try again."),
                            cx,
                        );
                    }
                    (Gh::Ready(_), None) if first => {
                        if let Some(root) = this.active_root() {
                            this.refresh_ci(root, cx);
                        }
                    }
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// What the palette says beside a GitHub command when `gh` cannot run it.
    pub(super) fn gh_unavailable(&self) -> Option<&'static str> {
        self.git.github.gh.as_ref()?.explain()
    }

    /// The latest run on the project's current branch, for the status bar.
    pub(super) fn ci_run(&self, root: &Path) -> Option<&CiRun> {
        let branch = self.cached_branch(root)?;
        let (seen, run) = self.git.github.ci.get(root)?;
        (*seen == branch).then_some(run.as_ref()).flatten()
    }

    /// Reads the latest run on the project's branch.
    pub(super) fn refresh_ci(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let Some(Gh::Ready(program)) = self.git.github.gh.clone() else {
            return;
        };
        let Some(branch) = self.cached_branch(&root) else {
            return;
        };
        if !self.git.github.asking.insert(root.clone()) {
            return;
        }
        cx.spawn(async move |this, cx| {
            let (task_root, task_branch) = (root.clone(), branch.clone());
            let found = cx
                .background_executor()
                .spawn(async move { gh::latest_run(&program, &task_root, &task_branch) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.git.github.asking.remove(&root);
                // A repository without a GitHub remote has no dot rather than an error.
                let run = found
                    .inspect_err(
                        |err| tracing::debug!(root = %root.display(), "gh run list: {err:#}"),
                    )
                    .ok()
                    .flatten();
                this.git.github.ci.insert(root, (branch, run));
                cx.notify();
            });
        })
        .detach();
    }

    /// A status run finished; reads the CI run of a branch the dot has not asked about yet.
    pub(super) fn ci_branch_seen(&mut self, root: &Path, cx: &mut Context<Self>) {
        let branch = self.cached_branch(root);
        let known = self.git.github.ci.get(root).map(|(seen, _)| seen);
        if branch.is_some() && known != branch.as_ref() {
            self.refresh_ci(root.to_path_buf(), cx);
        }
    }

    /// "Create Pull Request": GitHub's new pull request page for the pushed branch, in the browser.
    fn create_pull_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        match self.cached_tracking(&root) {
            None => {
                return self.transient_notice(
                    "Publish the branch first",
                    "Publish it from the status bar, then try again.",
                    cx,
                );
            }
            Some(t) if t.ahead > 0 => {
                let commits = if t.ahead == 1 { "commit" } else { "commits" };
                return self.transient_notice(
                    "Push your commits first",
                    format!(
                        "{} {commits} not on {} would be left out.",
                        t.ahead, t.upstream
                    ),
                    cx,
                );
            }
            Some(_) => {}
        }
        self.check_gh(
            Some(|this, program, _, cx| {
                let Some(root) = this.active_root() else {
                    return;
                };
                cx.spawn(async move |this, cx| {
                    let done = cx
                        .background_executor()
                        .spawn(async move { gh::create_pr_web(&program, &root) })
                        .await;
                    let _ = this.update(cx, |this, cx| match done {
                        Ok(()) => this.transient_notice(
                            "Pull request opened in the browser",
                            "Finish it on GitHub.",
                            cx,
                        ),
                        Err(err) => this.transient_notice(
                            "Could not create a pull request",
                            format!("{err:#}"),
                            cx,
                        ),
                    });
                })
                .detach();
            }),
            window,
            cx,
        );
    }

    /// "View Pull Request Checks": the current branch's pull request checks, in the palette.
    fn view_pull_request_checks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_root().is_none() {
            return;
        }
        self.check_gh(
            Some(|this, program, window, cx| {
                let Some(root) = this.active_root() else {
                    return;
                };
                cx.spawn_in(window, async move |this, cx| {
                    let found = cx
                        .background_executor()
                        .spawn(async move { gh::pr_checks(&program, &root) })
                        .await;
                    let _ = this.update_in(cx, |this, window, cx| match found {
                        Ok(checks) if checks.is_empty() => this.transient_notice(
                            "No checks",
                            "The pull request has no checks reported yet.",
                            cx,
                        ),
                        Ok(checks) => {
                            this.git.github.checks = checks;
                            this.open_palette(super::palette::Mode::Checks, window, cx);
                        }
                        Err(err) => this.transient_notice(
                            "Could not read the pull request checks",
                            format!("{err:#}"),
                            cx,
                        ),
                    });
                })
                .detach();
            }),
            window,
            cx,
        );
    }
}

/// Binds the GitHub commands on the shell's root element.
pub(super) fn bind_github_actions(el: gpui::Div, cx: &mut Context<Shell>) -> gpui::Div {
    el.on_action(
        cx.listener(|this, _: &CreatePullRequest, window, cx| this.create_pull_request(window, cx)),
    )
    .on_action(cx.listener(|this, _: &ViewPullRequestChecks, window, cx| {
        this.view_pull_request_checks(window, cx)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ci_tooltip_names_the_workflow_and_the_commit() {
        let run = CiRun {
            state: CiState::Failed,
            url: "https://github.com/o/r/actions/runs/1".into(),
            workflow: "CI".into(),
            title: "Fix the parser".into(),
        };
        assert_eq!(
            ci_tooltip(&run),
            "CI failed: Fix the parser · Click to open the run"
        );
        let bare = CiRun {
            workflow: String::new(),
            title: String::new(),
            state: CiState::Pending,
            ..run
        };
        assert_eq!(ci_tooltip(&bare), "CI in progress · Click to open the run");
    }
}
