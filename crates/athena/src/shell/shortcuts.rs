use std::rc::Rc;

use athena_workspace::watch::FolderWatcher;
use gpui::{Context, Window};

use super::Shell;
use super::notices::ToastAction;
use crate::keymap;

/// Problems listed on the toast; the rest are in app.log.
const SHOWN_PROBLEMS: usize = 3;

impl Shell {
    /// Applies keymap.json now and whenever it changes, with a toast while it has problems.
    pub(super) fn start_keymap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (changed, changes) = async_channel::unbounded::<()>();
        let watcher = keymap::path().ok().and_then(|path| {
            let dir = path.parent()?.to_path_buf();
            let file = std::sync::Mutex::new(keymap::FileChange::new(path));
            FolderWatcher::new(&dir, move |_| {
                if file.lock().is_ok_and(|mut f| f.changed()) {
                    let _ = changed.send_blocking(());
                }
            })
            .inspect_err(|e| tracing::warn!("not watching keymap.json for changes: {e}"))
            .ok()
        });
        cx.spawn_in(window, async move |this, cx| {
            let _watcher = watcher;
            let mut toast = None;
            loop {
                let applied = this.update(cx, |this, cx| {
                    if let Some(id) = toast.take() {
                        this.dismiss_toast(id, cx);
                    }
                    toast = this.apply_keymap(cx);
                });
                if applied.is_err() || changes.recv().await.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// Returns the toast reporting the file's problems, if it has any.
    fn apply_keymap(&mut self, cx: &mut Context<Self>) -> Option<u64> {
        let problems = keymap::reload(cx);
        crate::actions::rebuild_menus(&self.workspace.recent, cx);
        if problems.is_empty() {
            return None;
        }
        for problem in &problems {
            tracing::warn!("keymap.json: {problem}");
        }
        let mut body: Vec<&str> = problems
            .iter()
            .take(SHOWN_PROBLEMS)
            .map(String::as_str)
            .collect();
        let more = format!("and {} more", problems.len().saturating_sub(SHOWN_PROBLEMS));
        if problems.len() > SHOWN_PROBLEMS {
            body.push(&more);
        }
        let title = match problems.len() {
            1 => "keymap.json has a problem; the other shortcuts apply".to_string(),
            n => format!("keymap.json has {n} problems; the other shortcuts apply"),
        };
        let action = ToastAction {
            label: "Open keymap.json",
            run: Rc::new(|this: &mut Shell, window, cx| this.open_keymap_file(window, cx)),
        };
        Some(self.action_toast(title, body.join("\n"), action, cx))
    }

    /// Opens keymap.json as a tab, creating it with examples first if needed.
    pub(super) fn open_keymap_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match keymap::ensure_file() {
            Ok(path) if self.workspace.active.is_some() => self.open_file(path, window, cx),
            Ok(path) => self.transient_notice(
                "Open a project to edit shortcuts",
                format!(
                    "keymap.json opens as a tab in a project: {}",
                    path.display()
                ),
                cx,
            ),
            Err(e) => self.transient_notice("Could not create keymap.json", format!("{e:#}"), cx),
        }
    }
}
