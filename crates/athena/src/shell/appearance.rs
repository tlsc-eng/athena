use athena_ui::Appearance;
use athena_workspace::ThemeChoice;
use gpui::{Context, Window};

use super::Shell;

/// The appearance `choice` means for this window right now.
pub(super) fn resolve(choice: ThemeChoice, window: &Window) -> Appearance {
    match choice {
        ThemeChoice::Light => Appearance::Light,
        ThemeChoice::Dark => Appearance::Dark,
        ThemeChoice::System => athena_ui::appearance_for(window.appearance()),
    }
}

impl Shell {
    pub(super) fn set_theme_choice(
        &mut self,
        choice: ThemeChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace.theme = choice;
        athena_ui::set_appearance(resolve(choice, window), cx);
        self.schedule_save(cx);
        cx.notify();
    }
}
