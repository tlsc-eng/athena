use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use athena_proto::NoticeKind;
use athena_workspace::{ItemId, ItemKind};
use gpui::{Context, PromptLevel, Window};

use super::Shell;
use super::item::ItemView;
use super::palette::Mode;
use crate::claude_hooks;

impl Shell {
    /// Opens a terminal tab running `claude`; any per-folder profile choice is left to the user's shell.
    pub(super) fn new_claude_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_in_new_terminal("claude".into(), window, cx);
    }

    /// Opens a terminal tab running `claude -r`, so the user picks the session to resume.
    pub(super) fn continue_claude_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_in_new_terminal("claude -r".into(), window, cx);
    }

    pub(super) fn change_claude_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_palette(Mode::Claude, window, cx);
    }

    /// Opens a terminal tab in the active project and types `command` into it once it starts.
    pub(super) fn run_in_new_terminal(
        &mut self,
        command: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_in_new_terminal_tab(command, window, cx);
    }

    /// `run_in_new_terminal`, naming the tab it opened.
    pub(super) fn run_in_new_terminal_tab(
        &mut self,
        command: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(PathBuf, ItemId)> {
        let i = self.workspace.active?;
        self.new_terminal(window, cx);
        let project = &self.workspace.projects[i];
        let root = project.root.clone();
        let item = project
            .layout
            .as_ref()
            .and_then(|l| l.focused_pane())
            .and_then(|p| p.active_item())
            .cloned();
        let item = item.filter(|i| matches!(i.kind, ItemKind::Terminal { .. }))?;
        let Some(ItemView::Terminal(view)) = self.item_view(&root, &item, cx) else {
            return None;
        };
        view.update(cx, |v, _| v.run_on_start(format!("{command}\r")));
        Some((root, item.id))
    }

    /// Puts Athena's hooks into every Claude Code config folder, or takes them out, once per
    /// change of `claude.hooks`; the projects open now lose the per-project hooks of old Athenas.
    pub(super) fn sync_claude_hooks(&mut self, cx: &mut Context<Self>) {
        static SYNCED: Mutex<Option<bool>> = Mutex::new(None);
        let on = self.settings.file.claude_hooks();
        if SYNCED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(on)
            == Some(on)
        {
            return;
        }
        let Some(home) = std::env::home_dir() else {
            return;
        };
        let projects: Vec<PathBuf> = self
            .workspace
            .projects
            .iter()
            .map(|p| p.root.clone())
            .collect();
        let task = cx.background_executor().spawn(async move {
            claude_hooks::sync(&home, &claude_hooks::athena_command(), on, &projects)
        });
        cx.spawn(async move |this, cx| {
            if let Err(e) = task.await {
                tracing::warn!("Claude Code hooks: {e:#}");
                let _ = this.update(cx, |this, cx| {
                    let title = "Could not update Claude Code hooks".to_string();
                    this.local_notice(
                        NoticeKind::Message {
                            title,
                            body: format!("{e:#}"),
                        },
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    /// Takes the hooks an older Athena wrote into this project out, after the user confirms.
    pub(super) fn remove_project_claude_hooks(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.workspace.active_project().map(|p| p.root.clone()) else {
            return;
        };
        let file = claude_hooks::settings_path(&root);
        let detail = format!(
            "Athena will remove its entries from {}. Your other settings are kept. Athena's hooks \
             now live in each Claude Code config folder instead; turn them off with \
             \"claude.hooks\" in Settings.",
            file.display()
        );
        let answer = window.prompt(
            PromptLevel::Info,
            "Remove Athena's Claude Code hooks from this project?",
            Some(&detail),
            &["Remove hooks", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let result = claude_hooks::write(&file, &claude_hooks::athena_command(), false, false);
            let _ = this.update(cx, |this, cx| {
                let (title, body) = match result {
                    Ok(()) => (
                        "Claude Code hooks removed".to_string(),
                        file.display().to_string(),
                    ),
                    Err(e) => (
                        "Could not update Claude Code hooks".to_string(),
                        format!("{e:#}"),
                    ),
                };
                this.local_notice(NoticeKind::Message { title, body }, cx);
            });
        })
        .detach();
    }
}
