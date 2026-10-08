use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use pulldown_cmark::{CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd, html};

/// Relative images larger than this are left as links rather than inlined.
const MAX_INLINE_IMAGE: u64 = 5 * 1024 * 1024;
pub(crate) const LOCAL_SCHEME: &str = "athena-doc";

/// A document turned into the HTML that goes inside the page's content element.
#[derive(Debug, PartialEq)]
pub(crate) struct Rendered {
    pub body: String,
    pub mermaid: bool,
}

/// Mermaid sources (`.mmd`, `.mermaid`) are one diagram; everything else is Markdown.
pub(crate) fn render_file(path: &Path, text: &str) -> Rendered {
    let is_mermaid = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "mmd" | "mermaid"));
    if is_mermaid {
        return Rendered {
            body: format!("<pre class=\"mermaid\">{}</pre>", escape(text)),
            mermaid: true,
        };
    }
    render(text, path.parent().unwrap_or(Path::new("/")))
}

/// GitHub-flavoured Markdown with mermaid fences, local images inlined and local links rewritten.
pub(crate) fn render(text: &str, base: &Path) -> Rendered {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM;
    let mut events: Vec<Event> = Vec::new();
    let mut mermaid = false;
    let mut diagram: Option<String> = None;
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang)))
                if lang.split_whitespace().next() == Some("mermaid") =>
            {
                diagram = Some(String::new());
            }
            Event::Text(t) if diagram.is_some() => {
                diagram.get_or_insert_default().push_str(&t);
            }
            Event::End(TagEnd::CodeBlock) if diagram.is_some() => {
                let source = diagram.take().unwrap_or_default();
                mermaid = true;
                events.push(Event::Html(
                    format!("<pre class=\"mermaid\">{}</pre>\n", escape(&source)).into(),
                ));
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => events.push(Event::Start(Tag::Link {
                link_type,
                dest_url: link_target(&dest_url, base).map_or(dest_url, CowStr::from),
                title,
                id,
            })),
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) => events.push(Event::Start(Tag::Image {
                link_type,
                dest_url: inline_image(&dest_url, base).map_or(dest_url, CowStr::from),
                title,
                id,
            })),
            other => events.push(other),
        }
    }
    add_heading_ids(&mut events);
    let mut body = String::new();
    html::push_html(&mut body, events.into_iter());
    Rendered { body, mermaid }
}

/// Gives headings GitHub's anchor ids so `#section` links in the document work.
fn add_heading_ids(events: &mut [Event]) {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for i in 0..events.len() {
        let Event::Start(Tag::Heading { id: None, .. }) = &events[i] else {
            continue;
        };
        let mut text = String::new();
        for e in &events[i + 1..] {
            match e {
                Event::End(TagEnd::Heading(_)) => break,
                Event::Text(t) | Event::Code(t) => text.push_str(t),
                _ => {}
            }
        }
        let base = slug(&text);
        let n = seen.entry(base.clone()).or_default();
        let slug = if *n == 0 { base } else { format!("{base}-{n}") };
        *n += 1;
        if let Event::Start(Tag::Heading { id, .. }) = &mut events[i] {
            *id = Some(slug.into());
        }
    }
}

