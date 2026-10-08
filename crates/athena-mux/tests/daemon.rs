use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use athena_proto::{ClientMsg, Connection, NoticeKind, ServerMsg, connect, read_frame};

/// A daemon under a throwaway HOME; short path because unix socket paths are capped at 104 bytes.
struct Daemon {
    home: PathBuf,
    child: Child,
}

impl Daemon {
    fn start(tag: &str) -> Self {
        Self::start_with(tag, &[])
    }

    fn start_with(tag: &str, env: &[(&str, &str)]) -> Self {
        let home = PathBuf::from(format!("/tmp/athena-t{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_athena-mux"))
            .env("HOME", &home)
            .env("ATHENA_TEST_SECRET", "leaked")
            .env("ATHENA_NOTIFY_AFTER_SECS", "0")
            .env("SHELL", "/bin/zsh")
            .envs(env.iter().copied())
            .spawn()
            .unwrap();
        let daemon = Self { home, child };
        let deadline = Instant::now() + Duration::from_secs(5);
        // The file appears at bind, before listen and chmod; restricting it is the last setup step.
        while !daemon.ready() {
            assert!(Instant::now() < deadline, "daemon did not start");
            thread::sleep(Duration::from_millis(20));
        }
        daemon
    }

    fn socket(&self) -> PathBuf {
        self.home
            .join("Library/Application Support/athena/mux.sock")
    }

