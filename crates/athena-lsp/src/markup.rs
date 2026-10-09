use std::collections::HashMap;
use std::ops::Range;

use serde_json::Value;

/// A paragraph of prose or a code block from hover documentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkupBlock {
    Text(String),
    Code(String),
}

/// What the server says about the symbol under the pointer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hover {
    pub blocks: Vec<MarkupBlock>,
    /// The span the text is about, usually the hovered word.
    pub range: Option<crate::Range>,
}

pub(crate) fn parse_hover(result: &Value) -> Option<Hover> {
    let blocks = hover_blocks(result.get("contents")?);
    let range = result
        .get("range")
        .and_then(|r| serde_json::from_value(r.clone()).ok());
    (!blocks.is_empty()).then_some(Hover { blocks, range })
}

/// Hover contents in any of the protocol's shapes: MarkupContent, MarkedString, or a list of them.
pub(crate) fn hover_blocks(contents: &Value) -> Vec<MarkupBlock> {
    match contents {
        Value::String(md) => markdown_blocks(md),
        Value::Array(items) => items.iter().flat_map(hover_blocks).collect(),
        Value::Object(o) => {
            let value = o.get("value").and_then(Value::as_str).unwrap_or_default();
            if o.contains_key("language") {
                code_block(value)
            } else if o.get("kind").and_then(Value::as_str) == Some("plaintext") {
                value
                    .split("\n\n")
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(|p| MarkupBlock::Text(p.to_string()))
                    .collect()
            } else {
                markdown_blocks(value)
            }
        }
        _ => Vec::new(),
    }
}

fn code_block(code: &str) -> Vec<MarkupBlock> {
    let code = code.trim_end();
    if code.is_empty() {
        Vec::new()
    } else {
        vec![MarkupBlock::Code(code.to_string())]
    }
}

/// Markdown as plain paragraphs and verbatim code blocks; inline markup is dropped.
pub fn markdown_blocks(md: &str) -> Vec<MarkupBlock> {
    let mut out = Vec::new();
    let mut paragraph: Vec<String> = Vec::new();
    let mut code: Option<Vec<&str>> = None;
    let flush = |paragraph: &mut Vec<String>, out: &mut Vec<MarkupBlock>| {
        if !paragraph.is_empty() {
            out.push(MarkupBlock::Text(paragraph.join("")));
            paragraph.clear();
        }
    };
    for line in md.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            match code.take() {
                Some(lines) => out.extend(code_block(&lines.join("\n"))),
                None => {
                    flush(&mut paragraph, &mut out);
                    code = Some(Vec::new());
                }
            }
            continue;
        }
        if let Some(lines) = code.as_mut() {
            lines.push(line);
            continue;
        }
        let rule = trimmed.len() >= 3
            && ['-', '*', '_']
                .iter()
                .any(|&c| trimmed.chars().all(|ch| ch == c || ch == ' '));
        if trimmed.is_empty() || rule {
            flush(&mut paragraph, &mut out);
            continue;
        }
        if trimmed.starts_with('#') {
            flush(&mut paragraph, &mut out);
            out.push(MarkupBlock::Text(inline(
                trimmed.trim_start_matches('#').trim_start(),
            )));
            continue;
        }
        let text = inline(trimmed);
        let list_item = trimmed.starts_with(['-', '*', '+']) && trimmed[1..].starts_with(' ')
            || trimmed
                .split_once(". ")
                .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        // Markdown joins wrapped lines with a space; list items and hard breaks keep their line.
        if let Some(last) = paragraph.last_mut() {
            let hard = list_item || line.ends_with("  ") || last.ends_with('\\');
            if last.ends_with('\\') {
                last.pop();
            }
            paragraph.push(if hard { "\n".into() } else { " ".into() });
        }
        paragraph.push(text);
    }
    if let Some(lines) = code {
        out.extend(code_block(&lines.join("\n")));
    }
    flush(&mut paragraph, &mut out);
    out
}

