use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

/// Schemes a Cmd+click may hand to the system; anything else could launch local programs.
pub const OPENABLE_SCHEMES: &[&str] = &["http://", "https://"];

pub fn openable(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    OPENABLE_SCHEMES.iter().any(|s| lower.starts_with(s))
}

/// Finds a bare http(s) URL in `line` covering character `col`; returns its character range.
pub fn url_at(line: &[char], col: usize) -> Option<(usize, usize, String)> {
    if col >= line.len() || is_break(line[col]) {
        return None;
    }
    let mut start = col;
    while start > 0 && !is_break(line[start - 1]) {
        start -= 1;
    }
    let mut end = col + 1;
    while end < line.len() && !is_break(line[end]) {
        end += 1;
    }
    while end > start
        && matches!(
            line[end - 1],
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"'
        )
    {
        end -= 1;
    }
    let text: String = line[start..end].iter().collect();
    // A token like `see:https://x` still links from where the scheme starts.
    let lower = text.to_ascii_lowercase();
    let offset = OPENABLE_SCHEMES
        .iter()
        .filter_map(|s| lower.find(s))
        .min()?;
    let start = start + text[..offset].chars().count();
    let url: String = line[start..end].iter().collect();
    (col >= start && url.len() > "https://".len()).then_some((start, end, url))
}

