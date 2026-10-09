use std::ops::RangeInclusive;

use athena_ui::{ActiveTheme, CODE_ZOOM, Theme, UI_ZOOM};
use gpui::Context;

use super::Shell;

impl Shell {
    /// Steps editor and terminal text one size up or down, or back to the default with `None`.
    pub(super) fn zoom_font(&mut self, step: Option<i32>, cx: &mut Context<Self>) {
        let zoom = stepped(self.workspace.ui.font_zoom, step, CODE_ZOOM);
        if zoom == self.workspace.ui.font_zoom {
            return;
        }
        self.workspace.ui.font_zoom = zoom;
        cx.global_mut::<Theme>().set_code_zoom(zoom);
        self.schedule_save(cx);
        self.font_zoom_changed(zoom, cx);
        cx.notify();
    }

    /// Steps the whole interface one size up or down, or back to the default with `None`;
    /// written to settings.json only when the user keeps `window.zoom_level` there.
    pub(super) fn zoom_window(&mut self, step: Option<i32>, cx: &mut Context<Self>) {
        let zoom = stepped(self.workspace.ui.zoom_level, step, UI_ZOOM);
        if zoom == self.workspace.ui.zoom_level {
            return;
        }
        self.workspace.ui.zoom_level = zoom;
        self.schedule_save(cx);
        if self.settings.file.zoom_level.is_some() {
            self.write_setting(&["window", "zoom_level"], zoom.into(), cx);
        }
        self.sync_window_zoom(cx);
        cx.notify();
    }

    /// Puts settings.json's `window.zoom_level`, else the last zoom chosen, in force.
    pub(super) fn sync_window_zoom(&mut self, cx: &mut Context<Self>) {
        if let Some(level) = self.settings.file.zoom_level
            && level != self.workspace.ui.zoom_level
        {
            self.workspace.ui.zoom_level = level;
            self.schedule_save(cx);
        }
        if cx.theme().ui_zoom() != self.workspace.ui.zoom_level {
            cx.global_mut::<Theme>()
                .set_ui_zoom(self.workspace.ui.zoom_level);
        }
    }
}

/// `current` moved by `step` within `range`, or the default 0 for `None`; a hand-edited
/// workspace.json can hold any level.
fn stepped(current: i32, step: Option<i32>, range: RangeInclusive<i32>) -> i32 {
    step.map_or(0, |step| {
        current
            .saturating_add(step)
            .clamp(*range.start(), *range.end())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zoom_step_stays_in_range_even_from_an_absurd_saved_level() {
        assert_eq!(stepped(2, Some(1), UI_ZOOM), 3);
        assert_eq!(stepped(5, Some(1), UI_ZOOM), 5);
        assert_eq!(stepped(3, None, UI_ZOOM), 0);
        assert_eq!(stepped(i32::MAX, Some(1), CODE_ZOOM), *CODE_ZOOM.end());
        assert_eq!(stepped(i32::MIN, Some(-1), CODE_ZOOM), *CODE_ZOOM.start());
    }
}
