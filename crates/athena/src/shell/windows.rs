//! The app's windows: each has its own Shell and projects; the daemon's notices, the app socket,
//! the Claude Code IDE server and workspace.json are shared and handed to the right window here.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use athena_proto::{AppMsg, AppReply, Notice, PaneId};
use athena_workspace::{Project, WindowMode, WindowState, Workspace};
use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, Context, Global, Task, TitlebarOptions, Window,
    WindowBounds, WindowHandle, WindowOptions, point, px, size,
};

use super::Shell;
use super::item::ItemView;
use super::notices::{self, Log};
use crate::app_socket::Request;
use crate::ide;

/// Where a new window lands relative to the one it was opened from, as macOS cascades them.
const CASCADE: f32 = 28.;

pub(super) struct Windows {
    path: PathBuf,
    notices_path: PathBuf,
    hidden: bool,
    open: Vec<Open>,
    /// The window the user was last in, which commands from outside act on.
    focused: Option<AnyWindowHandle>,
    parked: Vec<Project>,
    app: AppFields,
    pub notices: Rc<RefCell<Log>>,
    ide: Option<ide::Server>,
    ide_clients: HashMap<u64, Option<i32>>,
    _ide_events: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
}

impl Global for Windows {}

struct Open {
    handle: WindowHandle<Shell>,
    /// The window's workspace as it last saved it.
    saved: Workspace,
}

/// The workspace fields every window shares and that settings.json does not carry.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct AppFields {
    pub recent: Vec<PathBuf>,
    pub usage_indicator: bool,
    pub font_zoom: i32,
    pub zoom_level: i32,
}

impl AppFields {
    pub fn of(workspace: &Workspace) -> Self {
        Self {
            recent: workspace.recent.clone(),
            usage_indicator: workspace.usage_indicator,
            font_zoom: workspace.ui.font_zoom,
            zoom_level: workspace.ui.zoom_level,
        }
    }
}

/// Opens a window for each saved one and starts what the whole app shares.
pub fn start(path: PathBuf, workspace: Workspace, folder: Option<PathBuf>, cx: &mut App) {
    let (windows, parked) = workspace.into_windows();
    let notices_path = path.with_file_name("notifications.json");
    cx.set_global(Windows {
        notices: Rc::new(RefCell::new(Log::load(&notices_path))),
        notices_path,
        path,
        // For automation: run the windows' logic without showing them or taking focus.
        hidden: std::env::var_os("ATHENA_HIDDEN").is_some(),
        open: Vec::new(),
        focused: None,
        parked,
        app: AppFields::of(&windows[0]),
        ide: None,
        ide_clients: HashMap::new(),
        _ide_events: None,
        _tasks: Vec::new(),
    });
    for workspace in windows {
        open_window(workspace, None, cx);
    }
    // The first window started the IDE server before the others' projects were known.
    refresh_ide_folders(cx);
    if let Some(folder) = folder
        && let Some(handle) = focused(cx)
    {
        let _ = handle.update(cx, |shell, _, cx| shell.open_folder(folder, cx));
    }
    let tasks = vec![
        serve_app_socket(cx),
        notices::subscribe(cx),
        route_banner_clicks(cx),
    ];
    cx.global_mut::<Windows>()._tasks = tasks;
    cx.on_action(|_: &crate::actions::NewWindow, cx| new_window(None, None, cx));
    cx.on_window_closed(|cx| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
    if !cx.global::<Windows>().hidden {
        cx.activate(true);
    }
}

pub(super) fn workspace_path(cx: &App) -> PathBuf {
    cx.global::<Windows>().path.clone()
}

/// Opens a window showing `workspace`; `near` cascades it from that window's place.
fn open_window(
    mut workspace: Workspace,
    near: Option<AnyWindowHandle>,
    cx: &mut App,
) -> Option<WindowHandle<Shell>> {
    let hidden = cx.global::<Windows>().hidden;
    let bounds = match (workspace.window, near) {
        (None, Some(near)) => cascade(near, cx),
        _ => None,
    };
    let bounds = bounds.unwrap_or_else(|| restore_bounds(workspace.window, cx));
    workspace.window = Some(super::window_state(bounds));
    let saved = workspace.clone();
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(bounds),
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
        |window, cx| cx.new(|cx| Shell::new(workspace, window, cx)),
    );
    let handle = match opened {
        Ok(handle) => handle,
        Err(e) => {
            tracing::error!("could not open a window: {e:#}");
            return None;
        }
    };
    let windows = cx.global_mut::<Windows>();
    windows.open.push(Open { handle, saved });
    if windows.focused.is_none() || !hidden {
        windows.focused = Some(handle.into());
    }
    Some(handle)
}

