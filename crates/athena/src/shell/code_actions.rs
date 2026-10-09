use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use athena_editor::EditorView;
use athena_lsp::{Client, CodeAction, Command, FileChange, Position, Range, TextEdit};
use athena_ui::MenuItem;
use athena_workspace::LinterTrust;
use gpui::{Context, Entity, EntityId, PromptLevel, Subscription, Task, WeakEntity, Window};

use super::Shell;
use super::edits::AskedAt;
use super::lsp::{CONFIRMED, NO_SERVER, confirm_buttons, document_key};
use crate::actions::ApplyCodeAction;
use crate::settings::Lightbulb;

/// The cursor rests this long before the server is asked whether its line has a quick fix.
const LIGHTBULB_DELAY: Duration = Duration::from_millis(250);
/// How long a command allowed from its prompt waits for the servers the allowing restarts.
const RESTART_WAIT: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(super) struct CodeActionState {
    /// One subscription per editor, for the cursor moves that move the lightbulb.
    watched: HashMap<EntityId, (WeakEntity<EditorView>, Subscription)>,
    lightbulb_task: Option<Task<()>>,
    menu: Option<Menu>,
    /// Whether a server command is asking to let the project's code run.
    asking: bool,
}

/// What a server command may do in a project, by the trust the project was given.
#[derive(Debug, PartialEq, Eq)]
enum Gate {
    Run,
    Ask,
    Refuse,
}

/// Server commands such as gopls's `go generate` and `go test` lenses build or run the project's
/// code, so they run only in a project allowed to, as Debug does.
fn command_gate(trust: Option<LinterTrust>) -> Gate {
    match trust {
        Some(LinterTrust::Allowed) => Gate::Run,
        Some(LinterTrust::NotAsked) => Gate::Ask,
        Some(LinterTrust::Denied) | None => Gate::Refuse,
    }
}

/// The actions the open Cmd+. menu offers, each with the index of the server it came from, the
/// file they are for, and the file versions they were asked for at.
type Menu = (Vec<Rc<Client>>, PathBuf, Vec<(usize, CodeAction)>, AskedAt);

/// Quick fixes first (preferred ones leading), then refactorings, then source actions, as
/// VS Code orders its menu; each group keeps the servers' order.
fn menu_order<T>(actions: &mut [(T, CodeAction)]) {
    let rank = |a: &CodeAction| {
        let kind = a.kind.as_deref().unwrap_or_default();
        match () {
            _ if a.is_quickfix() && a.preferred => 0,
            _ if a.is_quickfix() => 1,
            _ if kind.starts_with("refactor") => 2,
            _ if kind.starts_with("source") => 3,
            _ => 4,
        }
    };
    actions.sort_by_key(|(_, a)| rank(a));
}

fn group(a: &CodeAction) -> u8 {
    let kind = a.kind.as_deref().unwrap_or_default();
    match () {
        _ if a.is_quickfix() => 0,
        _ if kind.starts_with("refactor") => 1,
        _ if kind.starts_with("source") => 2,
        _ => 3,
    }
}

/// Format-on-save edits with organize imports folded in: both are made against the same text, so
/// a formatting edit inside the import block the import edits rewrite is dropped (that block
/// comes out formatted already) and the rest apply together in one step.
pub(super) fn merge_save_edits(imports: Vec<TextEdit>, format: Vec<TextEdit>) -> Vec<TextEdit> {
    let overlaps = |a: &Range, b: &Range| a.start < b.end && b.start < a.end || a.start == b.start;
    let kept: Vec<TextEdit> = format
        .into_iter()
        .filter(|f| !imports.iter().any(|i| overlaps(&i.range, &f.range)))
        .collect();
    imports.into_iter().chain(kept).collect()
}

/// Runs a server command, such as a code action's or a code lens's; any edit it makes arrives
/// as `workspace/applyEdit`.
async fn run_command(client: &Client, command: &Command) -> Result<(), String> {
    client
        .execute_command(command)
        .await
        .map(drop)
        .inspect_err(|why| {
            tracing::warn!("{} failed: {why}", command.command);
        })
}

