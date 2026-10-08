use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use athena_editor::EditorView;
use athena_lsp::{Client, CodeAction, FileChange, Position, Range, TextEdit};
use athena_ui::MenuItem;
use gpui::{Context, Entity, EntityId, Subscription, Task, WeakEntity, Window};

use super::Shell;
use super::lsp::{NO_SERVER, document_key};
use crate::actions::ApplyCodeAction;

/// The cursor rests this long before the server is asked whether its line has a quick fix.
const LIGHTBULB_DELAY: Duration = Duration::from_millis(250);

#[derive(Default)]
pub(super) struct CodeActionState {
    /// One subscription per editor, for the cursor moves that move the lightbulb.
    watched: HashMap<EntityId, (WeakEntity<EditorView>, Subscription)>,
    lightbulb_task: Option<Task<()>>,
    /// The actions the open Cmd+. menu offers, with the server and file they came from.
    menu: Option<(Rc<Client>, PathBuf, Vec<CodeAction>)>,
}

/// Quick fixes first (preferred ones leading), then refactorings, then source actions, as
/// VS Code orders its menu; each group keeps the server's order.
fn menu_order(actions: &mut [CodeAction]) {
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
    actions.sort_by_key(rank);
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

/// The edits gopls's organize imports makes to `doc`, empty when imports are already in order.
pub(super) async fn organize_imports(client: &Client, doc: &Path) -> Vec<TextEdit> {
    let start = Position {
        line: 0,
        character: 0,
    };
    let range = Range { start, end: start };
    let found = client
        .code_actions(doc, range, Vec::new(), Some(&["source.organizeImports"]))
        .await;
    let action = match found {
        Ok(actions) => actions.into_iter().next(),
        Err(why) => {
            tracing::debug!("organize imports failed: {why}");
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

    /// Asks again, once the cursor rests, whether its line has a quick fix; diagnostics that
    /// change under it ask too.
    pub(super) fn schedule_lightbulb(
        &mut self,
        editor: &Entity<EditorView>,
        cx: &mut Context<Self>,
    ) {
        let doc = document_key(editor.read(cx).path());
        let line = editor.read(cx).cursor_line() as u32;
        let diagnostics: Vec<serde_json::Value> = self
            .lsp
            .diagnostics
            .get(&doc)
            .map(|(_, list)| {
                list.iter()
                    .filter(|d| d.range.start.line <= line && line <= d.range.end.line)
                    .map(|d| d.raw.clone())
                    .collect()
            })
            .unwrap_or_default();
        let client = self.document_client(&doc);
        let (Some(client), false) = (client, diagnostics.is_empty()) else {
            self.code_actions.lightbulb_task = None;
            editor.update(cx, |e, cx| e.set_lightbulb(None, cx));
            return;
        };
        let weak = editor.downgrade();
        self.code_actions.lightbulb_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(LIGHTBULB_DELAY).await;
            let Ok(Some(at)) = weak.read_with(cx, |e, _| e.cursor_utf16()) else {
                return;
            };
            let at = Position {
                line: at.0,
                character: at.1,
            };
            let range = Range { start: at, end: at };
            let fixes = client
                .code_actions(&doc, range, diagnostics, Some(&["quickfix"]))
                .await
                .unwrap_or_default();
            let shown = fixes.iter().any(|a| a.disabled.is_none()).then_some(line);
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
        self.flush_change(&doc, &editor, cx);
        let Some(client) = self.document_client(&doc) else {
            return self.lsp_failed("No code actions", NO_SERVER.into(), cx);
        };
        let Some((line, character)) = editor.read(cx).cursor_utf16() else {
            return;
        };
        let at = Position { line, character };
        let diagnostics: Vec<serde_json::Value> = self
            .lsp
            .diagnostics
            .get(&doc)
            .map(|(_, list)| {
                list.iter()
                    .filter(|d| {
                        d.range.start <= at && at <= d.range.end || d.range.start.line == line
                    })
                    .map(|d| d.raw.clone())
                    .collect()
            })
            .unwrap_or_default();
        let weak = editor.downgrade();
        tracing::debug!(path = %doc.display(), line, "code actions");
        cx.spawn_in(window, async move |this, cx| {
            let range = Range { start: at, end: at };
            let found = client.code_actions(&doc, range, diagnostics, None).await;
            let _ = this.update_in(cx, |this, window, cx| {
                let mut actions = match found {
                    Ok(actions) => actions,
                    Err(why) => return this.lsp_failed("No code actions", why, cx),
                };
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
                for (i, action) in actions.iter().enumerate() {
                    if i > 0 && group(&actions[i - 1]) != group(action) {
                        items.push(MenuItem::separator());
                    }
                    items.push(
                        MenuItem::new(action.title.clone(), move |window, cx| {
                            window.dispatch_action(Box::new(ApplyCodeAction(i)), cx)
                        })
                        .disabled(action.disabled.is_some()),
                    );
                }
                this.code_actions.menu = Some((client, doc, actions));
                this.open_context_menu(position, items, window, cx);
            });
        })
        .detach();
    }

    /// Runs the action picked from the Cmd+. menu: its edit, then its command.
    pub(super) fn apply_code_action(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some((client, doc, mut actions)) = self.code_actions.menu.take() else {
            return;
        };
        if index >= actions.len() {
            return;
        }
        let action = actions.swap_remove(index);
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
            if let Some(edit) = &action.edit {
                let applied = this.update(cx, |this, cx| this.apply_workspace_edit(edit, cx));
                match applied {
                    Ok(Ok(_)) => {}
                    Ok(Err(why)) => {
                        let _ = this.update(cx, |this, cx| this.lsp_failed(&action.title, why, cx));
                        return;
                    }
                    Err(_) => return,
                }
            }
            if let Some(command) = &action.command
                && let Err(why) = client.execute_command(command).await
            {
                tracing::warn!("{} failed: {why}", command.command);
                let _ = this.update(cx, |this, cx| this.lsp_failed(&action.title, why, cx));
            }
        })
        .detach();
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
            action("Organize imports", "source.organizeImports", false),
            action("Extract function", "refactor.extract", false),
            action("Add import", "quickfix", false),
            action("Fix typo", "quickfix", true),
        ];
        menu_order(&mut list);
        let titles: Vec<&str> = list.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Fix typo",
                "Add import",
                "Extract function",
                "Organize imports"
            ]
        );
    }
}