/// Read from the saved state, as the window asking for a new one is busy dispatching that.
fn cascade(near: AnyWindowHandle, cx: &App) -> Option<WindowBounds> {
    let s = cx
        .global::<Windows>()
        .open
        .iter()
        .find(|o| AnyWindowHandle::from(o.handle) == near)?
        .saved
        .window?;
    Some(WindowBounds::Windowed(Bounds::new(
        point(px(s.x + CASCADE), px(s.y + CASCADE)),
        size(px(s.width), px(s.height)),
    )))
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

/// The open windows, in the order they were opened.
fn handles(cx: &App) -> Vec<WindowHandle<Shell>> {
    cx.global::<Windows>()
        .open
        .iter()
        .map(|o| o.handle)
        .collect()
}

/// The window commands from outside act on: the one last in use, else the first.
fn focused(cx: &App) -> Option<WindowHandle<Shell>> {
    let windows = cx.global::<Windows>();
    windows
        .open
        .iter()
        .find(|o| Some(AnyWindowHandle::from(o.handle)) == windows.focused)
        .or(windows.open.first())
        .map(|o| o.handle)
}

pub(super) fn set_focused(window: AnyWindowHandle, cx: &mut App) {
    cx.global_mut::<Windows>().focused = Some(window);
}

pub(super) fn is_focused(window: AnyWindowHandle, cx: &App) -> bool {
    focused(cx).is_some_and(|h| AnyWindowHandle::from(h) == window)
}

/// Keeps `workspace` as the window's latest state and writes the file for every window.
pub(super) fn store(window: AnyWindowHandle, workspace: Workspace, cx: &mut App) {
    let windows = cx.global_mut::<Windows>();
    // A closed window's late save would bring back its projects and its stale Open Recent.
    let Some(open) = windows
        .open
        .iter_mut()
        .find(|o| AnyWindowHandle::from(o.handle) == window)
    else {
        return;
    };
    open.saved = workspace.clone();
    let all: Vec<Workspace> = windows.open.iter().map(|o| o.saved.clone()).collect();
    let file = Workspace::join(&workspace, &all, &windows.parked);
    if let Err(err) = athena_workspace::save(&windows.path, &file) {
        tracing::error!("could not save the workspace: {err:#}");
    }
    if let Err(err) = notices::save(&windows.notices_path, &windows.notices.borrow().list) {
        tracing::error!("could not save notifications: {err:#}");
    }
    cx.defer(refresh_ide_folders);
}

/// Keeps the IDE lock file's folders in step with every window's projects.
pub(super) fn refresh_ide_folders(cx: &mut App) {
    let folders = open_roots(None, cx);
    if let Some(server) = cx.global_mut::<Windows>().ide.as_mut() {
        server.set_folders(folders);
    }
}

/// The roots open in every window but `except`, as they are now; a window in the middle of an
/// update gives those it last saved.
fn open_roots(except: Option<AnyWindowHandle>, cx: &App) -> Vec<PathBuf> {
    let roots =
        |w: &Workspace| -> Vec<PathBuf> { w.projects.iter().map(|p| p.root.clone()).collect() };
    cx.global::<Windows>()
        .open
        .iter()
        .filter(|o| Some(AnyWindowHandle::from(o.handle)) != except)
        .flat_map(|o| match o.handle.read(cx) {
            Ok(shell) => roots(&shell.workspace),
            Err(_) => roots(&o.saved),
        })
        .collect()
}

/// Hands changed app-wide fields to the other windows, so none of them saves them back.
pub(super) fn publish(window: Option<AnyWindowHandle>, fields: AppFields, cx: &mut App) {
    let windows = cx.global_mut::<Windows>();
    if windows.app == fields {
        return;
    }
    windows.app = fields.clone();
    let orphaned = athena_workspace::retain_parked(&mut windows.parked, &fields.recent);
    athena_term::kill_sessions(orphaned);
    let others: Vec<WindowHandle<Shell>> = windows
        .open
        .iter()
        .map(|o| o.handle)
        .filter(|h| Some(AnyWindowHandle::from(*h)) != window)
        .collect();
    cx.defer(move |cx| {
        for handle in others {
            let fields = fields.clone();
            let _ = handle.update(cx, |shell, window, cx| {
                shell.adopt_app_fields(fields, window, cx)
            });
        }
    });
}

/// The window other than `except` that has `root` open.
pub(super) fn holder_of(
    root: &Path,
    except: AnyWindowHandle,
    cx: &App,
) -> Option<WindowHandle<Shell>> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    handles(cx)
        .into_iter()
        .filter(|h| AnyWindowHandle::from(*h) != except)
        .find(|h| h.read(cx).is_ok_and(|s| s.has_root(&root)))
}

