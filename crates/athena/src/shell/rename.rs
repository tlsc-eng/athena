use athena_lsp::{Position, RenameTarget};
use gpui::{Context, Window};

use super::Shell;
use super::lsp::{NO_SERVER, document_key};

/// VS Code's words when nothing renameable is under the cursor.
const NOT_RENAMEABLE: &str = "The element can't be renamed.";

impl Shell {
    /// F2: asks the server what the symbol at the cursor is called, then opens the rename field.
    pub(super) fn lsp_rename_start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.focused_editor() else {
            return;
        };
        let doc = document_key(editor.read(cx).path());
        self.flush_change(&doc, &editor, cx);
        let Some((line, character)) = editor.read(cx).cursor_utf16() else {
            return;
        };
        let at = Position { line, character };
        let Some(client) = self.document_client(&doc) else {
            return self.lsp_failed("Rename failed", NO_SERVER.into(), cx);
        };
        let word = editor.read(cx).word_at_cursor();
        if !client.supports("/renameProvider/prepareProvider") {
            let Some((start, name)) = word else {
                return self.lsp_failed("Rename failed", NOT_RENAMEABLE.into(), cx);
            };
            self.lsp.renaming = Some((doc, at, editor.downgrade()));
            editor.update(cx, |e, cx| e.show_rename(start, name, window, cx));
            return;
        }
        let weak = editor.downgrade();
        cx.spawn_in(window, async move |this, cx| {
            let target = client.prepare_rename(&doc, at).await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(editor) = weak.upgrade() else {
                    return;
                };
                let (start, name) = match target {
                    Ok(Some(RenameTarget::Range(range, placeholder))) => {
                        let start = (range.start.line, range.start.character);
                        let end = (range.end.line, range.end.character);
                        let name = placeholder
                            .or_else(|| editor.read(cx).text_between(start, end))
                            .unwrap_or_default();
                        (start, name)
                    }
                    Ok(Some(RenameTarget::Word)) => match word {
                        Some((start, name)) => (start, name),
                        None => return this.lsp_failed("Rename failed", NOT_RENAMEABLE.into(), cx),
                    },
                    Ok(None) => return this.lsp_failed("Rename failed", NOT_RENAMEABLE.into(), cx),
                    Err(why) => {
                        tracing::debug!("prepareRename: {why}");
                        return this.lsp_failed("Rename failed", why, cx);
                    }
                };
                this.lsp.renaming = Some((doc, at, weak));
                editor.update(cx, |e, cx| e.show_rename(start, name, window, cx));
            });
        })
        .detach();
    }

    /// Enter in the rename field: renames everywhere the server finds the symbol.
    pub(super) fn lsp_rename_confirm(&mut self, name: String, cx: &mut Context<Self>) {
        let Some((doc, at, _)) = self.lsp.renaming.take() else {
            return;
        };
        let asked = self.versions_for_request(cx);
        let Some(client) = self.document_client(&doc) else {
            return self.lsp_failed("Rename failed", NO_SERVER.into(), cx);
        };
        tracing::debug!(path = %doc.display(), line = at.line, %name, "rename");
        cx.spawn(async move |this, cx| {
            let edit = client.rename(&doc, at, &name).await;
            let _ = this.update(cx, |this, cx| match edit {
                Ok(edit) if edit.is_empty() => this.lsp_failed(
                    "Rename failed",
                    "The server found nothing to rename.".into(),
                    cx,
                ),
                Ok(edit) => {
                    if let Err(why) = this.apply_requested_edit(&edit, &asked, cx) {
                        this.lsp_failed("Rename failed", why, cx);
                    }
                }
                Err(why) => {
                    tracing::warn!("rename failed: {why}");
                    this.lsp_failed("Rename failed", why, cx);
                }
            });
        })
        .detach();
    }
}
