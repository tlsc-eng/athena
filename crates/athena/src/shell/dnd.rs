use std::path::{Path, PathBuf};

use athena_ui::ActiveTheme;
use athena_workspace::{ItemId, PaneId};
use gpui::{
    Bounds, Context, FontWeight, IntoElement, Pixels, Point, PromptLevel, Render, SharedString,
    Window, div, prelude::*, px,
};

use super::Shell;
use super::fileops;

/// Fraction of a pane's width or height, from each edge, that drops as a split.
const EDGE_BAND: f32 = 0.25;

/// A tab being dragged; `pane` is where it came from.
#[derive(Clone, Debug)]
pub(super) struct TabDrag {
    pub pane: PaneId,
    pub item: ItemId,
    pub label: SharedString,
    /// A terminal, which the bottom panel takes too.
    pub terminal: bool,
}

/// A file tree entry being dragged, to move it (or copy it, with ⌥) into a folder.
#[derive(Clone, Debug)]
pub(super) struct TreeDrag {
    pub path: PathBuf,
    pub label: SharedString,
}

impl TreeDrag {
    /// Whether dropping on folder `dir` would do anything: not onto itself, inside itself, or
    /// back into its own folder unless copying.
    pub fn fits(&self, dir: &Path, copy: bool) -> bool {
        match fileops::drop_destination(&self.path, dir) {
            Ok(Some(_)) => true,
            Ok(None) => copy,
            Err(_) => false,
        }
    }
}

/// The caption that follows the cursor while a tab is dragged.
pub(super) struct TabGhost {
    pub label: SharedString,
}

impl Render for TabGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div()
            .h(px(28.))
            .px(px(12.))
            .flex()
            .items_center()
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_control)
            .shadow(vec![t.popover_shadow()])
            .text_size(t.typography.caption)
            .font_weight(FontWeight::MEDIUM)
            .text_color(t.color.content)
            .child(self.label.clone())
    }
}

/// Where in a pane a dragged tab lands: joining it, or splitting off one of its edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DropZone {
    Center,
    Left,
    Right,
    Top,
    Bottom,
}

/// The zone of `bounds` under `pos`: the nearest edge within its 25 % band, else the centre.
pub(super) fn zone_for(bounds: Bounds<Pixels>, pos: Point<Pixels>) -> DropZone {
    let w = f32::from(bounds.size.width).max(1.);
    let h = f32::from(bounds.size.height).max(1.);
    let x = (f32::from(pos.x - bounds.origin.x) / w).clamp(0., 1.);
    let y = (f32::from(pos.y - bounds.origin.y) / h).clamp(0., 1.);
    [
        (x, DropZone::Left),
        (1. - x, DropZone::Right),
        (y, DropZone::Top),
        (1. - y, DropZone::Bottom),
    ]
    .into_iter()
    .filter(|(d, _)| *d < EDGE_BAND)
    .min_by(|a, b| a.0.total_cmp(&b.0))
    .map_or(DropZone::Center, |(_, zone)| zone)
}

/// Where a tab dropped on a strip of `len` tabs ends up: just before the tab at `before`, or last.
/// `here` is its current index when it is already in that strip.
pub(super) fn strip_drop_index(here: Option<usize>, before: Option<usize>, len: usize) -> usize {
    match (here, before) {
        (Some(at), Some(target)) if at < target => target - 1,
        (_, Some(target)) => target,
        (Some(_), None) => len - 1,
        (None, None) => len,
    }
}

