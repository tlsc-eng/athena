use std::io::{IsTerminal, Read};

use athena_proto::{ClientMsg, ConnectError, NoticeKind, ServerMsg};

const USAGE: &str = "usage: athena [mux status | mux stop | notify --event <claude-stop|claude-needs-input|claude-running> | notify --title <t> [--body <b>]]";

/// Hook input larger than this is ignored; Claude Code sends a small JSON object.
const MAX_HOOK_INPUT: u64 = 64 * 1024;

/// Handles command-line subcommands; `None` means start the app.
pub fn run(args: Vec<String>) -> Option<i32> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        [] => return None,
        ["mux", "status"] => mux_status(),
        ["mux", "stop"] => mux_stop(),
        ["notify", rest @ ..] => notify(rest),
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

/// Raises a notice in Athena. Never fails loudly: it runs inside Claude Code hooks, which must not
/// break because Athena is closed.
fn notify(args: &[&str]) -> anyhow::Result<()> {
    let flag = |name: &str| {
        args.iter()
            .position(|a| *a == name)
            .and_then(|i| args.get(i + 1))
            .map(|s| s.to_string())
    };
    let hook_message = || {
        let mut input = String::new();
        if !std::io::stdin().is_terminal() {
            let _ = std::io::stdin()
                .take(MAX_HOOK_INPUT)
                .read_to_string(&mut input);
        }
        serde_json::from_str::<serde_json::Value>(&input)
            .ok()
            .and_then(|v| {
                v.get("message")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
            })
    };
    let kind = match (flag("--event").as_deref(), flag("--title")) {
        (Some("claude-stop"), _) => NoticeKind::ClaudeStopped,
        (Some("claude-running"), _) => NoticeKind::ClaudeRunning,
        (Some("claude-needs-input"), _) => NoticeKind::ClaudeNeedsInput {
            message: flag("--message").or_else(hook_message).unwrap_or_default(),
        },
        (None, Some(title)) => NoticeKind::Message {
            title,
            body: flag("--body").unwrap_or_default(),
        },
        _ => anyhow::bail!("{USAGE}"),
    };
    let pane = std::env::var("ATHENA_PANE_ID")
        .ok()
        .and_then(|p| p.parse().ok());
    if let Ok((conn, _)) = athena_proto::connect(&athena_proto::socket_path()?) {
        conn.send(&ClientMsg::Notify { pane, kind })?;
    }
    Ok(())
}
