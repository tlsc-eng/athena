use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use athena_workspace::ItemKind;
use athena_workspace::watch::FolderWatcher;
use gpui::{AppContext as _, Context, Entity, Window};

use super::Shell;
use super::item::ItemView;
use super::notices::ToastAction;
use super::settings::ConfigFile;
use super::shortcuts_ui::{KeymapEdit, ShortcutsEvent, ShortcutsView};
use crate::keymap;

/// Problems listed on the toast; the rest are in app.log.
const SHOWN_PROBLEMS: usize = 3;

/// Watches the folders holding `files`, and for a symlinked one its target's folder too, with one
/// watcher per folder; each file's sender hears when its bytes change.
pub(super) fn watch_files(files: Vec<(PathBuf, async_channel::Sender<()>)>) -> Vec<FolderWatcher> {
    type Watched = (
        Option<PathBuf>,
        Arc<Mutex<keymap::FileChange>>,
        async_channel::Sender<()>,
    );
    let mut folders: Vec<(PathBuf, Vec<Watched>)> = Vec::new();
    for (path, changed) in files {
        let file = Arc::new(Mutex::new(keymap::FileChange::new(path.clone())));
        let target = keymap::link_target(&path);
        let watched = path
            .parent()
            .map(|dir| (dir.to_path_buf(), None))
            .into_iter()
            .chain(target.and_then(|t| Some((t.parent()?.to_path_buf(), Some(t)))));
        for (dir, only) in watched {
            let entry = (only, file.clone(), changed.clone());
            match folders.iter_mut().find(|(d, _)| *d == dir) {
                Some((_, on)) => on.push(entry),
                None => folders.push((dir, vec![entry])),
            }
        }
    }
    folders
        .into_iter()
        .filter_map(|(dir, watched)| {
            let root = dir.clone();
            FolderWatcher::new(&dir, move |paths| {
                for (only, file, changed) in &watched {
                    // The target's folder may be a whole dotfiles checkout, or the home folder.
                    let relevant = only
                        .as_ref()
                        .is_none_or(|target| paths.iter().any(|p| p == target || *p == root));
                    if relevant && file.lock().is_ok_and(|mut f| f.changed()) {
                        let _ = changed.send_blocking(());
                    }
                }
            })
            .inspect_err(|e| tracing::warn!("not watching {}: {e}", dir.display()))
            .ok()
        })
        .collect()
}

impl Shell {
    /// Applies keymap.json now and whenever it changes, with a toast while it has problems.
    pub(super) fn start_keymap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let changes = self.config_changes(ConfigFile::Keymap);
        cx.spawn_in(window, async move |this, cx| {
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
        self.refresh_shortcuts_views(cx);
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

    /// The Keyboard Shortcuts tab, or keymap.json when no project is open to hold a tab.
    pub(super) fn open_shortcuts_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.workspace.active {
            Some(_) => self.open_kind(ItemKind::Shortcuts, window, cx),
            None => self.open_keymap_file(window, cx),
        }
    }

    pub(super) fn new_shortcuts_view(&mut self, cx: &mut Context<Self>) -> ItemView {
        let view = cx.new(ShortcutsView::new);
        cx.subscribe(&view, |this, _, event: &ShortcutsEvent, cx| match event {
            ShortcutsEvent::Edit(edit) => this.edit_keymap(edit, cx),
            ShortcutsEvent::OpenJson => match keymap::ensure_file() {
                Ok(path) => {
                    this.pending_open = Some(path);
                    cx.notify();
                }
                Err(e) => {
                    this.transient_notice("Could not create keymap.json", format!("{e:#}"), cx)
                }
            },
        })
        .detach();
        ItemView::Shortcuts(view)
    }

    /// Writes a change from the Keyboard Shortcuts tab to keymap.json and puts it in force now,
    /// ahead of the file watcher.
    fn edit_keymap(&mut self, edit: &KeymapEdit, cx: &mut Context<Self>) {
        let written = keymap::ensure_file()
            .and_then(|path| crate::settings::edit_at(&path, |text| edit.apply(text)));
        match written {
            Ok(_) => {
                keymap::reload(cx);
                crate::actions::rebuild_menus(&self.workspace.recent, cx);
            }
            Err(e) => self.transient_notice("keymap.json was not updated", format!("{e:#}"), cx),
        }
        self.refresh_shortcuts_views(cx);
    }

    fn refresh_shortcuts_views(&mut self, cx: &mut Context<Self>) {
        let views: Vec<Entity<ShortcutsView>> = self
            .items
            .values()
            .filter_map(|v| match v {
                ItemView::Shortcuts(v) => Some(v.clone()),
                _ => None,
            })
            .collect();
        for view in views {
            view.update(cx, |v, cx| v.reload(cx));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn one_watcher_serves_settings_and_keymap_and_tells_their_changes_apart() {
        let dir = std::env::temp_dir().join(format!("athena-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let (settings, keymap) = (dir.join("settings.json"), dir.join("keymap.json"));
        std::fs::write(&settings, "{}").unwrap();
        std::fs::write(&keymap, "[]").unwrap();
        let (settings_changed, settings_changes) = async_channel::unbounded();
        let (keymap_changed, keymap_changes) = async_channel::unbounded();
        let watchers = watch_files(vec![
            (settings.clone(), settings_changed),
            (keymap.clone(), keymap_changed),
        ]);
        assert_eq!(watchers.len(), 1, "one folder, one watcher");
        // FSEvents may report nothing for changes made right after the stream starts.
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(&keymap, "[ ]").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while keymap_changes.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        // Past the debounce, so a settings.json report would have arrived with it.
        std::thread::sleep(Duration::from_millis(400));
        let heard = (keymap_changes.len(), settings_changes.len());
        drop(watchers);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(heard, (1, 0));
    }
}
