use std::ops::Range;

use crate::syntax::Token;

const INSTRUCTIONS: &[&str] = &[
    "ADD",
    "ARG",
    "CMD",
    "COPY",
    "ENTRYPOINT",
    "ENV",
    "EXPOSE",
    "FROM",
    "HEALTHCHECK",
    "LABEL",
    "MAINTAINER",
    "ONBUILD",
    "RUN",
    "SHELL",
    "STOPSIGNAL",
    "USER",
    "VOLUME",
    "WORKDIR",
];

/// Highlights one Dockerfile line; continuation lines get strings and variables only.
pub fn dockerfile(line: &str) -> Vec<(Range<usize>, Token)> {
    let indent = line.len() - line.trim_start().len();
    let rest = &line[indent..];
    if rest.starts_with('#') {
        return vec![(indent..line.len(), Token::Comment)];
    }
    let mut out = Vec::new();
    let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let word = &rest[..word_end];
    let mut from = indent;
    if INSTRUCTIONS.iter().any(|i| i.eq_ignore_ascii_case(word)) {
        out.push((indent..indent + word_end, Token::Keyword));
        from = indent + word_end;
        if word.eq_ignore_ascii_case("FROM") {
            stage_name(line, from, &mut out);
        }
    }
    flags(line, from, &mut out);
    values(line, from, &mut out);
    out
}

/// `FROM image AS name`: the keyword and the stage name.
fn stage_name(line: &str, from: usize, out: &mut Vec<(Range<usize>, Token)>) {
    let mut words = words(line, from);
    while let Some(w) = words.next() {
        if line[w.clone()].eq_ignore_ascii_case("AS") {
            out.push((w, Token::Keyword));
            if let Some(name) = words.next() {
                out.push((name, Token::Label));
            }
            return;
        }
    }
}

fn flags(line: &str, from: usize, out: &mut Vec<(Range<usize>, Token)>) {
    for w in words(line, from) {
        if !line[w.clone()].starts_with("--") {
            break;
        }
        let end = line[w.clone()].find('=').map_or(w.end, |i| w.start + i);
        out.push((w.start..end, Token::Attribute));
    }
}

/// Byte ranges of whitespace-separated words from `from`.
fn words(line: &str, from: usize) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut at = from;
    std::iter::from_fn(move || {
        let rest = &line[at..];
        let start = at + (rest.len() - rest.trim_start().len());
        if start >= line.len() {
            return None;
        }
        let len = line[start..]
            .find(char::is_whitespace)
            .unwrap_or(line.len() - start);
        at = start + len;
        Some(start..at)
    })
}

/// Quoted strings, `$VAR` / `${VAR}` references and a trailing line continuation.
fn values(line: &str, from: usize, out: &mut Vec<(Range<usize>, Token)>) {
    let bytes = line.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            q @ (b'"' | b'\'') => {
                let end = line[i + 1..]
                    .find(q as char)
                    .map_or(line.len(), |j| i + j + 2);
                out.push((i..end, Token::String));
                variables(line, i..end, q == b'"', out);
                i = end;
            }
            b'$' => i = variable(line, i, out),
            _ => i += 1,
        }
    }
}

fn variables(
    line: &str,
    range: Range<usize>,
    interpolated: bool,
    out: &mut Vec<(Range<usize>, Token)>,
) {
    if !interpolated {
        return;
    }
    let mut i = range.start;
    while i < range.end {
        if line.as_bytes()[i] == b'$' {
            i = variable(line, i, out);
        } else {
            i += 1;
        }
    }
}

/// Marks a `$NAME` or `${...}` starting at `at`; returns the byte after it.
fn variable(line: &str, at: usize, out: &mut Vec<(Range<usize>, Token)>) -> usize {
    let rest = &line[at + 1..];
    let len = if rest.starts_with('{') {
        rest.find('}').map_or(rest.len(), |j| j + 1)
    } else {
        rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len())
    };
    if len == 0 {
        return at + 1;
    }
    out.push((at..at + 1 + len, Token::Embedded));
    at + 1 + len
}