/// Brings another window forward on its project at `root`.
pub(super) fn focus_project(handle: WindowHandle<Shell>, root: PathBuf, cx: &mut App) {
    cx.defer(move |cx| {
        let _ = handle.update(cx, |shell, window, cx| {
            window.activate_window();
            shell.open_folder(root, cx);
        });
    });
}

/// The project a window closed earlier left running at `root`, to reopen with its tabs.
pub(super) fn take_parked(root: &Path, cx: &mut App) -> Option<Project> {
    athena_workspace::take_parked(&mut cx.global_mut::<Windows>().parked, root)
}

/// Roots open in windows other than `except`, for the IDE server's lock file and breakpoints.
pub(super) fn other_roots(except: AnyWindowHandle, cx: &App) -> Vec<PathBuf> {
    open_roots(Some(except), cx)
}

/// Opens a window: empty, or showing `project` (taken from another window or picked).
pub(super) fn new_window(project: Option<Project>, near: Option<AnyWindowHandle>, cx: &mut App) {
    let windows = cx.global::<Windows>();
    let app = windows.app.clone();
    // Preferences come from a window's saved copy, as a new one would otherwise save defaults.
    let base = focused(cx)
        .and_then(|f| windows.open.iter().find(|o| o.handle == f))
        .map(|o| o.saved.clone())
        .unwrap_or_default();
    let mut workspace = Workspace {
        projects: Vec::new(),
        active: None,
        window: None,
        ui: athena_workspace::UiState::default(),
        recent: app.recent,
        usage_indicator: app.usage_indicator,
        windows: Vec::new(),
        parked: Vec::new(),
        ..base
    };
    workspace.ui.font_zoom = app.font_zoom;
    workspace.ui.zoom_level = app.zoom_level;
    if let Some(project) = project {
        workspace.adopt_project(project);
    }
    let near = near.or_else(|| focused(cx).map(Into::into));
    let projects = workspace.projects.clone();
    match open_window(workspace, near, cx) {
        Some(handle) => {
            let _ = handle.update(cx, |shell, window, cx| {
                shell.open_views(cx);
                shell.schedule_save(cx);
                shell.save_now(cx);
                window.activate_window();
            });
        }
        // Parked rather than dropped, so its tabs and shells can still be reopened.
        None => park_projects(None, projects, cx),
    }
}

/// Moves every other window's projects into `into` and closes those windows.
pub(super) fn merge_into(into: AnyWindowHandle, cx: &mut App) {
    let Some(target) = handles(cx)
        .into_iter()
        .find(|h| AnyWindowHandle::from(*h) == into)
    else {
        return;
    };
    let mut moved = Vec::new();
    let mut kept_alive = Vec::new();
    for handle in handles(cx) {
        if handle == target {
            continue;
        }
        let taken = handle.update(cx, |shell, _, cx| shell.detach_all(cx));
        if let Ok((projects, views)) = taken {
            moved.extend(projects);
            kept_alive.extend(views);
        }
        forget(handle.into(), cx);
        let _ = handle.update(cx, |_, window, _| window.remove_window());
    }
    let _ = target.update(cx, |shell, window, cx| {
        shell.adopt_projects(moved, cx);
        shell.open_views(cx);
        shell.save_now(cx);
        window.activate_window();
    });
    drop(kept_alive);
}

fn forget(window: AnyWindowHandle, cx: &mut App) {
    let windows = cx.global_mut::<Windows>();
    windows
        .open
        .retain(|o| AnyWindowHandle::from(o.handle) != window);
    if windows.focused == Some(window) {
        windows.focused = windows.open.first().map(|o| o.handle.into());
    }
}

/// Whether `window` is the only one left, whose closing quits as it always has.
pub(super) fn is_last(cx: &App) -> bool {
    cx.global::<Windows>().open.len() <= 1
}

/// Closes a window whose unsaved files are settled; its projects are parked with their shells
/// still running, and go to Open Recent.
pub(super) fn close(window: AnyWindowHandle, projects: Vec<Project>, cx: &mut App) {
    forget(window, cx);
    park_projects(Some(window), projects, cx);
    cx.defer(move |cx| {
        if let Some(next) = focused(cx) {
            let _ = next.update(cx, |shell, _, cx| shell.save_now(cx));
        }
        let _ = window.update(cx, |_, window, _| window.remove_window());
    });
}

fn park_projects(from: Option<AnyWindowHandle>, projects: Vec<Project>, cx: &mut App) {
    let windows = cx.global_mut::<Windows>();
    let mut recent = windows.app.recent.clone();
    athena_workspace::park(&mut windows.parked, &mut recent, projects);
    let fields = AppFields {
        recent,
        ..windows.app.clone()
    };
    publish(from, fields, cx);
}

