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
        let home = PathBuf::from(format!("/tmp/athena-t{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_athena-mux"))
            .env("HOME", &home)
            .env("ATHENA_TEST_SECRET", "leaked")
            .env("ATHENA_NOTIFY_AFTER_SECS", "0")
            .env("SHELL", "/bin/zsh")
            .spawn()
            .unwrap();
        let daemon = Self { home, child };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !daemon.socket().exists() {
            assert!(Instant::now() < deadline, "daemon did not start");
            thread::sleep(Duration::from_millis(20));
        }
        daemon
    }

    fn socket(&self) -> PathBuf {
        self.home
            .join("Library/Application Support/athena/mux.sock")
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
    let ServerMsg::Panes { panes } = next(&mut reader) else {
        panic!("expected Panes")
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
