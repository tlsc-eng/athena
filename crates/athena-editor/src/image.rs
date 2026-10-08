use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use athena_ui::{ActiveTheme, empty_state};
use gpui::{
    App, Bounds, Context, FocusHandle, Focusable, ImageSource, KeyBinding, ObjectFit, Pixels,
    Point, Render, ScrollWheelEvent, Window, actions, canvas, div, fill, img, point, prelude::*,
    px, size,
};

actions!(image_view, [ZoomIn, ZoomOut, ResetZoom]);

const EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "ico", "svg",
];
/// The steps Cmd+= and Cmd+- move between.
const ZOOM_STEPS: &[f32] = &[
    0.1, 0.25, 0.33, 0.5, 0.67, 0.75, 1., 1.25, 1.5, 2., 3., 4., 6., 8.,
];
const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 8.;
const CHECKER: f32 = 8.;
const STATUS_HEIGHT: f32 = 28.;

pub fn init(cx: &mut App) {
    let ctx = Some("ImageView");
    cx.bind_keys([
        KeyBinding::new("cmd-=", ZoomIn, ctx),
        KeyBinding::new("cmd-+", ZoomIn, ctx),
        KeyBinding::new("cmd--", ZoomOut, ctx),
        KeyBinding::new("cmd-0", ResetZoom, ctx),
    ]);
}

/// Files the image viewer opens instead of the text editor.
pub fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Info {
    width: f32,
    height: f32,
    bytes: u64,
}

/// A read-only image tab: fits the pane by default, zooms with Cmd+=/- and Cmd+wheel.
pub struct ImageView {
    path: PathBuf,
    focus: FocusHandle,
    info: Result<Info, String>,
    mtime: Option<SystemTime>,
    /// `None` fits the pane, shrinking large images but never enlarging small ones.
    zoom: Option<f32>,
    /// Top-left of the visible part of a zoomed image that overflows the pane.
    pan: Point<f32>,
    viewport: Rc<Cell<Bounds<Pixels>>>,
}

