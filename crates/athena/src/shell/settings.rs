use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use athena_editor::{EditorEvent, EditorView, Indent, Lang, SaveSettings};
use athena_lsp::{Diagnostic, Position, Range, ServerKind, Severity};
use athena_ui::{CODE_SIZE, CODE_ZOOM, Theme};
use athena_workspace::{ItemKind, LinterTrust, Preferences, ThemeChoice, Workspace};
use gpui::{AppContext as _, Context, Entity, Window};
use serde_json::{Value, json};

use super::Shell;
use super::item::ItemView;
use super::lsp::document_key;
use super::notices::ToastAction;
use super::settings_ui::{Scope, SettingsEvent, SettingsView};
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
    /// What is wrong in settings.json, keymap.json or a project's settings file as edited, by
    /// canonical path, shown in its editor beside language servers' diagnostics.
    file_problems: HashMap<PathBuf, Vec<Diagnostic>>,
    /// VS Code's JSON server was found, so JSON files open in it with Athena's schemas.
    json_server: bool,
}

/// Athena's own files, checked as they are edited.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConfigFile {
    Settings,
    ProjectSettings,
    Keymap,
}

/// Byte `at` of `text` as a language server counts it: line, and UTF-16 units into it.
fn position_of(text: &str, at: usize) -> Position {
    let before = &text[..at];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    Position {
        line: before.matches('\n').count() as u32,
        character: before[line_start..].encode_utf16().count() as u32,
    }
}

