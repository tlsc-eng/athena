use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use athena_dap::{
    BreakpointStatus, Client, Evaluated, Event, Launch, Scope, SourceBreakpoint, StackFrame,
    Stopped, Thread, Variable,
};
use athena_editor::{
    Breakpoint, DebugTestAt, EditorEvent, EditorView, EnableBreakpointAt, HoverBlock,
    SetBreakpointAt, ToggleBreakpointAt,
};
use athena_proto::{DebugFrameInfo, DebugStateInfo, DebugVariableInfo};
use athena_ui::motion::Opening;
use athena_ui::{InputEvent, TextInput};
use athena_workspace::LinterTrust;
use gpui::{
    Context, Entity, EntityId, PromptLevel, Subscription, Task, WeakEntity, Window, prelude::*,
};
use serde::{Deserialize, Serialize};

use super::Shell;
use super::drawer::DrawerTab;
use super::item::ItemView;
use super::lsp::{CONFIRMED, confirm_buttons, document_key};
use crate::actions;

/// Frames fetched for a stopped thread; deeper ones are rarely what anyone is after.
const STACK_DEPTH: u32 = 50;
const CONSOLE_LINES: usize = 5000;
/// Expanded variables are fetched again after each stop down to this depth.
const EXPAND_DEPTH: usize = 6;
/// What debug_state hands Claude, so a deep stack or a huge struct cannot flood it.
const CLAUDE_FRAMES: usize = 20;
const CLAUDE_LOCALS: usize = 50;
const CLAUDE_VALUE: usize = 200;

/// A breakpoint as breakpoints.json keeps it, with a one-based line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedBreakpoint {
    /// Relative to the project when inside it.
    path: PathBuf,
    line: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    condition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hit_condition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    log_message: Option<String>,
    #[serde(default = "enabled", skip_serializing_if = "is_true")]
    enabled: bool,
}

fn enabled() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

#[derive(Default, Serialize, Deserialize)]
struct SavedProject {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    breakpoints: Vec<SavedBreakpoint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    watch: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    projects: BTreeMap<PathBuf, SavedProject>,
}

/// What to debug once the project's code may run.
#[derive(Clone, Debug)]
enum Target {
    /// The first Go configuration in .vscode/launch.json, else the open file's package.
    Configured,
    Test {
        path: PathBuf,
        line: usize,
    },
    Again(Launch),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Phase {
    Starting,
    Running,
    Stopped(Stopped),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LineKind {
    Output,
    Error,
    Input,
    Result,
    Info,
}

pub(super) struct ConsoleLine {
    pub(super) kind: LineKind,
    pub(super) text: String,
}

pub(super) struct Session {
    client: Rc<Client>,
    pub(super) root: PathBuf,
    pub(super) launch: Launch,
    pub(super) phase: Phase,
    pub(super) threads: Vec<Thread>,
    pub(super) frames: Vec<StackFrame>,
    pub(super) thread: Option<i64>,
    pub(super) frame: Option<i64>,
    pub(super) scopes: Vec<Scope>,
    /// Children by variablesReference, valid until the program runs again.
    pub(super) children: HashMap<i64, Vec<Variable>>,
    pub(super) watches: Vec<(String, Result<Evaluated, String>)>,
    /// What the adapter made of each file's breakpoints, in the order they were sent.
    verified: HashMap<PathBuf, Vec<(usize, BreakpointStatus)>>,
    /// Counts stops and resumes, so a reply about an earlier stop is dropped.
    pub(super) generation: u64,
    stopping: bool,
    /// A step is under way; Delve reports it continued, but the marks stay until it stops.
    stepping: bool,
    pub(super) opened: Opening,
    _events: Task<()>,
}

impl Session {
    pub(super) fn client(&self) -> Rc<Client> {
        self.client.clone()
    }

    fn stopped(&self) -> Option<&Stopped> {
        match &self.phase {
            Phase::Stopped(s) => Some(s),
            _ => None,
        }
    }

    fn top_frame(&self) -> Option<&StackFrame> {
        let id = self.frame?;
        self.frames.iter().find(|f| f.id == id)
    }
}

#[derive(Default)]
pub(super) struct DebugState {
    saved_path: Option<PathBuf>,
    /// Breakpoints by file; a file's open editors move them as it is edited.
    breakpoints: BTreeMap<PathBuf, Vec<Breakpoint>>,
    pub(super) watch: HashMap<PathBuf, Vec<String>>,
    pub(super) session: Option<Session>,
    pub(super) console: Vec<ConsoleLine>,
    /// The last console line has no newline yet, so the next output continues it.
    console_open: bool,
    pub(super) console_scroll: gpui::ScrollHandle,
    /// New lines arrived, so the console scrolls to them on its next frame.
    pub(super) console_follow: std::cell::Cell<bool>,
    pub(super) console_input: Option<Entity<TextInput>>,
    pub(super) watch_input: Option<Entity<TextInput>>,
    /// Variables shown expanded, by their scope and names, kept across stops as VS Code keeps them.
    pub(super) expanded: HashSet<String>,
    asking: bool,
    editors: HashMap<EntityId, (WeakEntity<EditorView>, Subscription)>,
    _inputs: Vec<Subscription>,
}

impl DebugState {
    /// Loads the breakpoints and watch expressions kept beside `workspace` (workspace.json).
    pub(super) fn load(workspace: &Path) -> Self {
        let path = workspace.with_file_name("breakpoints.json");
        let saved: Saved = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let mut state = Self {
            saved_path: Some(path),
            ..Self::default()
        };
        for (root, project) in saved.projects {
            for b in project.breakpoints {
                let path = match b.path.is_absolute() {
                    true => b.path.clone(),
                    false => root.join(&b.path),
                };
                state
                    .breakpoints
                    .entry(document_key(&path))
                    .or_default()
                    .push(Breakpoint {
                        condition: b.condition,
                        hit_condition: b.hit_condition,
                        log_message: b.log_message,
                        enabled: b.enabled,
                        ..Breakpoint::at(b.line.saturating_sub(1))
                    });
            }
            if !project.watch.is_empty() {
                state.watch.insert(root, project.watch);
            }
        }
        state
    }

    fn save(&self, roots: &[PathBuf]) {
        let Some(path) = &self.saved_path else {
            return;
        };
        let mut saved = Saved::default();
        for (file, list) in &self.breakpoints {
            let root = roots
                .iter()
                .find(|r| file.starts_with(document_key(r)))
                .cloned()
                .unwrap_or_else(|| PathBuf::from("/"));
            let rel = file
                .strip_prefix(document_key(&root))
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| file.clone());
            let project = saved.projects.entry(root).or_default();
            project
                .breakpoints
                .extend(list.iter().map(|b| SavedBreakpoint {
                    path: rel.clone(),
                    line: b.line + 1,
                    condition: b.condition.clone(),
                    hit_condition: b.hit_condition.clone(),
                    log_message: b.log_message.clone(),
                    enabled: b.enabled,
                }));
        }
        for (root, watch) in &self.watch {
            saved.projects.entry(root.clone()).or_default().watch = watch.clone();
        }
        match serde_json::to_vec_pretty(&saved) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(path, bytes) {
                    tracing::warn!("could not save breakpoints: {e}");
                }
            }
            Err(e) => tracing::warn!("could not encode breakpoints: {e}"),
        }
    }
}

