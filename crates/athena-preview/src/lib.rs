//! A browser preview pane: a WKWebView laid over a gpui element.

mod doc;
mod markdown;
mod web;

use std::path::PathBuf;

use athena_ui::{ActiveTheme, Button, ButtonKind, InputEvent, TextInput};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task, Window, canvas,
    div, prelude::*, px,
};

pub use doc::{DocEvent, DocView, is_document_path, label_for as doc_label_for};
pub use web::restore_key_focus;
use web::{Mode, Web, WebEvent};

pub const DEFAULT_URL: &str = "http://localhost:3000";

pub enum PreviewEvent {
    Navigated(String),
}

pub struct PreviewView {
    root: PathBuf,
    url: String,
    input: Entity<TextInput>,
    focus: FocusHandle,
    web: Option<Web>,
    events: async_channel::Sender<WebEvent>,
    /// Set by the shell: false while the pane is off screen or something overlaps it.
    visible: bool,
    had_focus: bool,
    loading: bool,
    error: Option<String>,
    _input: Subscription,
    _events: Task<()>,
}

impl EventEmitter<PreviewEvent> for PreviewView {}

impl Focusable for PreviewView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Turns what was typed into a URL: `3000` and `localhost:3000` mean local http, bare hosts https.
pub fn normalize_url(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.chars().all(|c| c.is_ascii_digit()) {
        return Some(format!("http://localhost:{text}"));
    }
    let has_scheme = text.split_once(':').is_some_and(|(scheme, rest)| {
        scheme.chars().all(|c| c.is_ascii_alphabetic())
            && !rest.starts_with(|c: char| c.is_ascii_digit())
    });
    let url = if has_scheme {
        text.to_string()
    } else {
        let host = text.split(['/', ':']).next().unwrap_or("");
        let local = host == "localhost"
            || host.ends_with(".localhost")
            || host.ends_with(".local")
            || host.starts_with("127.")
            || host == "0.0.0.0"
            || text.starts_with("[::1]");
        format!("{}://{text}", if local { "http" } else { "https" })
    };
    web::allowed(&url).then_some(url)
}

/// What the tab shows: host and port, without the scheme.
pub fn label_for(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split('/').next().unwrap_or(rest).to_string()
}

impl PreviewView {
    pub fn new(root: PathBuf, url: String, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            let mut input = TextInput::new("localhost:3000", cx);
            input.set_text(url.clone(), cx);
            input
        });
        let _input = cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
            if let InputEvent::Submit = event {
                let text = input.read(cx).text().to_string();
                match normalize_url(&text) {
                    Some(url) => this.navigate(url, cx),
                    None => {
                        this.error = Some(format!(
                            "Athena previews http and https pages only, not {text}."
                        ));
                        cx.notify();
                    }
                }
            }
        });
        let (tx, rx) = async_channel::unbounded();
        let _events = cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                if this
                    .update(cx, |this, cx| this.web_event(event, cx))
                    .is_err()
                {
                    return;
                }
            }
        });
        Self {
            root,
            url,
            input,
            focus: cx.focus_handle(),
            web: None,
            events: tx,
            visible: false,
            had_focus: false,
            loading: false,
            error: None,
            _input,
            _events,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn label(&self) -> String {
        label_for(&self.url)
    }

    /// Shows or hides the page; it cannot be clipped by gpui, so anything drawn over it must hide it.
    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
        self.sync_hidden();
    }

    fn sync_hidden(&self) {
        if let Some(web) = &self.web {
            web.set_hidden(!self.visible || self.error.is_some());
        }
    }

    fn navigate(&mut self, url: String, cx: &mut Context<Self>) {
        self.url = url;
        self.error = None;
        self.loading = true;
        if let Some(web) = &self.web {
            web.load(&self.url);
        }
        self.sync_hidden();
        cx.emit(PreviewEvent::Navigated(self.url.clone()));
        cx.notify();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if self.error.is_some() {
            return self.navigate(self.url.clone(), cx);
        }
        if let Some(web) = &self.web {
            self.loading = true;
            web.reload();
            cx.notify();
        }
    }

    fn web_event(&mut self, event: WebEvent, cx: &mut Context<Self>) {
        match event {
            WebEvent::Committed(url) => {
                self.error = None;
                if url != self.url {
                    self.url = url.clone();
                    self.input.update(cx, |i, cx| i.set_text(url.clone(), cx));
                    cx.emit(PreviewEvent::Navigated(url));
                }
            }
            WebEvent::Finished => self.loading = false,
            WebEvent::Failed(message) => {
                self.loading = false;
                self.error = Some(message);
            }
            WebEvent::OpenLocal(_) | WebEvent::OpenExternal(_) => {}
        }
        self.sync_hidden();
        cx.notify();
    }

    fn place(&mut self, bounds: gpui::Bounds<gpui::Pixels>, window: &mut Window) {
        if self.web.is_none() {
            self.web = Web::new(&self.root, Mode::Browser, self.events.clone(), window);
            if let Some(web) = &self.web {
                web.load(&self.url);
                self.loading = true;
            }
        }
        if let Some(web) = &self.web {
            web.place(bounds, window);
        }
        self.sync_hidden();
    }

    /// Keeps AppKit's key view in step with gpui focus, so typing reaches the page only when its tab is focused.
    fn sync_key(&mut self, window: &Window) {
        let focused = self.focus.is_focused(window);
        if focused == self.had_focus {
            return;
        }
        self.had_focus = focused;
        if let Some(web) = &self.web {
            if focused && self.visible {
                web.take_key();
            } else {
                web.resign_key();
            }
        }
    }
}

