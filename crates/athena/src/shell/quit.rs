use std::path::Path;

use athena_editor::EditorView;
use gpui::{Context, Entity, PromptLevel, Window};

use super::Shell;
use super::item::ItemView;

impl Shell {
    /// Editors with unsaved changes, in every project or only the one at `root`.
    fn dirty_editors(&self, root: Option<&Path>, cx: &Context<Self>) -> Vec<Entity<EditorView>> {
        // Tabs on the same file share one buffer; list and save it once.
        let mut paths = std::collections::HashSet::new();
        self.items
            .iter()
            .filter(|((r, _), _)| root.is_none_or(|root| r == root))
            .filter_map(|(_, v)| match v {
                ItemView::Editor(e)
                    if e.read(cx).is_dirty()
                        && paths.insert(super::lsp::document_key(e.read(cx).path())) =>
                {
                    Some(e.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Quits, first asking what to do with unsaved files. Shells keep running in the daemon.
    pub(super) fn quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = self.dirty_editors(None, cx);
        self.settle_unsaved(dirty, "quitting", window, cx, |this, cx| {
            this.quit_settled = true;
            this.save_now(cx);
            cx.quit();
        });
    }

    /// A quit that skipped the Save question (Dock, logout, `osascript`) saves what auto save would
    /// and keeps recovery copies of the rest, so nothing unsaved is dropped.
    pub(super) fn flush_unsaved(&mut self, cx: &mut Context<Self>) {
        if self.quit_settled {
            return;
        }
        if self.autosave_delay().is_some() {
            for editor in self.dirty_editors(None, cx) {
                editor.update(cx, |e, cx| e.save(cx));
            }
        }
        let Ok(dir) = athena_proto::recovery_dir() else {
            return;
        };
        match athena_editor::recovery::write_dirty(&dir) {
            Ok(0) => {}
            Ok(n) => tracing::info!("kept copies of {n} unsaved files in {}", dir.display()),
            Err(err) => tracing::error!("could not keep copies of unsaved files: {err}"),
        }
    }

    /// Closes the active project once its unsaved files are settled: auto save saves them quietly,
    /// as closing a tab does; otherwise the user is asked, as on quit.
    pub(super) fn close_active_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        let mut dirty = self.dirty_editors(Some(&root), cx);
        if self.autosave_delay().is_some() {
            dirty.retain(|e| !e.update(cx, |e, cx| e.save(cx)));
        }
        self.settle_unsaved(dirty, "closing the project", window, cx, move |this, cx| {
            this.remove_project(&root, cx)
        });
    }

    /// Runs `then` at once if nothing is unsaved, else after Save (when every save worked) or
    /// Don't Save; `before` ends the question "Save changes to … before …?".
    fn settle_unsaved(
        &mut self,
        dirty: Vec<Entity<EditorView>>,
        before: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        if dirty.is_empty() {
            return then(self, cx);
        }
        let names: Vec<String> = dirty
            .iter()
            .filter_map(|e| {
                e.read(cx)
                    .path()
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .collect();
        let message = match names.as_slice() {
            [one] => format!("Save changes to {one} before {before}?"),
            _ => format!("Save changes to {} files before {before}?", names.len()),
        };
        let detail = names.join(", ");
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            Some(&detail),
            &["Save", "Don't Save", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(choice) = answer.await else { return };
            let _ = this.update(cx, |this, cx| {
                let go = match choice {
                    0 => dirty.iter().all(|e| e.update(cx, |e, cx| e.save(cx))),
                    1 => true,
                    _ => false,
                };
                if go {
                    then(this, cx);
                }
            });
        })
        .detach();
    }

    /// Drops the project at `root`, which may no longer be the active one once a prompt is answered.
    fn remove_project(&mut self, root: &Path, cx: &mut Context<Self>) {
        let Some(index) = self.workspace.projects.iter().position(|p| p.root == root) else {
            return;
        };
        let was_active = self.workspace.active == Some(index);
        self.drop_project_items(root, cx);
        self.history.forget_root(root);
        self.lsp_project_closed(root);
        self.git_project_closed(root);
        self.workspace.close_project(index);
        self.rail_from = self.workspace.active.unwrap_or(0);
        if was_active {
            self.zoomed = None;
            self.focus_pending = true;
            self.switch_count += 1;
        }
        self.schedule_save(cx);
        cx.notify();
    }
}