fn is_break(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}' | '`' | '|' | '\0'
        )
}

/// A place in a file named in terminal output, such as `src/main.go:12:5`; line and column are
/// one-based, as compilers print them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRef {
    pub path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

impl FileRef {
    /// The file this names, read relative to each of `dirs` in turn; the first that exists wins.
    pub fn resolve(&self, dirs: &[&Path]) -> Option<PathBuf> {
        let path = match self.path.strip_prefix("~/") {
            Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
            None => PathBuf::from(&self.path),
        };
        if path.is_absolute() {
            return path.is_file().then_some(path);
        }
        dirs.iter().map(|d| d.join(&path)).find(|p| p.is_file())
    }
}

static PYTHON: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"File "([^"]+)", line (\d+)"#).expect("valid regex"));

/// A path, then `:line[:col]` (Go, cargo, tsc --pretty, eslint unix) or `(line,col)` (tsc).
static PLACE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([\w.~/@+-]+)(?::(\d+)(?::(\d+))?|\((\d+)(?:, ?(\d+))?\))?").expect("valid regex")
});

/// Finds a file reference covering character `col` of `line`; returns its character range and
/// its text. A bare word only counts once it looks like a path: a slash, or a name with an extension.
pub fn file_at(line: &[char], col: usize) -> Option<(usize, usize, FileRef)> {
    if col >= line.len() {
        return None;
    }
    let text: String = line.iter().collect();
    let char_at = |byte: usize| text[..byte].chars().count();
    let number = |m: Option<regex::Match>| m.and_then(|m| m.as_str().parse().ok());
    for caps in PYTHON.captures_iter(&text) {
        let path = caps.get(1)?;
        let (start, end) = (char_at(path.start()), char_at(path.end()));
        if (start..end).contains(&col) {
            let place = FileRef {
                path: path.as_str().to_string(),
                line: number(caps.get(2)),
                column: None,
            };
            return Some((start, end, place));
        }
    }
    for caps in PLACE.captures_iter(&text) {
        let whole = caps.get(0)?;
        let (start, end) = (char_at(whole.start()), char_at(whole.end()));
        if !(start..end).contains(&col) {
            continue;
        }
        let raw = caps.get(1)?.as_str();
        let located = caps.get(2).or(caps.get(4)).is_some();
        // Sentence punctuation after a bare path is not part of it.
        let path = if located {
            raw
        } else {
            raw.trim_end_matches(['.', '-'])
        };
        if !looks_like_path(path) {
            return None;
        }
        let end = if located {
            end
        } else {
            start + path.chars().count()
        };
        let place = FileRef {
            path: path.to_string(),
            line: number(caps.get(2).or(caps.get(4))),
            column: number(caps.get(3).or(caps.get(5))),
        };
        return (col < end).then_some((start, end, place));
    }
    None
}

fn looks_like_path(path: &str) -> bool {
    if path.chars().all(|c| matches!(c, '.' | '/' | '~')) {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    path.contains('/')
        || name
            .char_indices()
            .any(|(i, c)| c == '.' && i + 1 < name.len() && i > 0)
        || name.starts_with('.') && name.len() > 1
}

/// The path of a `file://` URI, as programs print with OSC 8 hyperlinks.
pub fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // A host part (`file://mac.local/x`) is the local machine for a local terminal.
    let path = &rest[rest.find('/')?..];
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = path
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str, col: usize) -> Option<String> {
        let chars: Vec<char> = s.chars().collect();
        url_at(&chars, col).map(|(_, _, u)| u)
    }

    #[test]
    fn finds_urls_and_trims_punctuation() {
        let s = "see (https://tlsc.io/docs). ok";
        assert_eq!(at(s, 10).as_deref(), Some("https://tlsc.io/docs"));
        assert_eq!(at(s, 2), None);
        assert_eq!(
            at("open http://localhost:3000/admin, now", 12).as_deref(),
            Some("http://localhost:3000/admin")
        );
    }

    #[test]
    fn rejects_other_schemes() {
        assert_eq!(at("file:///etc/passwd", 3), None);
        assert!(!openable("file:///Applications/Calculator.app"));
        assert!(!openable("javascript:alert(1)"));
        assert!(openable("HTTPS://example.com"));
    }

    #[test]
    fn prefix_before_scheme_is_not_part_of_the_link() {
        let s = "url:https://a.dev/x";
        assert_eq!(at(s, 1), None);
        assert_eq!(at(s, 8).as_deref(), Some("https://a.dev/x"));
    }

    fn place(s: &str, col: usize) -> Option<(String, Option<u32>, Option<u32>)> {
        let chars: Vec<char> = s.chars().collect();
        file_at(&chars, col).map(|(_, _, f)| (f.path, f.line, f.column))
    }

    fn span(s: &str, col: usize) -> Option<String> {
        let chars: Vec<char> = s.chars().collect();
        file_at(&chars, col).map(|(a, b, _)| chars[a..b].iter().collect())
    }

    fn at_word(s: &str, word: &str) -> usize {
        s[..s.find(word).unwrap()].chars().count()
    }

    /// Lines copied from go 1.27, cargo 1.9x and python 3 runs.
    #[test]
    fn real_compiler_output() {
        let go = "./main.go:4:2: declared and not used: x";
        assert_eq!(place(go, 2), Some(("./main.go".into(), Some(4), Some(2))));
        let go_test = "    x_test.go:6: expected 1, got 2";
        assert_eq!(place(go_test, 4), Some(("x_test.go".into(), Some(6), None)));
        assert_eq!(place("--- FAIL: TestX (0.00s)", 10), None);
        let rustc = " --> src/main.rs:2:18";
        assert_eq!(
            place(rustc, 5),
            Some(("src/main.rs".into(), Some(2), Some(18)))
        );
        assert_eq!(place("2 |     let x: u32 = \"a\";", 10), None);
        let python = "  File \"/tmp/realout/t.py\", line 3, in <module>";
        assert_eq!(
            place(python, 10),
            Some(("/tmp/realout/t.py".into(), Some(3), None))
        );
    }

    #[test]
    fn go_build_vet_and_test_output() {
        let build = "./cmd/server/main.go:12:5: undefined: handler";
        assert_eq!(
            place(build, 3),
            Some(("./cmd/server/main.go".into(), Some(12), Some(5)))
        );
        assert_eq!(span(build, 3).as_deref(), Some("./cmd/server/main.go:12:5"));
        assert_eq!(place(build, at_word(build, "undefined")), None);
        let test = "    handler_test.go:42: expected 200, got 500";
        assert_eq!(
            place(test, 6),
            Some(("handler_test.go".into(), Some(42), None))
        );
        assert_eq!(span(test, 6).as_deref(), Some("handler_test.go:42"));
        let panic = "\t/Users/me/proj/internal/store/db.go:88 +0x1d4";
        assert_eq!(
            place(panic, 5),
            Some(("/Users/me/proj/internal/store/db.go".into(), Some(88), None))
        );
        assert_eq!(place("FAIL\texample.com/proj/store\t0.012s", 1), None);
    }

    #[test]
    fn typescript_and_eslint_output() {
        let tsc = "src/app/index.ts(12,5): error TS2322: Type 'string' is not assignable";
        assert_eq!(
            place(tsc, 4),
            Some(("src/app/index.ts".into(), Some(12), Some(5)))
        );
        assert_eq!(span(tsc, 4).as_deref(), Some("src/app/index.ts(12,5)"));
        let pretty = "src/app/index.ts:12:5 - error TS2322: Type 'string'";
        assert_eq!(
            place(pretty, 0),
            Some(("src/app/index.ts".into(), Some(12), Some(5)))
        );
        let header = "/Users/me/web/src/components/Button.tsx";
        assert_eq!(
            place(header, 10),
            Some((header.into(), None, None)),
            "eslint's stylish header names the file alone"
        );
        let row = "  12:5  error  'x' is assigned a value but never used  no-unused-vars";
        assert_eq!(place(row, 3), None, "its rows carry no file name");
        let unix = "/Users/me/web/src/a.ts:3:10: Missing semicolon. [Error/semi]";
        assert_eq!(
            place(unix, 2),
            Some(("/Users/me/web/src/a.ts".into(), Some(3), Some(10)))
        );
    }

    #[test]
    fn cargo_rustc_and_python_output() {
        let arrow = "  --> crates/athena/src/main.rs:41:9";
        assert_eq!(place(arrow, 2), None, "the arrow is not a path");
        assert_eq!(
            place(arrow, at_word(arrow, "crates")),
            Some(("crates/athena/src/main.rs".into(), Some(41), Some(9)))
        );
        let panicked = "thread 'main' panicked at src/lib.rs:7:5:";
        assert_eq!(
            span(panicked, at_word(panicked, "src")).as_deref(),
            Some("src/lib.rs:7:5")
        );
        let traceback = "  File \"/Users/me/app/server.py\", line 27, in handle";
        assert_eq!(
            place(traceback, at_word(traceback, "server")),
            Some(("/Users/me/app/server.py".into(), Some(27), None))
        );
        assert_eq!(
            span(traceback, at_word(traceback, "server")).as_deref(),
            Some("/Users/me/app/server.py")
        );
    }

    #[test]
    fn claude_code_mentions() {
        let said = "The bug is in `internal/api/handler.go:45` where the error is dropped.";
        assert_eq!(
            place(said, at_word(said, "handler")),
            Some(("internal/api/handler.go".into(), Some(45), None))
        );
        let edit = "● Update(src/components/Nav.tsx)";
        assert_eq!(
            place(edit, at_word(edit, "Nav")),
            Some(("src/components/Nav.tsx".into(), None, None))
        );
        assert_eq!(place(edit, at_word(edit, "Update")), None);
        let range = "See main.go:12-20 for the loop.";
        assert_eq!(
            place(range, at_word(range, "main")),
            Some(("main.go".into(), Some(12), None))
        );
        let sentence = "I edited main.go.";
        assert_eq!(
            span(sentence, at_word(sentence, "main")).as_deref(),
            Some("main.go")
        );
        assert_eq!(place("Done. All 3 tests pass", 2), None);
        assert_eq!(place("cd ..", 4), None);
    }

    #[test]
    fn references_resolve_against_each_directory_in_turn() {
        let dir = std::env::temp_dir().join(format!("athena-links-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::write(dir.join("pkg/a.go"), "package pkg\n").unwrap();
        let file = |p: &str| FileRef {
            path: p.into(),
            line: Some(1),
            column: None,
        };
        let pkg = dir.join("pkg");
        assert_eq!(file("a.go").resolve(&[&dir, &pkg]), Some(pkg.join("a.go")));
        assert_eq!(
            file("pkg/a.go").resolve(&[&dir]),
            Some(dir.join("pkg/a.go"))
        );
        assert_eq!(file("pkg").resolve(&[&dir]), None, "a folder is not opened");
        let absolute = dir.join("pkg/a.go").display().to_string();
        assert!(file(&absolute).resolve(&[]).is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_uris_lose_their_host_and_escapes() {
        assert_eq!(
            file_uri_path("file://mac.local/Users/me/my%20app/a.go"),
            Some(PathBuf::from("/Users/me/my app/a.go"))
        );
        assert_eq!(
            file_uri_path("file:///tmp/x.rs"),
            Some(PathBuf::from("/tmp/x.rs"))
        );
        assert_eq!(file_uri_path("https://x"), None);
    }
}