impl Focusable for ImageView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ImageView {
    pub fn open(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let (info, mtime) = read_info(&path);
        Self {
            path,
            focus: cx.focus_handle(),
            info,
            mtime,
            zoom: None,
            pan: Point::default(),
            viewport: Rc::default(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-reads the file if it changed on disk since it was shown.
    pub fn reload_if_changed(&mut self, cx: &mut Context<Self>) {
        let (info, mtime) = read_info(&self.path);
        if mtime == self.mtime && info == self.info {
            return;
        }
        ImageSource::from(self.path.clone()).remove_asset(cx);
        self.info = info;
        self.mtime = mtime;
        cx.notify();
    }

    fn view_size(&self) -> (f32, f32) {
        let view = self.viewport.get().size;
        (f32::from(view.width), f32::from(view.height))
    }

    fn scale(&self) -> f32 {
        match (self.zoom, &self.info) {
            (Some(z), _) => z,
            (None, Ok(info)) => fit_scale(*info, self.view_size()),
            (None, Err(_)) => 1.,
        }
    }

    fn step_zoom(&mut self, up: bool, cx: &mut Context<Self>) {
        let now = self.scale();
        let next = if up {
            ZOOM_STEPS.iter().copied().find(|s| *s > now + 0.001)
        } else {
            ZOOM_STEPS.iter().rev().copied().find(|s| *s < now - 0.001)
        };
        let (vw, vh) = self.view_size();
        self.set_zoom(next.unwrap_or(now), point(vw / 2., vh / 2.), cx);
    }

    /// Zooms keeping the image point under `anchor` (relative to the viewport) in place.
    fn set_zoom(&mut self, zoom: f32, anchor: Point<f32>, cx: &mut Context<Self>) {
        let Ok(info) = self.info else { return };
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let view = self.view_size();
        self.pan = zoomed_pan(info, view, self.scale(), self.pan, zoom, anchor);
        self.zoom = Some(zoom);
        cx.notify();
    }

    fn reset_zoom(&mut self, cx: &mut Context<Self>) {
        self.zoom = None;
        self.pan = Point::default();
        cx.notify();
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Ok(info) = self.info else { return };
        let delta = event.delta.pixel_delta(px(20.));
        if event.modifiers.platform {
            let origin = self.viewport.get().origin;
            let anchor = point(
                f32::from(event.position.x - origin.x),
                f32::from(event.position.y - origin.y),
            );
            let factor = (1. + f32::from(delta.y) * 0.01).clamp(0.5, 2.);
            self.set_zoom(self.scale() * factor, anchor, cx);
            return;
        }
        if self.zoom.is_none() {
            return;
        }
        let pan = point(
            self.pan.x - f32::from(delta.x),
            self.pan.y - f32::from(delta.y),
        );
        self.pan = clamp_pan(info, self.view_size(), self.scale(), pan);
        cx.notify();
    }

    fn status(&self, info: Info) -> String {
        let zoom = match self.zoom {
            None => format!("Fit · {}%", (self.scale() * 100.).round()),
            Some(z) => format!("{}%", (z * 100.).round()),
        };
        format!(
            "{} × {} · {} · {zoom}",
            info.width.round(),
            info.height.round(),
            human_size(info.bytes)
        )
    }
}

impl Render for ImageView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let root = div()
            .id("image-view")
            .track_focus(&self.focus)
            .key_context("ImageView")
            .size_full()
            .flex()
            .flex_col()
            .bg(t.color.surface_sunken)
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.step_zoom(true, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.step_zoom(false, cx)))
            .on_action(cx.listener(|this, _: &ResetZoom, _, cx| this.reset_zoom(cx)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| window.focus(&this.focus)),
            );
        let info = match &self.info {
            Ok(info) => *info,
            Err(error) => {
                return root.items_center().justify_center().child(empty_state(
                    "Can't show this image",
                    error.clone(),
                    None,
                    cx,
                ));
            }
        };
        let scale = self.scale();
        let (x, y) = origin(info, self.view_size(), scale, self.pan);
        let (w, h) = (info.width * scale, info.height * scale);
        let viewport = self.viewport.clone();
        let entity = cx.entity();
        let measure = canvas(
            move |bounds, _, cx| {
                if viewport.get() != bounds {
                    viewport.set(bounds);
                    entity.update(cx, |_, cx| cx.notify());
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let (light, dark) = (t.color.surface_active, t.color.surface);
        let checker = canvas(
            |_, _, _| {},
            move |bounds, _, window, _| paint_checker(bounds, light, dark, window),
        )
        .absolute()
        .size_full();
        let picture = div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(w))
            .h(px(h))
            .child(checker)
            .child(
                img(self.path.clone())
                    .size_full()
                    .object_fit(ObjectFit::Fill),
            );
        root.child(
            div()
                .id("image-canvas")
                .relative()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .on_scroll_wheel(cx.listener(Self::scroll_wheel))
                .child(measure)
                .child(picture),
        )
        .child(
            div()
                .flex_none()
                .h(px(STATUS_HEIGHT))
                .px(px(12.))
                .flex()
                .items_center()
                .justify_end()
                .border_t_1()
                .border_color(t.color.border)
                .bg(t.color.surface)
                .text_size(t.typography.caption)
                .text_color(t.color.content_muted)
                .child(self.status(info)),
        )
    }
}

/// Transparent pixels show over a checkerboard, painted only where the image is visible.
fn paint_checker(bounds: Bounds<Pixels>, light: gpui::Hsla, dark: gpui::Hsla, window: &mut Window) {
    let visible = bounds.intersect(&window.content_mask().bounds);
    if visible.size.width <= px(0.) || visible.size.height <= px(0.) {
        return;
    }
    window.paint_quad(fill(visible, dark));
    let cell = px(CHECKER);
    let col0 = ((visible.left() - bounds.left()) / cell).floor() as i64;
    let row0 = ((visible.top() - bounds.top()) / cell).floor() as i64;
    let col1 = ((visible.right() - bounds.left()) / cell).ceil() as i64;
    let row1 = ((visible.bottom() - bounds.top()) / cell).ceil() as i64;
    for row in row0..row1 {
        for col in col0..col1 {
            if (row + col) % 2 != 0 {
                continue;
            }
            let square = Bounds::new(
                point(
                    bounds.left() + cell * col as f32,
                    bounds.top() + cell * row as f32,
                ),
                size(cell, cell),
            )
            .intersect(&visible);
            window.paint_quad(fill(square, light));
        }
    }
}

/// Shrinks to fit the viewport but never enlarges, as VS Code shows images.
fn fit_scale(info: Info, (vw, vh): (f32, f32)) -> f32 {
    if vw <= 0. || vh <= 0. || info.width <= 0. || info.height <= 0. {
        return 1.;
    }
    (vw / info.width).min(vh / info.height).min(1.)
}

/// Where the image's top-left sits in the viewport: centred when smaller, panned when larger.
fn origin(info: Info, (vw, vh): (f32, f32), scale: f32, pan: Point<f32>) -> (f32, f32) {
    let (w, h) = (info.width * scale, info.height * scale);
    let x = if w <= vw { (vw - w) / 2. } else { -pan.x };
    let y = if h <= vh { (vh - h) / 2. } else { -pan.y };
    (x, y)
}

fn clamp_pan(info: Info, (vw, vh): (f32, f32), scale: f32, pan: Point<f32>) -> Point<f32> {
    let max_x = (info.width * scale - vw).max(0.);
    let max_y = (info.height * scale - vh).max(0.);
    point(pan.x.clamp(0., max_x), pan.y.clamp(0., max_y))
}

/// The pan that keeps the image pixel under `anchor` in place when going from `old` to `new`.
fn zoomed_pan(
    info: Info,
    view: (f32, f32),
    old: f32,
    pan: Point<f32>,
    new: f32,
    anchor: Point<f32>,
) -> Point<f32> {
    let (ox, oy) = origin(info, view, old, pan);
    let (ix, iy) = ((anchor.x - ox) / old, (anchor.y - oy) / old);
    clamp_pan(
        info,
        view,
        new,
        point(ix * new - anchor.x, iy * new - anchor.y),
    )
}

fn read_info(path: &Path) -> (Result<Info, String>, Option<SystemTime>) {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) => return (Err(format!("{}: {e}", path.display())), None),
    };
    let mtime = meta.modified().ok();
    let dims = if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
    {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| svg_size(&text))
            .ok_or_else(|| "The SVG has no width, height or viewBox.".to_string())
    } else {
        imagesize::size(path)
            .map(|s| (s.width as f32, s.height as f32))
            .map_err(|e| format!("{e}"))
    };
    let info = dims.map(|(width, height)| Info {
        width,
        height,
        bytes: meta.len(),
    });
    (info, mtime)
}

