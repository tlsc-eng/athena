mod actions;
mod app_socket;
mod claude_hooks;
mod cli;
mod mcp;
mod procinfo;
mod shell;
mod system_notify;
mod usage;

use std::path::PathBuf;

use athena_workspace::{WindowMode, WindowState, Workspace};
use gpui::{
    App, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, prelude::*, px,
    size,
};

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

            // For automation: run the window's logic without showing it or taking focus.
            let hidden = std::env::var_os("ATHENA_HIDDEN").is_some();
            let window_bounds = restore_bounds(workspace.window, cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(window_bounds),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Athena".into()),
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(12.), px(12.))),
                    }),
                    window_min_size: Some(size(px(640.), px(400.))),
                    show: !hidden,
                    focus: !hidden,
                    ..Default::default()
                },
                |window, cx| {
                    cx.new(|cx| {
                        let mut shell = shell::Shell::new(workspace, path, window, cx);
                        if let Some(folder) = cli::startup_folder() {
                            shell.open_folder(folder, cx);
                        }
                        shell
                    })
                },
            )
            .expect("open main window");
            cx.on_window_closed(|cx| cx.quit()).detach();
            if !hidden {
                cx.activate(true);
            }
        });
}

/// Logs a panic to app.log and keeps copies of unsaved files before the default hook runs.
fn install_panic_hook() {
    let recovery = athena_proto::recovery_dir().ok();
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
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

/// Saved bounds are used only if they still land on a connected display.
fn restore_bounds(saved: Option<WindowState>, cx: &App) -> WindowBounds {
    let fallback = || WindowBounds::Windowed(Bounds::centered(None, size(px(1280.), px(820.)), cx));
    let Some(s) = saved else { return fallback() };
    let bounds = Bounds::new(point(px(s.x), px(s.y)), size(px(s.width), px(s.height)));
    if !cx.displays().iter().any(|d| d.bounds().intersects(&bounds)) {
        return fallback();
    }
    match s.mode {
        WindowMode::Windowed => WindowBounds::Windowed(bounds),
        WindowMode::Maximized => WindowBounds::Maximized(bounds),
        WindowMode::Fullscreen => WindowBounds::Fullscreen(bounds),
    }
}