/// Strips links to their text, emphasis markers, backticks and backslash escapes.
fn inline(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' if chars.get(i + 1).is_some_and(|c| c.is_ascii_punctuation()) => {
                out.push(chars[i + 1]);
                i += 2;
            }
            '`' => i += 1,
            '*' if chars.get(i + 1) == Some(&'*') => i += 2,
            '[' => {
                let close = chars[i..].iter().position(|&c| c == ']').map(|p| p + i);
                match close {
                    Some(close) if chars.get(close + 1) == Some(&'(') => {
                        let end = chars[close..].iter().position(|&c| c == ')');
                        out.push_str(&inline(&chars[i + 1..close].iter().collect::<String>()));
                        i = end.map_or(chars.len(), |e| close + e + 1);
                    }
                    _ => {
                        out.push('[');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// A snippet as plain text, with the range to select afterwards: the first tab stop's placeholder,
/// else `$0`, in chars of the returned text. `None` leaves the cursor after the text.
pub fn expand_snippet(snippet: &str) -> (String, Option<Range<usize>>) {
    let (text, stops) = snippet_stops(snippet);
    let first = first_stop(&stops);
    (text, first)
}

/// Where the cursor goes first: the lowest numbered stop, else `$0`.
pub(crate) fn first_stop(stops: &[(u32, Range<usize>)]) -> Option<Range<usize>> {
    stops
        .iter()
        .filter(|(n, _)| *n > 0)
        .min_by_key(|(n, _)| *n)
        .or_else(|| stops.iter().find(|(n, _)| *n == 0))
        .map(|(_, r)| r.clone())
}

/// A snippet as plain text, with each tab stop's number and range in chars of that text; a
/// number used more than once mirrors one placeholder.
pub fn snippet_stops(snippet: &str) -> (String, Vec<(u32, Range<usize>)>) {
    let chars: Vec<char> = snippet.chars().collect();
    let expand = |placeholders: &HashMap<u32, Vec<char>>| {
        let mut snippet = Snippet {
            chars: &chars,
            placeholders,
            out: Vec::new(),
            stops: Vec::new(),
        };
        let mut i = 0;
        snippet.part(&mut i, false);
        (snippet.out, snippet.stops)
    };
    // A bare `$2` repeats the text `${2:...}` gives it, wherever that appears.
    let (first_out, first_stops) = expand(&HashMap::new());
    let placeholders = first_stops
        .iter()
        .filter(|(_, r)| !r.is_empty())
        .map(|(n, r)| (*n, first_out[r.clone()].to_vec()))
        .collect();
    let (out, stops) = expand(&placeholders);
    (out.into_iter().collect(), stops)
}

struct Snippet<'a> {
    chars: &'a [char],
    placeholders: &'a HashMap<u32, Vec<char>>,
    out: Vec<char>,
    stops: Vec<(u32, Range<usize>)>,
}

impl Snippet<'_> {
    fn part(&mut self, i: &mut usize, nested: bool) {
        let chars = self.chars;
        while *i < chars.len() {
            let c = chars[*i];
            match c {
                '\\' if chars
                    .get(*i + 1)
                    .is_some_and(|n| matches!(n, '$' | '}' | '\\')) =>
                {
                    self.out.push(chars[*i + 1]);
                    *i += 2;
                }
                '}' if nested => return,
                '$' => {
                    if !self.tab_stop(i) {
                        self.out.push('$');
                        *i += 1;
                    }
                }
                _ => {
                    self.out.push(c);
                    *i += 1;
                }
            }
        }
    }

    /// A stop with no text of its own, filled with the text another stop of its number gives.
    fn bare_stop(&mut self, number: Option<u32>) {
        let Some(n) = number else { return };
        let at = self.out.len();
        if let Some(text) = self.placeholders.get(&n) {
            self.out.extend(text);
        }
        self.stops.push((n, at..self.out.len()));
    }

    /// Reads a `$1`, `${1}`, `${1:text}`, `${1|a,b|}`, `$NAME` or `${NAME:text}` at `i`.
    fn tab_stop(&mut self, i: &mut usize) -> bool {
        let chars = self.chars;
        let at = self.out.len();
        let mut j = *i + 1;
        let braced = chars.get(j) == Some(&'{');
        if braced {
            j += 1;
        }
        let rest = &chars[j.min(chars.len())..];
        let digits = rest.iter().take_while(|c| c.is_ascii_digit()).count();
        let name = if digits == 0 {
            rest.iter()
                .take_while(|c| c.is_ascii_alphanumeric() || **c == '_')
                .count()
        } else {
            0
        };
        if digits == 0 && name == 0 {
            return false;
        }
        let number: Option<u32> = chars[j..j + digits].iter().collect::<String>().parse().ok();
        j += digits + name;
        if !braced {
            self.bare_stop(number);
            *i = j;
            return true;
        }
        match chars.get(j) {
            Some('}') => {
                self.bare_stop(number);
                *i = j + 1;
                true
            }
            Some(':') => {
                let mut k = j + 1;
                self.part(&mut k, true);
                if chars.get(k) != Some(&'}') {
                    self.out.truncate(at);
                    return false;
                }
                *i = k + 1;
                if let Some(n) = number {
                    self.stops.push((n, at..self.out.len()));
                }
                true
            }
            Some('|') if number.is_some() => {
                let rest = &chars[j + 1..];
                let Some(end) = rest.windows(2).position(|w| w == ['|', '}']) else {
                    return false;
                };
                let first = rest[..end].split(|c| *c == ',').next().unwrap_or_default();
                self.out.extend(first);
                self.stops.push((number.unwrap_or(0), at..self.out.len()));
                *i = j + 1 + end + 2;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn snippets_become_plain_text_with_the_first_stop_selected() {
        assert_eq!(expand_snippet("Println"), ("Println".into(), None));
        assert_eq!(
            expand_snippet("Println($0)"),
            ("Println()".into(), Some(8..8))
        );
        assert_eq!(
            expand_snippet("Println(${1:a ...any})$0"),
            ("Println(a ...any)".into(), Some(8..16))
        );
        assert_eq!(
            expand_snippet("for ${2:i} := ${1:0}; $2 < n; $2++ {\n\t$0\n}"),
            ("for i := 0; i < n; i++ {\n\t\n}".into(), Some(9..10))
        );
        assert_eq!(
            expand_snippet("${1|public,private|} x"),
            ("public x".into(), Some(0..6))
        );
        assert_eq!(
            expand_snippet("cost \\$5 ${1:{a\\}}"),
            ("cost $5 {a}".into(), Some(8..11))
        );
        assert_eq!(
            expand_snippet("${TM_SELECTED_TEXT:x}y$"),
            ("xy$".into(), None)
        );
        assert_eq!(
            expand_snippet("f(${1:g(${2:x})})"),
            ("f(g(x))".into(), Some(2..6))
        );
    }

    #[test]
    fn every_stop_and_its_mirrors_are_kept() {
        let (text, mut stops) = snippet_stops("for ${2:i} := ${1:0}; $2 < n; $2++ {\n\t$0\n}");
        stops.sort_by_key(|(n, r)| (*n, r.start));
        assert_eq!(text, "for i := 0; i < n; i++ {\n\t\n}");
        assert_eq!(
            stops,
            vec![(0, 26..26), (1, 9..10), (2, 4..5), (2, 12..13), (2, 19..20)]
        );
    }

    #[test]
    fn hover_markdown_keeps_code_blocks_and_flattens_prose() {
        let md = "```go\nfunc helper() int\n```\n\n---\n\nhelper returns **one**, see \
                  [`helper` on pkg.go.dev](https://pkg.go.dev/x#helper).\nSecond line.\n\n\
                  - first\n- second";
        assert_eq!(
            markdown_blocks(md),
            vec![
                MarkupBlock::Code("func helper() int".into()),
                MarkupBlock::Text(
                    "helper returns one, see helper on pkg.go.dev. Second line.".into()
                ),
                MarkupBlock::Text("- first\n- second".into()),
            ]
        );
        assert_eq!(
            markdown_blocks("snake_case and a\\_b"),
            vec![MarkupBlock::Text("snake_case and a_b".into())]
        );
    }

    #[test]
    fn hover_accepts_every_contents_shape() {
        let markup = json!({"kind": "markdown", "value": "# Title\nbody"});
        assert_eq!(
            hover_blocks(&markup),
            vec![
                MarkupBlock::Text("Title".into()),
                MarkupBlock::Text("body".into())
            ]
        );
        let marked = json!([{"language": "ts", "value": "const x: number"}, "doc"]);
        assert_eq!(
            hover_blocks(&marked),
            vec![
                MarkupBlock::Code("const x: number".into()),
                MarkupBlock::Text("doc".into())
            ]
        );
        let plain = json!({"kind": "plaintext", "value": "a *b*\n\nc"});
        assert_eq!(
            hover_blocks(&plain),
            vec![
                MarkupBlock::Text("a *b*".into()),
                MarkupBlock::Text("c".into())
            ]
        );
        assert!(hover_blocks(&json!(null)).is_empty());
    }
}
