use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use athena_ui::{ActiveTheme, Theme};
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, Hsla, Task, Window, canvas, div,
    prelude::*, px,
};

use crate::markdown::{self, Rendered};
use crate::web::{Mode, Web, WebEvent};

const MERMAID: &str = include_str!("../assets/mermaid.min.js");
const FONTS: &[(&str, &str, &[u8])] = &[
    (
        "Geist",
        "400",
        include_bytes!("../../athena-ui/assets/fonts/Geist-Regular.ttf"),
    ),
    (
        "Geist",
        "600",
        include_bytes!("../../athena-ui/assets/fonts/Geist-SemiBold.ttf"),
    ),
    (
        "Geist Mono",
        "400",
        include_bytes!("../../athena-ui/assets/fonts/GeistMono-Regular.ttf"),
    ),
];
/// Typing pauses this long before the preview follows the editor.
const FOLLOW_DELAY: Duration = Duration::from_millis(250);
/// Larger documents are not rendered; the editor still opens them.
const MAX_DOCUMENT: u64 = 10 * 1024 * 1024;

/// Markdown and Mermaid files, which can be shown rendered.
pub fn is_document_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "md" | "markdown" | "mdx" | "mmd" | "mermaid"
        )
    })
}

pub enum DocEvent {
    /// A link in the document points at this file.
    OpenFile(PathBuf),
}

/// A rendered Markdown or Mermaid file in a WKWebView, refreshed when the file changes.
pub struct DocView {
    root: PathBuf,
    path: PathBuf,
    focus: FocusHandle,
    web: Option<Web>,
    events: async_channel::Sender<WebEvent>,
    /// Set by the shell: false while the pane is off screen or something overlaps it.
    visible: bool,
    /// The page stays hidden until its first paint, so it never flashes.
    loaded: bool,
    had_focus: bool,
    mtime: Option<SystemTime>,
    /// What the page shows now; `None` until it has been loaded.
    shown: Option<Rendered>,
    error: Option<String>,
    pending_text: Option<Task<()>>,
    _events: Task<()>,
}

impl EventEmitter<DocEvent> for DocView {}

impl Focusable for DocView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl DocView {
    pub fn new(root: PathBuf, path: PathBuf, cx: &mut Context<Self>) -> Self {
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
            path,
            focus: cx.focus_handle(),
            web: None,
            events: tx,
            visible: false,
            loaded: false,
            had_focus: false,
            mtime: None,
            shown: None,
            error: None,
            pending_text: None,
            _events,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The tab label, as VS Code names its Markdown preview tabs.
    pub fn label(&self) -> String {
        label_for(&self.path)
    }

    /// Shows or hides the page; it cannot be clipped by gpui, so anything drawn over it must hide it.
    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
        self.sync_hidden();
    }

    fn sync_hidden(&self) {
        if let Some(web) = &self.web {
            web.set_hidden(!self.visible || !self.loaded || self.error.is_some());
        }
    }

    /// Re-renders if the file changed since it was last shown, keeping the scroll position.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if self.shown.is_some() && mtime == self.mtime {
            return;
        }
        self.mtime = mtime;
        match read_document(&self.path) {
            Ok(text) => self.render_text(&text, cx),
            Err(error) => {
                self.error = Some(error);
                self.sync_hidden();
                cx.notify();
            }
        }
    }

    /// Shows unsaved editor text shortly after typing pauses, as VS Code's preview follows the buffer.
    pub fn follow_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.pending_text = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FOLLOW_DELAY).await;
            this.update(cx, |this, cx| {
                this.pending_text = None;
                this.render_text(&text, cx);
            })
            .ok();
        }));
    }

    fn render_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(web) = &self.web else { return };
        self.error = None;
        let next = markdown::render_file(&self.path, text);
        match &self.shown {
            // Swapping the body in place keeps the scroll position; a page still loading, or one
            // that now needs Mermaid, is loaded afresh.
            Some(shown) if self.loaded && (shown.mermaid || !next.mermaid) => {
                if *shown != next {
                    web.run_script(&format!("athenaUpdate({})", js_string(&next.body)));
                }
            }
            _ => {
                self.loaded = false;
                web.load_html(&page(&next, cx.theme()));
            }
        }
        self.shown = Some(next);
        self.sync_hidden();
        cx.notify();
    }

    fn web_event(&mut self, event: WebEvent, cx: &mut Context<Self>) {
        match event {
            WebEvent::Finished => {
                self.loaded = true;
                self.error = None;
            }
            WebEvent::Failed(message) => self.error = Some(message),
            WebEvent::OpenLocal(path) => cx.emit(DocEvent::OpenFile(path)),
            WebEvent::OpenExternal(url) => cx.open_url(&url),
            WebEvent::Committed(_) => {}
        }
        self.sync_hidden();
        cx.notify();
    }

    fn place(
        &mut self,
        bounds: gpui::Bounds<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.web.is_none() {
            self.web = Web::new(&self.root, Mode::Document, self.events.clone(), window);
            self.refresh(cx);
        }
        if let Some(web) = &self.web {
            web.place(bounds, window);
        }
        self.sync_hidden();
    }

    /// Keeps AppKit's key view in step with gpui focus, so arrow keys scroll the page only when its tab is focused.
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

