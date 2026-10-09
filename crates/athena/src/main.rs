mod actions;
mod app_socket;
mod claude_hooks;
mod cli;
mod ide;
mod keymap;
mod mcp;
mod procinfo;
mod settings;
mod shell;
mod snapshots;
mod system_notify;
mod transcripts;
mod usage;

use std::path::PathBuf;

use athena_workspace::Workspace;
use gpui::{App, Application};

fn main() {
    if let Some(code) = cli::run(std::env::args().skip(1).collect()) {
        std::process::exit(code);
    }
    match athena_proto::app_log_path() {
        Ok(log) => {
            if let Err(err) = athena_proto::logging::init(&log, "ATHENA_LOG") {
                eprintln!("athena: logging to {}: {err}", log.display());
            }
        }
        Err(err) => eprintln!("athena: {err:#}"),
    }
    tracing::info!("athena {} starting", env!("CARGO_PKG_VERSION"));
    install_panic_hook();
    Application::new()
        .with_assets(athena_ui::Assets)
        .run(|cx: &mut App| {
            athena_ui::init(cx);
            actions::init(cx);
            athena_term::init(cx);
            athena_editor::init(cx);

            let mut path = workspace_path();
            let mut workspace = athena_workspace::load(&path).unwrap_or_else(|err| {
                if athena_workspace::is_corrupt(&err) {
                    match athena_workspace::set_aside(&path) {
                        Ok(aside) => tracing::error!(
                            "{err:#}; kept it as {} and starting with an empty workspace",
                            aside.display()
                        ),
                        Err(e) => tracing::error!("{err:#}; could not keep a copy: {e:#}"),
                    }
                } else {
                    // Saving over a file that could not be read would lose it for good.
                    path = path.with_file_name("workspace.unreadable-session.json");
                    tracing::error!(
                        "{err:#}; leaving it alone and saving this session to {}",
                        path.display()
                    );
                }
                Workspace::default()
            });
            workspace.prune_missing();
            shell::start(path, workspace, cli::startup_folder(), cx);
        });
}

/// Logs a panic to app.log and keeps copies of unsaved files before the default hook runs.
fn install_panic_hook() {
    let recovery = athena_proto::recovery_dir().ok();
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // A worker thread's panic leaves the window running, and the IDE server with it.
        if std::thread::current().name() == Some("main") {
            ide::on_panic();
        }
        let kept = recovery
            .as_deref()
            .map(athena_editor::recovery::write_dirty);
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!("{info}\nunsaved files kept: {kept:?}\n{backtrace}");
        default(info);
    }));
}

fn workspace_path() -> PathBuf {
    athena_proto::data_dir()
        .expect("application support directory")
        .join("workspace.json")
}
