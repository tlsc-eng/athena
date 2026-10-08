use std::path::{Path, PathBuf};
use std::time::Duration;

use athena_editor::EditorView;
use athena_lsp::{Position, Symbol, symbol_kind_label};
use athena_ui::motion::{self, Closing};
use athena_ui::{ActiveTheme, InputEvent, TextInput};
use gpui::{
    Action, Animation, AnyElement, ClickEvent, Context, Entity, Focusable, FontWeight,
    HighlightStyle, MouseButton, SharedString, StyledText, Subscription, Task, Window, div,
    prelude::*, px,
};

use super::Shell;
use super::fuzzy;
use super::lsp::document_key;
use crate::actions;

const MAX_FILES: usize = 20_000;
const MAX_ROWS: usize = 50;
/// A file's whole outline is listed, as VS Code does, up to this many symbols.
const MAX_SYMBOL_ROWS: usize = 1000;
/// Typing pauses this long before the servers are asked for workspace symbols.
const SYMBOL_QUERY_DELAY: Duration = Duration::from_millis(120);

pub(super) enum Target {
    File(PathBuf),
    Command(Box<dyn Action>),
    /// A command that starts Claude Code in this project.
    Claude(String),
    Branch(super::branches::BranchPick),
    Symbol(PathBuf, Position),
    /// A recently closed project folder.
    Folder(PathBuf),
}

