use std::ops::Range;

use regex::{Regex, RegexBuilder};

use crate::buffer::is_word;

/// The Match Case, Match Whole Word and Use Regular Expression toggles of a find field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FindOptions {
    pub case: bool,
    pub word: bool,
    pub regex: bool,
}

/// Compiles `query` as a regex, or as literal text when `opts.regex` is off; the error is one line.
pub fn compile(query: &str, opts: FindOptions) -> Result<Regex, String> {
    let pattern = if opts.regex {
        query.to_string()
    } else {
        regex::escape(query)
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(!opts.case)
        .multi_line(true)
        .crlf(true)
        .build()
        .map_err(|e| error_line(&e))
}

/// The regex crate's message without the pattern and caret it draws above it.
fn error_line(e: &regex::Error) -> String {
    let text = e.to_string();
    let last = text.lines().rev().find(|l| !l.trim().is_empty());
    last.unwrap_or(&text)
        .trim()
        .trim_start_matches("error: ")
        .to_string()
}

/// Whether `range` of `text` stands alone as VS Code's whole-word search sees it: an edge that is a
/// word char must not touch another word char.
pub fn is_whole_word(text: &str, range: &Range<usize>) -> bool {
    let matched = &text[range.clone()];
    let left = match (
        text[..range.start].chars().next_back(),
        matched.chars().next(),
    ) {
        (Some(before), Some(first)) => !is_word(before) || !is_word(first),
        _ => true,
    };
    let right = match (
        matched.chars().next_back(),
        text[range.end..].chars().next(),
    ) {
        (Some(last), Some(after)) => !is_word(last) || !is_word(after),
        _ => true,
    };
    left && right
}

/// Byte ranges of the non-empty matches of `re` in `text`; with `word`, only whole words, a
/// rejected match letting the search resume one char later.
pub fn find_matches(re: &Regex, text: &str, word: bool) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at <= text.len() {
        let Some(m) = re.find_at(text, at) else {
            break;
        };
        let range = m.range();
        if !range.is_empty() && (!word || is_whole_word(text, &range)) {
            at = range.end;
            out.push(range);
        } else {
            at = range.start + text[range.start..].chars().next().map_or(1, char::len_utf8);
        }
    }
    out
}

/// What the match of `re` at `range` becomes: `with` verbatim, or in regex mode with VS Code's
/// `$1`, `$&`, `\n` and `\t` filled in.
pub fn replacement(
    re: &Regex,
    text: &str,
    range: &Range<usize>,
    with: &str,
    opts: FindOptions,
) -> String {
    if !opts.regex {
        return with.to_string();
    }
    let Some(caps) = re.captures_at(text, range.start) else {
        return with.to_string();
    };
    let mut out = String::new();
    caps.expand(&template(with, re.captures_len()), &mut out);
    out
}

/// `with` in every match of `text`, line by line as project search reads files; how many it replaced.
pub fn replace_lines(re: &Regex, text: &str, with: &str, opts: FindOptions) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let mut last = 0;
        for range in find_matches(re, body, opts.word) {
            out.push_str(&body[last..range.start]);
            out.push_str(&replacement(re, body, &range, with, opts));
            last = range.end;
            count += 1;
        }
        out.push_str(&line[last..]);
    }
    (out, count)
}

