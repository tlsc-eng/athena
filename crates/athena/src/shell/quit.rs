use gpui::{Context, PromptLevel, Window};

use super::Shell;
use super::item::ItemView;

impl Shell {
    fn dirty_editors(&self, cx: &Context<Self>) -> Vec<gpui::Entity<athena_editor::EditorView>> {
        self.items
            .values()
            .filter_map(|v| match v {
                ItemView::Editor(e) if e.read(cx).is_dirty() => Some(e.clone()),
                _ => None,
            })
            .collect()
    }

    /// Quits, first asking what to do with unsaved files. Shells keep running in the daemon.
    pub(super) fn quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = self.dirty_editors(cx);
        if dirty.is_empty() {
            self.save_now();
            cx.quit();
            return;
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
            [one] => format!("Save changes to {one} before quitting?"),
            _ => format!("Save changes to {} files before quitting?", names.len()),
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
                let quit = match choice {
                    0 => dirty.iter().all(|e| e.update(cx, |e, cx| e.save(cx))),
                    1 => true,
                    _ => false,
                };
                if quit {
                    this.save_now();
                    cx.quit();
                }
            });
        })
        .detach();
    }
}