impl Clone for Target {
    fn clone(&self) -> Self {
        match self {
            Self::File(path) => Self::File(path.clone()),
            Self::Command(action) => Self::Command(action.boxed_clone()),
            Self::Claude(command) => Self::Claude(command.clone()),
            Self::Branch(pick) => Self::Branch(pick.clone()),
            Self::Symbol(path, at) => Self::Symbol(path.clone(), *at),
            Self::Folder(path) => Self::Folder(path.clone()),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Mode {
    Files,
    /// Files that open in a new pane beside the focused one.
    FilesBeside,
    Commands,
    Claude,
    Branches,
    /// The focused file's symbols; Go to File switches here when the query starts with `@`.
    Symbols,
    /// Symbols anywhere in the project, after `#`.
    WorkspaceSymbols,
    /// Recently closed project folders.
    Recent,
}

/// The mode Go to File's query asks for with its first character, as in VS Code.
fn mode_for_query(query: &str) -> Mode {
    match query.chars().next() {
        Some('@') => Mode::Symbols,
        Some('#') => Mode::WorkspaceSymbols,
        Some('>') => Mode::Commands,
        _ => Mode::Files,
    }
}

fn placeholder_hint(mode: Mode) -> &'static str {
    match mode {
        Mode::Files | Mode::FilesBeside => "No matching files",
        Mode::Symbols | Mode::WorkspaceSymbols => "No matching symbols",
        Mode::Recent => "No recently opened folders",
        _ => "No matching commands",
    }
}

/// Editor actions the palette offers while an editor is focused; Insert line above has no key.
const EDITOR_COMMANDS: &[(&str, &str)] = &[
    ("Indent lines", "editor::IndentLines"),
    ("Outdent lines", "editor::OutdentLines"),
    ("Toggle replace", "editor::FindReplace"),
    ("Insert line above", "editor::InsertLineAbove"),
    ("Insert line below", "editor::InsertLineBelow"),
    ("Rename symbol", "editor::RenameSymbol"),
    ("Quick fix", "editor::ShowCodeActions"),
    ("Go to implementations", "editor::GoToImplementation"),
    ("Go to type definition", "editor::GoToTypeDefinition"),
];

/// Commands offered for starting Claude; typing anything else offers that too.
const CLAUDE_COMMANDS: &[&str] = &["claude", "claude-tlsc", "claude-ai"];

pub(super) struct Entry {
    label: String,
    detail: Option<String>,
    /// A symbol's kind, shown at the right edge.
    kind: Option<&'static str>,
    /// What the query is matched against (a project-relative path for files).
    key: String,
    target: Target,
}

pub(super) struct Palette {
    mode: Mode,
    /// Opened as Go to File, so a leading `@`, `#` or `>` switches what is listed.
    switchable: bool,
    pub(super) input: Entity<TextInput>,
    placeholder_hint: &'static str,
    entries: Vec<Entry>,
    hits: Vec<(usize, Vec<usize>)>,
    selected: usize,
    /// Shown instead of the list while symbols load, or when there are none to load.
    status: Option<&'static str>,
    /// The editor whose symbols are listed and its cursor before the palette moved it.
    origin: Option<(Entity<EditorView>, (u32, u32))>,
    /// Bumped per symbol request so a slow answer never replaces a newer one.
    asked: u64,
    symbols_task: Option<Task<()>>,
    _subscription: Subscription,
}

/// Commands offered in the palette, in the order shown before any typing.
fn commands() -> Vec<(&'static str, Box<dyn Action>)> {
    vec![
        ("New terminal", Box::new(actions::NewTerminal)),
        ("Split right", Box::new(actions::SplitRight)),
        ("Split down", Box::new(actions::SplitDown)),
        ("Close tab", Box::new(actions::CloseTab)),
        ("Save as…", Box::new(actions::SaveAs)),
        ("Toggle auto save", Box::new(actions::ToggleAutoSave)),
        (
            "Toggle format on save",
            Box::new(actions::ToggleFormatOnSave),
        ),
        ("Zoom pane", Box::new(actions::TogglePaneZoom)),
        ("Next tab", Box::new(actions::NextTab)),
        ("Previous tab", Box::new(actions::PrevTab)),
        ("Go to file", Box::new(actions::QuickOpen)),
        ("Go to symbol in file", Box::new(actions::GoToSymbol)),
        (
            "Go to symbol in workspace",
            Box::new(actions::GoToWorkspaceSymbol),
        ),
        ("Go back", Box::new(actions::NavigateBack)),
        ("Go forward", Box::new(actions::NavigateForward)),
        (
            "Reveal active file in tree",
            Box::new(actions::RevealInTree),
        ),
        ("Find in project", Box::new(actions::FindInProject)),
        ("Source control changes", Box::new(actions::ShowChanges)),
        ("Toggle inline blame", Box::new(actions::ToggleBlame)),
        ("Switch branch…", Box::new(actions::SwitchBranch)),
        ("Open file to the side", Box::new(actions::QuickOpenBeside)),
        ("New Claude session", Box::new(actions::NewClaudeSession)),
        (
            "Change the command that starts Claude",
            Box::new(actions::ChangeClaudeCommand),
        ),
        (
            "Enable Claude Code hooks for this project",
            Box::new(actions::EnableClaudeHooks),
        ),
        (
            "Disable Claude Code hooks for this project",
            Box::new(actions::DisableClaudeHooks),
        ),
        (
            "Toggle Claude Code integration (diffs, selection, diagnostics)",
            Box::new(actions::ToggleIdeIntegration),
        ),
        ("Send selection to Claude", Box::new(actions::SendToClaude)),
        ("Toggle file tree", Box::new(actions::ToggleFileTree)),
        ("Problems", Box::new(actions::ShowProblems)),
        ("Go to next problem", Box::new(actions::NextProblem)),
        ("Go to previous problem", Box::new(actions::PrevProblem)),
        ("Notifications", Box::new(actions::ToggleNotifications)),
        ("Containers", Box::new(actions::ShowContainers)),
        ("Playwright", Box::new(actions::ShowPlaywright)),
        ("Run Playwright tests", Box::new(actions::RunPlaywright)),
        ("New browser preview", Box::new(actions::NewPreview)),
        ("Open Markdown preview", Box::new(actions::TogglePreview)),
        (
            "Enable Playwright MCP for Claude in this project",
            Box::new(actions::EnablePlaywrightMcp),
        ),
        (
            "Disable Playwright MCP for Claude in this project",
            Box::new(actions::DisablePlaywrightMcp),
        ),
        ("Open project", Box::new(actions::AddProject)),
        ("Open recent…", Box::new(actions::OpenRecent)),
        ("Clear recently opened", Box::new(actions::ClearRecent)),
        ("Close project", Box::new(actions::CloseProject)),
        ("Next project", Box::new(actions::NextProject)),
        ("Previous project", Box::new(actions::PrevProject)),
        ("Toggle full screen", Box::new(actions::ToggleFullScreen)),
        ("Zoom in", Box::new(actions::FontZoomIn)),
        ("Zoom out", Box::new(actions::FontZoomOut)),
        ("Reset zoom", Box::new(actions::FontZoomReset)),
    ]
}

/// Project files, honouring .gitignore and skipping hidden files.
fn project_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .hidden(true)
        .build()
        .flatten()
    {
        if entry.file_type().is_some_and(|t| t.is_file())
            && let Ok(rel) = entry.path().strip_prefix(root)
        {
            out.push(rel.to_string_lossy().into_owned());
            if out.len() >= MAX_FILES {
                break;
            }
        }
    }
    out.sort();
    out
}

impl Shell {
    pub(super) fn open_palette(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        let placeholder = match mode {
            Mode::Files => "Go to file…  (@ symbol, # workspace symbol, > command)",
            Mode::FilesBeside => "Open to the side…",
            Mode::Commands => "Run a command…",
            Mode::Claude => "Command that starts Claude Code here…",
            Mode::Branches => "Switch to a branch, or type a name to create one…",
            Mode::Symbols => "Go to symbol in file…",
            Mode::WorkspaceSymbols => "Go to symbol in workspace…",
            Mode::Recent => "Open a recent folder…",
        };
        let input = cx.new(|cx| TextInput::new(placeholder, cx));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Changed => this.palette_query_changed(window, cx),
                InputEvent::Up => this.move_palette(-1, cx),
                InputEvent::Down => this.move_palette(1, cx),
                InputEvent::Submit => this.run_palette(None, false, window, cx),
                InputEvent::SubmitBeside => this.run_palette(None, true, window, cx),
                InputEvent::Cancel => this.close_palette(window, cx),
            },
        );
        let entries = match mode {
            Mode::Files
            | Mode::FilesBeside
            | Mode::Branches
            | Mode::Symbols
            | Mode::WorkspaceSymbols => Vec::new(),
            Mode::Claude => claude_entries(""),
            Mode::Commands => self.command_entries(window, cx),
            Mode::Recent => self.recent_entries(),
        };
        let files = matches!(mode, Mode::Files | Mode::FilesBeside);
        window.focus(&input.focus_handle(cx));
        self.palette_closing = None;
        self.palette = Some(Palette {
            mode,
            switchable: mode == Mode::Files,
            input,
            placeholder_hint: placeholder_hint(mode),
            entries,
            hits: Vec::new(),
            selected: 0,
            status: None,
            origin: None,
            asked: 0,
            symbols_task: None,
            _subscription: subscription,
        });
        if files {
            self.load_palette_files(cx);
        }
        if mode == Mode::Symbols {
            self.load_document_symbols(cx);
        }
        self.filter_palette(cx);
        cx.notify();
    }

    /// Commands, with their key bindings, minus those that mean nothing here.
    fn command_entries(&self, window: &Window, cx: &mut Context<Self>) -> Vec<Entry> {
        let root = self.workspace.active_project().map(|p| p.root.as_path());
        let hooks_on = root.is_some_and(crate::claude_hooks::enabled);
        let hooks_any = root.is_some_and(crate::claude_hooks::installed);
        let mcp_on = root.is_some_and(athena_playwright::mcp_enabled);
        let playwright = root.is_some_and(|r| athena_playwright::find_config(r).is_some());
        let mut commands = commands();
        // The editor's own actions, so they only mean something with an editor focused.
        if self.focused_editor().is_some() {
            if let Ok(action) = cx.build_action("editor::GoToLine", None) {
                let at = commands.iter().position(|(l, _)| *l == "Go to file");
                commands.insert(at.map_or(0, |i| i + 1), ("Go to line", action));
            }
            for (label, name) in EDITOR_COMMANDS {
                if let Ok(action) = cx.build_action(name, None) {
                    commands.push((label, action));
                }
            }
        }
        commands
            .into_iter()
            .filter(|(label, _)| {
                !(label.starts_with("Enable Claude Code hooks") && hooks_on
                    || label.starts_with("Disable Claude Code hooks") && !hooks_any
                    || label.starts_with("Enable Playwright MCP") && (mcp_on || !playwright)
                    || label.starts_with("Disable Playwright MCP") && !mcp_on
                    || *label == "Run Playwright tests" && !playwright)
            })
            .map(|(label, action)| Entry {
                kind: None,
                detail: window
                    .highest_precedence_binding_for_action(action.as_ref())
                    .map(|b| keystrokes(&b)),
                key: label.to_string(),
                label: label.to_string(),
                target: Target::Command(action),
            })
            .collect()
    }

    /// Closed project folders that still exist, newest first.
    fn recent_entries(&self) -> Vec<Entry> {
        self.workspace
            .recent
            .iter()
            .filter(|root| root.is_dir())
            .map(|root| {
                let key = actions::display_path(root);
                let label = super::item::file_label(root);
                Entry {
                    detail: root.parent().map(actions::display_path),
                    kind: None,
                    label,
                    key,
                    target: Target::Folder(root.clone()),
                }
            })
            .collect()
    }

    /// Lists the project's files once they have been walked, off the main thread.
    fn load_palette_files(&mut self, cx: &mut Context<Self>) {
        if let Some(root) = self.workspace.active_project().map(|p| p.root.clone()) {
            let walk = cx
                .background_executor()
                .spawn(async move { (project_files(&root), root) });
            cx.spawn(async move |this, cx| {
                let (files, root) = walk.await;
                let _ = this.update(cx, |this, cx| {
                    let Some(palette) = this
                        .palette
                        .as_mut()
                        .filter(|p| matches!(p.mode, Mode::Files | Mode::FilesBeside))
                    else {
                        return;
                    };
                    palette.entries = files
                        .into_iter()
                        .map(|rel| {
                            let path = root.join(&rel);
                            let (dir, name) = match rel.rfind('/') {
                                Some(i) => (Some(rel[..i].to_string()), rel[i + 1..].to_string()),
                                None => (None, rel.clone()),
                            };
                            Entry {
                                label: name,
                                detail: dir,
                                kind: None,
                                key: rel,
                                target: Target::File(path),
                            }
                        })
                        .collect();
                    this.filter_palette(cx);
                });
            })
            .detach();
        }
    }

    /// Go to File's query changed: a leading `@`, `#` or `>` switches what is listed first.
    fn palette_query_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(palette) = self.palette.as_ref() else {
            return;
        };
        let query = palette.input.read(cx).text().to_string();
        if palette.switchable {
            let wanted = mode_for_query(&query);
            if wanted != palette.mode {
                self.switch_palette(wanted, window, cx);
            }
        }
        if self.palette.as_ref().map(|p| p.mode) == Some(Mode::WorkspaceSymbols) {
            self.request_workspace_symbols(cx);
        }
        self.filter_palette(cx);
    }

    fn switch_palette(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        self.restore_palette_origin(cx);
        let commands = (mode == Mode::Commands).then(|| self.command_entries(window, cx));
        let Some(palette) = self.palette.as_mut() else {
            return;
        };
        palette.mode = mode;
        palette.placeholder_hint = placeholder_hint(mode);
        palette.entries = commands.unwrap_or_default();
        palette.status = None;
        palette.asked += 1;
        palette.symbols_task = None;
        match mode {
            Mode::Files => self.load_palette_files(cx),
            Mode::Symbols => self.load_document_symbols(cx),
            _ => {}
        }
    }

    /// What the query matches against, without the character that picked the mode.
    fn palette_needle(palette: &Palette, cx: &gpui::App) -> String {
        let query = palette.input.read(cx).text();
        if palette.switchable && palette.mode != Mode::Files {
            query
                .chars()
                .skip(1)
                .collect::<String>()
                .trim_start()
                .to_string()
        } else {
            query.to_string()
        }
    }

    /// Lists the focused editor's symbols, outermost first, as its language server reports them.
    fn load_document_symbols(&mut self, cx: &mut Context<Self>) {
        let editor = self.focused_editor();
        let client = editor
            .as_ref()
            .and_then(|e| self.document_client(&document_key(e.read(cx).path())));
        let origin = editor
            .as_ref()
            .and_then(|e| Some((e.clone(), e.read(cx).cursor_utf16()?)));
        let Some(palette) = self.palette.as_mut() else {
            return;
        };
        palette.origin = origin;
        let (Some(editor), Some(client)) = (editor, client) else {
            palette.status = Some(if palette.origin.is_some() {
                "No language server lists symbols for this file."
            } else {
                "Open a file to list its symbols."
            });
            return;
        };
        palette.status = Some("Loading symbols…");
        palette.asked += 1;
        let asked = palette.asked;
        let path = editor.read(cx).path().to_path_buf();
        let doc = document_key(&path);
        self.flush_change(&doc, &editor, cx);
        cx.spawn(async move |this, cx| {
            let found = client.document_symbols(&doc).await;
            let _ = this.update(cx, |this, cx| {
                let Some(palette) = this.palette.as_mut().filter(|p| p.asked == asked) else {
                    return;
                };
                palette.status = None;
                match found {
                    Ok(symbols) => {
                        palette.entries = symbol_entries(symbols, Some(&path), None);
                        if palette.entries.is_empty() {
                            palette.status = Some("This file has no symbols.");
                        }
                    }
                    Err(why) => {
                        tracing::warn!("document symbols failed: {why}");
                        palette.status = Some("The language server could not list symbols.");
                    }
                }
                this.filter_palette(cx);
            });
        })
        .detach();
    }

    /// Asks every server of the active project for symbols matching the query, once typing pauses.
    fn request_workspace_symbols(&mut self, cx: &mut Context<Self>) {
        let root = self.active_root();
        let clients = root
            .as_ref()
            .map(|r| self.project_clients(r))
            .unwrap_or_default();
        let Some(palette) = self.palette.as_mut() else {
            return;
        };
        let query = Self::palette_needle(palette, cx);
        palette.asked += 1;
        let asked = palette.asked;
        if query.is_empty() || clients.is_empty() {
            palette.entries.clear();
            palette.symbols_task = None;
            palette.status = Some(if clients.is_empty() {
                "Open a Go or TypeScript file to start its language server."
            } else {
                "Type to search for symbols in the project."
            });
            return;
        }
        palette.status = palette.entries.is_empty().then_some("Searching symbols…");
        palette.symbols_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SYMBOL_QUERY_DELAY).await;
            let mut symbols = Vec::new();
            for client in clients {
                match client.workspace_symbols(&query).await {
                    Ok(found) => symbols.extend(found),
                    Err(why) => tracing::debug!("workspace symbols failed: {why}"),
                }
            }
            let _ = this.update(cx, |this, cx| {
                let Some(palette) = this.palette.as_mut().filter(|p| p.asked == asked) else {
                    return;
                };
                palette.entries = symbol_entries(symbols, None, root.as_deref());
                palette.status = None;
                this.filter_palette(cx);
            });
        }));
    }

    /// Moves the editor to the selected symbol while the list is open, as VS Code previews it.
    fn preview_palette_symbol(&mut self, cx: &mut Context<Self>) {
        let Some(palette) = self.palette.as_ref().filter(|p| p.mode == Mode::Symbols) else {
            return;
        };
        let target = palette
            .hits
            .get(palette.selected)
            .and_then(|(i, _)| palette.entries.get(*i));
        if let (Some((editor, _)), Some(Target::Symbol(_, at))) =
            (palette.origin.as_ref(), target.map(|e| &e.target))
        {
            let (editor, at) = (editor.clone(), *at);
            editor.update(cx, |e, cx| e.go_to_position(at.line, at.character, cx));
        }
    }

    /// Puts the editor's cursor back where it was before symbols were previewed.
    fn restore_palette_origin(&mut self, cx: &mut Context<Self>) {
        if let Some((editor, (line, character))) =
            self.palette.as_mut().and_then(|p| p.origin.take())
        {
            editor.update(cx, |e, cx| e.go_to_position(line, character, cx));
        }
    }

    /// Opens the palette with `query` already typed.
    pub(super) fn open_palette_with(
        &mut self,
        mode: Mode,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_palette(mode, window, cx);
        if let Some(palette) = &self.palette {
            palette.input.update(cx, |i, cx| i.set_text(query, cx));
        }
    }

    fn filter_palette(&mut self, cx: &mut Context<Self>) {
        let Some(palette) = self.palette.as_mut() else {
            return;
        };
        let query = Self::palette_needle(palette, cx);
        if palette.mode == Mode::Claude {
            palette.entries = claude_entries(&query);
        }
        if palette.mode == Mode::Branches {
            palette.entries = super::branches::entries(&self.git.branches, &query)
                .into_iter()
                .map(|b| Entry {
                    kind: None,
                    label: b.label,
                    detail: b.detail,
                    key: b.key,
                    target: Target::Branch(b.pick),
                })
                .collect();
        }
        let limit = match palette.mode {
            Mode::Symbols | Mode::WorkspaceSymbols => MAX_SYMBOL_ROWS,
            _ => MAX_ROWS,
        };
        let mut hits: Vec<(i32, usize, Vec<usize>)> = palette
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| fuzzy::score(&query, &e.key).map(|(s, pos)| (s, i, pos)))
            .collect();
        if !query.is_empty() {
            hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        palette.hits = hits
            .into_iter()
            .take(limit)
            .map(|(_, i, pos)| (i, pos))
            .collect();
        palette.selected = 0;
        if !query.is_empty() {
            self.preview_palette_symbol(cx);
        }
        cx.notify();
    }

    fn move_palette(&mut self, step: isize, cx: &mut Context<Self>) {
        if let Some(p) = self.palette.as_mut()
            && !p.hits.is_empty()
        {
            p.selected = (p.selected as isize + step).rem_euclid(p.hits.len() as isize) as usize;
            self.preview_palette_symbol(cx);
            cx.notify();
        }
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.restore_palette_origin(cx);
        if let Some(palette) = self.palette.take() {
            self.fade_out_palette(palette, cx);
        }
        self.focus_active_item(window, cx);
        cx.notify();
    }

    /// Keeps a dismissed palette drawn while it fades; focus has already moved on.
    fn fade_out_palette(&mut self, palette: Palette, cx: &mut Context<Self>) {
        let generation = self.next_generation();
        self.palette_closing = Some((palette, Closing::new(generation)));
        let t = cx.theme();
        let delay = motion::exit_delay(t.motion.reduced, t.motion.fast);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .palette_closing
                    .as_ref()
                    .is_some_and(|(_, c)| c.generation == generation)
                {
                    this.palette_closing = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Runs the selected row, or `row`; `beside` opens a file in a new pane next to the focused one.
    fn run_palette(
        &mut self,
        row: Option<usize>,
        beside: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .palette
            .as_ref()
            .is_none_or(|p| p.hits.get(row.unwrap_or(p.selected)).is_none())
        {
            return;
        }
        // Back to where the cursor was first, so Go Back returns there rather than to a preview.
        self.restore_palette_origin(cx);
        let Some(palette) = self.palette.take() else {
            return;
        };
        let Some((index, _)) = palette.hits.get(row.unwrap_or(palette.selected)) else {
            return;
        };
        let beside = beside || palette.mode == Mode::FilesBeside;
        let target = palette.entries.get(*index).map(|e| e.target.clone());
        self.fade_out_palette(palette, cx);
        self.focus_active_item(window, cx);
        match target {
            Some(Target::File(path)) if beside => self.open_file_beside(path, window, cx),
            Some(Target::File(path)) => self.open_file(path, window, cx),
            Some(Target::Command(action)) => window.dispatch_action(action, cx),
            Some(Target::Claude(command)) => self.start_claude_with(command, window, cx),
            Some(Target::Branch(pick)) => self.run_branch(pick, cx),
            Some(Target::Symbol(path, at)) => self.lsp.jump = Some((path, at)),
            Some(Target::Folder(root)) => self.open_folder(root, cx),
            None => {}
        }
        cx.notify();
    }

    pub(super) fn render_palette(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (palette, closing) = match (&self.palette, &self.palette_closing) {
            (Some(palette), _) => (palette, None),
            (None, Some((palette, closing))) => (palette, Some(*closing)),
            (None, None) => return None,
        };
        let t = cx.theme().clone();
        let rows: Vec<AnyElement> = palette
            .hits
            .iter()
            .enumerate()
            .map(|(row, (index, positions))| {
                let entry = &palette.entries[*index];
                let selected = row == palette.selected;
                // Positions index `key`; for files the label is the name at the end of it.
                let offset = entry.key.chars().count() - entry.label.chars().count();
                let label_bytes: Vec<usize> = entry.label.char_indices().map(|(b, _)| b).collect();
                let bold: Vec<_> = positions
                    .iter()
                    .filter(|&&p| p >= offset)
                    .filter_map(|&p| {
                        let b = *label_bytes.get(p - offset)?;
                        let len = entry.label[b..].chars().next()?.len_utf8();
                        Some((
                            b..b + len,
                            HighlightStyle {
                                font_weight: Some(FontWeight::SEMIBOLD),
                                ..Default::default()
                            },
                        ))
                    })
                    .collect();
                div()
                    .id(("palette-row", row))
                    .h(px(32.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.))
                    .cursor_pointer()
                    .text_color(if selected {
                        t.color.accent
                    } else {
                        t.color.content
                    })
                    .when(!selected, |el| el.hover(|s| s.bg(t.color.surface_hover)))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.run_palette(Some(row), event.modifiers().platform, window, cx)
                    }))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap(px(8.))
                            .min_w_0()
                            .overflow_hidden()
                            .child(
                                StyledText::new(SharedString::from(entry.label.clone()))
                                    .with_highlights(bold),
                            )
                            .children(
                                matches!(
                                    entry.target,
                                    Target::File(_)
                                        | Target::Branch(_)
                                        | Target::Symbol(..)
                                        | Target::Folder(_)
                                )
                                .then(|| entry.detail.clone())
                                .flatten()
                                .map(|d| {
                                    div()
                                        .text_size(t.typography.caption)
                                        .text_color(t.color.content_muted)
                                        .child(d)
                                }),
                            ),
                    )
                    .children(
                        matches!(entry.target, Target::Command(_))
                            .then(|| entry.detail.clone())
                            .flatten()
                            .or_else(|| entry.kind.map(str::to_string))
                            .map(|k| {
                                div()
                                    .font_family(t.typography.mono.clone())
                                    .text_size(t.typography.caption)
                                    .text_color(t.color.content_muted)
                                    .child(k)
                            }),
                    )
                    .into_any_element()
            })
            .collect();

        let empty = palette.hits.is_empty() && !palette.input.read(cx).text().is_empty();
        let message = palette.status.or(empty.then_some(palette.placeholder_hint));
        let panel = div()
            .id("palette")
            .w(px(560.))
            .max_h(px(44. + 32. * 8.5))
            .flex()
            .flex_col()
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_panel)
            .shadow(vec![t.popover_shadow()])
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .when(closing.is_some(), |el| {
                el.capture_any_mouse_down(|_, _, cx| cx.stop_propagation())
            })
            .child(
                div()
                    .h(px(44.))
                    .flex_none()
                    .px(px(14.))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(t.color.border)
                    .child(palette.input.clone()),
            )
            .child(
                div()
                    .id("palette-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .py(px(4.))
                    .when(palette.status.is_none(), |el| el.children(rows))
                    .children(message.map(|message| {
                        div()
                            .h(px(32.))
                            .px(px(12.))
                            .flex()
                            .items_center()
                            .text_color(t.color.content_muted)
                            .child(message)
                    })),
            );
        let panel = match closing {
            Some(closing) => motion::animate_exit(
                t.motion.reduced,
                panel,
                ("palette-exit", closing.generation),
                t.motion.fast,
                |el, d| el.opacity(1. - d).top(px(2. * d)),
            ),
            None => motion::animate_if(
                t.motion.reduced,
                panel,
                "palette-enter",
                Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
                |el, d| el.opacity(d).top(px(2. * (1. - d))),
            ),
        };
        Some(
            div()
                .id("palette-layer")
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(96.))
                .when(closing.is_none(), |el| {
                    el.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.close_palette(window, cx)),
                    )
                })
                .child(panel)
                .into_any_element(),
        )
    }
}

