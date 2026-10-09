use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use athena_editor::{EditorView, Indent, Lang, SaveSettings};
use athena_ui::{CODE_SIZE, CODE_ZOOM, Theme};
use athena_workspace::{LinterTrust, Preferences, Workspace};
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
    /// Each project's `.vscode/settings.json` with its `.athena/settings.json` laid over it.
    pub(super) projects: HashMap<PathBuf, ProjectSettings>,
    project_toast: Option<u64>,
}

pub(super) struct ProjectSettings {
    settings: Settings,
    /// The two files' modification times when read, `None` for a missing one.
    stamps: [Option<SystemTime>; 2],
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Reads a project's settings files; problems in its `.athena/settings.json` are returned to
/// report, while `.vscode/settings.json` is read leniently as VS Code's extensions share it.
fn read_project(root: &Path) -> (Settings, Vec<String>) {
    let vscode = std::fs::read_to_string(root.join(settings::VSCODE_FILE))
        .map(|text| settings::parse_vscode(&text, root))
        .unwrap_or_default();
    let (own, problems) = match std::fs::read_to_string(root.join(settings::PROJECT_FILE)) {
        Ok(text) => match settings::parse_project(&text) {
            Ok(parsed) => parsed,
            Err(why) => (Settings::default(), vec![why]),
        },
        Err(_) => Default::default(),
    };
    (vscode.overlaid(&own), problems)
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
            projects: HashMap::new(),
            project_toast: None,
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

    /// The project holding `path`, the innermost when projects nest.
    pub(super) fn project_root_of(&self, path: &Path) -> Option<PathBuf> {
        self.workspace
            .projects
            .iter()
            .map(|p| &p.root)
            .filter(|root| {
                path.starts_with(root) || path.starts_with(super::lsp::document_key(root))
            })
            .max_by_key(|root| root.as_os_str().len())
            .cloned()
    }

    /// The settings in force for files of the project at `root`: settings.json with the
    /// project's own laid over it, those choosing what runs only once the project is trusted.
    pub(super) fn settings_for(&self, root: Option<&Path>) -> Cow<'_, Settings> {
        let Some((root, project)) = root.and_then(|r| Some((r, self.settings.projects.get(r)?)))
        else {
            return Cow::Borrowed(&self.settings.file);
        };
        Cow::Owned(match self.linter_trust(root) {
            Some(LinterTrust::Allowed) => self.settings.file.overlaid(&project.settings),
            _ => self
                .settings
                .file
                .overlaid(&project.settings.without_programs()),
        })
    }

    /// Whether the project at `root` has settings that wait for it to be trusted.
    pub(super) fn project_changes_programs(&self, root: &Path) -> bool {
        self.settings
            .projects
            .get(root)
            .is_some_and(|p| p.settings.changes_programs())
    }

    /// Reads the project's settings files again if they changed since; true if they had.
    fn refresh_project_settings(&mut self, root: &Path, cx: &mut Context<Self>) -> bool {
        let stamps = [
            modified(&root.join(settings::VSCODE_FILE)),
            modified(&root.join(settings::PROJECT_FILE)),
        ];
        if self
            .settings
            .projects
            .get(root)
            .is_some_and(|p| p.stamps == stamps)
        {
            return false;
        }
        let (settings, problems) = read_project(root);
        if let Some(id) = self.settings.project_toast.take() {
            self.dismiss_toast(id, cx);
        }
        if !problems.is_empty() {
            for problem in &problems {
                tracing::warn!("{}: {problem}", root.join(settings::PROJECT_FILE).display());
            }
            let title = match problems.len() {
                1 => ".athena/settings.json has a problem; the other settings apply".to_string(),
                n => format!(".athena/settings.json has {n} problems; the other settings apply"),
            };
            let path = root.join(settings::PROJECT_FILE);
            let action = ToastAction {
                label: "Open .athena/settings.json",
                run: Rc::new(move |this: &mut Shell, window, cx| {
                    this.open_file(path.clone(), window, cx)
                }),
            };
            let body: Vec<String> = problems.into_iter().take(SHOWN_PROBLEMS).collect();
            self.settings.project_toast =
                Some(self.action_toast(title, body.join("\n"), action, cx));
        }
        let asks =
            settings.changes_programs() && self.linter_trust(root) == Some(LinterTrust::NotAsked);
        self.settings
            .projects
            .insert(root.to_path_buf(), ProjectSettings { settings, stamps });
        if asks {
            self.ask_project_trust(root, cx);
        }
        true
    }