/// Highlights one `.env` line: `export`, the key, `=`, the value and references inside it.
pub fn dotenv(line: &str) -> Vec<(Range<usize>, Token)> {
    let indent = line.len() - line.trim_start().len();
    let rest = &line[indent..];
    if rest.starts_with('#') {
        return vec![(indent..line.len(), Token::Comment)];
    }
    let mut out = Vec::new();
    let mut key_start = indent;
    if let Some(after) = rest.strip_prefix("export")
        && after.starts_with(char::is_whitespace)
    {
        out.push((indent..indent + 6, Token::Keyword));
        key_start = indent + 6 + (after.len() - after.trim_start().len());
    }
    let Some(eq) = line[key_start..].find('=').map(|i| key_start + i) else {
        return out;
    };
    let key_end = key_start + line[key_start..eq].trim_end().len();
    out.push((key_start..key_end, Token::Property));
    out.push((eq..eq + 1, Token::Operator));
    let value_start = eq + 1 + (line[eq + 1..].len() - line[eq + 1..].trim_start().len());
    let value = &line[value_start..];
    let (value_end, quote) = match value.as_bytes().first() {
        Some(q @ (b'"' | b'\'')) => (
            value[1..]
                .find(*q as char)
                .map_or(line.len(), |j| value_start + j + 2),
            Some(*q),
        ),
        // An unquoted value ends where a ` #` comment starts.
        _ => (
            value.find(" #").map_or(line.len(), |j| value_start + j),
            None,
        ),
    };
    let value_end = value_start + line[value_start..value_end].trim_end().len();
    if value_end > value_start {
        out.push((value_start..value_end, Token::String));
        variables(line, value_start..value_end, quote != Some(b'\''), &mut out);
    }
    let tail = &line[value_end..];
    if let Some(hash) = tail.find('#') {
        out.push((value_end + hash..line.len(), Token::Comment));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(line: &str, found: Vec<(Range<usize>, Token)>) -> Vec<(&str, Token)> {
        found.into_iter().map(|(r, t)| (&line[r], t)).collect()
    }

    #[test]
    fn dockerfile_keywords_and_comments() {
        let line = "FROM golang:1.23 AS build";
        let t = spans(line, dockerfile(line));
        assert!(t.contains(&("FROM", Token::Keyword)));
        assert!(t.contains(&("AS", Token::Keyword)));
        assert!(t.contains(&("build", Token::Label)));
        let line = "  # syntax=docker/dockerfile:1";
        assert_eq!(
            spans(line, dockerfile(line)),
            vec![(line.trim_start(), Token::Comment)]
        );
        let line = "COPY --from=build \"/out/${APP}\" /bin/$APP";
        let t = spans(line, dockerfile(line));
        assert!(t.contains(&("--from", Token::Attribute)));
        assert!(t.contains(&("\"/out/${APP}\"", Token::String)));
        assert!(t.contains(&("${APP}", Token::Embedded)));
        assert!(t.contains(&("$APP", Token::Embedded)));
        let line = "    && apt-get install -y git";
        assert!(spans(line, dockerfile(line)).is_empty());
    }

    #[test]
    fn dotenv_keys_are_properties() {
        let line = "export DATABASE_URL=\"postgres://${HOST}/db\" # local";
        let t = spans(line, dotenv(line));
        assert!(t.contains(&("export", Token::Keyword)));
        assert!(t.contains(&("DATABASE_URL", Token::Property)));
        assert!(t.contains(&("=", Token::Operator)));
        assert!(t.contains(&("\"postgres://${HOST}/db\"", Token::String)));
        assert!(t.contains(&("${HOST}", Token::Embedded)));
        assert!(t.contains(&("# local", Token::Comment)));
        let line = "PORT=8080 # dev";
        let t = spans(line, dotenv(line));
        assert!(t.contains(&("8080", Token::String)));
        assert!(t.contains(&("# dev", Token::Comment)));
        let line = "KEY='$literal'";
        assert!(
            !spans(line, dotenv(line))
                .iter()
                .any(|(_, t)| *t == Token::Embedded)
        );
    }
}