/// `⌘⇧P`-style label for a binding.
fn keystrokes(binding: &gpui::KeyBinding) -> String {
    binding
        .keystrokes()
        .iter()
        .map(|k| {
            let m = k.modifiers();
            let mut s = String::new();
            if m.control {
                s.push('⌃');
            }
            if m.alt {
                s.push('⌥');
            }
            if m.shift {
                s.push('⇧');
            }
            if m.platform {
                s.push('⌘');
            }
            let key = match k.key() {
                "enter" => "↩".to_string(),
                "left" => "←".into(),
                "right" => "→".into(),
                "up" => "↑".into(),
                "down" => "↓".into(),
                other => other.to_uppercase(),
            };
            s + &key
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Palette rows for symbols; `file` is the editor's own spelling of the path they are all in,
/// and `root` makes workspace results show where they live.
fn symbol_entries(symbols: Vec<Symbol>, file: Option<&Path>, root: Option<&Path>) -> Vec<Entry> {
    let canonical = root.map(document_key);
    symbols
        .into_iter()
        .map(|s| {
            let path = match (file, root, canonical.as_deref()) {
                (Some(file), _, _) => file.to_path_buf(),
                (None, Some(root), Some(canonical)) => s
                    .path
                    .strip_prefix(canonical)
                    .map_or_else(|_| s.path.clone(), |rest| root.join(rest)),
                _ => s.path.clone(),
            };
            let place = root.map(|r| path.strip_prefix(r).unwrap_or(&path).display().to_string());
            let detail = match (s.container, place) {
                (Some(c), Some(p)) => Some(format!("{c} · {p}")),
                (c, p) => c.or(p),
            };
            Entry {
                label: s.name.clone(),
                detail,
                kind: Some(symbol_kind_label(s.kind)),
                key: s.name,
                target: Target::Symbol(path, s.range.start),
            }
        })
        .collect()
}

fn claude_entries(query: &str) -> Vec<Entry> {
    let typed = query.trim();
    let mut names: Vec<String> = CLAUDE_COMMANDS.iter().map(|c| c.to_string()).collect();
    if !typed.is_empty() && !names.iter().any(|n| n == typed) {
        names.insert(0, typed.to_string());
    }
    names
        .into_iter()
        .map(|name| Entry {
            label: name.clone(),
            detail: None,
            kind: None,
            key: name.clone(),
            target: Target::Claude(name),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use athena_lsp::Range;

    #[test]
    fn the_first_character_picks_what_go_to_file_lists() {
        assert_eq!(mode_for_query("@main"), Mode::Symbols);
        assert_eq!(mode_for_query("#Server"), Mode::WorkspaceSymbols);
        assert_eq!(mode_for_query(">split"), Mode::Commands);
        assert_eq!(mode_for_query("src/@x"), Mode::Files);
        assert_eq!(mode_for_query(""), Mode::Files);
    }

    #[test]
    fn workspace_symbols_open_through_the_project_spelling_of_their_path() {
        let dir = std::env::temp_dir().join(format!("athena-palette-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        let canonical = dir.canonicalize().unwrap();
        let at = Position {
            line: 3,
            character: 5,
        };
        let symbol = |name: &str, container: Option<&str>| Symbol {
            name: name.into(),
            kind: 12,
            container: container.map(str::to_string),
            path: canonical.join("pkg/run.go"),
            range: Range { start: at, end: at },
        };
        let entries = symbol_entries(vec![symbol("Run", Some("pkg"))], None, Some(&dir));
        assert_eq!(entries[0].detail.as_deref(), Some("pkg · pkg/run.go"));
        assert_eq!(entries[0].kind, Some("function"));
        assert!(
            matches!(&entries[0].target, Target::Symbol(p, a) if *p == dir.join("pkg/run.go") && *a == at)
        );
        let open = dir.join("pkg/run.go");
        let entries = symbol_entries(vec![symbol("helper", None)], Some(&open), None);
        assert_eq!(entries[0].detail, None);
        assert!(matches!(&entries[0].target, Target::Symbol(p, _) if *p == open));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