impl Shell {
    /// A tree entry dropped on folder `dir`: moved after VS Code's confirmation, or copied with ⌥.
    pub(super) fn drop_tree_entry(
        &mut self,
        drag: &TreeDrag,
        dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let from = drag.path.clone();
        let copy = window.modifiers().alt;
        let dest = match fileops::drop_destination(&from, &dir) {
            Ok(Some(dest)) if !copy || std::fs::symlink_metadata(&dest).is_err() => dest,
            Ok(Some(_) | None) if copy => fileops::free_copy_name(&dir, &drag.label),
            Ok(_) => return,
            Err(err) => {
                return self.transient_notice("Could not move that", format!("{err:#}"), cx);
            }
        };
        if copy {
            let copying = cx
                .background_executor()
                .spawn(async move { fileops::copy(&from, &dest).map(|()| dest) });
            cx.spawn(async move |this, cx| {
                let (result, dest) = match copying.await {
                    Ok(dest) => (Ok(()), dest),
                    Err(err) => (Err(err), PathBuf::new()),
                };
                let _ = this.update(cx, |this, cx| this.after_tree_drop(result, &dest, cx));
            })
            .detach();
            return;
        }
        let name = drag.label.clone();
        let into = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.display().to_string());
        let confirm = self.settings.file.confirm_drag_and_drop().then(|| {
            window.prompt(
                PromptLevel::Info,
                &format!("Are you sure you want to move “{name}” into “{into}”?"),
                None,
                &["Move", "Move and Don’t Ask Again", "Cancel"],
                cx,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Some(answer) = confirm {
                match answer.await {
                    Ok(0) => {}
                    Ok(1) => {
                        let stop = |this: &mut Shell, _: &mut Window, cx: &mut Context<Shell>| {
                            this.write_setting(
                                &["explorer", "confirmDragAndDrop"],
                                false.into(),
                                cx,
                            )
                        };
                        let _ = this.update_in(cx, stop);
                    }
                    _ => return,
                }
            }
            if std::fs::symlink_metadata(&dest).is_ok() {
                let Ok(replace) = this.update_in(cx, |_, window, cx| {
                    window.prompt(
                        PromptLevel::Warning,
                        &format!("“{name}” already exists in “{into}”. Replace it?"),
                        Some("The one there goes to the Trash, where you can restore it."),
                        &["Replace", "Cancel"],
                        cx,
                    )
                }) else {
                    return;
                };
                if !matches!(replace.await, Ok(0)) {
                    return;
                }
                if let Err(err) = fileops::trash(&dest) {
                    let _ = this.update(cx, |this, cx| {
                        this.transient_notice("Could not move that", format!("{err:#}"), cx)
                    });
                    return;
                }
            }
            let _ = this.update(cx, |this, cx| {
                let result = fileops::rename(&from, &dest);
                if result.is_ok() {
                    this.retarget_items(&from, &dest, cx);
                    this.reload_changed_files(cx);
                }
                this.after_tree_drop(result, &dest, cx);
            });
        })
        .detach();
    }

    fn after_tree_drop(&mut self, result: anyhow::Result<()>, dest: &Path, cx: &mut Context<Self>) {
        if let Err(err) = result {
            return self.transient_notice("Could not move that", format!("{err:#}"), cx);
        }
        self.tree.invalidate();
        if let Some(root) = self.active_root() {
            self.tree.reveal(&root, dest);
        }
        self.git_kick(cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    #[test]
    fn a_tree_entry_fits_any_folder_but_itself_inside_itself_or_its_own_unless_copied() {
        let drag = TreeDrag {
            path: PathBuf::from("/p/src"),
            label: "src".into(),
        };
        assert!(drag.fits(Path::new("/p/docs"), false));
        assert!(!drag.fits(Path::new("/p/src"), true));
        assert!(!drag.fits(Path::new("/p/src/inner"), true));
        assert!(!drag.fits(Path::new("/p"), false));
        assert!(drag.fits(Path::new("/p"), true));
    }

    #[test]
    fn a_dropped_tab_lands_just_before_the_tab_under_it() {
        assert_eq!(strip_drop_index(Some(0), Some(2), 3), 1, "moving right");
        assert_eq!(strip_drop_index(Some(2), Some(0), 3), 0, "moving left");
        assert_eq!(strip_drop_index(None, Some(1), 3), 1, "from another pane");
    }

    #[test]
    fn the_empty_strip_end_appends() {
        assert_eq!(strip_drop_index(Some(0), None, 3), 2);
        assert_eq!(strip_drop_index(None, None, 3), 3);
    }

    fn pane() -> Bounds<Pixels> {
        Bounds::new(point(px(100.), px(50.)), size(px(400.), px(200.)))
    }

    fn at(x: f32, y: f32) -> DropZone {
        zone_for(pane(), point(px(x), px(y)))
    }

    #[test]
    fn edge_bands_split_and_the_middle_joins() {
        assert_eq!(at(300., 150.), DropZone::Center);
        assert_eq!(at(110., 150.), DropZone::Left);
        assert_eq!(at(490., 150.), DropZone::Right);
        assert_eq!(at(300., 55.), DropZone::Top);
        assert_eq!(at(300., 245.), DropZone::Bottom);
    }

    #[test]
    fn band_is_a_quarter_of_each_side() {
        assert_eq!(at(100. + 99., 150.), DropZone::Left);
        assert_eq!(at(100. + 101., 150.), DropZone::Center);
        assert_eq!(at(300., 50. + 49.), DropZone::Top);
        assert_eq!(at(300., 50. + 51.), DropZone::Center);
    }

    #[test]
    fn corners_go_to_the_nearer_edge() {
        assert_eq!(at(105., 80.), DropZone::Left);
        assert_eq!(at(130., 52.), DropZone::Top);
    }

    #[test]
    fn positions_outside_clamp_to_the_nearest_edge() {
        assert_eq!(at(0., 150.), DropZone::Left);
        assert_eq!(at(300., 900.), DropZone::Bottom);
    }
}