/// The source breakpoints of one file, as the adapter wants them.
fn source_breakpoints(list: &[Breakpoint]) -> Vec<SourceBreakpoint> {
    list.iter()
        .filter(|b| b.enabled)
        .map(|b| SourceBreakpoint {
            line: b.line as u32 + 1,
            condition: b.condition.clone(),
            hit_condition: b.hit_condition.clone(),
            log_message: b.log_message.clone(),
        })
        .collect()
}

/// The frame a stop shows first: the top one with source, as the program's own code is the
/// interesting part when it stops inside the runtime.
fn first_frame_with_source(frames: &[StackFrame]) -> Option<&StackFrame> {
    frames
        .iter()
        .find(|f| f.path.as_ref().is_some_and(|p| p.exists()))
        .or(frames.first())
}

fn shorten(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

/// The key a variable's expansion is remembered by.
pub(super) fn variable_key(parent: &str, name: &str) -> String {
    format!("{parent}\u{1f}{name}")
}

/// Binds the debugging commands and the gutter's breakpoint actions on the shell's root element.
pub(super) fn bind_debug_actions(el: gpui::Div, cx: &mut Context<Shell>) -> gpui::Div {
    el.on_action(
        cx.listener(|this, _: &actions::StartDebugging, window, cx| {
            this.start_or_continue(window, cx)
        }),
    )
    .on_action(cx.listener(|this, _: &actions::StopDebugging, _, cx| this.stop_debugging(cx)))
    .on_action(
        cx.listener(|this, _: &actions::RestartDebugging, window, cx| {
            this.restart_debugging(window, cx)
        }),
    )
    .on_action(cx.listener(|this, _: &actions::PauseDebugging, _, cx| this.pause_debugging(cx)))
    .on_action(cx.listener(|this, _: &actions::StepOver, _, cx| this.step("next", cx)))
    .on_action(cx.listener(|this, _: &actions::StepInto, _, cx| this.step("stepIn", cx)))
    .on_action(cx.listener(|this, _: &actions::StepOut, _, cx| this.step("stepOut", cx)))
    .on_action(cx.listener(|this, _: &actions::ShowDebug, _, cx| {
        this.toggle_drawer_tab(DrawerTab::Debug, cx)
    }))
    .on_action(
        cx.listener(|this, _: &actions::DebugTestAtCursor, window, cx| {
            this.debug_test_at_cursor(window, cx)
        }),
    )
    .on_action(
        cx.listener(|this, _: &actions::OpenLaunchConfig, window, cx| {
            this.open_launch_config(window, cx)
        }),
    )
    .on_action(
        cx.listener(|this, _: &actions::RemoveAllBreakpoints, _, cx| {
            this.remove_all_breakpoints(cx)
        }),
    )
    .on_action(cx.listener(|this, a: &ToggleBreakpointAt, _, cx| {
        this.toggle_breakpoint(a.path.clone(), a.line, cx)
    }))
    .on_action(cx.listener(|this, a: &SetBreakpointAt, _, cx| {
        let mut b = Breakpoint::at(a.line);
        b.condition = a.condition.clone();
        b.hit_condition = a.hit_condition.clone();
        b.log_message = a.log_message.clone();
        this.put_breakpoint(a.path.clone(), b, cx)
    }))
    .on_action(cx.listener(|this, a: &EnableBreakpointAt, _, cx| {
        this.enable_breakpoint(a.path.clone(), a.line, a.enabled, cx)
    }))
    .on_action(cx.listener(|this, a: &DebugTestAt, window, cx| {
        this.start_debugging(
            Target::Test {
                path: a.path.clone(),
                line: a.line,
            },
            window,
            cx,
        )
    }))
}

impl Shell {
    /// Hands a newly opened editor its file's breakpoints and the paused line, and follows its
    /// saves, which settle where its edits moved the breakpoints.
    pub(super) fn debug_opened(&mut self, view: &Entity<EditorView>, cx: &mut Context<Self>) {
        let subscription = cx.subscribe(view, |this, view, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::Saved) {
                let doc = document_key(view.read(cx).path());
                this.settle_breakpoints(&doc, cx);
                this.send_breakpoints(&doc, cx);
            }
        });
        let weak = view.downgrade();
        self.debug
            .editors
            .retain(|_, (editor, _)| editor.upgrade().is_some());
        self.debug
            .editors
            .insert(view.entity_id(), (weak, subscription));
        // The view is not among the shell's items yet, so it is handed its list directly.
        let doc = document_key(view.read(cx).path());
        let list = self.marked_breakpoints(&doc, Some(self.file_breakpoints(&doc, cx)));
        let stopped = self.stopped_line_in(&doc);
        let paused = self.paused();
        view.update(cx, |v, cx| {
            v.set_breakpoints(list, cx);
            v.set_debug_paused(paused);
            v.set_stopped_line(stopped, cx);
        });
    }

    fn paused(&self) -> bool {
        self.debug
            .session
            .as_ref()
            .is_some_and(|s| s.stopped().is_some())
    }

    /// The paused line in `doc`, and whether it is the top frame's.
    fn stopped_line_in(&self, doc: &Path) -> Option<(usize, bool)> {
        let session = self.debug.session.as_ref()?;
        session.stopped()?;
        let frame = session.top_frame()?;
        let path = frame.path.as_ref()?;
        let top = session.frames.first().is_some_and(|f| f.id == frame.id);
        (document_key(path) == doc).then(|| (frame.line.saturating_sub(1) as usize, top))
    }

    /// A file's breakpoints as its open editor has moved them, else as last stored.
    fn file_breakpoints(&self, doc: &Path, cx: &Context<Self>) -> Vec<Breakpoint> {
        match self.editors_showing(doc, cx).first() {
            Some(editor) => editor.read(cx).breakpoints().to_vec(),
            None => self.debug.breakpoints.get(doc).cloned().unwrap_or_default(),
        }
    }

    /// Takes the lines an open editor moved the file's breakpoints to as the stored ones.
    fn settle_breakpoints(&mut self, doc: &Path, cx: &mut Context<Self>) {
        let list = self.file_breakpoints(doc, cx);
        let stored = self.debug.breakpoints.get(doc).cloned().unwrap_or_default();
        if list
            .iter()
            .map(|b| b.line)
            .ne(stored.iter().map(|b| b.line))
        {
            self.set_file_breakpoints(doc, list, cx);
        }
    }

    fn set_file_breakpoints(&mut self, doc: &Path, list: Vec<Breakpoint>, cx: &mut Context<Self>) {
        let mut list: Vec<Breakpoint> = list
            .into_iter()
            .map(|b| Breakpoint {
                unverified: false,
                ..b
            })
            .collect();
        list.sort_by_key(|b| b.line);
        list.dedup_by_key(|b| b.line);
        match list.is_empty() {
            true => self.debug.breakpoints.remove(doc),
            false => self.debug.breakpoints.insert(doc.to_path_buf(), list),
        };
        let roots: Vec<PathBuf> = self
            .workspace
            .projects
            .iter()
            .map(|p| p.root.clone())
            .collect();
        self.debug.save(&roots);
        self.push_breakpoints(doc, cx);
        cx.notify();
    }

    /// Shows a file's breakpoints in its editors, hollow where the debugger could not place them.
    fn push_breakpoints(&mut self, doc: &Path, cx: &mut Context<Self>) {
        let list = self.marked_breakpoints(doc, self.debug.breakpoints.get(doc).cloned());
        for editor in self.editors_showing(doc, cx) {
            editor.update(cx, |e, cx| e.set_breakpoints(list.clone(), cx));
        }
    }

    /// `list` (else none) with the ones the running session could not place marked.
    fn marked_breakpoints(&self, doc: &Path, list: Option<Vec<Breakpoint>>) -> Vec<Breakpoint> {
        let mut list = list.unwrap_or_default();
        if let Some(statuses) = self
            .debug
            .session
            .as_ref()
            .and_then(|s| s.verified.get(doc))
        {
            for b in &mut list {
                b.unverified = statuses
                    .iter()
                    .any(|(line, status)| *line == b.line && !status.verified);
            }
        }
        list
    }

    pub(super) fn toggle_breakpoint(&mut self, path: PathBuf, line: usize, cx: &mut Context<Self>) {
        let doc = document_key(&path);
        let mut list = self.file_breakpoints(&doc, cx);
        match list.iter().position(|b| b.line == line) {
            Some(at) => {
                list.remove(at);
            }
            None => list.push(Breakpoint::at(line)),
        }
        self.set_file_breakpoints(&doc, list, cx);
        self.send_breakpoints(&doc, cx);
    }

    fn put_breakpoint(&mut self, path: PathBuf, b: Breakpoint, cx: &mut Context<Self>) {
        let doc = document_key(&path);
        let mut list = self.file_breakpoints(&doc, cx);
        list.retain(|x| x.line != b.line);
        list.push(b);
        self.set_file_breakpoints(&doc, list, cx);
        self.send_breakpoints(&doc, cx);
    }

    pub(super) fn enable_breakpoint(
        &mut self,
        path: PathBuf,
        line: usize,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let doc = document_key(&path);
        let mut list = self.file_breakpoints(&doc, cx);
        for b in list.iter_mut().filter(|b| b.line == line) {
            b.enabled = enabled;
        }
        self.set_file_breakpoints(&doc, list, cx);
        self.send_breakpoints(&doc, cx);
    }

    /// Every breakpoint of the active project, by file then line.
    pub(super) fn project_breakpoints(&self) -> Vec<(PathBuf, Breakpoint)> {
        let Some(root) = self.active_root().map(|r| document_key(&r)) else {
            return Vec::new();
        };
        self.debug
            .breakpoints
            .iter()
            .filter(|(path, _)| path.starts_with(&root))
            .flat_map(|(path, list)| list.iter().map(|b| (path.clone(), b.clone())))
            .collect()
    }

    fn remove_all_breakpoints(&mut self, cx: &mut Context<Self>) {
        let files: Vec<PathBuf> = self
            .project_breakpoints()
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        for doc in files {
            self.set_file_breakpoints(&doc, Vec::new(), cx);
            self.send_breakpoints(&doc, cx);
        }
    }

    /// Tells a running session about a file's breakpoints, then marks the ones it could not place.
    fn send_breakpoints(&mut self, doc: &Path, cx: &mut Context<Self>) {
        let Some(session) = self.debug.session.as_ref() else {
            return;
        };
        if session.phase == Phase::Starting || !doc.starts_with(document_key(&session.root)) {
            return;
        }
        let client = session.client.clone();
        let list = self.debug.breakpoints.get(doc).cloned().unwrap_or_default();
        let doc = doc.to_path_buf();
        cx.spawn(async move |this, cx| {
            let sent = source_breakpoints(&list);
            let statuses = client.set_breakpoints(&doc, &sent).await;
            let _ = this.update(cx, |this, cx| this.note_verified(&doc, &sent, statuses, cx));
        })
        .detach();
    }

    fn note_verified(
        &mut self,
        doc: &Path,
        sent: &[SourceBreakpoint],
        statuses: Result<Vec<BreakpointStatus>, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.debug.session.as_mut() else {
            return;
        };
        match statuses {
            Ok(statuses) => {
                let lines = sent.iter().map(|b| b.line as usize - 1);
                session
                    .verified
                    .insert(doc.to_path_buf(), lines.zip(statuses).collect());
            }
            Err(why) => self.console_line(
                LineKind::Error,
                format!("Could not set breakpoints in {}: {why}", doc.display()),
            ),
        }
        self.push_breakpoints(doc, cx);
    }

    /// F5: continues a paused program, else starts debugging.
    fn start_or_continue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.debug.session.as_ref().map(|s| &s.phase) {
            Some(Phase::Stopped(_)) => self.continue_debugging(cx),
            Some(_) => {}
            None => self.start_debugging(Target::Configured, window, cx),
        }
    }

    fn debug_test_at_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.focused_editor() else {
            return;
        };
        let (path, line) = {
            let e = editor.read(cx);
            (e.path().to_path_buf(), e.cursor_line())
        };
        self.start_debugging(Target::Test { path, line }, window, cx);
    }

    /// Starts debugging once the project's code may run: debugging builds and runs it.
    fn start_debugging(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        if self.debug.session.is_some() {
            return self.transient_notice(
                "Already debugging",
                "Stop the running session (Shift+F5) before starting another.",
                cx,
            );
        }
        let root = match &target {
            Target::Test { path, .. } => self
                .workspace
                .projects
                .iter()
                .map(|p| p.root.clone())
                .find(|r| path.starts_with(r)),
            _ => self.active_root(),
        };
        let Some(root) = root else {
            return;
        };
        let name = self
            .workspace
            .projects
            .iter()
            .find(|p| p.root == root)
            .map(|p| p.name())
            .unwrap_or_default();
        match self.linter_trust(&root) {
            Some(LinterTrust::Allowed) => self.launch_debug(root, target, cx),
            Some(LinterTrust::Denied) => self.transient_notice(
                "Debugging waits for this project to be allowed",
                format!(
                    "Debugging builds and runs {name}'s code, which you chose not to allow. To \
                     allow it, run \"Allow project code\" from the command palette."
                ),
                cx,
            ),
            _ if self.debug.asking => {}
            _ => {
                self.debug.asking = true;
                let answer = window.prompt(
                    PromptLevel::Warning,
                    &format!("Allow {name}'s code to run?"),
                    Some(&format!(
                        "Debugging builds {name} and runs it under Delve on this Mac. Allowing \
                         also lets the linters and TypeScript it installs run, and its settings \
                         choose what language servers run."
                    )),
                    &confirm_buttons("Cancel", "Allow and Debug"),
                    cx,
                );
                cx.spawn_in(window, async move |this, cx| {
                    let answer = answer.await;
                    let _ = this.update(cx, |this, cx| {
                        this.debug.asking = false;
                        if answer == Ok(CONFIRMED) && this.active_root().as_ref() == Some(&root) {
                            this.change_linter_trust(true, cx);
                            this.launch_debug(root, target, cx);
                        }
                    });
                })
                .detach();
            }
        }
    }

    /// What to launch: a test, the first Go configuration in launch.json, or the open file's
    /// package; only called once the project is trusted, as launch.json can name any program.
    fn resolve_target(
        &self,
        root: &Path,
        target: Target,
        cx: &Context<Self>,
    ) -> Result<Launch, String> {
        let file = self
            .focused_editor()
            .map(|e| e.read(cx).path().to_path_buf())
            .filter(|p| p.starts_with(root));
        match target {
            Target::Again(launch) => Ok(launch),
            Target::Test { path, line } => test_launch(&path, line, self, cx),
            Target::Configured => {
                let configs = read_launch_json(root)?;
                let config = match configs.into_iter().next() {
                    Some(config) => config,
                    None if file
                        .as_ref()
                        .is_some_and(|f| f.extension().is_some_and(|e| e == "go")) =>
                    {
                        athena_dap::default_config()
                    }
                    None => athena_dap::LaunchConfig {
                        program: "${workspaceFolder}".into(),
                        ..athena_dap::default_config()
                    },
                };
                config.resolve(&athena_dap::Context {
                    workspace: root,
                    file: file.as_deref(),
                })
            }
        }
    }

    fn launch_debug(&mut self, root: PathBuf, target: Target, cx: &mut Context<Self>) {
        let launch = match self.resolve_target(&root, target, cx) {
            Ok(launch) => launch,
            Err(why) => return self.transient_notice("Can't start debugging", why, cx),
        };
        let Some(dlv) = athena_dap::find_delve() else {
            return self.transient_notice(
                "Delve is not installed",
                format!(
                    "Debugging Go needs Delve on your PATH. Install it with: {}",
                    athena_dap::DELVE_INSTALL_HINT
                ),
                cx,
            );
        };
        // Delve builds what is on disk, so unsaved edits are saved first, as VS Code does.
        let dirty: Vec<Entity<EditorView>> = self
            .items
            .iter()
            .filter(|((r, _), _)| *r == root)
            .filter_map(|(_, v)| match v {
                ItemView::Editor(e) if e.read(cx).is_dirty() => Some(e.clone()),
                _ => None,
            })
            .collect();
        for editor in dirty {
            editor.update(cx, |e, cx| e.save(cx));
        }
        let (client, events) =
            match Client::start(athena_dap::delve_adapter(dlv, launch.cwd.clone())) {
                Ok(started) => started,
                Err(e) => {
                    return self.transient_notice("Can't start Delve", format!("{e:#}"), cx);
                }
            };
        let client = Rc::new(client);
        tracing::info!(program = %launch.program.display(), mode = ?launch.mode, "debugging");
        self.debug.console.clear();
        self.debug.console_open = false;
        self.console_line(
            LineKind::Info,
            format!("Starting {} ({})", launch.name, launch.program.display()),
        );
        let events = cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this
                    .update(cx, |this, cx| this.debug_event(event, cx))
                    .is_err()
                {
                    return;
                }
            }
        });
        let arguments = launch.delve_arguments(&client.scratch().join("debug-bin"));
        self.debug.session = Some(Session {
            client: client.clone(),
            root,
            launch,
            phase: Phase::Starting,
            threads: Vec::new(),
            frames: Vec::new(),
            thread: None,
            frame: None,
            scopes: Vec::new(),
            children: HashMap::new(),
            watches: Vec::new(),
            verified: HashMap::new(),
            generation: 0,
            stopping: false,
            stepping: false,
            opened: Opening::now(),
            _events: events,
        });
        self.show_drawer_tab(DrawerTab::Debug, cx);
        cx.spawn(async move |this, cx| {
            let started = async {
                client.initialize("go").await?;
                client.launch(arguments).await
            };
            if let Err(why) = started.await {
                let _ = this.update(cx, |this, cx| {
                    this.console_line(LineKind::Error, why.clone());
                    if why.contains("debugserver") || why.contains("authoriz") {
                        this.console_line(
                            LineKind::Info,
                            "macOS asks for an administrator password before Delve may control \
                             a program; `DevToolsSecurity -enable` stops it asking."
                                .into(),
                        );
                    }
                    this.end_session(cx);
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn debug_event(&mut self, event: Event, cx: &mut Context<Self>) {
        if self.debug.session.is_none() {
            return;
        }
        match event {
            Event::Initialized => self.configure_session(cx),
            Event::Stopped(stopped) => self.on_stopped(stopped, cx),
            Event::Continued { .. } if self.debug.session.as_ref().is_some_and(|s| s.stepping) => {}
            Event::Continued { .. } => self.on_resumed(cx),
            Event::Output { category, text } => {
                let kind = match category.as_str() {
                    "stderr" => LineKind::Error,
                    "console" | "important" => LineKind::Info,
                    _ => LineKind::Output,
                };
                self.console_output(kind, &text);
            }
            Event::Breakpoint { breakpoint, .. } => self.on_breakpoint_changed(breakpoint, cx),
            Event::Thread { .. } => {}
            Event::Exited { code } => {
                self.console_line(LineKind::Info, format!("Process exited with code {code}."));
            }
            Event::Terminated => self.end_session(cx),
            Event::Closed(why) => {
                if !self.debug.session.as_ref().is_some_and(|s| s.stopping) {
                    self.console_line(LineKind::Error, why);
                }
                self.end_session(cx);
            }
        }
        cx.notify();
    }

    /// Sends every breakpoint of the session's project, then lets the program start.
    fn configure_session(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.debug.session.as_mut() else {
            return;
        };
        session.phase = Phase::Running;
        let client = session.client.clone();
        let root = document_key(&session.root);
        let files: Vec<(PathBuf, Vec<SourceBreakpoint>)> = self
            .debug
            .breakpoints
            .keys()
            .filter(|path| path.starts_with(&root))
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .map(|doc| {
                let list = self.file_breakpoints(&doc, cx);
                (doc, source_breakpoints(&list))
            })
            .collect();
        cx.spawn(async move |this, cx| {
            for (doc, sent) in files {
                let statuses = client.set_breakpoints(&doc, &sent).await;
                let _ = this.update(cx, |this, cx| this.note_verified(&doc, &sent, statuses, cx));
            }
            if let Err(why) = client.configuration_done().await {
                let _ = this.update(cx, |this, cx| {
                    this.console_line(LineKind::Error, why);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn on_breakpoint_changed(&mut self, changed: BreakpointStatus, cx: &mut Context<Self>) {
        let Some(id) = changed.id else {
            return;
        };
        let Some(session) = self.debug.session.as_mut() else {
            return;
        };
        let mut touched = None;
        for (doc, statuses) in &mut session.verified {
            for (_, status) in statuses.iter_mut().filter(|(_, s)| s.id == Some(id)) {
                status.verified = changed.verified;
                touched = Some(doc.clone());
            }
        }
        if let Some(doc) = touched {
            self.push_breakpoints(&doc, cx);
        }
    }

    fn on_stopped(&mut self, stopped: Stopped, cx: &mut Context<Self>) {
        let Some(session) = self.debug.session.as_mut() else {
            return;
        };
        session.generation += 1;
        session.stepping = false;
        session.phase = Phase::Stopped(stopped.clone());
        let generation = session.generation;
        let client = session.client.clone();
        let wanted = stopped.thread_id.or(session.thread);
        cx.spawn(async move |this, cx| {
            let threads = client.threads().await.unwrap_or_default();
            let Some(thread) = wanted.or_else(|| threads.first().map(|t| t.id)) else {
                return;
            };
            let frames = client.stack_trace(thread, STACK_DEPTH).await;
            let _ = this.update(cx, |this, cx| {
                let Some(session) = this.debug.session.as_mut() else {
                    return;
                };
                if session.generation != generation {
                    return;
                }
                session.threads = threads;
                session.thread = Some(thread);
                match frames {
                    Ok(frames) => {
                        session.frame = first_frame_with_source(&frames).map(|f| f.id);
                        session.frames = frames;
                    }
                    Err(why) => this.console_line(LineKind::Error, why),
                }
                this.select_frame(None, true, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// Shows a frame of the paused thread: its line in the editor, its variables and the watches.
    pub(super) fn select_frame(
        &mut self,
        frame: Option<i64>,
        reveal: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.debug.session.as_mut() else {
            return;
        };
        if let Some(frame) = frame {
            session.frame = Some(frame);
        }
        let Some(frame) = session.top_frame().cloned() else {
            self.refresh_debug_marks(cx);
            return;
        };
        session.scopes.clear();
        session.children.clear();
        let generation = session.generation;
        let client = session.client.clone();
        let watches = self.watch_expressions();
        let expanded = self.debug.expanded.clone();
        cx.spawn(async move |this, cx| {
            let scopes = client.scopes(frame.id).await.unwrap_or_default();
            let mut children = HashMap::new();
            let mut queue: Vec<(String, i64, usize)> = scopes
                .iter()
                .filter(|s| !s.expensive)
                .map(|s| (s.name.clone(), s.variables_reference, 0))
                .collect();
            while let Some((key, reference, depth)) = queue.pop() {
                let Ok(vars) = client.variables(reference).await else {
                    continue;
                };
                for v in &vars {
                    let child = variable_key(&key, &v.name);
                    if v.variables_reference > 0
                        && depth < EXPAND_DEPTH
                        && expanded.contains(&child)
                    {
                        queue.push((child, v.variables_reference, depth + 1));
                    }
                }
                children.insert(reference, vars);
            }
            let mut results = Vec::new();
            for expression in watches {
                let value = client.evaluate(&expression, Some(frame.id), "watch").await;
                results.push((expression, value));
            }
            let _ = this.update(cx, |this, cx| {
                let Some(session) = this.debug.session.as_mut() else {
                    return;
                };
                if session.generation != generation || session.frame != Some(frame.id) {
                    return;
                }
                session.scopes = scopes;
                session.children = children;
                session.watches = results;
                cx.notify();
            });
        })
        .detach();
        if reveal && let Some(path) = frame.path.clone().filter(|p| p.exists()) {
            self.open_file_link(path, Some(frame.line), Some(frame.column.max(1)), cx);
        }
        self.refresh_debug_marks(cx);
    }

    /// Puts the paused line, and whether hovers evaluate, in every open editor.
    fn refresh_debug_marks(&mut self, cx: &mut Context<Self>) {
        let paused = self.paused();
        let editors: Vec<Entity<EditorView>> = self
            .items
            .values()
            .filter_map(|v| match v {
                ItemView::Editor(e) => Some(e.clone()),
                _ => None,
            })
            .collect();
        for editor in editors {
            let doc = document_key(editor.read(cx).path());
            let stopped = self.stopped_line_in(&doc);
            editor.update(cx, |e, cx| {
                e.set_debug_paused(paused);
                e.set_stopped_line(stopped, cx);
            });
        }
        cx.notify();
    }

    fn on_resumed(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.debug.session.as_mut() else {
            return;
        };
        session.generation += 1;
        session.phase = Phase::Running;
        session.frames.clear();
        session.frame = None;
        session.scopes.clear();
        session.children.clear();
        session.watches.clear();
        self.refresh_debug_marks(cx);
    }

    fn continue_debugging(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.debug.session.as_ref() else {
            return;
        };
        let Some(thread) = session.thread.filter(|_| session.stopped().is_some()) else {
            return;
        };
        let client = session.client.clone();
        self.on_resumed(cx);
        self.run_request(async move { client.continue_(thread).await }, cx);
    }

    /// F10, F11 and Shift+F11; the paused line stays drawn until the step stops, so it never
    /// flickers off and on.
    fn step(&mut self, command: &'static str, cx: &mut Context<Self>) {
        let Some(session) = self.debug.session.as_mut() else {
            return;
        };
        let Some(thread) = session.thread.filter(|_| session.stopped().is_some()) else {
            return;
        };
        session.generation += 1;
        session.phase = Phase::Running;
        session.stepping = true;
        let client = session.client.clone();
        self.run_request(
            async move {
                match command {
                    "next" => client.next(thread).await,
                    "stepIn" => client.step_in(thread).await,
                    _ => client.step_out(thread).await,
                }
            },
            cx,
        );
        cx.notify();
    }

    fn pause_debugging(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.debug.session.as_ref() else {
            return;
        };
        if session.phase != Phase::Running {
            return;
        }
        let client = session.client.clone();
        let thread = session
            .thread
            .or(session.threads.first().map(|t| t.id))
            .unwrap_or(1);
        self.run_request(async move { client.pause(thread).await }, cx);
    }

    fn run_request(
        &mut self,
        request: impl std::future::Future<Output = Result<serde_json::Value, String>> + 'static,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            if let Err(why) = request.await {
                let _ = this.update(cx, |this, cx| {
                    this.console_line(LineKind::Error, why);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Shift+F5: ends the session and the program it runs.
    pub(super) fn stop_debugging(&mut self, cx: &mut Context<Self>) {
        self.stop_then(None, cx);
    }

    fn restart_debugging(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.debug.session.as_ref() else {
            return;
        };
        let again = (session.root.clone(), Target::Again(session.launch.clone()));
        self.stop_then(Some(again), cx);
    }

    fn stop_then(&mut self, then: Option<(PathBuf, Target)>, cx: &mut Context<Self>) {
        let Some(session) = self.debug.session.as_mut() else {
            return;
        };
        if session.stopping {
            return;
        }
        session.stopping = true;
        let client = session.client.clone();
        cx.spawn(async move |this, cx| {
            let _ = client.disconnect().await;
            let _ = this.update(cx, |this, cx| {
                this.end_session(cx);
                if let Some((root, target)) = then {
                    this.launch_debug(root, target, cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn end_session(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.debug.session.take() else {
            return;
        };
        if !session.stopping {
            self.console_line(LineKind::Info, "Debugging ended.".into());
        }
        let docs: Vec<PathBuf> = session.verified.keys().cloned().collect();
        drop(session);
        for doc in docs {
            self.push_breakpoints(&doc, cx);
        }
        self.refresh_debug_marks(cx);
    }

    /// Kills a running session's adapter and program as Athena quits.
    pub(super) fn debug_quit(&mut self) {
        self.debug.session = None;
    }

    /// While paused, a hover shows the value under the pointer; otherwise, or when the debugger
    /// cannot evaluate it, the language server's documentation.
    pub(super) fn debug_hover(
        &mut self,
        editor: &Entity<EditorView>,
        request: u64,
        at: (u32, u32),
        cx: &mut Context<Self>,
    ) {
        let session = self
            .debug
            .session
            .as_ref()
            .filter(|s| s.stopped().is_some());
        let expression = editor
            .read(cx)
            .debug_paused()
            .then(|| editor.read(cx).expression_at(at.0, at.1))
            .flatten();
        let (Some(session), Some(expression)) = (session, expression) else {
            return self.lsp_hover(editor, request, at, cx);
        };
        let client = session.client.clone();
        let frame = session.frame;
        let weak = editor.downgrade();
        cx.spawn(
            async move |this, cx| match client.evaluate(&expression, frame, "hover").await {
                Ok(value) => {
                    let mut blocks =
                        vec![HoverBlock::Code(format!("{expression} = {}", value.result))];
                    if let Some(ty) = value.type_name {
                        blocks.push(HoverBlock::Text(ty));
                    }
                    let _ = weak.update(cx, |e, cx| e.show_hover(request, blocks, cx));
                }
                Err(_) => {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(editor) = weak.upgrade() {
                            this.lsp_hover(&editor, request, at, cx);
                        }
                    });
                }
            },
        )
        .detach();
    }

    fn watch_expressions(&self) -> Vec<String> {
        self.debug
            .session
            .as_ref()
            .and_then(|s| self.debug.watch.get(&s.root))
            .cloned()
            .unwrap_or_default()
    }

    pub(super) fn add_watch(&mut self, expression: String, cx: &mut Context<Self>) {
        let Some(root) = self.debug_root() else {
            return;
        };
        let list = self.debug.watch.entry(root).or_default();
        if !list.contains(&expression) {
            list.push(expression);
        }
        self.watch_changed(cx);
    }

    pub(super) fn remove_watch(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(root) = self.debug_root() else {
            return;
        };
        if let Some(list) = self.debug.watch.get_mut(&root)
            && index < list.len()
        {
            list.remove(index);
        }
        self.watch_changed(cx);
    }

    fn watch_changed(&mut self, cx: &mut Context<Self>) {
        let roots: Vec<PathBuf> = self
            .workspace
            .projects
            .iter()
            .map(|p| p.root.clone())
            .collect();
        self.debug.save(&roots);
        if self.paused() {
            self.select_frame(None, false, cx);
        }
        cx.notify();
    }

    /// The session's project while debugging, else the active one.
    pub(super) fn debug_root(&self) -> Option<PathBuf> {
        self.debug
            .session
            .as_ref()
            .map(|s| s.root.clone())
            .or_else(|| self.active_root())
    }

    /// Opens a variable's children, fetching them the first time.
    pub(super) fn toggle_variable(&mut self, key: String, reference: i64, cx: &mut Context<Self>) {
        if !self.debug.expanded.remove(&key) {
            self.debug.expanded.insert(key);
            if let Some(session) = self.debug.session.as_ref()
                && !session.children.contains_key(&reference)
            {
                let client = session.client.clone();
                let generation = session.generation;
                cx.spawn(async move |this, cx| {
                    let vars = client.variables(reference).await;
                    let _ = this.update(cx, |this, cx| {
                        let Some(session) = this.debug.session.as_mut() else {
                            return;
                        };
                        if session.generation == generation {
                            session.children.insert(reference, vars.unwrap_or_default());
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
        }
        cx.notify();
    }

    pub(super) fn ensure_debug_inputs(&mut self, cx: &mut Context<Self>) {
        if self.debug.console_input.is_some() {
            return;
        }
        let console = cx.new(|cx| TextInput::new("Evaluate in the selected frame", cx));
        let watch = cx.new(|cx| TextInput::new("Add an expression to watch", cx));
        self.debug._inputs = vec![
            cx.subscribe(&console, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Submit | InputEvent::SubmitBeside) {
                    let text = input.read(cx).text().trim().to_string();
                    input.update(cx, |i, cx| i.set_text("", cx));
                    if !text.is_empty() {
                        this.evaluate_in_console(text, cx);
                    }
                }
            }),
            cx.subscribe(&watch, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Submit | InputEvent::SubmitBeside) {
                    let text = input.read(cx).text().trim().to_string();
                    input.update(cx, |i, cx| i.set_text("", cx));
                    if !text.is_empty() {
                        this.add_watch(text, cx);
                    }
                }
            }),
        ];
        self.debug.console_input = Some(console);
        self.debug.watch_input = Some(watch);
    }

    fn evaluate_in_console(&mut self, expression: String, cx: &mut Context<Self>) {
        self.console_line(LineKind::Input, format!("> {expression}"));
        let Some(session) = self.debug.session.as_ref() else {
            self.console_line(LineKind::Error, "Not debugging; start with F5.".into());
            return cx.notify();
        };
        if session.stopped().is_none() {
            self.console_line(
                LineKind::Error,
                "The program is running; pause it to evaluate.".into(),
            );
            return cx.notify();
        }
        let client = session.client.clone();
        let frame = session.frame;
        cx.spawn(async move |this, cx| {
            let value = client.evaluate(&expression, frame, "repl").await;
            let _ = this.update(cx, |this, cx| {
                match value {
                    Ok(v) => this.console_line(LineKind::Result, v.result),
                    Err(why) => this.console_line(LineKind::Error, why),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn console_line(&mut self, kind: LineKind, text: String) {
        self.debug.console_open = false;
        for line in text.lines() {
            self.debug.console.push(ConsoleLine {
                kind,
                text: line.to_string(),
            });
        }
        self.trim_console();
    }

    /// Program output, which may end mid-line and continue in the next event.
    fn console_output(&mut self, kind: LineKind, text: &str) {
        let mut pieces = text.split('\n').peekable();
        while let Some(piece) = pieces.next() {
            let last = pieces.peek().is_none();
            if last && piece.is_empty() {
                self.debug.console_open = false;
                break;
            }
            match self.debug.console.last_mut() {
                Some(line) if self.debug.console_open && line.kind == kind => {
                    line.text.push_str(piece)
                }
                _ => self.debug.console.push(ConsoleLine {
                    kind,
                    text: piece.to_string(),
                }),
            }
            self.debug.console_open = last;
        }
        self.trim_console();
    }

    fn trim_console(&mut self) {
        self.debug.console_follow.set(true);
        let over = self.debug.console.len().saturating_sub(CONSOLE_LINES);
        if over > 0 {
            self.debug.console.drain(..over);
        }
    }

    /// Opens .vscode/launch.json, writing VS Code's Go starter first if the project has none.
    fn open_launch_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        let path = root.join(".vscode").join("launch.json");
        if !path.exists() {
            let written = std::fs::create_dir_all(root.join(".vscode"))
                .and_then(|()| std::fs::write(&path, LAUNCH_TEMPLATE));
            if let Err(e) = written {
                return self.transient_notice(
                    "Can't create launch.json",
                    format!("{}: {e}", path.display()),
                    cx,
                );
            }
        }
        self.open_file(path, window, cx);
    }

    /// The debugger's state for Claude's debug_state tool, for the project at `root`.
    pub(super) fn debug_state_for_claude(&self, root: &Path) -> DebugStateInfo {
        let mut info = DebugStateInfo {
            project: root.to_path_buf(),
            status: "not_debugging".into(),
            ..DebugStateInfo::default()
        };
        let Some(session) = self.debug.session.as_ref().filter(|s| s.root == root) else {
            return info;
        };
        info.configuration = Some(session.launch.name.clone());
        info.program = Some(session.launch.program.clone());
        info.status = match session.phase {
            Phase::Starting => "starting",
            Phase::Running => "running",
            Phase::Stopped(_) => "paused",
        }
        .into();
        let Some(stopped) = session.stopped() else {
            return info;
        };
        info.reason = Some(stopped.reason.clone());
        info.description = stopped.description.clone().or(stopped.text.clone());
        info.thread = session
            .threads
            .iter()
            .find(|t| Some(t.id) == session.thread)
            .map(|t| t.name.clone());
        let frame_info = |f: &StackFrame| DebugFrameInfo {
            name: f.name.clone(),
            path: f.path.clone(),
            line: f.line,
            column: f.column,
        };
        info.stack = session
            .frames
            .iter()
            .take(CLAUDE_FRAMES)
            .map(frame_info)
            .collect();
        info.frames_total = session.frames.len() as u32;
        info.location = session.top_frame().map(frame_info);
        let locals: Vec<&Variable> = session
            .scopes
            .iter()
            .filter(|s| !s.expensive)
            .filter_map(|s| session.children.get(&s.variables_reference))
            .flatten()
            .collect();
        info.locals_total = locals.len() as u32;
        info.locals = locals
            .into_iter()
            .take(CLAUDE_LOCALS)
            .map(|v| DebugVariableInfo {
                name: v.name.clone(),
                value: shorten(&v.value, CLAUDE_VALUE),
                type_name: v.type_name.as_deref().map(|t| shorten(t, CLAUDE_VALUE)),
            })
            .collect();
        info
    }
}

/// The Go test on `line` of a test file, as a Delve test launch of its package.
fn test_launch(
    path: &Path,
    line: usize,
    shell: &Shell,
    cx: &Context<Shell>,
) -> Result<Launch, String> {
    if path.extension().is_none_or(|e| e != "go") {
        return Err("Athena debugs Go tests; Vitest and Jest tests run without a debugger.".into());
    }
    let text = shell
        .editors_showing(&document_key(path), cx)
        .first()
        .and_then(|e| e.read(cx).text())
        .or_else(|| std::fs::read_to_string(path).ok())
        .ok_or_else(|| format!("Could not read {}", path.display()))?;
    let tests = athena_editor::find_tests(athena_editor::Lang::Go, path, &text);
    let test = tests
        .iter()
        .filter(|t| t.line <= line && line <= t.end_line)
        .max_by_key(|t| t.line)
        .ok_or("Put the cursor in a Go test function to debug it.")?;
    let dir = path.parent().ok_or("The test file has no folder")?;
    let pattern = athena_testing::go_subtest_pattern(&test.titles);
    Ok(Launch::test(test.titles.join("/"), dir, pattern))
}

/// The Go configurations in the project's .vscode/launch.json, none when it has no such file.
fn read_launch_json(root: &Path) -> Result<Vec<athena_dap::LaunchConfig>, String> {
    let path = root.join(".vscode").join("launch.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    let value: serde_json::Value = serde_json::from_str(&crate::keymap::strip_jsonc(&text))
        .map_err(|e| format!(".vscode/launch.json is not valid JSON: {e}"))?;
    athena_dap::go_configurations(&value).map_err(|e| format!(".vscode/launch.json: {e}"))
}

const LAUNCH_TEMPLATE: &str = r#"{
  // Athena debugs the first "go" configuration with F5.
  "version": "0.2.0",
  "configurations": [
    {
      "name": "Launch Package",
      "type": "go",
      "request": "launch",
      "mode": "auto",
      "program": "${fileDirname}"
    }
  ]
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakpoints_round_trip_through_the_saved_file_relative_to_their_project() {
        let dir = std::env::temp_dir().join(format!("athena-debug-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        let root = document_key(&dir.join("proj"));
        let file = root.join("main.go");
        let mut state = DebugState::load(&dir.join("workspace.json"));
        state.breakpoints.insert(
            file.clone(),
            vec![
                Breakpoint::at(4),
                Breakpoint {
                    condition: Some("i > 2".into()),
                    enabled: false,
                    ..Breakpoint::at(9)
                },
            ],
        );
        state.watch.insert(root.clone(), vec!["p.X".into()]);
        state.save(std::slice::from_ref(&root));
        let text = std::fs::read_to_string(dir.join("breakpoints.json")).unwrap();
        assert!(text.contains("\"path\": \"main.go\""), "{text}");
        assert!(text.contains("\"line\": 5"), "one-based on disk: {text}");
        let loaded = DebugState::load(&dir.join("workspace.json"));
        assert_eq!(loaded.breakpoints.get(&file), state.breakpoints.get(&file));
        assert_eq!(loaded.watch.get(&root), Some(&vec!["p.X".to_string()]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_breakpoints_are_not_sent_and_lines_become_one_based() {
        let list = vec![
            Breakpoint {
                log_message: Some("x={x}".into()),
                ..Breakpoint::at(0)
            },
            Breakpoint {
                enabled: false,
                ..Breakpoint::at(3)
            },
        ];
        let sent = source_breakpoints(&list);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].line, 1);
        assert_eq!(sent[0].log_message.as_deref(), Some("x={x}"));
    }

    #[test]
    fn claude_s_debug_state_fits_one_app_socket_frame_and_reads_back() {
        let long = "x".repeat(5000);
        let frame = DebugFrameInfo {
            name: "main.main".into(),
            path: Some("/p/main.go".into()),
            line: 12,
            column: 3,
        };
        let info = DebugStateInfo {
            project: "/p".into(),
            status: "paused".into(),
            reason: Some("breakpoint".into()),
            location: Some(frame.clone()),
            stack: vec![frame; CLAUDE_FRAMES],
            frames_total: 50,
            locals: (0..CLAUDE_LOCALS)
                .map(|i| DebugVariableInfo {
                    name: format!("v{i}"),
                    value: shorten(&long, CLAUDE_VALUE),
                    type_name: Some(shorten(&long, CLAUDE_VALUE)),
                })
                .collect(),
            locals_total: 400,
            ..DebugStateInfo::default()
        };
        let reply = athena_proto::AppReply::Debug(Box::new(info));
        let mut out = Vec::new();
        athena_proto::write_frame(&mut out, &reply).unwrap();
        assert!(out.len() < 64 * 1024, "{} bytes", out.len());
        let back: Option<athena_proto::AppReply> =
            athena_proto::read_frame(&mut out.as_slice()).unwrap();
        assert_eq!(back, Some(reply));
    }

    #[test]
    fn a_stop_in_the_runtime_shows_the_first_frame_with_source() {
        let frame = |id: i64, path: Option<&str>| StackFrame {
            id,
            name: format!("f{id}"),
            path: path.map(PathBuf::from),
            line: 1,
            column: 1,
            hint: None,
        };
        let here = std::env::current_dir().unwrap().join("Cargo.toml");
        let frames = vec![
            frame(1, None),
            frame(2, Some("/no/such/runtime/panic.go")),
            frame(3, Some(here.to_str().unwrap())),
        ];
        assert_eq!(first_frame_with_source(&frames).map(|f| f.id), Some(3));
        assert_eq!(first_frame_with_source(&frames[..2]).map(|f| f.id), Some(1));
        assert_eq!(shorten("abcdef", 3), "abc…");
        assert_eq!(shorten("ab", 3), "ab");
    }
}
