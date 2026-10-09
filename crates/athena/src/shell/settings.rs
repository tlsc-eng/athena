use std::borrow::Cow;
use std::rc::Rc;

use athena_editor::{EditorView, Indent, Lang, SaveSettings};
use athena_ui::{CODE_SIZE, CODE_ZOOM, Theme};
use athena_workspace::{Preferences, Workspace};
use gpui::{Context, Entity, Window};
use serde_json::Value;

use super::Shell;
use super::item::ItemView;
use super::notices::ToastAction;
use crate::settings::{self, Settings};

/// Problems listed on the toast; the rest are in app.log.
const SHOWN_PROBLEMS: usize = 3;

pub(super) struct SettingsState {
    pub(super) file: Settings,
    /// What workspace.json holds for the fields settings.json can set, so removing a key from
    /// settings.json brings back the choice made before.
    fallback: Preferences,
    /// What the file had wrong when it was read at launch; `Err` when none of it could be used.
    problems: Result<Vec<String>, Vec<String>>,
    /// Settings were put in force without a window, so those that need one (the theme, Claude
    /// Code integration, git autofetch) wait for the watcher's next read.
    unapplied: bool,
    toast: Option<u64>,
}

impl SettingsState {
    /// Reads settings.json and lets it override `workspace`.
    pub(super) fn load(workspace: &mut Workspace) -> Self {
        let (file, problems) = match settings::load() {
            Ok((file, problems)) => (file, Ok(problems)),
            Err(why) => (Settings::default(), Err(vec![why])),
        };
        let fallback = workspace.preferences();
        workspace.set_preferences(file.over(fallback));
        if let Some(size) = file.editor.font_size {
            workspace.ui.font_zoom = zoom_for(size);
        }
        Self {
            file,
            fallback,
            problems,
            unapplied: false,
            toast: None,
        }
    }

    /// `workspace` as workspace.json keeps it, its own values under settings.json's.
    pub(super) fn persisted<'a>(&self, workspace: Cow<'a, Workspace>) -> Cow<'a, Workspace> {
        if workspace.preferences() == self.fallback {
            return workspace;
        }
        let mut owned = workspace.into_owned();
        owned.set_preferences(self.fallback);
        Cow::Owned(owned)
    }
}

fn zoom_for(font_size: f32) -> i32 {
    ((font_size - CODE_SIZE).round() as i32).clamp(*CODE_ZOOM.start(), *CODE_ZOOM.end())
}

