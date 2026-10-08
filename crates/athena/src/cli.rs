use athena_proto::{ClientMsg, ConnectError, ServerMsg};

const USAGE: &str = "usage: athena [mux status | mux stop]";

/// Handles command-line subcommands; `None` means start the app.
pub fn run(args: Vec<String>) -> Option<i32> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        [] => return None,
        ["mux", "status"] => mux_status(),
        ["mux", "stop"] => mux_stop(),
        _ => {
            eprintln!("{USAGE}");
            return Some(2);
        }
    };
    Some(match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("athena: {err:#}");
            1
        }
    })
}

fn mux_status() -> anyhow::Result<()> {
    let (conn, mut reader) = match athena_proto::connect(&athena_proto::socket_path()?) {
        Ok(pair) => pair,
        Err(ConnectError::NotRunning) => {
            println!("session daemon: not running");
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };
    conn.send(&ClientMsg::ListPanes)?;
    let Some(ServerMsg::Panes { panes }) = athena_proto::read_frame(&mut reader)? else {
        anyhow::bail!("unexpected reply from session daemon");
    };
    println!(
        "session daemon: pid {}, {} sessions",
        conn.daemon_pid,
        panes.len()
    );
    for pane in panes {
        let state = if pane.alive { "running" } else { "exited" };
        println!(
            "  {}  {}x{}  {:8}  {}",
            pane.id,
            pane.cols,
            pane.rows,
            state,
            pane.cwd.display()
        );
    }
    Ok(())
}

fn mux_stop() -> anyhow::Result<()> {
    match athena_proto::connect(&athena_proto::socket_path()?) {
        Ok((conn, _)) => {
            conn.send(&ClientMsg::Shutdown)?;
            println!("session daemon stopped; its shells were hung up");
            Ok(())
        }
        Err(ConnectError::NotRunning) => {
            println!("session daemon: not running");
            Ok(())
        }
        Err(ConnectError::VersionMismatch { pid, .. }) => {
            // SAFETY: plain kill(2) on the pid the daemon reported for itself.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            println!("session daemon (older build) stopped");
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}
