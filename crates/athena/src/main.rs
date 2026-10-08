mod actions;
mod app_socket;
mod cli;
mod shell;
mod system_notify;

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
    Application::new()
        .with_assets(athena_ui::Assets)
        .run(|cx: &mut App| {
            athena_ui::init(cx);
            actions::init(cx);
            athena_term::init(cx);
            athena_editor::init(cx);

            let path = workspace_path();
            let mut workspace = athena_workspace::load(&path).unwrap_or_else(|err| {
                eprintln!("athena: {err:#}; starting with an empty workspace");
                let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
                Workspace::default()
            });
            workspace.prune_missing();

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
            cx.activate(true);
        });
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