/// The edits a whole-file source action such as `source.organizeImports` (gopls) or
/// `source.fixAll.eslint` makes to `doc`, empty when it has nothing to change.
pub(super) async fn source_action_edits(client: &Client, doc: &Path, kind: &str) -> Vec<TextEdit> {
    let start = Position {
        line: 0,
        character: 0,
    };
    let range = Range { start, end: start };
    let found = client
        .code_actions(doc, range, Vec::new(), Some(&[kind]))
        .await;
    let action = match found {
        Ok(actions) => actions.into_iter().next(),
        Err(why) => {
            tracing::debug!("{kind} failed: {why}");
            None
        }
    };
    let action = match action {
        Some(a) if a.needs_resolve() => client.resolve_code_action(&a).await.ok(),
        other => other,
    };
    let Some(edit) = action.and_then(|a| a.edit) else {
        return Vec::new();
    };
    edit.changes
        .into_iter()
        .filter_map(|c| match c {
            FileChange::Edit { path, edits, .. } if document_key(&path) == doc => Some(edits),
            _ => None,
        })
        .flatten()
        .collect()
}

/// The server that took over from `old` at `root` once it is ready to run `command`; `None` if
/// none is within [`RESTART_WAIT`].
async fn restarted_client(
    this: &WeakEntity<Shell>,
    cx: &mut gpui::AsyncApp,
    root: &Path,
    old: &Rc<Client>,
    command: &Command,
) -> Option<Rc<Client>> {
    let until = Instant::now() + RESTART_WAIT;
    while Instant::now() < until {
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
        let found = this
            .update(cx, |this, _| {
                this.project_clients(root)
                    .into_iter()
                    .find(|c| !Rc::ptr_eq(c, old) && c.executes(&command.command))
            })
            .ok()?;
        if found.is_some() {
            return found;
        }
    }
    None
}

impl Shell {
    /// Follows an editor's cursor so its line shows a lightbulb when it has a quick fix.
    pub(super) fn watch_for_lightbulb(
        &mut self,
        editor: &Entity<EditorView>,
        cx: &mut Context<Self>,
    ) {
        self.code_actions
            .watched
            .retain(|_, (e, _)| e.upgrade().is_some());
        if self.code_actions.watched.contains_key(&editor.entity_id()) {
            return;
        }
        let subscription = cx.subscribe(editor, |this, editor, event, cx| {
            if matches!(event, athena_editor::EditorEvent::CursorMoved { .. }) {
                this.schedule_lightbulb(&editor, cx);
            }
        });
        self.code_actions
            .watched
            .insert(editor.entity_id(), (editor.downgrade(), subscription));
    }