/// An SVG's intrinsic size from its root `width`/`height`, else its `viewBox`.
fn svg_size(text: &str) -> Option<(f32, f32)> {
    let start = text.find("<svg")?;
    let tag = &text[start..start + text[start..].find('>')?];
    let attr = |name: &str| -> Option<&str> {
        let mut rest = tag;
        loop {
            let at = rest.find(name)?;
            let before = rest[..at].chars().last();
            rest = &rest[at + name.len()..];
            let trimmed = rest.trim_start();
            if before.is_some_and(char::is_whitespace) && trimmed.starts_with('=') {
                let value = trimmed[1..].trim_start();
                let quote = value.chars().next()?;
                let value = &value[1..];
                return Some(&value[..value.find(quote)?]);
            }
        }
    };
    let length = |v: &str| -> Option<f32> {
        let v = v.trim().trim_end_matches("px");
        if v.ends_with('%') {
            return None;
        }
        v.parse().ok().filter(|n: &f32| *n > 0.)
    };
    let view_box = attr("viewBox").and_then(|v| {
        let n: Vec<f32> = v
            .split([' ', ','])
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        (n.len() == 4 && n[2] > 0. && n[3] > 0.).then(|| (n[2], n[3]))
    });
    match (
        attr("width").and_then(length),
        attr("height").and_then(length),
    ) {
        (Some(w), Some(h)) => Some((w, h)),
        (Some(w), None) => view_box.map(|(vw, vh)| (w, w * vh / vw)),
        (None, Some(h)) => view_box.map(|(vw, vh)| (h * vw / vh, h)),
        (None, None) => view_box,
    }
}

fn human_size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.0} KB", bytes as f64 / 1024.),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_paths_are_recognised_by_extension() {
        assert!(is_image_path(Path::new("/x/logo.PNG")));
        assert!(is_image_path(Path::new("/x/icon.svg")));
        assert!(is_image_path(Path::new("a.jpeg")));
        assert!(!is_image_path(Path::new("/x/main.go")));
        assert!(!is_image_path(Path::new("/x/png")));
    }

    #[test]
    fn svg_size_reads_attributes_then_view_box() {
        assert_eq!(
            svg_size(r#"<?xml?><svg xmlns="x" width="24px" height='16'>"#),
            Some((24., 16.))
        );
        assert_eq!(
            svg_size(r#"<svg viewBox="0 0 100 50" stroke-width="2">"#),
            Some((100., 50.))
        );
        assert_eq!(
            svg_size(r#"<svg width="200" viewBox="0,0,100,50">"#),
            Some((200., 100.))
        );
        assert_eq!(svg_size(r#"<svg width="100%" height="100%">"#), None);
    }

    #[test]
    fn sizes_read_like_finder() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(213 * 1024), "213 KB");
        assert_eq!(human_size(3 * 1_048_576 / 2), "1.5 MB");
    }

    #[test]
    fn fit_never_enlarges_and_zoom_keeps_the_anchor() {
        let info = Info {
            width: 400.,
            height: 200.,
            bytes: 1,
        };
        assert_eq!(fit_scale(info, (800., 600.)), 1.);
        assert_eq!(fit_scale(info, (200., 600.)), 0.5);
        let view = (200., 600.);
        let anchor = point(100., 50.);
        let pan = zoomed_pan(info, view, 0.5, Point::default(), 2., anchor);
        let (ox, oy) = origin(info, view, 2., pan);
        assert_eq!(
            (anchor.x - ox) / 2.,
            200.,
            "the anchor stays over the same pixel"
        );
        assert_eq!(oy, 100., "an image shorter than the pane stays centred");
        assert_eq!(clamp_pan(info, view, 2., point(-5., 9.)), point(0., 0.));
    }
}
