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
}