/// Quits once every window has settled its unsaved files, the window in use asking first.
pub(super) fn quit(cx: &mut App) {
    let mut order = handles(cx);
    if let Some(first) = focused(cx) {
        order.retain(|h| *h != first);
        order.insert(0, first);
    }
    settle_next(order, cx);
}

fn settle_next(mut rest: Vec<WindowHandle<Shell>>, cx: &mut App) {
    if rest.is_empty() {
        // Marked only now, as a Cancel in a later window keeps every window's unsaved files.
        for handle in handles(cx) {
            let _ = handle.update(cx, |shell, _, _| shell.quit_settled = true);
        }
        return cx.quit();
    }
    let handle = rest.remove(0);
    let after = rest.clone();
    let settled = handle.update(cx, |shell, window, cx| {
        shell.settle_for_quit(window, cx, move |cx| settle_next(rest, cx))
    });
    // A window that went away has nothing left to settle.
    if settled.is_err() {
        settle_next(after, cx);
    }
}

pub(super) fn ide(cx: &App) -> Option<&ide::Server> {
    cx.try_global::<Windows>()?.ide.as_ref()
}

pub(super) fn ide_clients(cx: &App) -> HashMap<u64, Option<i32>> {
    cx.try_global::<Windows>()
        .map(|w| w.ide_clients.clone())
        .unwrap_or_default()
}

pub(super) fn set_ide(
    server: ide::Server,
    events: async_channel::Receiver<ide::Event>,
    cx: &mut App,
) {
    let task = cx.spawn(async move |cx| {
        while let Ok(event) = events.recv().await {
            if cx.update(|cx| route_ide(event, cx)).is_err() {
                return;
            }
        }
    });
    let windows = cx.global_mut::<Windows>();
    windows.ide = Some(server);
    windows._ide_events = Some(task);
}

pub(super) fn stop_ide(cx: &mut App) {
    let windows = cx.global_mut::<Windows>();
    windows._ide_events = None;
    windows.ide_clients.clear();
    if let Some(server) = windows.ide.take() {
        server.turn_off();
    }
}

/// Claude Code hears that pending proposals were rejected, and the lock file goes.
pub(super) fn quit_ide(cx: &mut App) {
    if cx.has_global::<Windows>() {
        cx.global_mut::<Windows>().ide = None;
    }
}

/// The project of a Claude Code process running in another window's terminal.
pub(super) fn claude_root_elsewhere(
    pid: i32,
    except: AnyWindowHandle,
    cx: &App,
) -> Option<PathBuf> {
    handles(cx)
        .into_iter()
        .filter(|h| AnyWindowHandle::from(*h) != except)
        .find_map(|h| h.read(cx).ok()?.claude_terminal(pid, cx).map(|(r, _)| r))
}

fn route_ide(event: ide::Event, cx: &mut App) {
    match event {
        ide::Event::OpenDiff {
            key,
            path,
            contents,
        } => {
            let Some(handle) = path_holder(&path, cx).or_else(|| focused(cx)) else {
                return;
            };
            let _ = handle.update(cx, |shell, window, cx| {
                shell.show_proposal(key, path, contents, window, cx)
            });
        }
        ide::Event::CloseDiff { key } => {
            let id = key.id();
            for handle in handles(cx) {
                let _ = handle.update(cx, |shell, window, cx| {
                    shell.ide.proposals.remove(&id);
                    shell.close_proposal_tab(&id, window, cx);
                });
            }
        }
        ide::Event::Diagnostics { path, reply } => {
            let mut all = Vec::new();
            for handle in handles(cx) {
                if let Ok(shell) = handle.read(cx) {
                    all.extend(shell.ide_diagnostics(path.as_deref()));
                }
            }
            let _ = reply.send(all);
        }
        ide::Event::Client { client, pid } => {
            cx.global_mut::<Windows>().ide_clients.insert(client, pid);
            for handle in handles(cx) {
                let _ = handle.update(cx, |shell, _, cx| shell.watch_selection(cx));
            }
        }
        ide::Event::Disconnected { client } => {
            let clients = &mut cx.global_mut::<Windows>().ide_clients;
            clients.remove(&client);
            if clients.is_empty() {
                for handle in handles(cx) {
                    let _ = handle.update(cx, |shell, _, _| shell.ide._selection = None);
                }
            }
        }
    }
}

/// The window whose project holds `path`, the deepest root winning.
fn path_holder(path: &Path, cx: &App) -> Option<WindowHandle<Shell>> {
    handles(cx)
        .into_iter()
        .filter_map(|h| Some((h.read(cx).ok()?.depth_of(path)?, h)))
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, h)| h)
}

fn session_holder(session: PaneId, cx: &App) -> Option<WindowHandle<Shell>> {
    handles(cx)
        .into_iter()
        .find(|h| h.read(cx).is_ok_and(|s| s.find_session(session).is_some()))
}

