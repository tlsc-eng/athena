use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
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

/// Stops a run: at its next check, or at once when the app is about to go.
#[derive(Debug, Default)]
pub struct Stop {
    requested: AtomicBool,
    /// The running process group; cleared before its leader is reaped, so the id is never stale.
    group: Mutex<Option<libc::pid_t>>,
}

impl Stop {
    /// The run ends its process group gracefully and reports `Ended::Cancelled`.
    pub fn request(&self) {
        self.requested.store(true, Ordering::Relaxed);
    }

    /// Kills the running process group now, for when the app is quitting.
    pub fn kill_now(&self) {
        self.request();
        let group = self.group.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(group) = *group {
            // SAFETY: the group's leader is not reaped while it is recorded here.
            unsafe { libc::killpg(group, libc::SIGKILL) };
        }
    }

    fn requested(&self) -> bool {
        self.requested.load(Ordering::Relaxed)
    }
}

/// Runs `job` through a login shell, so goenv, nodenv and Homebrew are on PATH as in a terminal;
/// the whole process group is killed when it is stopped, `limit` passes, or the runner exits.
pub fn run(job: &Job, limit: Duration, stop: &Stop) -> Result<Finished> {
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
    run_command(cmd, limit, stop)
}

fn run_command(mut cmd: Command, limit: Duration, stop: &Stop) -> Result<Finished> {
    if stop.requested() {
        return Ok(Finished {
            stdout: String::new(),
            stderr: String::new(),
            code: None,
            ended: Ended::Cancelled,
        });
    }
    let mut child = cmd.spawn().context("could not start the test run")?;
    let group = child.id() as libc::pid_t;
    *stop.group.lock().unwrap_or_else(PoisonError::into_inner) = Some(group);
    let stdout = stream(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = stream(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = Instant::now() + limit;
    let ended = loop {
        if stop.requested() {
            terminate(group);
            break Ended::Cancelled;
        }
        if exited(group) {
            break Ended::Exited;
        }
        if Instant::now() >= deadline {
            terminate(group);
            break Ended::TimedOut;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    {
        let mut recorded = stop.group.lock().unwrap_or_else(PoisonError::into_inner);
        // Whatever the runner left behind goes too, so nothing holds the pipes or outlives the run.
        // SAFETY: the leader is not reaped yet, so the group id is still ours.
        unsafe { libc::killpg(group, libc::SIGKILL) };
        *recorded = None;
    }
    let status = child.wait()?;
    Ok(Finished {
        stdout: stdout.collect(),
        stderr: stderr.collect(),
        code: (ended == Ended::Exited).then(|| status.code()).flatten(),
        ended,
    })
}

/// A pipe read as it fills, so what arrived is there even if something outside the group keeps
/// it open.
struct Stream {
    out: Arc<Mutex<Vec<u8>>>,
    reader: JoinHandle<()>,
}

impl Stream {
    fn collect(self) -> String {
        let grace = Instant::now() + Duration::from_secs(1);
        while !self.reader.is_finished() && Instant::now() < grace {
            std::thread::sleep(Duration::from_millis(10));
        }
        let out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&out).into_owned()
    }
}

fn stream(pipe: Option<Box<dyn Read + Send>>) -> Stream {
    let out = Arc::new(Mutex::new(Vec::new()));
    let sink = out.clone();
    let reader = std::thread::spawn(move || {
        let Some(mut pipe) = pipe else { return };
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => sink
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    });
    Stream { out, reader }
}

/// Whether the group's leader has exited, leaving it unreaped so its id cannot be reused yet.
fn exited(pid: libc::pid_t) -> bool {
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
        return std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted;
    }
    info.si_pid == pid
}

/// Test binaries and workers are the shell's descendants, so the group gets a moment to stop.
fn terminate(group: libc::pid_t) {
    // SAFETY: signalling a process group whose leader we have not reaped.
    unsafe { libc::killpg(group, libc::SIGTERM) };
    let grace = Instant::now() + Duration::from_millis(500);
    while Instant::now() < grace && !exited(group) {
        std::thread::sleep(Duration::from_millis(20));
    }
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
        let never = Stop::default();
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
        let never = Stop::default();
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
        let stop = Stop::default();
        stop.request();
        let done = run_command(sh("sleep 30"), Duration::from_secs(60), &stop).unwrap();
        assert_eq!(done.ended, Ended::Cancelled);
        assert_eq!(done.code, None);
    }

    fn marker(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("athena-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn output_shows_and_leftovers_die_when_an_orphan_holds_the_pipe() {
        let never = Stop::default();
        let started = Instant::now();
        let marker = marker("orphan");
        let script = format!("echo out; (sleep 2; touch '{}') & exit 0", marker.display());
        let done = run_command(sh(&script), Duration::from_secs(30), &never).unwrap();
        assert_eq!((done.stdout.as_str(), done.code), ("out\n", Some(0)));
        assert_eq!(done.ended, Ended::Exited);
        assert!(started.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(2500));
        assert!(!marker.exists(), "the orphan outlived the run");
    }

    #[test]
    fn output_printed_before_the_time_limit_is_kept() {
        let never = Stop::default();
        let done = run_command(
            sh("echo partial; sleep 30"),
            Duration::from_millis(300),
            &never,
        )
        .unwrap();
        assert_eq!(
            (done.stdout.as_str(), done.ended),
            ("partial\n", Ended::TimedOut)
        );
    }

    #[test]
    fn killing_now_ends_the_group_without_waiting_for_the_run() {
        let stop = Arc::new(Stop::default());
        let marker = marker("quit");
        let script = format!("(sleep 1; touch '{}') & sleep 30", marker.display());
        let runner = {
            let stop = stop.clone();
            std::thread::spawn(move || run_command(sh(&script), Duration::from_secs(60), &stop))
        };
        let started = Instant::now();
        while stop.group.lock().unwrap().is_none() && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.kill_now();
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!marker.exists(), "the run outlived kill_now");
        assert_eq!(runner.join().unwrap().unwrap().ended, Ended::Cancelled);
    }

    #[test]
    fn a_report_written_before_the_time_limit_is_still_read() {
        let file = marker("jest-report.json");
        let job = Job {
            framework: crate::Framework::Jest,
            dir: std::env::temp_dir(),
            program: "jest".into(),
            args: Vec::new(),
            report: Some(file.clone()),
        };
        let report = r#"{"testResults":[{"name":"/w/a.test.js","status":"passed","message":"",
            "assertionResults":[{"ancestorTitles":[],"title":"adds","status":"passed",
            "failureMessages":[]}]}]}"#;
        let script = format!(
            "printf '%s' '{report}' > '{}'; echo 'Jest did not exit'; sleep 30",
            file.display()
        );
        let never = Stop::default();
        let done = run_command(sh(&script), Duration::from_millis(500), &never).unwrap();
        assert_eq!(done.ended, Ended::TimedOut);
        let parsed = crate::report(&job, &done, None).unwrap();
        assert_eq!(parsed.count(crate::Outcome::Passed), 1);
    }
}