    fn ready(&self) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(self.socket()).is_ok_and(|m| m.permissions().mode() & 0o777 == 0o600)
    }

    fn connect(&self) -> (Connection, UnixStream) {
        connect(&self.socket()).unwrap()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// Next reply, skipping foreground-process updates, which the daemon sends on its own schedule.
fn next(reader: &mut UnixStream) -> ServerMsg {
    loop {
        let msg = read_frame(reader)
            .unwrap()
            .expect("daemon closed the connection");
        if !matches!(msg, ServerMsg::Foreground { .. }) {
            return msg;
        }
    }
}

/// Reads until `needle` shows up in a pane's output, returning everything read.
fn read_until(reader: &mut UnixStream, needle: &str) -> String {
    reader
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut seen = String::new();
    while !seen.contains(needle) {
        if let ServerMsg::Output { data, .. } = next(reader) {
            seen.push_str(&String::from_utf8_lossy(&data));
        }
    }
    seen
}

fn spawn_pane(conn: &Connection, reader: &mut UnixStream) -> u64 {
    conn.send(&ClientMsg::Spawn {
        cwd: "/tmp".into(),
        rows: 24,
        cols: 80,
    })
    .unwrap();
    let ServerMsg::Spawned { pane } = next(reader) else {
        panic!("expected Spawned")
    };
    pane
}

#[test]
fn session_survives_client_disconnect_and_replays() {
    let daemon = Daemon::start("replay");
    let (conn, mut reader) = daemon.connect();
    conn.send(&ClientMsg::Spawn {
        cwd: "/tmp".into(),
        rows: 24,
        cols: 80,
    })
    .unwrap();
    let ServerMsg::Spawned { pane } = next(&mut reader) else {
        panic!("expected Spawned")
    };
    conn.send(&ClientMsg::Attach { pane }).unwrap();
    assert!(matches!(
        next(&mut reader),
        ServerMsg::Attached {
            rows: 24,
            cols: 80,
            ..
        }
    ));
    conn.send(&ClientMsg::Input {
        pane,
        data: b"echo athena-$((40+2))\r".to_vec(),
    })
    .unwrap();
    read_until(&mut reader, "athena-42");
    drop((conn, reader));

    let (conn, mut reader) = daemon.connect();
    conn.send(&ClientMsg::Attach { pane }).unwrap();
    assert!(matches!(next(&mut reader), ServerMsg::Attached { .. }));
    let mut replay = String::new();
    loop {
        match next(&mut reader) {
            ServerMsg::Output { data, .. } => replay.push_str(&String::from_utf8_lossy(&data)),
            ServerMsg::ReplayDone { .. } => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(replay.contains("athena-42"), "history was not replayed");

    conn.send(&ClientMsg::Kill { pane }).unwrap();
    conn.send(&ClientMsg::ListPanes).unwrap();
    // Live output and the kill's Exited can still arrive ahead of the reply.
    let panes = loop {
        match next(&mut reader) {
            ServerMsg::Panes { panes } => break panes,
            ServerMsg::Output { .. } | ServerMsg::Exited { .. } | ServerMsg::Foreground { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    };
    assert!(panes.is_empty());
}

#[test]
fn unknown_pane_is_reported() {
    let daemon = Daemon::start("nopane");
    let (conn, mut reader) = daemon.connect();
    conn.send(&ClientMsg::Attach { pane: 1 }).unwrap();
    assert!(matches!(next(&mut reader), ServerMsg::Error { .. }));
}

#[test]
fn socket_is_owner_only_and_second_daemon_refuses() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = Daemon::start("lock");
    let mode = std::fs::metadata(daemon.socket())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    let status = Command::new(env!("CARGO_BIN_EXE_athena-mux"))
        .env("HOME", &daemon.home)
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "a second daemon must not start while the first holds the lock"
    );
}

#[test]
fn shutdown_removes_socket() {
    let mut daemon = Daemon::start("stop");
    let (conn, _reader) = daemon.connect();
    conn.send(&ClientMsg::Shutdown).unwrap();
    let status = daemon.child.wait().unwrap();
    assert!(status.success());
    assert!(!daemon.socket().exists());
}

#[test]
fn shells_do_not_inherit_the_daemon_environment() {
    let daemon = Daemon::start("env");
    let (conn, mut reader) = daemon.connect();
    conn.send(&ClientMsg::Spawn {
        cwd: "/tmp".into(),
        rows: 24,
        cols: 80,
    })
    .unwrap();
    let ServerMsg::Spawned { pane } = next(&mut reader) else {
        panic!("expected Spawned")
    };
    conn.send(&ClientMsg::Attach { pane }).unwrap();
    let probe =
        b"echo \"secret=[${ATHENA_TEST_SECRET}] home=[${HOME}] pane=[${ATHENA_PANE_ID}]\"\r";
    conn.send(&ClientMsg::Input {
        pane,
        data: probe.to_vec(),
    })
    .unwrap();
    let out = read_until(&mut reader, &format!("pane=[{pane}]"));
    assert!(out.contains("secret=[]"), "secret leaked: {out}");
    assert!(out.contains(&format!("home=[{}]", daemon.home.display())));
}

#[test]
fn reports_the_foreground_program() {
    let daemon = Daemon::start("fg");
    let (conn, mut reader) = daemon.connect();
    conn.send(&ClientMsg::Spawn {
        cwd: "/tmp".into(),
        rows: 24,
        cols: 80,
    })
    .unwrap();
    let ServerMsg::Spawned { pane } = next(&mut reader) else {
        panic!("expected Spawned")
    };
    conn.send(&ClientMsg::Attach { pane }).unwrap();
    conn.send(&ClientMsg::Input {
        pane,
        data: b"sleep 3\r".to_vec(),
    })
    .unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    loop {
        if let ServerMsg::Foreground {
            process: Some(p), ..
        } = read_frame(&mut reader).unwrap().unwrap()
            && p.name == "sleep"
        {
            assert!(p.path.is_absolute());
            break;
        }
    }
}

#[test]
fn sigterm_stops_the_daemon_even_if_the_parent_ignored_it() {
    use std::os::unix::process::CommandExt;
    let home = PathBuf::from(format!("/tmp/athena-t{}-term", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_athena-mux"));
    cmd.env("HOME", &home);
    // SAFETY: only async-signal-safe signal(2) between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
            Ok(())
        });
    }
    let child = cmd.spawn().unwrap();
    let mut daemon = Daemon { home, child };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !daemon.socket().exists() {
        assert!(Instant::now() < deadline, "daemon did not start");
        thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: signalling our own child.
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(5);
    while daemon.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "SIGTERM was ignored");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn zsh_integration_reports_finished_commands() {
    let daemon = Daemon::start("notice");
    let (conn, mut reader) = daemon.connect();
    conn.send(&ClientMsg::Subscribe).unwrap();
    conn.send(&ClientMsg::Spawn {
        cwd: "/tmp".into(),
        rows: 24,
        cols: 80,
    })
    .unwrap();
    let ServerMsg::Spawned { pane } = next(&mut reader) else {
        panic!("expected Spawned")
    };
    conn.send(&ClientMsg::Input {
        pane,
        data: b"sleep 0.2; false\r".to_vec(),
    })
    .unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    loop {
        if let ServerMsg::Notice(n) = read_frame(&mut reader).unwrap().unwrap() {
            let NoticeKind::CommandFinished {
                exit_code, command, ..
            } = n.kind
            else {
                continue;
            };
            assert_eq!(n.pane, Some(pane));
            assert_eq!(exit_code, 1);
            assert_eq!(command.as_deref(), Some("sleep 0.2; false"));
            break;
        }
    }
}

#[test]
fn notices_wait_for_a_subscriber() {
    let daemon = Daemon::start("backlog");
    let (sender, _r) = daemon.connect();
    let kind = NoticeKind::Message {
        title: "t\x1b".into(),
        body: "b".into(),
    };
    sender
        .send(&ClientMsg::Notify { pane: None, kind })
        .unwrap();
    thread::sleep(Duration::from_millis(100));
    let (conn, mut reader) = daemon.connect();
    conn.send(&ClientMsg::Subscribe).unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let Some(ServerMsg::Notice(n)) = read_frame(&mut reader).unwrap() else {
        panic!("expected the buffered notice")
    };
    assert_eq!(
        n.kind,
        NoticeKind::Message {
            title: "t".into(),
            body: "b".into()
        }
    );
}

#[test]
fn a_client_that_stops_reading_does_not_stall_other_clients() {
    use std::sync::{Arc, Mutex};

    let daemon = Daemon::start("lag");
    let (stalled, mut stalled_reader) = daemon.connect();
    let flood = spawn_pane(&stalled, &mut stalled_reader);
    let shared = spawn_pane(&stalled, &mut stalled_reader);
    stalled.send(&ClientMsg::Subscribe).unwrap();
    stalled.send(&ClientMsg::Attach { pane: flood }).unwrap();
    stalled.send(&ClientMsg::Attach { pane: shared }).unwrap();
    // macOS refuses setsockopt once the daemon has shut the socket down, so set it now.
    stalled_reader
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let (healthy, mut healthy_reader) = daemon.connect();
    let own = spawn_pane(&healthy, &mut healthy_reader);
    healthy.send(&ClientMsg::Attach { pane: shared }).unwrap();
    healthy.send(&ClientMsg::Attach { pane: own }).unwrap();
    healthy_reader
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let ticks = Arc::new(Mutex::new(String::new()));
    let foreground = Arc::new(Mutex::new(Vec::<String>::new()));
    let (seen_ticks, seen_foreground) = (ticks.clone(), foreground.clone());
    thread::spawn(move || {
        loop {
            match read_frame::<_, ServerMsg>(&mut healthy_reader) {
                Ok(Some(ServerMsg::Output { pane, data })) if pane == shared => {
                    seen_ticks
                        .lock()
                        .unwrap()
                        .push_str(&String::from_utf8_lossy(&data));
                }
                Ok(Some(ServerMsg::Foreground {
                    pane,
                    process: Some(p),
                })) if pane == own => seen_foreground.lock().unwrap().push(p.name),
                Ok(Some(_)) => {}
                Ok(None) => return,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return,
            }
        }
    });

    let input = |conn: &Connection, pane, text: &str| {
        conn.send(&ClientMsg::Input {
            pane,
            data: text.as_bytes().to_vec(),
        })
        .unwrap();
    };
    input(&healthy, flood, "yes athena-flood\r");
    input(
        &healthy,
        shared,
        "i=0; while :; do i=$((i+1)); echo tick-$i; sleep 0.02; done\r",
    );
    // Long enough for the flood to fill the stalled client's socket and queue.
    thread::sleep(Duration::from_secs(3));
    input(&healthy, own, "sleep 30\r");
    let before = ticks.lock().unwrap().matches("tick-").count();
    thread::sleep(Duration::from_secs(3));
    let after = ticks.lock().unwrap().matches("tick-").count();
    assert!(
        after > before + 20,
        "the shared pane stalled: {before} ticks, then {after}"
    );
    assert!(
        foreground.lock().unwrap().iter().any(|n| n == "sleep"),
        "foreground updates stalled: {:?}",
        foreground.lock().unwrap()
    );

    // The daemon hung up on the client that stopped reading: what it had buffered, then EOF.
    let end = loop {
        match read_frame::<_, ServerMsg>(&mut stalled_reader) {
            Ok(Some(_)) => {}
            other => break other,
        }
    };
    assert!(
        !matches!(&end, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "the stalled client is still connected"
    );
}

/// Pids of processes whose command line contains `needle`, with their parents.
fn processes_matching(needle: &str) -> Vec<(i32, i32)> {
    let out = Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains(needle))
        .filter_map(|l| {
            let mut fields = l.split_whitespace();
            Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
        })
        .collect()
}

fn children_of(parent: i32) -> Vec<i32> {
    let out = Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut fields = l.split_whitespace();
            let pid: i32 = fields.next()?.parse().ok()?;
            let ppid: i32 = fields.next()?.parse().ok()?;
            (ppid == parent).then_some(pid)
        })
        .collect()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the pid exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Pty masters the daemon holds open, as lsof sees them.
fn open_ptys(daemon: u32) -> usize {
    let out = Command::new("lsof")
        .args(["-p", &daemon.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("ptmx"))
        .count()
}

#[test]
fn killing_a_pane_also_ends_programs_that_ignore_the_hangup() {
    let daemon = Daemon::start("hup");
    let script = daemon.home.join("stubborn.sh");
    std::fs::write(&script, "trap '' HUP\necho stubborn-ready\nsleep 1000\n").unwrap();
    let (conn, mut reader) = daemon.connect();
    let pane = spawn_pane(&conn, &mut reader);
    conn.send(&ClientMsg::Attach { pane }).unwrap();
    conn.send(&ClientMsg::Input {
        pane,
        data: format!("trap '' HUP; sh {}\r", script.display()).into_bytes(),
    })
    .unwrap();
    read_until(&mut reader, "stubborn-ready\r\n");
    let script = script.display().to_string();
    let deadline = Instant::now() + Duration::from_secs(5);
    let (sh, sleep) = loop {
        let sh = processes_matching(&script).first().map(|(pid, _)| *pid);
        if let Some(sh) = sh
            && let Some(sleep) = children_of(sh).first()
        {
            break (sh, *sleep);
        }
        assert!(Instant::now() < deadline, "the script did not start");
        thread::sleep(Duration::from_millis(50));
    };
    let shell = processes_matching(&script)[0].1;
    assert!(open_ptys(daemon.child.id()) > 0);

    conn.send(&ClientMsg::Kill { pane }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while [shell, sh, sleep].into_iter().any(alive) || open_ptys(daemon.child.id()) > 0 {
        assert!(
            Instant::now() < deadline,
            "still running after the kill: shell {} sh {} sleep {}, {} ptys open",
            alive(shell),
            alive(sh),
            alive(sleep),
            open_ptys(daemon.child.id())
        );
        thread::sleep(Duration::from_millis(100));
    }
    conn.send(&ClientMsg::ListPanes).unwrap();
    let panes = loop {
        if let ServerMsg::Panes { panes } = next(&mut reader) {
            break panes;
        }
    };
    assert!(panes.is_empty());
}

fn list_panes(conn: &Connection, reader: &mut UnixStream) -> Vec<u64> {
    conn.send(&ClientMsg::ListPanes).unwrap();
    loop {
        if let ServerMsg::Panes { panes } = next(reader) {
            return panes.into_iter().map(|p| p.id).collect();
        }
    }
}

#[test]
fn a_pane_no_client_ever_attached_to_is_ended_but_a_detached_one_is_kept() {
    let daemon = Daemon::start_with("orphan", &[("ATHENA_UNATTACHED_SECS", "1")]);
    let (conn, mut reader) = daemon.connect();
    let orphan = spawn_pane(&conn, &mut reader);
    let kept = spawn_pane(&conn, &mut reader);
    conn.send(&ClientMsg::Attach { pane: kept }).unwrap();
    drop((conn, reader));

    let (conn, mut reader) = daemon.connect();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let panes = list_panes(&conn, &mut reader);
        assert!(panes.contains(&kept), "the detached pane was ended");
        if !panes.contains(&orphan) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the orphaned pane is still there"
        );
        thread::sleep(Duration::from_millis(200));
    }
}