fn diagnostic(
    text: &str,
    (range, why, fatal): (std::ops::Range<usize>, String, bool),
) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: position_of(text, range.start),
            end: position_of(text, range.end),
        },
        severity: if fatal {
            Severity::Error
        } else {
            Severity::Warning
        },
        message: why,
        source: Some("athena".into()),
        raw: Value::Null,
    }
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
            file_problems: HashMap::new(),
            json_server: false,
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
        self.find_json_server(cx);
        cx.spawn_in(window, async move |this, cx| {
            let _watchers = watchers;
            while changes.recv().await.is_ok() {
                let reloaded = this.update_in(cx, |this, window, cx| {
                    let result = settings::load();
                    if let Ok((file, _)) = &result {
                        this.apply_settings(file.clone(), Some(window), cx);
                    }
                    this.report_settings(result.map(|(_, p)| p).map_err(|why| vec![why]), cx);
                    this.refresh_settings_views(cx);
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
            self.refresh_settings_views(cx);
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
        let semantic = e.semantic_highlighting != Some(false);
        let lenses = e.code_lens != Some(false);
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
            v.set_semantic_highlighting(semantic, cx);
            v.set_code_lens(lenses, cx);
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
        self.watch_config_problems(editor, cx);
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
        self.refresh_settings_views(cx);
    }

    /// Removes a setting from settings.json, as the Settings tab's Reset does.
    fn unset_setting(&mut self, keys: &[&str], cx: &mut Context<Self>) {
        let unset = settings::ensure_file()
            .and_then(|path| settings::unset_at(&path, keys))
            .map_err(|e| format!("{e:#}"))
            .and_then(|text| settings::parse(&text));
        match unset {
            Ok((file, _)) => self.apply_settings(file, None, cx),
            Err(why) => self.transient_notice("settings.json was not updated", why, cx),
        }
        self.refresh_settings_views(cx);
    }

    /// Sets, or with `None` removes, a setting in the project's `.athena/settings.json`.
    fn write_project_setting(
        &mut self,
        root: &Path,
        keys: &[&str],
        value: Option<&Value>,
        cx: &mut Context<Self>,
    ) {
        if value.is_some() && !settings::schema::project_may_set(keys) {
            let why = format!(
                "\"{}\" is not a project setting; set it in settings.json",
                keys.join(".")
            );
            self.transient_notice(".athena/settings.json was not updated", why, cx);
            return;
        }
        let written = settings::ensure_project_file(root).and_then(|path| {
            match value {
                Some(value) => settings::write_at(&path, keys, value),
                None => settings::unset_at(&path, keys),
            }
            .map(|_| path)
        });
        match written {
            Ok(path) => self.project_settings_saved(&path, cx),
            Err(e) => self.transient_notice(
                ".athena/settings.json was not updated",
                format!("{e:#}"),
                cx,
            ),
        }
        self.refresh_settings_views(cx);
    }

    /// Cmd+,: the Settings tab, or settings.json when no project is open to hold a tab.
    pub(super) fn open_settings_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.workspace.active {
            Some(_) => self.open_kind(ItemKind::Settings, window, cx),
            None => self.open_settings_file(window, cx),
        }
    }

    pub(super) fn new_settings_view(&mut self, root: &Path, cx: &mut Context<Self>) -> ItemView {
        let fallback = self.settings_fallback();
        let view = cx.new(|cx| SettingsView::new(root.to_path_buf(), fallback, cx));
        let root = root.to_path_buf();
        cx.subscribe(
            &view,
            move |this, _, event: &SettingsEvent, cx| match event {
                SettingsEvent::Set {
                    scope: Scope::User,
                    keys,
                    value,
                } => this.write_setting(keys, value.clone(), cx),
                SettingsEvent::Unset {
                    scope: Scope::User,
                    keys,
                } => this.unset_setting(keys, cx),
                SettingsEvent::Set {
                    scope: Scope::Project,
                    keys,
                    value,
                } => this.write_project_setting(&root, keys, Some(value), cx),
                SettingsEvent::Unset {
                    scope: Scope::Project,
                    keys,
                } => this.write_project_setting(&root, keys, None, cx),
                SettingsEvent::OpenJson(scope) => {
                    let path = match scope {
                        Scope::User => settings::ensure_file(),
                        Scope::Project => settings::ensure_project_file(&root),
                    };
                    match path {
                        Ok(path) => {
                            this.pending_open = Some(path);
                            cx.notify();
                        }
                        Err(e) => {
                            this.transient_notice("Could not create the file", format!("{e:#}"), cx)
                        }
                    }
                }
            },
        )
        .detach();
        ItemView::Settings(view)
    }

    /// What applies where no settings file sets a setting: workspace.json's last choice.
    fn settings_fallback(&self) -> HashMap<String, Value> {
        let p = self.settings.fallback;
        let theme = match p.theme {
            ThemeChoice::System => "system",
            ThemeChoice::Light => "light",
            ThemeChoice::Dark => "dark",
        };
        let font = CODE_SIZE + self.workspace.ui.font_zoom as f32;
        [
            (
                "editor.format_on_save",
                json!(p.format_on_save.unwrap_or(false)),
            ),
            ("editor.word_wrap", json!(p.word_wrap)),
            ("editor.autosave_delay_ms", json!(p.autosave_delay_ms)),
            ("editor.font_size", json!(font as i64)),
            ("ide_integration", json!(p.ide_integration)),
            ("theme", json!(theme)),
            ("window.zoom_level", json!(self.workspace.ui.zoom_level)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    /// Shows open Settings tabs the files as they are now.
    fn refresh_settings_views(&mut self, cx: &mut Context<Self>) {
        let views: Vec<Entity<SettingsView>> = self
            .items
            .values()
            .filter_map(|v| match v {
                ItemView::Settings(v) => Some(v.clone()),
                _ => None,
            })
            .collect();
        if views.is_empty() {
            return;
        }
        let fallback = self.settings_fallback();
        for view in views {
            view.update(cx, |v, cx| v.reload(fallback.clone(), cx));
        }
    }

    fn config_file(&self, path: &Path) -> Option<ConfigFile> {
        let doc = document_key(path);
        let same = |p: anyhow::Result<PathBuf>| p.is_ok_and(|p| document_key(&p) == doc);
        if same(settings::path()) {
            Some(ConfigFile::Settings)
        } else if same(crate::keymap::path()) {
            Some(ConfigFile::Keymap)
        } else if path.ends_with(settings::PROJECT_FILE) && self.project_root_of(path).is_some() {
            Some(ConfigFile::ProjectSettings)
        } else {
            None
        }
    }

    /// Checks Athena's own settings and keymap files as they are edited, before they are saved.
    fn watch_config_problems(&mut self, editor: &Entity<EditorView>, cx: &mut Context<Self>) {
        let Some(kind) = self.config_file(editor.read(cx).path()) else {
            return;
        };
        self.check_config_file(editor, kind, cx);
        cx.subscribe(editor, move |this, editor, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::Edited { .. }) {
                this.check_config_file(&editor, kind, cx);
            }
        })
        .detach();
    }

    fn check_config_file(
        &mut self,
        editor: &Entity<EditorView>,
        kind: ConfigFile,
        cx: &mut Context<Self>,
    ) {
        let Some(text) = editor.read(cx).text() else {
            return;
        };
        let found = match kind {
            ConfigFile::Settings => settings::problems_at(&text, false),
            ConfigFile::ProjectSettings => settings::problems_at(&text, true),
            ConfigFile::Keymap => crate::keymap::problems_at(&text, |name, args| {
                cx.build_action(name, args)
                    .map_err(|e| anyhow::anyhow!("{e}"))
            }),
        };
        let list = found.into_iter().map(|p| diagnostic(&text, p)).collect();
        let doc = document_key(editor.read(cx).path());
        self.settings.file_problems.insert(doc.clone(), list);
        self.push_markers(editor, &doc, cx);
    }

    /// Looks for the optional JSON server off the main thread, as the login shell's PATH is read
    /// to find it, then opens the JSON files already open in it.
    fn find_json_server(&mut self, cx: &mut Context<Self>) {
        let find = cx
            .background_executor()
            .spawn(async { athena_lsp::find_program(ServerKind::Json.program()).is_some() });
        cx.spawn(async move |this, cx| {
            if !find.await {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                this.settings.json_server = true;
                let open: Vec<(PathBuf, Entity<EditorView>)> = this
                    .items
                    .iter()
                    .filter_map(|((root, _), view)| match view {
                        ItemView::Editor(e) if e.read(cx).lang() == Some(Lang::Json) => {
                            Some((root.clone(), e.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                for (root, editor) in open {
                    this.lsp_opened(&root, &editor, cx);
                }
            });
        })
        .detach();
    }

    /// Whether `kind`'s program is there to start; only the optional JSON server is looked for.
    pub(super) fn server_installed(&self, kind: ServerKind) -> bool {
        kind != ServerKind::Json || self.settings.json_server
    }

    /// The JSON server's settings: settings.json and keymap.json checked against their schemas.
    pub(super) fn json_server_settings(&self) -> Value {
        let folder = settings::path()
            .ok()
            .and_then(|p| Some(p.parent()?.file_name()?.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "athena".into());
        let mut commands: Vec<&str> = gpui::generate_list_of_all_registered_actions()
            .map(|a| a.name)
            .filter(|n| {
                !["zed::", "text_input::", "context_menu::"]
                    .iter()
                    .any(|p| n.starts_with(p))
            })
            .collect();
        commands.sort_unstable();
        // The server puts "**/" before each pattern and matches the file's whole URI.
        json!({"json": {
            "validate": {"enable": true},
            "schemas": [
                {
                    "fileMatch": [format!("{folder}/settings.json"), settings::PROJECT_FILE],
                    "schema": settings::schema::json_schema(),
                },
                {
                    "fileMatch": [format!("{folder}/keymap.json")],
                    "schema": crate::keymap::json_schema(&commands),
                },
            ],
        }})
    }

    /// What Athena found wrong in its own file at `doc`, as edited.
    pub(super) fn config_problems(&self, doc: &Path) -> impl Iterator<Item = &Diagnostic> {
        self.settings.file_problems.get(doc).into_iter().flatten()
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