impl Render for DocView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_key(window);
        let t = cx.theme().clone();
        let entity = cx.entity();
        let page = canvas(
            |_, _, _| {},
            move |bounds, _, window, cx| {
                entity.update(cx, |this, cx| this.place(bounds, window, cx));
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
                        .child(format!("Can't preview {}", file_name(&self.path))),
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
            .key_context("DocView")
            .relative()
            .size_full()
            .bg(t.color.surface_sunken)
            .child(page)
            .children(message)
    }
}

pub fn label_for(path: &Path) -> String {
    format!("Preview {}", file_name(path))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn read_document(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_DOCUMENT {
        return Err("The file is larger than 10 MB.".into());
    }
    std::fs::read_to_string(path).map_err(|e| e.to_string())
}

fn hex(color: Hsla) -> String {
    let c = color.to_rgb();
    let byte = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(c.r), byte(c.g), byte(c.b))
}

/// `text` as a JavaScript string literal.
fn js_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            '<' => out.push_str("\\u003c"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn font_faces() -> &'static str {
    static FACES: OnceLock<String> = OnceLock::new();
    FACES.get_or_init(|| {
        FONTS
            .iter()
            .map(|(family, weight, bytes)| {
                format!(
                    "@font-face{{font-family:'{family}';font-weight:{weight};src:url(data:font/ttf;base64,{}) format('truetype');}}\n",
                    markdown::base64(bytes)
                )
            })
            .collect()
    })
}

