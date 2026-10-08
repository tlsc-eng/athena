use athena_proto::NoticeKind;
use athena_workspace::ItemKind;
use gpui::{Context, PromptLevel, Window};

use super::Shell;
use super::item::ItemView;
use super::palette::Mode;
use crate::claude_hooks;

impl Shell {
    /// Opens a terminal tab running Claude Code, asking once per project which command starts it.
    pub(super) fn new_claude_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self
            .workspace
            .active_project()
            .and_then(|p| p.claude_command.clone())
        {
            Some(command) => self.start_claude_with(command, window, cx),
            None => self.open_palette(Mode::Claude, window, cx),
        }
    }

    pub(super) fn change_claude_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_palette(Mode::Claude, window, cx);
    }

    pub(super) fn start_claude_with(
        &mut self,
        command: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(i) = self.workspace.active else {
            return;
        };
        self.workspace.projects[i].claude_command = Some(command.clone());
        self.new_terminal(window, cx);
        let project = &self.workspace.projects[i];
        let root = project.root.clone();
        let item = project
            .layout
            .as_ref()
            .and_then(|l| l.focused_pane())
            .and_then(|p| p.active_item())
            .cloned();
        if let Some(item) = item.filter(|i| matches!(i.kind, ItemKind::Terminal { .. }))
            && let Some(ItemView::Terminal(view)) = self.item_view(&root, &item, cx)
        {
            view.update(cx, |v, _| v.run_on_start(format!("{command}\r")));
        }
    }

    /// Adds or removes Athena's Claude Code hooks after the user confirms the file it touches.
    pub(super) fn set_claude_hooks(
        &mut self,
        enable: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.workspace.active_project().map(|p| p.root.clone()) else {
            return;
        };
        let file = claude_hooks::settings_path(&root);
        let (message, button) = if enable {
            ("Add Claude Code hooks to this project?", "Add hooks")
        } else {
            (
                "Remove Athena's Claude Code hooks from this project?",
                "Remove hooks",
            )
        };
        let detail = format!(
            "Athena will edit {}, adding entries that run `athena notify` when Claude starts working, \
             finishes, or needs your input. Your other settings are kept. Claude Code keeps this file \
             out of version control.",
            file.display()
        );
        let answer = window.prompt(
            PromptLevel::Info,
            message,
            Some(&detail),
            &[button, "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let result = claude_hooks::write(&root, &claude_hooks::athena_command(), enable);
            let _ = this.update(cx, |this, cx| {
                let (title, body) = match result {
                    Ok(()) if enable => (
                        "Claude Code hooks added".to_string(),
                        file.display().to_string(),
                    ),
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