impl Render for PreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_key(window);
        let t = cx.theme().clone();
        let entity = cx.entity();
        let page = canvas(
            |_, _, _| {},
            move |bounds, _, window, cx| {
                entity.update(cx, |this, _| this.place(bounds, window));
            },
        )
        .size_full();
        let message = self.error.clone().map(|error| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .text_size(t.typography.caption)
                .child(
                    div()
                        .text_color(t.color.content)
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(format!("Could not open {}", self.label())),
                )
                .child(
                    div()
                        .max_w(px(360.))
                        .text_center()
                        .text_color(t.color.content_muted)
                        .child(error),
                )
        });
        div()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(36.))
                    .flex_none()
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .border_b_1()
                    .border_color(t.color.border)
                    .text_size(t.typography.caption)
                    .child(
                        Button::new("preview-back", "Back", ButtonKind::Ghost).on_click(
                            cx.listener(|this, _, _, _| {
                                if let Some(web) = &this.web {
                                    web.back();
                                }
                            }),
                        ),
                    )
                    .child(
                        Button::new("preview-forward", "Forward", ButtonKind::Ghost).on_click(
                            cx.listener(|this, _, _, _| {
                                if let Some(web) = &this.web {
                                    web.forward();
                                }
                            }),
                        ),
                    )
                    .child(
                        Button::new("preview-reload", "Reload", ButtonKind::Ghost)
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    )
                    .child(div().flex_1().min_w_0().child(self.input.clone()))
                    .child(
                        div()
                            .w(px(56.))
                            .flex_none()
                            .text_right()
                            .text_color(t.color.content_muted)
                            .child(if self.loading { "Loading" } else { "" }),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(page)
                    .children(message),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_addresses_become_urls() {
        assert_eq!(
            normalize_url("3000").as_deref(),
            Some("http://localhost:3000")
        );
        assert_eq!(
            normalize_url(" localhost:5173/app ").as_deref(),
            Some("http://localhost:5173/app")
        );
        assert_eq!(
            normalize_url("127.0.0.1:8080").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(normalize_url("tlsc.io").as_deref(), Some("https://tlsc.io"));
        assert_eq!(
            normalize_url("http://x.test").as_deref(),
            Some("http://x.test")
        );
        assert_eq!(normalize_url("file:///etc/hosts"), None);
        assert_eq!(normalize_url("javascript:alert(1)"), None);
        assert_eq!(normalize_url(""), None);
    }

    #[test]
    fn labels_drop_the_scheme_and_path() {
        assert_eq!(label_for("http://localhost:3000/a/b"), "localhost:3000");
        assert_eq!(label_for("https://tlsc.io"), "tlsc.io");
    }
}
