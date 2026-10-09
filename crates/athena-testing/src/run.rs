use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::Job;

/// Why a run stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    Exited,
    TimedOut,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct Finished {
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
    pub ended: Ended,
}

/// Runs `job` through a login shell, so goenv, nodenv and Homebrew are on PATH as in a terminal;
/// the whole process group is killed when `cancel` is set or `limit` passes.
pub fn run(job: &Job, limit: Duration, cancel: &AtomicBool) -> Result<Finished> {
    let mut cmd = Command::new("/bin/zsh");
    cmd.args(["-lc", "cd \"$1\" && shift && exec \"$@\"", "athena"])
        .arg(&job.dir)
        .arg(&job.program)
        .args(&job.args)
        .env("NO_COLOR", "1")
        .env("FORCE_COLOR", "0")
        .env("CI", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    run_command(cmd, limit, cancel)
}

fn run_command(mut cmd: Command, limit: Duration, cancel: &AtomicBool) -> Result<Finished> {
    let mut child = cmd.spawn().context("could not start the test run")?;
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = Instant::now() + limit;
    let (status, ended) = loop {
        if let Some(status) = child.try_wait()? {
            break (Some(status), Ended::Exited);
        }
        if cancel.load(Ordering::Relaxed) {
            kill_group(&mut child);
            break (None, Ended::Cancelled);
        }
        if Instant::now() >= deadline {
            kill_group(&mut child);
            break (None, Ended::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // A child the runner left behind can hold the pipes open; what arrived by then is enough.
    let collect = |rx: mpsc::Receiver<Vec<u8>>| {
        String::from_utf8_lossy(&rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default())
            .into_owned()
    };
    Ok(Finished {
        stdout: collect(stdout),
        stderr: collect(stderr),
        code: status.and_then(|s| s.code()),
        ended,
    })
}

fn drain(pipe: Option<Box<dyn Read + Send>>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut out);
        }
        let _ = tx.send(out);
    });
    rx
}

/// Test binaries and workers are the shell's descendants, so the group goes, not just the shell.
fn kill_group(child: &mut Child) {
    let group = child.id() as libc::pid_t;
    // SAFETY: signalling a process group we created; a stale id only makes kill fail.
    unsafe { libc::killpg(group, libc::SIGTERM) };
    let grace = Instant::now() + Duration::from_millis(500);
    while Instant::now() < grace {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: as above.
    unsafe { libc::killpg(group, libc::SIGKILL) };
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        cmd
    }

    #[test]
    fn output_and_exit_code_are_kept() {
        let never = AtomicBool::new(false);
        let done = run_command(
            sh("echo out; echo err >&2; exit 3"),
            Duration::from_secs(10),
            &never,
        )
        .unwrap();
        assert_eq!(
            (done.stdout.as_str(), done.stderr.as_str()),
            ("out\n", "err\n")
        );
        assert_eq!((done.code, done.ended), (Some(3), Ended::Exited));
    }

    #[test]
    fn the_time_limit_kills_the_whole_group() {
        let never = AtomicBool::new(false);
        let started = Instant::now();
        let marker = std::env::temp_dir().join(format!("athena-kill-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let script = format!("(sleep 2; touch '{}') & sleep 30", marker.display());
        let done = run_command(sh(&script), Duration::from_millis(300), &never).unwrap();
        assert_eq!(done.ended, Ended::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(2500));
        assert!(!marker.exists(), "the background child outlived the run");
    }

    #[test]
    fn cancelling_stops_the_run() {
        let cancel = AtomicBool::new(true);
        let done = run_command(sh("sleep 30"), Duration::from_secs(60), &cancel).unwrap();
        assert_eq!(done.ended, Ended::Cancelled);
        assert_eq!(done.code, None);
    }
}
