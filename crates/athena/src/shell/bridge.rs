use std::path::PathBuf;

use athena_proto::{ActiveFile, AppMsg, AppReply, PaneId, ProjectInfo, TerminalInfo};
use athena_term::ClaudeState;
use athena_workspace::{ItemKind, resolve_in_roots};
use gpui::{Context, PromptLevel, Window};

use super::Shell;
use super::item::ItemView;

const MAX_LINES: u32 = 2000;
const MAX_SELECTION: usize = 64 * 1024;
const MAX_RUN: usize = 4096;
const CONFIRM_FOR: std::time::Duration = std::time::Duration::from_secs(60);

impl Shell {
    /// Answers a request from `athena` or the MCP bridge.
    pub(super) fn handle_app(
        &mut self,
        msg: AppMsg,
        caller: Option<PaneId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AppReply {
        match msg {
            AppMsg::OpenProject { path } => {
                self.open_folder(path, cx);
                window.activate_window();
                cx.activate(true);
                AppReply::Ok
            }
            AppMsg::ListProjects => AppReply::Projects(
                self.workspace
                    .projects
                    .iter()
                    .enumerate()
                    .map(|(i, p)| ProjectInfo {
                        root: p.root.clone(),
                        name: p.name(),
                        active: Some(i) == self.workspace.active,
                    })
                    .collect(),
            ),
            AppMsg::ActiveFile => AppReply::ActiveFile(self.active_file(cx)),
            AppMsg::OpenFile { path, line } => match self.open_scoped(path, line, window, cx) {
                Ok(()) => AppReply::Ok,
                Err(e) => AppReply::Error(e),
            },
            AppMsg::ListTerminals => AppReply::Terminals(self.terminals(cx)),
            AppMsg::ReadTerminal { session, lines } => {
                self.read_terminal(session, lines.min(MAX_LINES), cx)
            }
            AppMsg::Identify { .. } => AppReply::Ok,
            AppMsg::RunInTerminal { .. } => AppReply::Error("needs confirmation".into()),
            AppMsg::WhoAmI => AppReply::Caller {
                session: caller,
                project: caller.and_then(|s| self.session_root(s)),
            },
        }
    }

    /// Accepts a client's claimed pane only if that pane's foreground program (the shell, or
    /// `claude` running in it) is one of the client's ancestors.
    pub(super) fn verify_caller(
        &self,
        claimed: Option<PaneId>,
        lineage: &[i32],
        cx: &Context<Self>,
    ) -> Option<PaneId> {
        let session = claimed?;
        let foreground = self.items.values().find_map(|v| match v {
            ItemView::Terminal(t) if t.read(cx).session() == Some(session) => {
                t.read(cx).foreground_pid()
            }
            _ => None,
        })?;
        lineage.contains(&foreground).then_some(session)
    }

    /// Asks the user before typing anything Claude sends into a terminal; no answer means no.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn confirm_run(
        &mut self,
        session: PaneId,
        text: String,
        newline: bool,
        caller: Option<PaneId>,
        reply: std::sync::mpsc::SyncSender<AppReply>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.items.values().find_map(|v| match v {
            ItemView::Terminal(t) if t.read(cx).session() == Some(session) => Some(t.clone()),
            _ => None,
        });
        let Some(target) = target else {
            let _ = reply.send(AppReply::Error(format!(
                "terminal {session} is not open in Athena"
            )));
            return;
        };
        if text.len() > MAX_RUN {
            let _ = reply.send(AppReply::Error(format!(
                "command is longer than {MAX_RUN} bytes"
            )));
            return;
        }
        let who = match caller.and_then(|s| self.session_root(s)) {
            Some(root) => format!(
                "Claude in {}",
                root.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
            None => "A program outside Athena".to_string(),
        };
        let place = target.read(cx).label();
        let detail = format!(
            "{who} wants to type this into \"{place}\"{}:\n\n{text}",
            if newline { " and press Return" } else { "" }
        );
        let answer = window.prompt(
            PromptLevel::Warning,
            "Run in terminal?",
            Some(&detail),
            &["Run", "Don't Run"],
            cx,
        );
        cx.spawn_in(window, async move |_, cx| {
            let timeout = cx.background_executor().timer(CONFIRM_FOR);
            let choice = futures::future::select(answer, timeout).await;
            let approved = matches!(choice, futures::future::Either::Left((Ok(0), _)));
            let result = if approved {
                let mut bytes = text.into_bytes();
                if newline {
                    bytes.push(b'\r');
                }
                let _ = cx.update(|_, cx| target.update(cx, |t, cx| t.type_text(bytes, cx)));
                AppReply::Ok
            } else if matches!(choice, futures::future::Either::Right(_)) {
                AppReply::Error("no answer within 60 seconds; nothing was typed".into())
            } else {
                AppReply::Error("the user declined; nothing was typed".into())
            };
            let _ = reply.send(result);
        })
        .detach();
    }

    fn session_root(&self, session: PaneId) -> Option<PathBuf> {
        self.workspace.projects.iter().find_map(|p| {
            p.layout
                .as_ref()?
                .items()
                .any(|i| {
                    i.kind
                        == ItemKind::Terminal {
                            session: Some(session),
                        }
                })
                .then(|| p.root.clone())
        })
    }

    fn active_file(&self, cx: &Context<Self>) -> Option<ActiveFile> {
        let project = self.workspace.active_project()?;
        let item = project.layout.as_ref()?.focused_pane()?.active_item()?;
        let ItemKind::Editor { path } = &item.kind else {
            return None;
        };
        let Some(ItemView::Editor(view)) = self.items.get(&(project.root.clone(), item.id)) else {
            return None;
        };
        let editor = view.read(cx);
        let (line, column, selection) = editor.cursor()?;
        let selection = selection.map(|s| s.chars().take(MAX_SELECTION).collect());
        Some(ActiveFile {
            path: path.clone(),
            line,
            column,
            selection,
            modified: editor.is_dirty(),
        })
    }

    fn open_scoped(
        &mut self,
        path: PathBuf,
        line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let roots: Vec<PathBuf> = self
            .workspace
            .projects
            .iter()
            .map(|p| p.root.clone())
            .collect();
        let path = resolve_in_roots(&path, &roots)?;
        if path.is_dir() {
            return Err(format!("{} is a folder", path.display()));
        }
        let index = self
            .workspace
            .projects
            .iter()
            .position(|p| p.root.canonicalize().is_ok_and(|r| path.starts_with(r)))
            .ok_or("no project holds that file")?;
        self.switch_to(index, cx);
        self.open_file(path.clone(), window, cx);
        // Create the editor now rather than at the next frame, so a following get_active_file sees it.
        if let Some(ItemView::Editor(view)) = self.editor_for(&path, cx)
            && let Some(line) = line
        {
            view.update(cx, |v, cx| v.go_to_line(line, cx));
        }
        Ok(())
    }

    fn editor_for(&mut self, path: &PathBuf, cx: &mut Context<Self>) -> Option<ItemView> {
        let project = self.workspace.active_project()?;
        let root = project.root.clone();
        let item = project
            .layout
            .as_ref()?
            .items()
            .find(|i| matches!(&i.kind, ItemKind::Editor { path: p } if p == path))?
            .clone();
        self.item_view(&root, &item, cx)
    }

    fn terminals(&self, cx: &Context<Self>) -> Vec<TerminalInfo> {
        let mut out = Vec::new();
        for project in &self.workspace.projects {
            let Some(layout) = &project.layout else {
                continue;
            };
            for item in layout.items() {
                let ItemKind::Terminal { session } = item.kind else {
                    continue;
                };
                let view = match self.items.get(&(project.root.clone(), item.id)) {
                    Some(ItemView::Terminal(v)) => Some(v.read(cx)),
                    _ => None,
                };
                let foreground = view.and_then(|v| v.foreground());
                out.push(TerminalInfo {
                    session,
                    project: project.root.clone(),
                    title: view.map_or_else(|| "Terminal".into(), |v| v.label()),
                    cwd: foreground.as_ref().and_then(|(_, cwd)| cwd.clone()),
                    program: foreground.map(|(name, _)| name),
                    claude: view.and_then(|v| v.claude_state()).map(|s| match s {
                        ClaudeState::Running => "running".into(),
                        ClaudeState::Waiting => "waiting_input".into(),
                    }),
                });
            }
        }
        out
    }

    fn read_terminal(&self, session: PaneId, lines: u32, cx: &Context<Self>) -> AppReply {
        let view = self.items.values().find_map(|v| match v {
            ItemView::Terminal(t) if t.read(cx).session() == Some(session) => Some(t.clone()),
            _ => None,
        });
        match view {
            Some(view) => AppReply::Lines(view.read(cx).text_lines(lines as usize)),
            None => AppReply::Error(format!(
                "terminal {session} is not open in Athena; open its tab first"
            )),
        }
    }
}