pub(super) fn route_notice(notice: Notice, cx: &mut App) {
    let holder = notice.pane.and_then(|pane| session_holder(pane, cx));
    if let Some(handle) = holder.or_else(|| focused(cx)) {
        let _ = handle.update(cx, |shell, window, cx| shell.on_notice(notice, window, cx));
    }
}

fn route_banner_clicks(cx: &mut App) -> Task<()> {
    let (clicks, banner_clicks) = async_channel::unbounded::<u64>();
    crate::system_notify::init(clicks);
    cx.spawn(async move |cx| {
        while let Ok(id) = banner_clicks.recv().await {
            let shown = cx.update(|cx| {
                cx.activate(true);
                show_notification(id, None, cx);
            });
            if shown.is_err() {
                return;
            }
        }
    })
}

/// Shows a notification in the window holding its project, else in the window in use.
fn show_notification(id: u64, except: Option<AnyWindowHandle>, cx: &mut App) {
    let project = cx
        .global::<Windows>()
        .notices
        .borrow()
        .project(id)
        .flatten();
    let holder = project.and_then(|root| {
        handles(cx)
            .into_iter()
            .find(|h| h.read(cx).is_ok_and(|s| s.has_root(&root)))
    });
    let Some(handle) = holder.or_else(|| except.is_none().then(|| focused(cx)).flatten()) else {
        return;
    };
    if Some(AnyWindowHandle::from(handle)) == except {
        return;
    }
    let _ = handle.update(cx, |shell, window, cx| {
        window.activate_window();
        shell.open_notification(id, window, cx);
    });
}

/// Opens a notification clicked in `from` whose project another window holds.
pub(super) fn open_notification_elsewhere(id: u64, from: AnyWindowHandle, cx: &mut App) {
    cx.defer(move |cx| show_notification(id, Some(from), cx));
}

/// Which window answers a request from `athena` or the MCP bridge.
#[derive(Debug, PartialEq)]
enum Target {
    Window(usize),
    Every,
}

fn target(
    msg: &AppMsg,
    caller: Option<usize>,
    focused: usize,
    path_holder: impl Fn(&Path) -> Option<usize>,
    session_holder: impl Fn(PaneId) -> Option<usize>,
) -> Target {
    let by_path = |path: &Path| Target::Window(path_holder(path).unwrap_or(focused));
    match msg {
        AppMsg::ListProjects
        | AppMsg::ListTerminals
        | AppMsg::OpenEditors
        | AppMsg::Diagnostics { path: None } => Target::Every,
        AppMsg::Diagnostics { path: Some(path) }
        | AppMsg::OpenFile { path, .. }
        | AppMsg::OpenDiff { path, .. }
        | AppMsg::ReadBuffer { path, .. }
        | AppMsg::DocumentSymbols { path }
        | AppMsg::ClaudeEdited { path, .. } => by_path(path),
        AppMsg::LspDefinition { at } | AppMsg::LspReferences { at } => by_path(&at.path),
        AppMsg::ReadTerminal { session, .. } | AppMsg::RunInTerminal { session, .. } => {
            Target::Window(session_holder(*session).unwrap_or(focused))
        }
        // A folder already open elsewhere is brought forward by the window itself.
        AppMsg::OpenProject { .. } | AppMsg::ActiveFile => Target::Window(focused),
        AppMsg::WhoAmI
        | AppMsg::Identify { .. }
        | AppMsg::ClaudeTodos { .. }
        | AppMsg::ClaudePlan { .. }
        | AppMsg::RunTests { .. }
        | AppMsg::TestResults
        | AppMsg::DebugState => Target::Window(caller.unwrap_or(focused)),
    }
}

/// One answer from every window's; only the window in use has an active project or editor.
fn merge(replies: Vec<(bool, AppReply)>) -> AppReply {
    let mut merged: Option<AppReply> = None;
    for (in_use, reply) in replies {
        let reply = match reply {
            AppReply::Projects(mut list) if !in_use => {
                list.iter_mut().for_each(|p| p.active = false);
                AppReply::Projects(list)
            }
            AppReply::Editors(mut list) if !in_use => {
                list.iter_mut().for_each(|e| e.active = false);
                AppReply::Editors(list)
            }
            other => other,
        };
        merged = Some(match (merged, reply) {
            (None, reply) => reply,
            (Some(AppReply::Projects(mut a)), AppReply::Projects(b)) => {
                a.extend(b);
                AppReply::Projects(a)
            }
            (Some(AppReply::Terminals(mut a)), AppReply::Terminals(b)) => {
                a.extend(b);
                AppReply::Terminals(a)
            }
            (Some(AppReply::Editors(mut a)), AppReply::Editors(b)) => {
                a.extend(b);
                AppReply::Editors(a)
            }
            (Some(AppReply::Diagnostics(mut a)), AppReply::Diagnostics(b)) => {
                a.extend(b);
                AppReply::Diagnostics(a)
            }
            (Some(kept), _) => kept,
        });
    }
    merged.unwrap_or_else(|| AppReply::Error("no Athena window is open".into()))
}