    /// Puts the project's settings in force again: its editors and running servers follow.
    pub(super) fn project_settings_changed(&mut self, root: &Path, cx: &mut Context<Self>) {
        let editors: Vec<Entity<EditorView>> = self
            .items
            .iter()
            .filter(|((r, _), _)| r == root)
            .filter_map(|(_, view)| match view {
                ItemView::Editor(e) => Some(e.clone()),
                _ => None,
            })
            .collect();
        for editor in &editors {
            self.apply_editor_settings(editor, cx);
        }
        self.lsp_settings_changed(cx);
    }

    /// A saved file that is one of a project's settings files puts it in force.
    pub(super) fn project_settings_saved(&mut self, path: &Path, cx: &mut Context<Self>) {
        let ours = path.ends_with(settings::PROJECT_FILE) || path.ends_with(settings::VSCODE_FILE);
        let Some(root) = self.project_root_of(path).filter(|_| ours) else {
            return;
        };
        if self.refresh_project_settings(&root, cx) {
            self.project_settings_changed(&root, cx);
        }
    }

    /// Format on save, whitespace tidying, word wrap, auto save and inlay hints for `editor`,
    /// by its language; an `.editorconfig` still wins over the tidying settings.
    pub(super) fn apply_editor_settings(
        &self,
        editor: &Entity<EditorView>,
        cx: &mut Context<Self>,
    ) {
        let lang = editor.read(cx).lang();
        let root = self.project_root_of(editor.read(cx).path());
        let file = self.settings_for(root.as_deref());
        let e = file.editor_for(lang);
        let script = matches!(lang, Some(Lang::TypeScript | Lang::Tsx | Lang::JavaScript));
        let eslint_fixes = script && e.fix_all_on_save.unwrap_or(file.eslint_fix_on_save());
        let organize =
            e.organize_imports_on_save == Some(true) && (lang == Some(Lang::Go) || script);
        let format = match eslint_fixes || organize {
            true => Some(true),
            false => e.format_on_save.or(self.workspace.format_on_save),
        };
        let wrap = file.editor.word_wrap.unwrap_or(self.workspace.word_wrap);
        let wrap_language = file.language_word_wrap(lang);
        let ms = e
            .autosave_delay_ms
            .unwrap_or(self.workspace.autosave_delay_ms);
        let autosave = (ms > 0).then(|| std::time::Duration::from_millis(ms));
        let inlays = e.inlay_hints != Some(false);
        let brackets = e.bracket_pair_colorization != Some(false);
        let linked = e.linked_editing == Some(true);
        let minimap = e.minimap != Some(false);
        let tidy = SaveSettings {
            trim_trailing_whitespace: e.trim_trailing_whitespace,
            insert_final_newline: e.insert_final_newline,
        };
        editor.update(cx, |v, cx| {
            v.set_format_on_save(format);
            v.set_word_wrap_default(wrap, cx);
            v.set_word_wrap_language(wrap_language, cx);
            v.set_autosave(autosave, cx);
            v.set_inlay_hints(inlays, cx);
            v.set_bracket_pair_colorization(brackets, cx);
            v.set_linked_editing(linked, cx);
            v.set_minimap(minimap, cx);
            v.set_save_settings(tidy);
        });
    }

    /// Settings a newly opened editor takes once: those above, and `tab_size` for a file whose
    /// indentation shows no style of its own.
    pub(super) fn editor_settings_opened(
        &mut self,
        editor: &Entity<EditorView>,
        cx: &mut Context<Self>,
    ) {
        let root = self.project_root_of(editor.read(cx).path());
        if let Some(root) = &root {
            self.refresh_project_settings(root, cx);
        }
        self.apply_editor_settings(editor, cx);
        let lang = editor.read(cx).lang();
        let Some(size) = self.settings_for(root.as_deref()).editor_for(lang).tab_size else {
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

    /// Shows or hides every editor's minimap, as VS Code's View: Toggle Minimap.
    pub(super) fn toggle_minimap(&mut self, cx: &mut Context<Self>) {
        let on = self.settings.file.editor.minimap == Some(false);
        self.write_setting(&["editor", "minimap", "enabled"], on.into(), cx);
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
