use std::path::PathBuf;
use std::thread;

/// Restored terminals all connect at once on launch; exactly one daemon must start.
#[test]
fn concurrent_callers_share_one_spawned_daemon() {
    // Short path: unix socket paths are capped at 104 bytes.
    let home = PathBuf::from(format!("/tmp/athena-s{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    // SAFETY: the only test in this binary, before it starts any thread; the daemon inherits HOME.
    unsafe { std::env::set_var("HOME", &home) };
    let socket = athena_proto::socket_path().unwrap();
    let log = athena_proto::log_path().unwrap();
    let daemon = PathBuf::from(env!("CARGO_BIN_EXE_athena-mux"));

    let callers: Vec<_> = (0..8)
        .map(|_| {
            let (socket, daemon, log) = (socket.clone(), daemon.clone(), log.clone());
            thread::spawn(move || {
                athena_proto::connect_or_spawn(&socket, &daemon, &log)
                    .unwrap()
                    .0
                    .daemon_pid
            })
        })
        .collect();
    let pids: Vec<u32> = callers.into_iter().map(|t| t.join().unwrap()).collect();

    // SAFETY: plain kill(2) on the daemon this test started.
    unsafe { libc::kill(pids[0] as libc::pid_t, libc::SIGTERM) };
    let text = std::fs::read_to_string(&log).unwrap();
    let _ = std::fs::remove_dir_all(&home);
    assert!(pids.iter().all(|p| *p == pids[0]), "{pids:?}");
    assert_eq!(text.matches("listening").count(), 1, "{text}");
    assert!(!text.contains("already running"), "{text}");
}