fn serve_app_socket(cx: &mut App) -> Task<()> {
    let requests = crate::app_socket::listen();
    cx.spawn(async move |cx| {
        while let Ok(request) = requests.recv().await {
            if cx.update(|cx| serve(request, cx)).is_err() {
                return;
            }
        }
    })
}

fn serve(request: Request, cx: &mut App) {
    let windows = handles(cx);
    let Some(in_use) = focused(cx).and_then(|f| windows.iter().position(|h| *h == f)) else {
        let _ = request
            .reply
            .send(AppReply::Error("no Athena window is open".into()));
        return;
    };
    let holder = |session| {
        windows
            .iter()
            .position(|h| h.read(cx).is_ok_and(|s| s.find_session(session).is_some()))
    };
    let (caller_at, caller) = request
        .claimed
        .and_then(|claimed| {
            let at = holder(claimed)?;
            let shell = windows[at].read(cx).ok()?;
            Some((
                Some(at),
                shell.verify_caller(Some(claimed), &request.lineage, cx),
            ))
        })
        .unwrap_or((None, None));
    let caller_at = caller.and(caller_at);
    let path_holder = |path: &Path| {
        windows
            .iter()
            .enumerate()
            .filter_map(|(i, h)| Some((h.read(cx).ok()?.depth_of(path)?, i)))
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, i)| i)
    };
    match target(&request.msg, caller_at, in_use, path_holder, holder) {
        Target::Every => {
            let mut replies = Vec::new();
            for (i, handle) in windows.iter().enumerate() {
                let msg = request.msg.clone();
                let reply = handle.update(cx, |shell, window, cx| {
                    shell.handle_app(msg, caller, window, cx)
                });
                if let Ok(reply) = reply {
                    replies.push((i == in_use, reply));
                }
            }
            let _ = request.reply.send(merge(replies));
        }
        Target::Window(i) => {
            let Request { msg, reply, .. } = request;
            let answered = windows[i].update(cx, |shell, window, cx| {
                if let AppMsg::RunInTerminal {
                    session,
                    text,
                    newline,
                } = msg
                {
                    return shell.confirm_run(session, text, newline, caller, reply, window, cx);
                }
                if shell.answer_later(&msg, &reply, cx) {
                    return;
                }
                let _ = reply.send(shell.handle_app(msg, caller, window, cx));
            });
            if answered.is_err() {
                tracing::warn!("a window closed while answering a request");
            }
        }
    }
}

impl Shell {
    pub(super) fn has_root(&self, root: &Path) -> bool {
        self.workspace
            .projects
            .iter()
            .any(|p| p.root == root || p.root.canonicalize().is_ok_and(|r| r == root))
    }

    /// How deep the root of the project holding `path` is, if one of this window's does.
    pub(super) fn depth_of(&self, path: &Path) -> Option<usize> {
        let real = path.canonicalize().ok();
        self.workspace
            .projects
            .iter()
            .filter(|p| {
                path.starts_with(&p.root) || real.as_ref().is_some_and(|r| r.starts_with(&p.root))
            })
            .map(|p| p.root.components().count())
            .max()
    }

    pub(super) fn adopt_app_fields(
        &mut self,
        fields: AppFields,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let usage_was_on = self.workspace.usage_indicator;
        self.workspace.recent = fields.recent.clone();
        self.workspace.usage_indicator = fields.usage_indicator;
        // The window that changed them already put them in force and in settings.json.
        self.workspace.ui.zoom_level = fields.zoom_level;
        self.workspace.ui.font_zoom = fields.font_zoom;
        if fields.usage_indicator && !usage_was_on {
            self.start_usage(window, cx);
        }
        cx.notify();
    }

