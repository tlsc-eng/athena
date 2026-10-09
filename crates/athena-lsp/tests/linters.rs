//! Runs against a project that installs the linters, named by ATHENA_LINTER_FIXTURE:
//! `npm i -D eslint @eslint/js vscode-langservers-extracted @biomejs/biome` with an
//! eslint.config.js turning on `no-var` and `prefer-const`. Skipped when it is not set.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use athena_lsp::{Client, Diagnostic, Event, Position, Range, ServerKind};

const SOURCE: &str = "var count = 1;\nlet total = 2;\nexport const twice = count * total;\n";

fn fixture() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("ATHENA_LINTER_FIXTURE")?);
    Some(root.canonicalize().expect("fixture exists"))
}

fn start(root: &Path, kind: ServerKind, file: &Path) -> (Client, async_channel::Receiver<Event>) {
    let program = athena_lsp::project_server(root, kind).expect("installed in the fixture");
    let settings = match kind {
        ServerKind::Eslint => athena_lsp::eslint_settings(root),
        _ => serde_json::Value::Null,
    };
    let config = Arc::new(RwLock::new(settings));
    let (client, events) = Client::start_local(kind, program, root.to_path_buf(), config);
    client.did_open(file, "javascript", 1, SOURCE.into());
    (client, events)
}

/// Waits for the server to start, then for its diagnostics: pulled from ESLint, published by Biome.
fn diagnostics(
    client: &Client,
    events: &async_channel::Receiver<Event>,
    file: &Path,
) -> Vec<Diagnostic> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        match events.try_recv() {
            Ok(Event::Ready) if client.supports("/diagnosticProvider") => {
                return block_on(client.pull_diagnostics(file)).unwrap();
            }
            Ok(Event::Diagnostics { path, list }) if path == file && !list.is_empty() => {
                return list;
            }
            Ok(Event::Stopped(why)) => panic!("stopped: {why}"),
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    panic!("no diagnostics in time");
}

#[test]
fn the_projects_eslint_and_biome_report_and_fix_problems() {
    let Some(root) = fixture() else {
        eprintln!("ATHENA_LINTER_FIXTURE not set; skipping");
        return;
    };
    let file = root.join("athena-linter-probe.js");
    std::fs::write(&file, SOURCE).unwrap();
    let start_of_file = Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: Position {
            line: 0,
            character: 0,
        },
    };
    for (kind, rule, fix_all) in [
        (ServerKind::Eslint, "no-var", "source.fixAll.eslint"),
        (ServerKind::Biome, "lint/style/noVar", "source.fixAll.biome"),
    ] {
        let (client, events) = start(&root, kind, &file);
        let found = diagnostics(&client, &events, &file);
        let var = found
            .iter()
            .find(|d| d.raw["code"] == rule)
            .unwrap_or_else(|| panic!("{kind:?} reports {rule}: {found:?}"));
        assert_eq!(var.range.start.line, 0);
        let fixes =
            block_on(client.code_actions(&file, start_of_file, Vec::new(), Some(&[fix_all])))
                .unwrap();
        assert!(
            fixes.iter().any(|a| a.edit.is_some()),
            "{kind:?} offers a fix-all edit: {fixes:?}"
        );
    }
    let _ = std::fs::remove_file(&file);
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::pin::pin;
    use std::task::{Context, Poll, Wake, Waker};
    struct Thread(std::thread::Thread);
    impl Wake for Thread {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Thread(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        std::thread::park_timeout(Duration::from_millis(100));
    }
}
