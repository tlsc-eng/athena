//! Reads Claude Code's suggested next prompt from the dim placeholder on its input line.

use daedalus_term::StyledCell;

const MARKERS: [char; 2] = ['❯', '>'];
const MAX_CHARS: usize = 500;
const MIN_RULE: usize = 20;
const EXAMPLE_PREFIX: &str = "Try \"";

pub(super) fn parse(rows: &[Vec<StyledCell>]) -> Option<String> {
    let at = rows.iter().rposition(|row| marker_col(row).is_some())?;
    let row = &rows[at];
    let start = marker_col(row)? + 2;
    if !rows
        .get(at.checked_sub(1)?)
        .is_some_and(|above| has_rule(above))
    {
        return None;
    }
    let mut body = row.get(start..).unwrap_or_default();
    if let Some((last, rest)) = body.split_last()
        && last.c == '│'
    {
        body = rest;
        while let Some((last, rest)) = body.split_last()
            && last.c == ' '
        {
            body = rest;
        }
    }
    let mut dim_seen = false;
    for (i, cell) in body.iter().enumerate() {
        if cell.c == ' ' {
            continue;
        }
        if cell.dim {
            dim_seen = true;
        } else if !(i == 0 && cell.inverse) {
            return None;
        }
    }
    if !dim_seen || wrapped(rows.get(at + 1), start) {
        return None;
    }
    let text: String = body.iter().map(|cell| cell.c).collect();
    let text = text.trim();
    let rejected = text.is_empty()
        || text.chars().count() > MAX_CHARS
        || text.chars().any(char::is_control)
        || text.starts_with(EXAMPLE_PREFIX);
    (!rejected).then(|| text.to_string())
}

fn marker_col(row: &[StyledCell]) -> Option<usize> {
    let col = row.iter().position(|cell| cell.c != ' ')?;
    let followed = row
        .get(col + 1)
        .is_some_and(|c| matches!(c.c, ' ' | '\u{a0}'));
    (col <= 2 && MARKERS.contains(&row[col].c) && followed).then_some(col)
}

fn has_rule(row: &[StyledCell]) -> bool {
    let mut run = 0;
    for cell in row {
        run = if cell.c == '─' { run + 1 } else { 0 };
        if run >= MIN_RULE {
            return true;
        }
    }
    false
}

fn wrapped(below: Option<&Vec<StyledCell>>, start: usize) -> bool {
    below.is_some_and(|row| row.iter().skip(start).any(|cell| cell.dim && cell.c != ' '))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(screen: &str) -> Vec<Vec<StyledCell>> {
        screen
            .lines()
            .map(|line| {
                let mut cells = Vec::new();
                let (mut dim, mut inverse) = (false, false);
                for c in line.chars() {
                    match c {
                        '{' => dim = true,
                        '}' => dim = false,
                        '[' => inverse = true,
                        ']' => inverse = false,
                        c => cells.push(StyledCell { c, dim, inverse }),
                    }
                }
                cells
            })
            .collect()
    }

    fn screen(prompt: &str) -> String {
        let rule = "─".repeat(40);
        format!("{rule}\n{prompt}\n{rule}\n  ? for shortcuts")
    }

    fn parsed(prompt: &str) -> Option<String> {
        parse(&rows(&screen(prompt)))
    }

    #[test]
    fn a_finished_screen_yields_the_suggestion() {
        assert_eq!(
            parsed("❯ [r]{un the tests}").as_deref(),
            Some("run the tests")
        );
    }

    #[test]
    fn the_cursor_cell_is_optional() {
        assert_eq!(
            parsed("❯ {run the tests}").as_deref(),
            Some("run the tests")
        );
    }

    #[test]
    fn the_greater_than_marker_works() {
        assert_eq!(
            parsed("> {run the tests}").as_deref(),
            Some("run the tests")
        );
    }

    #[test]
    fn plain_draft_text_is_not_a_suggestion() {
        assert_eq!(parsed("❯ run the tests"), None);
    }

    #[test]
    fn mixed_dim_and_plain_text_is_not_a_suggestion() {
        assert_eq!(parsed("❯ {run the} tests"), None);
    }

    #[test]
    fn an_empty_prompt_is_not_a_suggestion() {
        assert_eq!(parsed("❯ [ ]"), None);
    }

    #[test]
    fn a_prompt_row_without_a_rule_above_is_ignored() {
        let got = parse(&rows("some output\n❯ {run the tests}\n  ? for shortcuts"));
        assert_eq!(got, None);
    }

    #[test]
    fn only_the_last_prompt_row_counts() {
        let rule = "─".repeat(40);
        let old = format!("{rule}\n❯ {{old suggestion}}\n{rule}\nsome output\n");
        let got = parse(&rows(&format!("{old}{}", screen("❯ my own draft"))));
        assert_eq!(got, None);
    }

    #[test]
    fn a_wrapped_suggestion_is_dropped() {
        let rule = "─".repeat(40);
        let got = parse(&rows(&format!(
            "{rule}\n❯ {{run the tests and}}\n  {{commit}}\n{rule}"
        )));
        assert_eq!(got, None);
    }

    #[test]
    fn the_first_session_example_is_not_a_suggestion() {
        assert_eq!(parsed("❯ {Try \"fix lint errors\"}"), None);
    }

    #[test]
    fn a_suggestion_over_the_limit_is_dropped() {
        assert_eq!(parsed(&format!("❯ {{{}}}", "a".repeat(501))), None);
    }
}