    /// Asks again, once the cursor rests, whether its line has a quick fix, or with
    /// `editor.lightbulb` set to `"all"` any refactoring; diagnostics that change under it ask too.
    pub(super) fn schedule_lightbulb(
        &mut self,
        editor: &Entity<EditorView>,
        cx: &mut Context<Self>,
    ) {
        let doc = document_key(editor.read(cx).path());
        let line = editor.read(cx).cursor_line() as u32;
        let lightbulb = {
            let root = self.project_root_of(editor.read(cx).path());
            self.settings_for(root.as_deref())
                .editor_for(editor.read(cx).lang())
                .lightbulb
                .unwrap_or(Lightbulb::QuickFixes)
        };
        let kinds: &'static [&'static str] = match lightbulb {
            Lightbulb::All => &["quickfix", "refactor"],
            _ => &["quickfix"],
        };
        let servers: Vec<(Rc<Client>, Vec<serde_json::Value>)> = self
            .document_servers(&doc)
            .into_iter()
            .map(|(client, list)| {
                let on_line = list
                    .into_iter()
                    .filter(|d| d.range.start.line <= line && line <= d.range.end.line)
                    .map(|d| d.raw.clone())
                    .collect::<Vec<_>>();
                (client, on_line)
            })
            // Quick fixes answer diagnostics; refactorings are worth asking for anywhere.
            .filter(|(_, on_line)| lightbulb == Lightbulb::All || !on_line.is_empty())
            .collect();
        if servers.is_empty() || lightbulb == Lightbulb::Off {
            self.code_actions.lightbulb_task = None;
            editor.update(cx, |e, cx| e.set_lightbulb(None, cx));
            return;
        }
        let weak = editor.downgrade();
        self.code_actions.lightbulb_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LIGHTBULB_DELAY).await;
            // Sent only now, as the cursor rests, so typing still batches its changes.
            let Some(editor) = weak.upgrade() else {
                return;
            };
            let _ = this.update(cx, |this, cx| this.flush_change(&doc, &editor, cx));
            drop(editor);
            let Ok(Some(at)) = weak.read_with(cx, |e, _| e.cursor_utf16()) else {
                return;
            };
            let at = Position {
                line: at.0,
                character: at.1,
            };
            let range = Range { start: at, end: at };
            let asked = servers.into_iter().map(|(client, diagnostics)| {
                let doc = doc.clone();
                async move {
                    client
                        .code_actions(&doc, range, diagnostics, Some(kinds))
                        .await
                        .unwrap_or_default()
                }
            });
            let fixes = futures::future::join_all(asked).await;
            let shown = fixes
                .iter()
                .flatten()
                .any(|a| a.disabled.is_none())
                .then_some(line);
            let _ = weak.update(cx, |e, cx| {
                if e.cursor_line() as u32 == line {
                    e.set_lightbulb(shown, cx);
                }
            });
        }));
    }

    /// Re-checks the lightbulb of every editor on `doc`, as new diagnostics may add or end a fix.
    pub(super) fn diagnostics_moved_lightbulb(&mut self, doc: &Path, cx: &mut Context<Self>) {
        if let Some(editor) = self
            .focused_editor()
            .filter(|e| document_key(e.read(cx).path()) == doc)
        {
            self.schedule_lightbulb(&editor, cx);
        }
    }

    /// Cmd+. or the lightbulb: lists the fixes and refactorings at the cursor in a menu there.
    pub(super) fn show_code_actions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.focused_editor() else {
            return;
        };
        let doc = document_key(editor.read(cx).path());
        let asked = self.versions_for_request(cx);
        let servers = self.document_servers(&doc);
        if servers.is_empty() {
            return self.lsp_failed("No code actions", NO_SERVER.into(), cx);
        }
        let Some((line, character)) = editor.read(cx).cursor_utf16() else {
            return;
        };
        let at = Position { line, character };
        let (clients, diagnostics): (Vec<Rc<Client>>, Vec<Vec<serde_json::Value>>) = servers
            .into_iter()
            .map(|(client, list)| {
                let here = list
                    .into_iter()
                    .filter(|d| {
                        d.range.start <= at && at <= d.range.end || d.range.start.line == line
                    })
                    .map(|d| d.raw.clone())
                    .collect();
                (client, here)
            })
            .unzip();
        let weak = editor.downgrade();
        tracing::debug!(path = %doc.display(), line, servers = clients.len(), "code actions");
        cx.spawn_in(window, async move |this, cx| {
            let range = Range { start: at, end: at };
            let asked_each = clients
                .iter()
                .zip(diagnostics)
                .map(|(client, diagnostics)| client.code_actions(&doc, range, diagnostics, None));
            let found = futures::future::join_all(asked_each).await;
            let _ = this.update_in(cx, |this, window, cx| {
                let mut actions = Vec::new();
                let mut failure = None;
                for (i, result) in found.into_iter().enumerate() {
                    match result {
                        Ok(list) => actions.extend(list.into_iter().map(|a| (i, a))),
                        Err(why) => failure = failure.or(Some(why)),
                    }
                }
                if let (true, Some(why)) = (actions.is_empty(), failure) {
                    return this.lsp_failed("No code actions", why, cx);
                }
                if actions.is_empty() {
                    return this.lsp_failed(
                        "No code actions",
                        "There are no fixes or refactorings here.".into(),
                        cx,
                    );
                }
                let Some(position) = weak.upgrade().and_then(|e| e.read(cx).cursor_anchor()) else {
                    return;
                };
                menu_order(&mut actions);
                let mut items = Vec::new();
                for (i, (_, action)) in actions.iter().enumerate() {
                    if i > 0 && group(&actions[i - 1].1) != group(action) {
                        items.push(MenuItem::separator());
                    }
                    items.push(
                        MenuItem::new(action.title.clone(), move |window, cx| {
                            window.dispatch_action(Box::new(ApplyCodeAction(i)), cx)
                        })
                        .disabled(action.disabled.is_some()),
                    );
                }
                this.code_actions.menu = Some((clients, doc, actions, asked));
                this.open_context_menu(position, items, window, cx);
            });
        })
        .detach();
    }

    /// Runs the action picked from the Cmd+. menu: its edit, then its command.
    pub(super) fn apply_code_action(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some((clients, doc, mut actions, asked)) = self.code_actions.menu.take() else {
            return;
        };
        if index >= actions.len() {
            return;
        }
        let (server, action) = actions.swap_remove(index);
        let client = clients[server].clone();
        tracing::debug!(title = %action.title, "code action");
        if let Some(editor) = self.editor_for_doc(&doc, cx) {
            self.flush_change(&doc, &editor, cx);
        }
        let resolves = client.supports("/codeActionProvider/resolveProvider");
        cx.spawn(async move |this, cx| {
            let action = match action.needs_resolve() && resolves {
                true => match client.resolve_code_action(&action).await {
                    Ok(resolved) => resolved,
                    Err(why) => {
                        let _ = this.update(cx, |this, cx| this.lsp_failed(&action.title, why, cx));
                        return;
                    }
                },
                false => action,
            };
            let _ = this.update(cx, |this, cx| {
                let Some(command) = action.command.clone() else {
                    this.apply_action_edit(&action, &asked, cx);
                    return;
                };
                // The edit waits with the command, so a refused action changes nothing.
                this.run_project_command(
                    &doc,
                    client,
                    command,
                    move |this, client, cx| {
                        if this.apply_action_edit(&action, &asked, cx)
                            && let Some(command) = action.command
                        {
                            this.spawn_command(client, command, cx);
                        }
                    },
                    cx,
                );
            });
        })
        .detach();
    }

    /// Applies a code action's edit, if it has one; false when it could not be.
    fn apply_action_edit(
        &mut self,
        action: &CodeAction,
        asked: &AskedAt,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(edit) = &action.edit else {
            return true;
        };
        match self.apply_requested_edit(edit, asked, cx) {
            Ok(_) => true,
            Err(why) => {
                self.lsp_failed(&action.title, why, cx);
                false
            }
        }
    }

    /// Has `client` run `command`, reporting a failure under the command's title.
    pub(super) fn spawn_command(
        &mut self,
        client: Rc<Client>,
        command: Command,
        cx: &mut Context<Self>,
    ) {
        tracing::debug!(command = command.command, "server command runs");
        cx.spawn(async move |this, cx| {
            if let Err(why) = run_command(&client, &command).await {
                let _ = this.update(cx, |this, cx| this.lsp_failed(&command.title, why, cx));
            }
        })
        .detach();
    }

    /// Calls `run` with the server to run `command` on once the project holding `path` may run
    /// its code, asking first if it never was asked; allowing restarts the servers, so `run` gets
    /// the restarted one.
    pub(super) fn run_project_command(
        &mut self,
        path: &Path,
        client: Rc<Client>,
        command: Command,
        run: impl FnOnce(&mut Self, Rc<Client>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let root = self.project_root_of(path);
        let name = root
            .as_ref()
            .and_then(|r| self.workspace.projects.iter().find(|p| p.root == *r))
            .map(|p| p.name());
        let trust = root.as_deref().and_then(|r| self.linter_trust(r));
        match (command_gate(trust), root, name) {
            (Gate::Run, ..) => run(self, client, cx),
            (Gate::Ask, Some(root), Some(name)) => {
                self.ask_to_run(root, name, client, command, run, cx)
            }
            (_, _, name) => {
                let body = match name {
                    Some(name) => format!(
                        "It has the language server build or run {name}'s code, which you chose \
                         not to allow. To allow it, run \"Allow project code\" from the command \
                         palette."
                    ),
                    None => "It has the language server build or run code, which Athena does \
                             only for files of a project you open and allow."
                        .into(),
                };
                self.transient_notice(
                    format!("\"{}\" waits for this project to be allowed", command.title),
                    body,
                    cx,
                );
            }
        }
    }

    fn ask_to_run(
        &mut self,
        root: PathBuf,
        name: String,
        client: Rc<Client>,
        command: Command,
        run: impl FnOnce(&mut Self, Rc<Client>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        if std::mem::replace(&mut self.code_actions.asking, true) {
            return;
        }
        let shell = cx.entity();
        // Deferred: a prompt cannot open while the window is busy handling this click.
        cx.defer(move |cx| {
            let window = cx
                .active_window()
                .or_else(|| cx.windows().into_iter().next());
            let asked = window.map(|window| {
                window.update(cx, |_, window, cx| {
                    let answer = window.prompt(
                        PromptLevel::Warning,
                        &format!("Allow {name}'s code to run?"),
                        Some(&format!(
                            "\"{}\" has the language server build or run {name}'s code on this \
                             Mac. Allowing also lets the linters and TypeScript it installs run, \
                             and its settings choose what language servers run.",
                            command.title
                        )),
                        &confirm_buttons("Cancel", "Allow and Run"),
                        cx,
                    );
                    shell.update(cx, |_, cx| {
                        cx.spawn(async move |this, cx| {
                            let answer = answer.await;
                            let allowed = this.update(cx, |this, cx| {
                                this.code_actions.asking = false;
                                let allowed = answer == Ok(CONFIRMED)
                                    && this.active_root().as_ref() == Some(&root);
                                if allowed {
                                    this.change_linter_trust(true, cx);
                                }
                                allowed
                            });
                            if !allowed.unwrap_or(false) {
                                return;
                            }
                            let restarted =
                                restarted_client(&this, cx, &root, &client, &command).await;
                            let _ = this.update(cx, |this, cx| match restarted {
                                Some(client) => run(this, client, cx),
                                None => this.lsp_failed(
                                    &command.title,
                                    "The language server did not restart after the project was \
                                     allowed. Try again once it has."
                                        .into(),
                                    cx,
                                ),
                            });
                        })
                        .detach();
                    });
                })
            });
            if !matches!(asked, Some(Ok(()))) {
                shell.update(cx, |this, _| this.code_actions.asking = false);
            }
        });
    }

    fn editor_for_doc(&self, doc: &Path, cx: &Context<Self>) -> Option<Entity<EditorView>> {
        self.editors_showing(doc, cx).into_iter().next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(start: (u32, u32), end: (u32, u32), text: &str) -> TextEdit {
        let pos = |(line, character)| Position { line, character };
        TextEdit {
            range: Range {
                start: pos(start),
                end: pos(end),
            },
            text: text.into(),
        }
    }

    #[test]
    fn organize_imports_and_formatting_merge_without_fighting_over_the_import_block() {
        let text = "package main\n\nimport \"os\"\n\nfunc  main() { fmt.Println( ) }\n";
        let imports = vec![edit((2, 7), (2, 11), "\"fmt\"")];
        let format = vec![
            edit((2, 8), (2, 10), "OS"),
            edit((4, 4), (4, 6), " "),
            edit((4, 27), (4, 28), ""),
        ];
        let merged = merge_save_edits(imports, format);
        assert_eq!(
            merged.len(),
            3,
            "the formatting edit inside the import is dropped"
        );
        let out = athena_lsp::apply_text_edits(text, &merged).unwrap();
        assert_eq!(
            out,
            "package main\n\nimport \"fmt\"\n\nfunc main() { fmt.Println() }\n"
        );
        assert_eq!(
            merge_save_edits(Vec::new(), vec![edit((0, 0), (0, 1), "x")]).len(),
            1
        );
    }

    #[test]
    fn server_commands_run_only_in_an_allowed_project_and_ask_where_never_asked() {
        assert_eq!(command_gate(Some(LinterTrust::Allowed)), Gate::Run);
        assert_eq!(command_gate(Some(LinterTrust::NotAsked)), Gate::Ask);
        assert_eq!(command_gate(Some(LinterTrust::Denied)), Gate::Refuse);
        assert_eq!(
            command_gate(None),
            Gate::Refuse,
            "a file outside any project"
        );
    }

    #[test]
    fn the_menu_lists_preferred_fixes_then_fixes_refactorings_and_source_actions() {
        let action = |title: &str, kind: &str, preferred: bool| CodeAction {
            title: title.into(),
            kind: Some(kind.into()),
            preferred,
            disabled: None,
            edit: None,
            command: None,
            raw: serde_json::Value::Null,
        };
        let mut list = vec![
            (
                0,
                action("Organize imports", "source.organizeImports", false),
            ),
            (0, action("Extract function", "refactor.extract", false)),
            (
                1,
                action("Disable no-console for this line", "quickfix", false),
            ),
            (0, action("Add import", "quickfix", false)),
            (1, action("Fix typo", "quickfix", true)),
        ];
        menu_order(&mut list);
        let titles: Vec<&str> = list.iter().map(|(_, a)| a.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Fix typo",
                "Disable no-console for this line",
                "Add import",
                "Extract function",
                "Organize imports"
            ]
        );
    }
}