impl Shell {
    /// Applies settings.json whenever it changes, with a toast while it has problems.
    pub(super) fn start_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (changed, changes) = async_channel::unbounded::<()>();
        let watchers = settings::path()
            .map(|path| super::shortcuts::watch_keymap(path, changed))
            .unwrap_or_default();
        let problems = std::mem::replace(&mut self.settings.problems, Ok(Vec::new()));
        self.report_settings(problems, cx);
        cx.spawn_in(window, async move |this, cx| {
            let _watchers = watchers;
            while changes.recv().await.is_ok() {
                let reloaded = this.update_in(cx, |this, window, cx| {
                    let result = settings::load();
                    if let Ok((file, _)) = &result {
                        this.apply_settings(file.clone(), Some(window), cx);
                    }
                    this.report_settings(result.map(|(_, p)| p).map_err(|why| vec![why]), cx);
                });
                if reloaded.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// Shows what is wrong with settings.json, or clears the last report; `Err` means none of
    /// the file could be used, so the settings in force stay as they were.
    fn report_settings(
        &mut self,
        result: Result<Vec<String>, Vec<String>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.settings.toast.take() {
            self.dismiss_toast(id, cx);
        }
        let (problems, title) = match result {
            Ok(problems) if problems.is_empty() => return,
            Err(problems) if problems.is_empty() => return,
            Ok(problems) => {
                let title = match problems.len() {
                    1 => "settings.json has a problem; the other settings apply".to_string(),
                    n => format!("settings.json has {n} problems; the other settings apply"),
                };
                (problems, title)
            }
            Err(problems) => (
                problems,
                "settings.json could not be read; the settings in force stay".to_string(),
            ),
        };
        for problem in &problems {
            tracing::warn!("settings.json: {problem}");
        }
        let mut body: Vec<String> = problems.iter().take(SHOWN_PROBLEMS).cloned().collect();
        if problems.len() > SHOWN_PROBLEMS {
            body.push(format!("and {} more", problems.len() - SHOWN_PROBLEMS));
        }
        let action = ToastAction {
            label: "Open settings.json",
            run: Rc::new(|this: &mut Shell, window, cx| this.open_settings_file(window, cx)),
        };
        self.settings.toast = Some(self.action_toast(title, body.join("\n"), action, cx));
    }

    /// Puts `file` in force. Without a window the theme and Claude Code integration are left
    /// alone, as a toggle that just wrote them has already applied them.
    fn apply_settings(
        &mut self,
        file: Settings,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        if file == self.settings.file && !(self.settings.unapplied && window.is_some()) {
            return;
        }
        let old = std::mem::replace(&mut self.settings.file, file);
        let before = self.workspace.preferences();
        let mut now = self.settings.file.over(self.settings.fallback);
        self.settings.unapplied = window.is_none();
        if window.is_none() {
            // The watcher reads the file again in a moment, with the window.
            now.theme = before.theme;
            now.ide_integration = before.ide_integration;
        }
        self.workspace.set_preferences(now);
        if let Some(window) = window {
            if now.theme != before.theme {
                athena_ui::set_appearance(super::appearance::resolve(now.theme, window), cx);
            }
            if now.ide_integration != before.ide_integration {
                match now.ide_integration {
                    true => self.start_ide(window, cx),
                    false => self.stop_ide(window, cx),
                }
            }
            self.set_autofetch(self.settings.file.git_autofetch(), window, cx);
        }
        let font_size = self.settings.file.editor.font_size;
        if font_size != old.editor.font_size
            && let Some(size) = font_size
        {
            self.workspace.ui.font_zoom = zoom_for(size);
            cx.global_mut::<Theme>()
                .set_code_zoom(self.workspace.ui.font_zoom);
        }
        let editors: Vec<Entity<EditorView>> = self
            .items
            .values()
            .filter_map(|view| match view {
                ItemView::Editor(e) => Some(e.clone()),
                _ => None,
            })
            .collect();
        for editor in &editors {
            self.apply_editor_settings(editor, cx);
        }
        self.lsp_settings_changed(cx);
        self.schedule_save(cx);
        cx.notify();
    }

    /// Format on save, whitespace tidying, word wrap, auto save and inlay hints for `editor`,
    /// by its language; an `.editorconfig` still wins over the tidying settings.
    pub(super) fn apply_editor_settings(
        &self,
        editor: &Entity<EditorView>,
        cx: &mut Context<Self>,
    ) {
        let lang = editor.read(cx).lang();
        let e = self.settings.file.editor_for(lang);
        let format = e.format_on_save.or(self.workspace.format_on_save);
        let wrap = e.word_wrap.unwrap_or(self.workspace.word_wrap);
        let ms = e
            .autosave_delay_ms
            .unwrap_or(self.workspace.autosave_delay_ms);
        let autosave = (ms > 0).then(|| std::time::Duration::from_millis(ms));
        let inlays = e.inlay_hints != Some(false);
        let tidy = SaveSettings {
            trim_trailing_whitespace: e.trim_trailing_whitespace,
            insert_final_newline: e.insert_final_newline,
        };
        editor.update(cx, |v, cx| {
            v.set_format_on_save(format);
            v.set_word_wrap_default(wrap, cx);
            v.set_autosave(autosave, cx);
            v.set_inlay_hints(inlays, cx);
            v.set_save_settings(tidy);
        });
    }

    /// Settings a newly opened editor takes once: those above, and `tab_size` for a file whose
    /// indentation shows no style of its own.
    pub(super) fn editor_settings_opened(
        &self,
        editor: &Entity<EditorView>,
        cx: &mut Context<Self>,
    ) {
        self.apply_editor_settings(editor, cx);
        let lang = editor.read(cx).lang();
        let Some(size) = self.settings.file.editor_for(lang).tab_size else {
            return;
        };
        let undetectable = lang != Some(Lang::Go)
            && editor.read(cx).text().is_some_and(|text| {
                !text
                    .lines()
                    .any(|l| l.starts_with([' ', '\t']) && !l.trim().is_empty())
            });
        if undetectable {
            editor.update(cx, |v, cx| v.set_indent(Indent::Spaces(size), cx));
        }
    }

    /// Writes one setting to settings.json, as a palette toggle changes it; if the file cannot be
    /// written the choice still holds, kept in workspace.json as before settings.json existed.
    pub(super) fn write_setting(&mut self, keys: &[&str], value: Value, cx: &mut Context<Self>) {
        let written = settings::write(keys, &value)
            .map_err(|e| format!("{e:#}"))
            .and_then(|text| settings::parse(&text));
        match written {
            Ok((file, _)) => self.apply_settings(file, None, cx),
            Err(why) => {
                tracing::warn!("settings.json not updated: {why}");
                self.settings.fallback = self.workspace.preferences();
                self.schedule_save(cx);
                self.transient_notice(
                    "settings.json was not updated",
                    format!("{why}\nThe change holds, and is kept in workspace.json."),
                    cx,
                );
            }
        }
    }

    /// Opens settings.json as a tab, creating it with every setting commented out first if needed.
    pub(super) fn open_settings_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match settings::ensure_file() {
            Ok(path) if self.workspace.active.is_some() => self.open_file(path, window, cx),
            Ok(path) => self.transient_notice(
                "Open a project to edit settings",
                format!(
                    "settings.json opens as a tab in a project: {}",
                    path.display()
                ),
                cx,
            ),
            Err(e) => self.transient_notice("Could not create settings.json", format!("{e:#}"), cx),
        }
    }

    /// Shows inlay hints with a curated set turned on for each server, or hides them all.
    pub(super) fn toggle_inlay_hints(&mut self, cx: &mut Context<Self>) {
        let on = self.settings.file.editor.inlay_hints != Some(true);
        self.write_setting(&["editor", "inlay_hints"], on.into(), cx);
        let (title, body) = match on {
            true => (
                "Inlay hints are on",
                "Parameter names and inferred types show in the code, as far as the language \
                 server offers them; lsp.<server> in settings.json picks which.",
            ),
            false => ("Inlay hints are off", "Language servers' hints are hidden."),
        };
        self.transient_notice(title, body, cx);
    }

    /// Editors and terminals at `zoom` steps from the default size; written to settings.json
    /// only when the user keeps `editor.font_size` there.
    pub(super) fn font_zoom_changed(&mut self, zoom: i32, cx: &mut Context<Self>) {
        if self.settings.file.editor.font_size.is_some() {
            let size = CODE_SIZE + zoom as f32;
            let size = match size.fract() == 0. {
                true => Value::from(size as i64),
                false => Value::from(size),
            };
            self.write_setting(&["editor", "font_size"], size, cx);
        }
    }
}
