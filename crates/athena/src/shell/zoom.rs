use athena_ui::{CODE_ZOOM, Theme};
use gpui::Context;

use super::Shell;

impl Shell {
    /// Steps editor and terminal text one size up or down, or back to the default with `None`.
    pub(super) fn zoom_font(&mut self, step: Option<i32>, cx: &mut Context<Self>) {
        let zoom = step.map_or(0, |step| {
            (self.workspace.ui.font_zoom + step).clamp(*CODE_ZOOM.start(), *CODE_ZOOM.end())
        });
        if zoom == self.workspace.ui.font_zoom {
            return;
        }
        self.workspace.ui.font_zoom = zoom;
        cx.global_mut::<Theme>().set_code_zoom(zoom);
        self.schedule_save(cx);
        self.font_zoom_changed(zoom, cx);
        cx.notify();
    }
}
