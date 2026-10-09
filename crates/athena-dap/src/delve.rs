use std::path::PathBuf;

use crate::client::{Adapter, Transport};

/// What to tell the user when Delve is not installed; Athena never installs it.
pub const DELVE_INSTALL_HINT: &str = "go install github.com/go-delve/delve/cmd/dlv@latest";

/// Delve on the login shell's PATH, as gopls is found.
pub fn find_delve() -> Option<PathBuf> {
    athena_lsp::find_program("dlv")
}

/// `dlv dap`, which has no stdio mode, so it dials back to a socket Athena listens on.
pub fn delve_adapter(program: PathBuf, cwd: PathBuf) -> Adapter {
    Adapter {
        program,
        args: vec!["dap".into(), "--client-addr".into(), "unix:{socket}".into()],
        cwd,
        login_shell: true,
        transport: Transport::DialIn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Client, Event};
    use crate::launch::{Launch, Mode};
    use crate::types::SourceBreakpoint;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};
        struct Unpark(std::thread::Thread);
        impl Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                return out;
            }
            std::thread::park_timeout(Duration::from_millis(50));
        }
    }

    /// The next event `wanted` accepts, collecting program output on the way.
    fn wait_for(
        events: &async_channel::Receiver<Event>,
        output: &mut String,
        wanted: impl Fn(&Event) -> bool,
    ) -> Event {
        let deadline = Instant::now() + Duration::from_secs(240);
        loop {
            match events.try_recv() {
                Ok(Event::Output { text, .. }) => output.push_str(&text),
                Ok(event) if wanted(&event) => return event,
                Ok(Event::Closed(why)) => panic!("delve closed: {why}\n{output}"),
                Ok(_) => {}
                Err(_) if Instant::now() > deadline => panic!("no event in time\n{output}"),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    /// Launching a program under Delve on macOS asks for an administrator password unless
    /// Developer Mode is on, which a test run must never do.
    fn debugging_needs_no_password() -> bool {
        std::process::Command::new("/usr/sbin/DevToolsSecurity")
            .arg("-status")
            .output()
            .is_ok_and(|o| {
                let text = String::from_utf8_lossy(&o.stdout) + String::from_utf8_lossy(&o.stderr);
                text.contains("currently enabled")
            })
    }

    const MAIN: &str = "package main

import \"fmt\"

type point struct{ X, Y int }

func main() {
\ttotal := 0
\tp := point{X: 1, Y: 2}
\tfor i := 0; i < 3; i++ {
\t\ttotal += i
\t}
\tfmt.Println(\"total\", total, p)
}
";

    #[test]
    fn delve_stops_at_a_breakpoint_reads_a_variable_steps_and_runs_to_the_end() {
        let Some(dlv) = find_delve() else {
            eprintln!("skipped: dlv is not on the login PATH");
            return;
        };
        if !debugging_needs_no_password() && std::env::var_os("ATHENA_DAP_TEST").is_none() {
            eprintln!(
                "skipped: Developer Mode is off (DevToolsSecurity -enable), so macOS would ask"
            );
            return;
        }
        let dir = std::env::temp_dir().join(format!("athena-dap-delve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("go.mod"),
            "module example.com/athenadap\n\ngo 1.21\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.go"), MAIN).unwrap();
        let main = dir.join("main.go");

        let (client, events) = Client::start(delve_adapter(dlv, dir.clone())).unwrap();
        let mut output = String::new();
        block_on(client.initialize("go")).unwrap();
        let launch = Launch {
            name: "test".into(),
            mode: Mode::Debug,
            program: dir.clone(),
            args: Vec::new(),
            env: Vec::new(),
            build_flags: None,
            cwd: dir.clone(),
        };
        let arguments = launch.delve_arguments(&client.scratch().join("bin"));
        block_on(client.launch(arguments)).unwrap();
        wait_for(&events, &mut output, |e| *e == Event::Initialized);
        let set = vec![SourceBreakpoint {
            line: 11,
            ..Default::default()
        }];
        let status = block_on(client.set_breakpoints(&main, &set)).unwrap();
        assert!(status[0].verified, "{status:?}");
        block_on(client.configuration_done()).unwrap();

        let Event::Stopped(stopped) =
            wait_for(&events, &mut output, |e| matches!(e, Event::Stopped(_)))
        else {
            unreachable!()
        };
        assert_eq!(stopped.reason, "breakpoint");
        let thread = stopped.thread_id.unwrap();
        let frames = block_on(client.stack_trace(thread, 20)).unwrap();
        assert_eq!(frames[0].name, "main.main");
        assert_eq!(frames[0].line, 11);
        assert_eq!(
            frames[0]
                .path
                .as_deref()
                .map(Path::canonicalize)
                .map(Result::unwrap),
            Some(main.canonicalize().unwrap())
        );
        let scopes = block_on(client.scopes(frames[0].id)).unwrap();
        let locals = block_on(client.variables(scopes[0].variables_reference)).unwrap();
        let total = locals.iter().find(|v| v.name == "total").unwrap();
        assert_eq!(total.value, "0");
        let p = locals.iter().find(|v| v.name == "p").unwrap();
        assert!(p.variables_reference > 0, "{p:?}");
        let fields = block_on(client.variables(p.variables_reference)).unwrap();
        assert!(
            fields.iter().any(|f| f.name == "X" && f.value == "1"),
            "{fields:?}"
        );
        let sum = block_on(client.evaluate("p.X + p.Y", Some(frames[0].id), "repl")).unwrap();
        assert_eq!(sum.result, "3");

        block_on(client.next(thread)).unwrap();
        let Event::Stopped(stepped) =
            wait_for(&events, &mut output, |e| matches!(e, Event::Stopped(_)))
        else {
            unreachable!()
        };
        assert_eq!(stepped.reason, "step");
        let line = block_on(client.stack_trace(thread, 1)).unwrap()[0].line;
        assert_ne!(line, 11, "next moved on");

        block_on(client.set_breakpoints(&main, &[])).unwrap();
        block_on(client.continue_(thread)).unwrap();
        wait_for(&events, &mut output, |e| *e == Event::Terminated);
        assert!(output.contains("total 3"), "{output}");
        let _ = block_on(client.disconnect());
        drop(client);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
