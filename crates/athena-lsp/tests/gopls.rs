use std::time::{Duration, Instant};

use athena_lsp::{Client, Event, Position, ServerKind, Severity};

fn next_event(events: &async_channel::Receiver<Event>, deadline: Instant) -> Option<Event> {
    while Instant::now() < deadline {
        if let Ok(event) = events.try_recv() {
            return Some(event);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

#[test]
fn gopls_reports_an_unused_import_and_finds_definitions() {
    if athena_lsp::find_program("gopls").is_none() {
        eprintln!("gopls not installed; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("athena-lsp-go-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    std::fs::write(dir.join("go.mod"), "module example.com/probe\n\ngo 1.21\n").unwrap();
    let source = "package main\n\nimport \"os\"\n\nfunc helper() int { return 1 }\n\nfunc main() { _ = helper() }\n";
    let file = dir.join("main.go");
    std::fs::write(&file, source).unwrap();

    let (client, events) = Client::start(ServerKind::Go, dir.clone());
    client.did_open(&file, "go", 1, source.into());

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut unused = None;
    while unused.is_none() {
        match next_event(&events, deadline).expect("no diagnostics from gopls in time") {
            Event::Diagnostics { path, list } if path == file => {
                unused = list.into_iter().find(|d| d.message.contains("\"os\""));
            }
            Event::Stopped(why) => panic!("gopls stopped: {why}"),
            _ => {}
        }
    }
    let unused = unused.unwrap();
    assert_eq!(unused.severity, Severity::Error);
    assert_eq!(unused.range.start.line, 2);

    let call = Position {
        line: 6,
        character: 19,
    };
    let found = futures_lite_block_on(client.definition(&file, call));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, file);
    assert_eq!(found[0].range.start.line, 4);

    drop(client);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A minimal executor: the definition future only waits on a channel the reader thread fills.
fn futures_lite_block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::pin::pin;
    use std::task::{Context, Poll, Wake, Waker};
    struct Thread(std::thread::Thread);
    impl Wake for Thread {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(std::sync::Arc::new(Thread(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        std::thread::park_timeout(Duration::from_millis(100));
    }
}
