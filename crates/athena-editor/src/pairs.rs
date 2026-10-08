use crate::syntax::Lang;

const BRACKETS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];

/// Characters an auto-closed pair may sit before, as VS Code's default `autoCloseBefore`.
const CLOSE_BEFORE: &str = ";:.,=}])>";

/// The pairs a language auto-closes: brackets everywhere, quotes where the language uses them.
pub(crate) fn pairs_for(lang: Option<Lang>) -> Vec<(char, char)> {
    let mut pairs = BRACKETS.to_vec();
    let double = !matches!(lang, Some(Lang::Markdown));
    // Rust's `'` starts lifetimes, and prose is full of apostrophes.
    let single = matches!(
        lang,
        Some(
            Lang::Go
                | Lang::TypeScript
                | Lang::Tsx
                | Lang::JavaScript
                | Lang::Python
                | Lang::Shell
                | Lang::Css
                | Lang::Html
                | Lang::Yaml
                | Lang::Toml
                | Lang::Dockerfile
                | Lang::DotEnv
        )
    );
    let backtick = matches!(
        lang,
        Some(
            Lang::Go
                | Lang::TypeScript
                | Lang::Tsx
                | Lang::JavaScript
                | Lang::Shell
                | Lang::Markdown
        )
    );
    for (on, quote) in [(double, '"'), (single, '\''), (backtick, '`')] {
        if on {
            pairs.push((quote, quote));
        }
    }
    pairs
}

pub(crate) fn closer_of(pairs: &[(char, char)], open: char) -> Option<char> {
    pairs.iter().find(|(o, _)| *o == open).map(|(_, c)| *c)
}

fn is_quote(c: char) -> bool {
    matches!(c, '"' | '\'' | '`')
}

/// Whether typing `open` between `prev` and `next` also inserts its closer.
pub(crate) fn should_close(
    open: char,
    prev: Option<char>,
    next: Option<char>,
    in_string_or_comment: bool,
) -> bool {
    if in_string_or_comment {
        return false;
    }
    let before_ok = next.is_none_or(|n| n.is_whitespace() || CLOSE_BEFORE.contains(n));
    // A quote after a word char is an apostrophe or a closing quote, never an opening one.
    let after_ok =
        !is_quote(open) || prev.is_none_or(|p| !(crate::buffer::is_word(p) || p == open));
    before_ok && after_ok
}

/// Up to this many auto-inserted closers are remembered for typing over and deleting.
const KEPT: usize = 8;

/// Closers the cursor auto-inserted, as char offsets, so only those are typed over or deleted with
/// their opener (VS Code's "auto" overtype and delete).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct AutoClosed {
    at: [usize; KEPT],
    len: u8,
}

impl AutoClosed {
    pub fn contains(&self, at: usize) -> bool {
        self.at[..self.len as usize].contains(&at)
    }

    pub fn push(&mut self, at: usize) {
        if self.len as usize == KEPT {
            self.at.copy_within(1.., 0);
            self.len -= 1;
        }
        self.at[self.len as usize] = at;
        self.len += 1;
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Moves each entry through `map`, dropping those it returns `None` for.
    pub fn retain_map(&mut self, mut map: impl FnMut(usize) -> Option<usize>) {
        let mut kept = 0;
        for i in 0..self.len as usize {
            if let Some(at) = map(self.at[i]) {
                self.at[kept] = at;
                kept += 1;
            }
        }
        self.len = kept as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closes_before_blanks_and_closers_only() {
        assert!(should_close('(', Some('x'), None, false));
        assert!(should_close('(', None, Some(' '), false));
        assert!(should_close('[', Some('('), Some(')'), false));
        assert!(should_close('{', None, Some(';'), false));
        assert!(!should_close('(', None, Some('x'), false), "before a word");
        assert!(!should_close('(', None, Some('"'), false));
    }

    #[test]
    fn quotes_do_not_close_after_a_word_or_inside_strings_and_comments() {
        assert!(should_close('"', Some(' '), None, false));
        assert!(should_close('"', Some('('), Some(')'), false));
        assert!(!should_close('\'', Some('n'), Some(' '), false), "don't");
        assert!(!should_close('"', Some('"'), None, false));
        assert!(!should_close('"', Some(' '), None, true));
        assert!(!should_close('(', Some(' '), None, true));
    }

    #[test]
    fn languages_pick_their_quotes() {
        let has = |lang, c| closer_of(&pairs_for(lang), c).is_some();
        assert!(has(Some(Lang::Go), '`'));
        assert!(has(Some(Lang::TypeScript), '\''));
        assert!(!has(Some(Lang::Rust), '\''), "lifetimes");
        assert!(has(Some(Lang::Rust), '"'));
        assert!(!has(Some(Lang::Markdown), '"'));
        assert!(has(Some(Lang::Markdown), '`'));
        assert!(has(None, '('));
        assert!(!has(None, '\''));
    }

    #[test]
    fn remembers_a_bounded_number_of_closers() {
        let mut closed = AutoClosed::default();
        for at in 0..10 {
            closed.push(at);
        }
        assert!(!closed.contains(1), "the oldest are forgotten");
        assert!(closed.contains(9));
        closed.retain_map(|at| (at % 2 == 0).then_some(at + 100));
        assert!(closed.contains(102) && !closed.contains(103) && !closed.contains(2));
    }
}