fn slug(text: &str) -> String {
    text.trim()
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

fn has_scheme(url: &str) -> bool {
    url.split_once(':').is_some_and(|(scheme, _)| {
        !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

fn resolve(url: &str, base: &Path) -> PathBuf {
    let path = percent_decode(url);
    let joined = match path.strip_prefix('/') {
        Some(_) => PathBuf::from(&path),
        None => base.join(&path),
    };
    normalize(&joined)
}

/// Folds `.` and `..` without touching the disk, so links to missing files still resolve.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Local links open in Athena: they become `athena-doc:` URLs the page hands back to the app.
fn link_target(url: &str, base: &Path) -> Option<String> {
    if url.is_empty() || url.starts_with('#') || url.starts_with("//") || has_scheme(url) {
        return None;
    }
    let (path, fragment) = url
        .split_once('#')
        .map_or((url, None), |(p, f)| (p, Some(f)));
    let full = resolve(path, base);
    let mut out = format!(
        "{LOCAL_SCHEME}://{}",
        percent_encode(&full.to_string_lossy())
    );
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    Some(out)
}

/// The path an `athena-doc:` URL points at.
pub(crate) fn local_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix(LOCAL_SCHEME)?.strip_prefix("://")?;
    let path = rest.split(['#', '?']).next().unwrap_or(rest);
    Some(PathBuf::from(percent_decode(path)))
}

/// The page has no base URL, so relative images travel inside it as data: URIs.
fn inline_image(url: &str, base: &Path) -> Option<String> {
    if url.is_empty() || url.starts_with("//") || has_scheme(url) {
        return None;
    }
    let path = resolve(url.split(['#', '?']).next().unwrap_or(url), base);
    let mime = match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        _ => return None,
    };
    if std::fs::metadata(&path).ok()?.len() > MAX_INLINE_IMAGE {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    Some(format!("data:{mime};base64,{}", base64(&bytes)))
}

pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn percent_encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_get_github_anchor_ids() {
        let out = render("# Hello World\n\n## Hello World\n", Path::new("/x"));
        assert!(out.body.contains("<h1 id=\"hello-world\">Hello World</h1>"));
        assert!(out.body.contains("<h2 id=\"hello-world-1\">"));
        assert!(!out.mermaid);
    }

    #[test]
    fn mermaid_fences_become_diagrams() {
        let out = render(
            "```mermaid\ngraph TD\n  A-->B\n```\n\n```rust\nfn x() {}\n```\n",
            Path::new("/x"),
        );
        assert!(
            out.body
                .contains("<pre class=\"mermaid\">graph TD\n  A--&gt;B\n</pre>")
        );
        assert!(out.body.contains("<code class=\"language-rust\">"));
        assert!(out.mermaid);
        let mmd = render_file(Path::new("/x/flow.mmd"), "graph LR\nA-->B");
        assert_eq!(mmd.body, "<pre class=\"mermaid\">graph LR\nA--&gt;B</pre>");
    }

    #[test]
    fn tables_and_task_lists_render() {
        let out = render(
            "| a | b |\n|---|---|\n| 1 | 2 |\n\n- [x] done\n",
            Path::new("/x"),
        );
        assert!(out.body.contains("<table>"));
        assert!(out.body.contains("type=\"checkbox\""));
    }

    #[test]
    fn local_links_point_back_into_athena() {
        let out = render(
            "[a](docs/My%20File.md#usage) [b](https://tlsc.io) [c](#top) [d](mailto:x@y)",
            Path::new("/repo"),
        );
        assert!(
            out.body
                .contains("href=\"athena-doc:///repo/docs/My%20File.md#usage\""),
            "{}",
            out.body
        );
        assert!(out.body.contains("href=\"https://tlsc.io\""));
        assert!(out.body.contains("href=\"#top\""));
        assert!(out.body.contains("href=\"mailto:x@y\""));
        assert_eq!(
            local_path("athena-doc:///repo/docs/My%20File.md#usage"),
            Some(PathBuf::from("/repo/docs/My File.md"))
        );
        assert_eq!(local_path("https://x"), None);
    }

    #[test]
    fn dot_segments_in_links_are_folded() {
        let out = render(
            "[a](../other/x.md#s) [b](./y.md) [c](/repo/a/../b.md) [d](../../../../up.md)",
            Path::new("/repo/docs"),
        );
        for href in [
            "athena-doc:///repo/other/x.md#s",
            "athena-doc:///repo/docs/y.md",
            "athena-doc:///repo/b.md",
            "athena-doc:///up.md",
        ] {
            assert!(
                out.body.contains(&format!("href=\"{href}\"")),
                "{}",
                out.body
            );
        }
    }

    #[test]
    fn relative_images_are_inlined() {
        let dir = std::env::temp_dir().join(format!("athena-md-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("dot.png"), [1u8, 2, 3, 4]).unwrap();
        let out = render("![dot](dot.png) ![web](https://x/y.png)", &dir);
        assert!(out.body.contains("src=\"data:image/png;base64,AQIDBA==\""));
        assert!(out.body.contains("src=\"https://x/y.png\""));
        let up = render("![dot](missing/../dot.png)", &dir);
        assert!(up.body.contains("src=\"data:image/png;base64,AQIDBA==\""));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn raw_text_is_escaped() {
        assert_eq!(
            escape("<a href='x'>&</a>"),
            "&lt;a href=&#39;x&#39;&gt;&amp;&lt;/a&gt;"
        );
    }
}