/// A VS Code replace string as the regex crate's expansion syntax: `$n` and `$nn` name groups
/// only up to `groups`, `$&` is the match, any other `$` is literal, and `\n`, `\t`, `\\` escape.
fn template(with: &str, groups: usize) -> String {
    let mut out = String::with_capacity(with.len());
    let mut chars = with.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '$' => match chars.peek().copied() {
                Some('$') => {
                    chars.next();
                    out.push_str("$$");
                }
                Some('&') => {
                    chars.next();
                    out.push_str("${0}");
                }
                Some(d) if d.is_ascii_digit() => {
                    chars.next();
                    let mut n = d.to_digit(10).unwrap_or(0) as usize;
                    if let Some(e) = chars.peek().and_then(|e| e.to_digit(10))
                        && n * 10 + (e as usize) < groups
                    {
                        chars.next();
                        n = n * 10 + e as usize;
                    }
                    out.push_str(&format!("${{{n}}}"));
                }
                _ => out.push_str("$$"),
            },
            '\\' => match chars.peek().copied() {
                Some('n') => {
                    chars.next();
                    out.push('\n');
                }
                Some('t') => {
                    chars.next();
                    out.push('\t');
                }
                Some('\\') => {
                    chars.next();
                    out.push('\\');
                }
                _ => out.push('\\'),
            },
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(case: bool, word: bool, regex: bool) -> FindOptions {
        FindOptions { case, word, regex }
    }

    fn found<'a>(query: &str, text: &'a str, o: FindOptions) -> Vec<&'a str> {
        let re = compile(query, o).unwrap();
        find_matches(&re, text, o.word)
            .into_iter()
            .map(|r| &text[r])
            .collect()
    }

    #[test]
    fn case_word_and_regex_toggles_combine() {
        let text = "Foo foo food FOO_bar foo.";
        assert_eq!(found("foo", text, opts(false, false, false)).len(), 5);
        assert_eq!(found("foo", text, opts(true, false, false)).len(), 3);
        assert_eq!(
            found("foo", text, opts(false, true, false)),
            ["Foo", "foo", "foo"]
        );
        assert_eq!(found("foo", text, opts(true, true, false)), ["foo", "foo"]);
        assert_eq!(
            found("f.o", text, opts(false, false, false)),
            Vec::<&str>::new()
        );
        assert_eq!(found("f.o", text, opts(true, false, true)).len(), 3);
        assert_eq!(
            found("fo+d?", text, opts(true, true, true)),
            ["foo", "food", "foo"]
        );
        assert_eq!(
            found("FOO", text, opts(true, true, true)),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn whole_words_respect_unicode_letters() {
        let text = "cafés café Über über naïve_x naïve";
        assert_eq!(found("café", text, opts(false, true, false)), ["café"]);
        assert_eq!(
            found("über", text, opts(false, true, false)),
            ["Über", "über"]
        );
        assert_eq!(found("über", text, opts(true, true, false)), ["über"]);
        assert_eq!(found("naïve", text, opts(false, true, false)), ["naïve"]);
        assert_eq!(
            found("ü", text, opts(false, true, false)),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn a_rejected_whole_word_match_does_not_hide_one_starting_inside_it() {
        assert_eq!(found("aa", "aaa aa", opts(true, true, false)), ["aa"]);
        assert_eq!(found("a+", "ba aa", opts(true, true, true)), ["aa"]);
    }

    #[test]
    fn whole_word_ignores_edges_that_are_not_word_chars() {
        assert_eq!(found("(x", "f(x)", opts(true, true, false)), ["(x"]);
        assert_eq!(found("x)", "f(x)y", opts(true, true, false)), ["x)"]);
        assert_eq!(
            found("(x", "a(xy", opts(true, true, false)),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn regex_errors_are_one_line() {
        let err = compile("a(b", opts(false, false, true)).unwrap_err();
        assert!(!err.contains('\n'), "{err}");
        assert!(err.contains("unclosed group"), "{err}");
        assert!(compile("a(b", opts(false, false, false)).is_ok());
    }

    #[test]
    fn line_anchors_hold_with_crlf() {
        let text = "foo\r\nbar foo\r\n";
        assert_eq!(found("foo$", text, opts(true, false, true)), ["foo", "foo"]);
        assert_eq!(found("^bar", text, opts(true, false, true)), ["bar"]);
    }

    #[test]
    fn empty_regex_matches_are_skipped() {
        assert_eq!(found("x*", "axb", opts(true, false, true)), ["x"]);
    }

    fn replaced(query: &str, text: &str, with: &str, o: FindOptions) -> String {
        replace_lines(&compile(query, o).unwrap(), text, with, o).0
    }

    #[test]
    fn regex_replacement_fills_in_groups_as_vs_code_does() {
        let o = opts(true, false, true);
        assert_eq!(replaced(r"(\w+)=(\w+)", "a=b c=d", "$2=$1", o), "b=a d=c");
        assert_eq!(replaced(r"(\w+)", "ab", "$1x", o), "abx");
        assert_eq!(replaced(r"(\w+)", "ab", "[$&]", o), "[ab]");
        assert_eq!(replaced(r"(\w+)", "ab", "$$1", o), "$1");
        assert_eq!(replaced(r"(\w+)", "ab", "$name", o), "$name");
        assert_eq!(replaced(r"(\w+)", "ab", "$12", o), "ab2");
        assert_eq!(replaced(r"(\w),", "a,b", r"$1\n", o), "a\nb");
        assert_eq!(replaced(r"(\w),", "a,b", r"\\t", o), r"\tb");
    }

    #[test]
    fn literal_replacement_keeps_dollars() {
        let o = opts(false, false, false);
        assert_eq!(replaced("old", "old OLD\n", "$1 $&", o), "$1 $& $1 $&\n");
    }

    #[test]
    fn replacing_lines_keeps_their_breaks_and_counts() {
        let o = opts(true, true, true);
        let re = compile("x$", o).unwrap();
        let (text, n) = replace_lines(&re, "x\r\nax\nx", "y", o);
        assert_eq!((text.as_str(), n), ("y\r\nax\ny", 2));
    }
}