    /// The red button: closes this window, unless it is the last one, whose closing quits.
    pub(super) fn close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if is_last(cx) {
            return self.quit(window, cx);
        }
        let mut dirty = self.dirty_editors(None, cx);
        if self.autosave_delay().is_some() {
            dirty.retain(|e| !e.update(cx, |e, cx| e.save(cx)));
        }
        self.settle_unsaved(dirty, "closing the window", window, cx, |this, cx| {
            let (projects, _views) = this.detach_all(cx);
            close(this.window_handle, projects, cx);
        });
    }

    /// Takes every project out, with their shells still running, and stops what ran for them.
    pub(super) fn detach_all(&mut self, cx: &mut Context<Self>) -> (Vec<Project>, Vec<ItemView>) {
        let roots: Vec<PathBuf> = self
            .workspace
            .projects
            .iter()
            .map(|p| p.root.clone())
            .collect();
        let mut projects = Vec::new();
        let mut views = Vec::new();
        for root in roots {
            if let Some((project, kept)) = self.detach(&root, cx) {
                projects.push(project);
                views.extend(kept);
            }
        }
        self.tests_quit();
        self.debug_quit();
        (projects, views)
    }

    /// Takes the project at `root` out of this window without closing its tabs' shells; the
    /// editors returned hold their unsaved text until the window taking the project opens them.
    fn detach(&mut self, root: &Path, cx: &mut Context<Self>) -> Option<(Project, Vec<ItemView>)> {
        let index = self
            .workspace
            .projects
            .iter()
            .position(|p| p.root == root)?;
        self.capture_view_states(cx);
        let was_active = self.workspace.active == Some(index);
        let views: Vec<ItemView> = self
            .release_project_items(root)
            .into_iter()
            .filter(|v| matches!(v, ItemView::Editor(_)))
            .collect();
        self.history.forget_root(root);
        self.lsp_project_closed(root);
        self.git_project_closed(root);
        self.debug_project_closed(root, cx);
        self.debug.forget_project(root);
        let project = self.workspace.detach_project(index)?;
        let project = Workspace {
            projects: vec![project],
            ..Workspace::default()
        };
        let project = super::claude_ide::without_proposals(&project)
            .unwrap_or(project)
            .projects
            .remove(0);
        self.rail_from = self.workspace.active.unwrap_or(0);
        if was_active {
            self.zoomed = None;
            self.focus_pending = true;
            self.switch_count += 1;
        }
        self.schedule_save(cx);
        cx.notify();
        Some((project, views))
    }

    /// Takes in projects moved from other windows, keeping the project that was in front.
    pub(super) fn adopt_projects(&mut self, projects: Vec<Project>, cx: &mut Context<Self>) {
        let front = self.workspace.active;
        for project in projects {
            let root = project.root.clone();
            self.workspace.adopt_project(project);
            self.debug.reload_project(&root);
        }
        if front.is_some() {
            self.workspace.active = front;
        }
        self.group_worktrees();
        self.switch_count += 1;
        self.focus_pending = true;
        self.git_kick(cx);
        self.schedule_save(cx);
        cx.notify();
    }

    /// Opens every editor tab now, so a moved file's unsaved text is picked up before the
    /// window it came from lets go of it.
    pub(super) fn open_views(&mut self, cx: &mut Context<Self>) {
        let editors: Vec<(PathBuf, athena_workspace::Item)> = self
            .workspace
            .projects
            .iter()
            .flat_map(|p| {
                p.items()
                    .filter(|i| matches!(i.kind, athena_workspace::ItemKind::Editor { .. }))
                    .map(|i| (p.root.clone(), i.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (root, item) in editors {
            self.item_view(&root, &item, cx);
        }
    }

    pub(super) fn move_to_new_window(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let Some((project, views)) = self.detach(&root, cx) else {
            return;
        };
        let near = self.window_handle;
        cx.defer(move |cx| {
            new_window(Some(project), Some(near), cx);
            drop(views);
        });
    }

    pub(super) fn move_active_to_new_window(&mut self, cx: &mut Context<Self>) {
        if let Some(root) = self.active_root() {
            self.move_to_new_window(root, cx);
        }
    }

    /// Right-click on a rail project.
    pub(super) fn open_rail_menu(
        &mut self,
        index: usize,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.workspace.projects.get(index).map(|p| p.root.clone()) else {
            return;
        };
        let mut items = vec![super::menus::shell_item(
            "Move Project to New Window",
            cx,
            {
                let root = root.clone();
                move |this, _, cx| this.move_to_new_window(root.clone(), cx)
            },
        )];
        if !is_last(cx) {
            items.push(super::menus::shell_item(
                "Merge All Windows",
                cx,
                |this, _, cx| {
                    let into = this.window_handle;
                    cx.defer(move |cx| merge_into(into, cx));
                },
            ));
        }
        items.push(athena_ui::MenuItem::separator());
        items.push(super::menus::shell_item(
            "Close Project",
            cx,
            move |this, w, cx| {
                if let Some(i) = this.workspace.projects.iter().position(|p| p.root == root) {
                    this.switch_to(i, cx);
                    this.close_active_project(w, cx);
                }
            },
        ));
        self.open_context_menu(position, items, window, cx);
    }

    /// Picks a folder and opens it in a new window, or brings forward the window that has it.
    pub(super) fn open_in_new_window(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open in New Window".into()),
        });
        let near = self.window_handle;
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(root) = paths.into_iter().next() else {
                return;
            };
            let _ = cx.update(|cx| {
                let root = root.canonicalize().unwrap_or(root);
                if let Some(holder) = holder_of(&root, near, cx).or_else(|| {
                    handles(cx).into_iter().find(|h| {
                        AnyWindowHandle::from(*h) == near
                            && h.read(cx).is_ok_and(|s| s.has_root(&root))
                    })
                }) {
                    return focus_project(holder, root, cx);
                }
                let project = take_parked(&root, cx).unwrap_or_else(|| Project::new(root));
                new_window(Some(project), Some(near), cx);
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athena_proto::{EditorInfo, ProjectInfo, SourcePosition, TerminalInfo};

    fn route(msg: AppMsg, caller: Option<usize>) -> Target {
        let path_holder = |p: &Path| {
            if p.starts_with("/b") {
                Some(1)
            } else if p.starts_with("/a") {
                Some(0)
            } else {
                None
            }
        };
        target(&msg, caller, 2, path_holder, |s| (s == 7).then_some(1))
    }

    #[test]
    fn requests_go_to_the_window_holding_their_file_session_or_caller() {
        let at = |path: &str| SourcePosition {
            path: path.into(),
            line: 1,
            column: Some(1),
            symbol: None,
        };
        for (msg, expected) in [
            (AppMsg::ListProjects, Target::Every),
            (AppMsg::ListTerminals, Target::Every),
            (AppMsg::OpenEditors, Target::Every),
            (AppMsg::Diagnostics { path: None }, Target::Every),
            (
                AppMsg::Diagnostics {
                    path: Some("/b/x.go".into()),
                },
                Target::Window(1),
            ),
            (
                AppMsg::OpenFile {
                    path: "/a/x.go".into(),
                    line: None,
                },
                Target::Window(0),
            ),
            (
                AppMsg::OpenFile {
                    path: "/elsewhere/x.go".into(),
                    line: None,
                },
                Target::Window(2),
            ),
            (
                AppMsg::LspReferences { at: at("/b/y.ts") },
                Target::Window(1),
            ),
            (
                AppMsg::ReadTerminal {
                    session: 7,
                    lines: 10,
                },
                Target::Window(1),
            ),
            (
                AppMsg::RunInTerminal {
                    session: 8,
                    text: "ls".into(),
                    newline: true,
                },
                Target::Window(2),
            ),
            (AppMsg::OpenProject { path: "/b".into() }, Target::Window(2)),
            (AppMsg::ActiveFile, Target::Window(2)),
            (AppMsg::TestResults, Target::Window(2)),
        ] {
            assert_eq!(route(msg.clone(), None), expected, "{msg:?}");
        }
        assert_eq!(route(AppMsg::WhoAmI, Some(0)), Target::Window(0));
        assert_eq!(route(AppMsg::DebugState, Some(1)), Target::Window(1));
        assert_eq!(
            route(
                AppMsg::RunTests {
                    path: None,
                    name: None
                },
                Some(0)
            ),
            Target::Window(0)
        );
    }

    #[test]
    fn lists_from_every_window_are_joined_with_one_active_project() {
        let project = |root: &str, active| ProjectInfo {
            root: root.into(),
            name: root.into(),
            active,
        };
        let merged = merge(vec![
            (false, AppReply::Projects(vec![project("/a", true)])),
            (
                true,
                AppReply::Projects(vec![project("/b", false), project("/c", true)]),
            ),
        ]);
        assert_eq!(
            merged,
            AppReply::Projects(vec![
                project("/a", false),
                project("/b", false),
                project("/c", true)
            ])
        );

        let editor = |path: &str, active| EditorInfo {
            path: path.into(),
            project: "/a".into(),
            dirty: false,
            active,
        };
        let merged = merge(vec![
            (true, AppReply::Editors(vec![editor("/a/x", true)])),
            (false, AppReply::Editors(vec![editor("/b/y", true)])),
        ]);
        assert_eq!(
            merged,
            AppReply::Editors(vec![editor("/a/x", true), editor("/b/y", false)])
        );

        let terminal = |session| TerminalInfo {
            session,
            project: "/a".into(),
            title: "zsh".into(),
            cwd: None,
            program: None,
            claude: None,
        };
        assert_eq!(
            merge(vec![
                (false, AppReply::Terminals(vec![terminal(Some(1))])),
                (true, AppReply::Terminals(vec![terminal(Some(2))])),
            ]),
            AppReply::Terminals(vec![terminal(Some(1)), terminal(Some(2))])
        );
        assert_eq!(
            merge(vec![]),
            AppReply::Error("no Athena window is open".into())
        );
    }
}