fn nonce() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    let random = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes));
    if random.is_err() {
        let n = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        bytes = n.to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The whole page: Athena's palette, inline fonts, and Mermaid only when the document uses it.
fn page(doc: &Rendered, t: &Theme) -> String {
    let c = &t.color;
    let nonce = nonce();
    let (bg, surface, border, text, strong, muted, faint, accent) = (
        hex(c.surface_sunken),
        hex(c.surface),
        hex(c.border),
        hex(t.terminal.foreground),
        hex(c.content),
        hex(c.content_muted),
        hex(c.content_disabled),
        hex(c.accent),
    );
    let mermaid = if doc.mermaid {
        format!(
            "<script nonce=\"{nonce}\">{MERMAID}</script>\n<script nonce=\"{nonce}\">\
mermaid.initialize({{startOnLoad:false,securityLevel:'strict',theme:'base',darkMode:true,\
fontFamily:'Geist, -apple-system, sans-serif',themeVariables:{{background:'{bg}',\
primaryColor:'{surface}',primaryTextColor:'{strong}',primaryBorderColor:'{border_strong}',\
secondaryColor:'{accent_surface}',tertiaryColor:'{surface}',lineColor:'{muted}',\
textColor:'{text}',mainBkg:'{surface}',nodeBorder:'{border_strong}',clusterBkg:'{bg}',\
clusterBorder:'{border}',edgeLabelBackground:'{bg}',noteBkgColor:'{accent_surface}',\
noteTextColor:'{strong}',noteBorderColor:'{accent}',actorBkg:'{surface}',\
actorBorder:'{border_strong}',actorTextColor:'{strong}',signalColor:'{muted}',\
signalTextColor:'{text}',fontSize:'14px'}}}});\
document.addEventListener('DOMContentLoaded',function(){{mermaid.run();}});</script>",
            border_strong = hex(c.border_strong),
            accent_surface = hex(c.surface_accent),
        )
    } else {
        String::new()
    };
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'nonce-{nonce}'; style-src 'unsafe-inline'; img-src data: https: http:; font-src data:; frame-src 'none'; base-uri 'none'; form-action 'none'">
<script nonce="{nonce}">window.athenaUpdate=function(html){{document.getElementById('content').innerHTML=html;if(window.mermaid)mermaid.run();}};</script>
{mermaid}
<style>
{fonts}
:root{{color-scheme:dark}}
html{{background:{bg}}}
body{{margin:0;padding:24px 32px 64px;color:{text};font:15px/1.6 Geist,-apple-system,sans-serif;-webkit-font-smoothing:antialiased}}
#content{{max-width:880px;margin:0 auto}}
h1,h2,h3,h4,h5,h6{{color:{strong};font-weight:600;line-height:1.25;margin:1.5em 0 .6em}}
h1{{font-size:2em;padding-bottom:.3em;border-bottom:1px solid {border}}}
h2{{font-size:1.5em;padding-bottom:.3em;border-bottom:1px solid {border}}}
h3{{font-size:1.25em}} h4{{font-size:1em}} h5,h6{{font-size:.875em;color:{muted}}}
#content>:first-child{{margin-top:0}}
p,ul,ol,table,pre,blockquote{{margin:0 0 1em}}
a{{color:{accent};text-decoration:none}} a:hover{{text-decoration:underline}}
code,pre{{font-family:'Geist Mono',ui-monospace,monospace;font-size:13px}}
code{{background:{surface};border:1px solid {border};border-radius:2px;padding:.1em .35em}}
pre{{background:{surface};border:1px solid {border};border-radius:4px;padding:12px 16px;overflow:auto;line-height:1.5}}
pre code{{background:none;border:0;padding:0}}
pre.mermaid{{background:none;border:0;text-align:center;font-family:Geist,-apple-system,sans-serif}}
blockquote{{margin-left:0;padding:0 1em;color:{muted};border-left:3px solid {border}}}
.markdown-alert{{color:{text};border-left-color:{accent}}}
table{{border-collapse:collapse;display:block;overflow:auto;max-width:100%}}
th,td{{border:1px solid {border};padding:6px 13px}} th{{color:{strong};font-weight:600;background:{surface}}}
tr:nth-child(2n) td{{background:rgba(255,255,255,.02)}}
hr{{border:0;border-top:1px solid {border};margin:24px 0}}
img{{max-width:100%}}
li+li{{margin-top:.25em}} li>input[type=checkbox]{{margin:0 .4em 0 -1.2em;accent-color:{accent}}}
ul:has(>li>input[type=checkbox]){{list-style:none}}
del{{color:{faint}}}
::selection{{background:{selection}}}
</style></head>
<body><div id="content">{body}</div>
</body></html>"#,
        fonts = font_faces(),
        selection = hex(c.surface_accent),
        body = doc.body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_are_recognised_by_extension() {
        assert!(is_document_path(Path::new("/x/README.md")));
        assert!(is_document_path(Path::new("flow.MMD")));
        assert!(!is_document_path(Path::new("main.go")));
        assert_eq!(label_for(Path::new("/x/README.md")), "Preview README.md");
    }

    #[test]
    fn js_strings_cannot_close_the_script() {
        assert_eq!(
            js_string("a\"b\\c\n</script>"),
            r#""a\"b\\c\n\u003c/script>""#
        );
    }

    #[test]
    fn pages_carry_mermaid_only_when_needed() {
        let theme = Theme::dark(false);
        let plain = page(&markdown::render("# x", Path::new("/")), &theme);
        assert!(!plain.contains("mermaid.initialize"));
        assert!(plain.contains("<h1 id=\"x\">x</h1>"));
        assert!(plain.contains("background:#0b0807"));
        let diagram = page(
            &markdown::render_file(Path::new("/a.mmd"), "graph TD\nA-->B"),
            &theme,
        );
        assert!(diagram.contains("mermaid.initialize"));
    }
}
