use std::ops::Range;
use std::rc::Rc;

use athena_editor::{EditorView, Lens};
use athena_lsp::{Client, CodeLens};
use gpui::{Context, Entity};

use super::Shell;
use super::lsp::document_key;

/// Answers an editor keeps, so a click on a lens still on screen finds it after newer answers.
const KEPT_ANSWERS: usize = 3;

/// One code lens answer: the request it answered, the server, and its resolved lenses.
pub(super) type LensAnswer = (u64, Rc<Client>, Vec<CodeLens>);

/// Whether Athena can run a lens: the server runs its command, or it lists references.
fn runnable(client: &Client, lens: &CodeLens) -> bool {
    !lens.locations().is_empty()
        || lens
            .command
            .as_ref()
            .is_some_and(|c| client.executes(&c.command))
}

/// The lenses as the editor shows them, each named by its index.
fn shown(client: &Client, lenses: &[CodeLens]) -> Vec<Lens> {
    lenses
        .iter()
        .enumerate()
        .filter_map(|(i, lens)| {
            Some(Lens {
                line: lens.range.start.line,
                title: lens.command.as_ref()?.title.clone(),
                id: runnable(client, lens).then_some(i),
            })
        })
        .collect()
}

impl Shell {
    /// Asks the server for the lenses on `lines`, resolving together those that came without
    /// a command, as VS Code resolves only the lenses it shows.
    pub(super) fn lsp_code_lens(
        &mut self,
        editor: &Entity<EditorView>,
        request: u64,
        lines: Range<u32>,
        cx: &mut Context<Self>,
    ) {
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, editor, cx);
        let Some(client) = self
            .document_client(&doc)
            .filter(|c| c.supports("/codeLensProvider"))
        else {
            editor.update(cx, |e, cx| e.show_code_lenses(request, Vec::new(), cx));
            return;
        };
        let weak = editor.downgrade();
        let id = editor.entity_id();
        cx.spawn(async move |this, cx| {
            let found = client.code_lens(&doc).await.unwrap_or_else(|why| {
                tracing::debug!("code lens failed: {why}");
                Vec::new()
            });
            let wanted: Vec<CodeLens> = found
                .into_iter()
                .filter(|l| lines.contains(&l.range.start.line))
                .collect();
            let resolved =
                futures::future::join_all(wanted.iter().map(|l| client.resolve_code_lens(l))).await;
            let lenses: Vec<CodeLens> = resolved
                .into_iter()
                .zip(wanted)
                .map(|(resolved, asked)| resolved.unwrap_or(asked))
                .filter(|l| l.command.is_some())
                .collect();
            tracing::debug!("code lens → {}", lenses.len());
            let shown = shown(&client, &lenses);
            let _ = this.update(cx, |this, _| {
                let kept = this.lsp.lenses.entry(id).or_default();
                kept.push((request, client, lenses));
                if kept.len() > KEPT_ANSWERS {
                    kept.remove(0);
                }
            });
            let _ = weak.update(cx, |e, cx| e.show_code_lenses(request, shown, cx));
        })
        .detach();
    }

    /// Runs a clicked lens: lists the references it carries, or has the server run its command
    /// once the project may run code.
    pub(super) fn run_code_lens(
        &mut self,
        editor: &Entity<EditorView>,
        request: u64,
        id: usize,
        cx: &mut Context<Self>,
    ) {
        let Some((client, lens)) = self
            .lsp
            .lenses
            .get(&editor.entity_id())
            .and_then(|kept| kept.iter().find(|(r, _, _)| *r == request))
            .and_then(|(_, client, lenses)| Some((client.clone(), lenses.get(id)?.clone())))
        else {
            return;
        };
        let places = lens.locations();
        if !places.is_empty() {
            return self.show_locations(places, cx);
        }
        let Some(command) = lens.command else {
            return;
        };
        let path = editor.read(cx).path().to_path_buf();
        self.run_project_command(
            &path,
            client,
            command.clone(),
            move |this, client, cx| this.spawn_command(client, command, cx),
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athena_lsp::{Command, Position};

    fn lens(line: u32, command: Option<(&str, &str)>, arguments: serde_json::Value) -> CodeLens {
        let at = Position { line, character: 0 };
        CodeLens {
            range: athena_lsp::Range { start: at, end: at },
            command: command.map(|(title, command)| Command {
                title: title.into(),
                command: command.into(),
                arguments: Some(arguments),
            }),
            raw: serde_json::Value::Null,
        }
    }

    #[test]
    fn only_lenses_athena_can_run_are_clickable_and_unresolved_ones_are_not_shown() {
        let (client, _) = Client::start_local(
            athena_lsp::ServerKind::Go,
            "/nonexistent/gopls".into(),
            "/tmp".into(),
            athena_lsp::Config::default(),
        );
        let references = serde_json::json!(["file:///p/a.ts", {"line": 1, "character": 0},
            [{"uri": "file:///p/b.ts", "range": {"start": {"line": 2, "character": 0},
                                                  "end": {"line": 2, "character": 3}}}]]);
        let lenses = [
            lens(
                1,
                Some(("1 reference", "editor.action.showReferences")),
                references,
            ),
            lens(
                4,
                Some(("run go generate", "gopls.generate")),
                serde_json::json!([]),
            ),
            lens(6, None, serde_json::Value::Null),
        ];
        let shown = shown(&client, &lenses);
        assert_eq!(
            shown,
            [
                Lens {
                    line: 1,
                    title: "1 reference".into(),
                    id: Some(0)
                },
                Lens {
                    line: 4,
                    title: "run go generate".into(),
                    id: None
                },
            ],
            "a server that never started runs nothing; a lens with no command is not shown"
        );
    }
}
