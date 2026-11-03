mod actions;
mod app_socket;
mod claude_hooks;
mod cli;
mod companion;
mod ide;
mod keymap;
mod mcp;
mod power;
mod procinfo;
mod review_comments;
mod settings;
mod shell;
mod snapshots;
mod system_notify;
mod transcripts;
mod usage;
mod worktree_forks;

use std::path::PathBuf;

use daedalus_workspace::Workspace;
use gpui::{App, Application};

fn main() {
    if let Some(code) = cli::run(std::env::args().skip(1).collect()) {
        std::process::exit(code);
    }
    match daedalus_proto::app_log_path() {
        Ok(log) => {
            if let Err(err) = daedalus_proto::logging::init(&log, "DAEDALUS_LOG") {
                eprintln!("daedalus: logging to {}: {err}", log.display());
            }
        }
        Err(err) => eprintln!("daedalus: {err:#}"),
    }
    tracing::info!("daedalus {} starting", env!("CARGO_PKG_VERSION"));
    install_panic_hook();
    Application::new()
        .with_assets(daedalus_ui::Assets)
        .run(|cx: &mut App| {
            daedalus_ui::init(cx);
            actions::init(cx);
            daedalus_term::init(cx);
            daedalus_editor::init(cx);

            let mut path = workspace_path();
            let mut workspace = daedalus_workspace::load(&path).unwrap_or_else(|err| {
                if daedalus_workspace::is_corrupt(&err) {
                    match daedalus_workspace::set_aside(&path) {
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
    let recovery = daedalus_proto::recovery_dir().ok();
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // A worker thread's panic leaves the window running, and the IDE server with it.
        if std::thread::current().name() == Some("main") {
            ide::on_panic();
        }
        let kept = recovery
            .as_deref()
            .map(daedalus_editor::recovery::write_dirty);
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!("{info}\nunsaved files kept: {kept:?}\n{backtrace}");
        default(info);
    }));
}

fn workspace_path() -> PathBuf {
    daedalus_proto::data_dir()
        .expect("application support directory")
        .join("workspace.json")
}
