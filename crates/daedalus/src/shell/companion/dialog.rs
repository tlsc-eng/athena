//! Reads the dialog Claude Code has open from the pane's visible text.
//!
//! Anything the parser does not recognise yields `None` or `Unknown`, never a guess, because the answer executor treats a mismatch as a reason to stop.

use crate::companion::protocol::{Dialog, DialogKind, DialogOption, DialogTab};

const FOOTERS: [&str; 4] = [
    "Esc to cancel",
    "Enter to confirm",
    "Enter to select",
    "to navigate",
];
const POINTER: char = '❯';
const ASK_AGAIN: &str = "Yes, and don’t ask again for";
const MAX_LINES: usize = 12;
const MAX_OPTIONS: usize = 12;
const MAX_LINE_CHARS: usize = 200;
const MAX_TEXT_CHARS: usize = 500;
/// A pointer further right than this is body text, not a list cursor.
const MAX_POINTER_COL: usize = 4;

type Line = Vec<char>;

struct Row {
    slot: char,
    index: Option<u32>,
    text: String,
}

/// Parses the screen lines into a dialog, or `None` when no dialog footer is on screen.
pub(super) fn parse(lines: &[String]) -> Option<Dialog> {
    let footer = lines
        .iter()
        .rposition(|line| FOOTERS.iter().any(|f| line.contains(f)))?;
    let screen: Vec<Line> = lines[..footer].iter().map(|l| normalise(l)).collect();
    let (start, end) = block_bounds(&screen);
    let block = &screen[start..end];
    let pointers: Vec<usize> = block
        .iter()
        .enumerate()
        .filter(|(_, line)| pointer_col(line).is_some())
        .map(|(at, _)| at)
        .collect();
    let [anchor] = pointers[..] else {
        return Some(unknown(block, Vec::new()));
    };
    let pc = pointer_col(&block[anchor]).unwrap_or_default();
    Some(read(block, anchor, pc))
}

fn read(block: &[Line], anchor: usize, pc: usize) -> Dialog {
    let Some(found) = find_rows(block, anchor, pc) else {
        return unknown(block, Vec::new());
    };
    let Found {
        rows,
        submit,
        highlight_on_submit,
        mut partial,
    } = found;
    let first = rows[0].0;
    if rows.iter().any(|(_, r)| matches!(r.slot, '↑' | '↓')) {
        partial = true;
    }
    let indexed = rows[0].1.index.is_some();
    if indexed && rows[0].1.index != Some(1) && !partial {
        return unknown(block, Vec::new());
    }
    partial |= unplaced(
        block,
        &rows,
        submit.as_ref().map(|(at, _)| *at),
        pc,
        indexed,
    );

    let mut options: Vec<DialogOption> = rows
        .iter()
        .enumerate()
        .map(|(i, (at, row))| {
            let end = rows.get(i + 1).map_or(block.len(), |(next, _)| *next);
            option(&row.text, description(block, *at, end, pc))
        })
        .collect();
    let mut highlight = rows
        .iter()
        .position(|(_, r)| r.slot == POINTER)
        .map(|at| at as u32);
    if highlight_on_submit {
        highlight = Some(options.len() as u32);
    }
    if options.len() > MAX_OPTIONS {
        options.truncate(MAX_OPTIONS);
        partial = true;
    }

    let multi = options.iter().any(|o| o.checked.is_some());
    if multi && options.iter().any(|o| o.checked.is_none() && !o.input) {
        partial = true;
    }

    let mut head: Vec<String> = block[..first].iter().map(|l| clean(l)).collect();
    let tabs = take_tabs(&mut head);
    let review = block
        .iter()
        .any(|l| clean(l).starts_with("Review your answers"));
    let question = !tabs.is_empty() || multi || options.iter().any(|o| o.label == "Other");
    let kind = if review {
        DialogKind::Review
    } else if question {
        DialogKind::Question
    } else if options.len() >= 2 {
        DialogKind::Select
    } else {
        DialogKind::Unknown
    };
    if kind == DialogKind::Unknown {
        return unknown(block, options);
    }

    let (title, lines, prompt) = if kind == DialogKind::Question {
        (question_title(&head), Vec::new(), None)
    } else {
        titled(&head)
    };
    let key = key(
        kind,
        &tabs,
        title.as_deref(),
        prompt.as_deref(),
        &lines,
        &options,
    );
    Dialog {
        key,
        kind,
        title,
        lines,
        prompt,
        tabs,
        multi,
        options,
        submit: submit.map(|(_, label)| label),
        highlight,
        partial,
    }
}

struct Found {
    rows: Vec<(usize, Row)>,
    submit: Option<(usize, String)>,
    highlight_on_submit: bool,
    partial: bool,
}

/// Collects the option rows around the pointer, or `None` when the pointer is not on a row.
fn find_rows(block: &[Line], anchor: usize, pc: usize) -> Option<Found> {
    if submit_at(&block[anchor], pc).is_some() {
        let mut at = anchor;
        let from = loop {
            at = at.checked_sub(1)?;
            let line = &block[at];
            if is_blank(line) {
                continue;
            }
            let row = row_at(line, pc)?;
            row.index?;
            break (at, row);
        };
        let rows = grow_indexed(block, pc, from);
        let submit = submit_below(block, pc, rows.last()?.0)?;
        return (submit.0 == anchor).then_some(Found {
            rows,
            submit: Some(submit),
            highlight_on_submit: true,
            partial: false,
        });
    }
    let row = row_at(&block[anchor], pc)?;
    if row.index.is_some() {
        let rows = grow_indexed(block, pc, (anchor, row));
        let submit = submit_below(block, pc, rows.last()?.0);
        return Some(Found {
            rows,
            submit,
            highlight_on_submit: false,
            partial: false,
        });
    }
    let mut first = anchor;
    while first > 0 && plain_row(&block[first - 1], pc) {
        first -= 1;
    }
    let mut last = anchor;
    while last + 1 < block.len() && plain_row(&block[last + 1], pc) {
        last += 1;
    }
    let rows = (first..=last)
        .filter_map(|at| row_at(&block[at], pc).map(|r| (at, r)))
        .collect();
    Some(Found {
        rows,
        submit: None,
        highlight_on_submit: false,
        partial: false,
    })
}

fn plain_row(line: &Line, pc: usize) -> bool {
    row_at(line, pc).is_some_and(|r| r.index.is_none())
}

/// Extends one numbered row to the run of rows numbered one apart, skipping descriptions and blanks.
fn grow_indexed(block: &[Line], pc: usize, from: (usize, Row)) -> Vec<(usize, Row)> {
    let mut rows = vec![from];
    while let Some(index) = rows[0].1.index.and_then(|i| i.checked_sub(1))
        && let Some(prev) = neighbour(block, pc, rows[0].0, false, index)
    {
        rows.insert(0, prev);
    }
    while let Some(index) = rows[rows.len() - 1].1.index.map(|i| i + 1)
        && let Some(next) = neighbour(block, pc, rows[rows.len() - 1].0, true, index)
    {
        rows.push(next);
    }
    rows
}

fn neighbour(
    block: &[Line],
    pc: usize,
    from: usize,
    down: bool,
    want: u32,
) -> Option<(usize, Row)> {
    let mut at = from;
    loop {
        at = if down { at + 1 } else { at.checked_sub(1)? };
        let line = block.get(at)?;
        if is_blank(line) || indent(line) > pc + 2 {
            continue;
        }
        let row = row_at(line, pc)?;
        return (row.index == Some(want)).then_some((at, row));
    }
}

fn submit_below(block: &[Line], pc: usize, after: usize) -> Option<(usize, String)> {
    for (at, line) in block.iter().enumerate().skip(after + 1) {
        if let Some(label) = submit_at(line, pc) {
            return Some((at, label));
        }
        if !is_blank(line) && indent(line) <= pc + 2 {
            return None;
        }
    }
    None
}

/// A non-blank line inside or after the rows that is neither a row, a description nor the submit row.
fn unplaced(
    block: &[Line],
    rows: &[(usize, Row)],
    submit: Option<usize>,
    pc: usize,
    indexed: bool,
) -> bool {
    let first = rows[0].0;
    (first..block.len()).any(|at| {
        let line = &block[at];
        let placed = is_blank(line)
            || Some(at) == submit
            || rows.iter().any(|(row, _)| *row == at)
            || (indexed && indent(line) > pc + 2);
        !placed
    })
}

fn description(block: &[Line], row: usize, end: usize, pc: usize) -> Option<String> {
    let text: Vec<String> = block[row + 1..end]
        .iter()
        .take_while(|l| !is_blank(l) && indent(l) > pc + 2)
        .map(|l| clean(l))
        .collect();
    (!text.is_empty()).then(|| cap(&text.join(" "), MAX_LINE_CHARS))
}

fn option(text: &str, description: Option<String>) -> DialogOption {
    let (checked, label) = if let Some(rest) = text.strip_prefix("[✔] ") {
        (Some(true), rest.trim())
    } else if let Some(rest) = text.strip_prefix("[ ] ") {
        (Some(false), rest.trim())
    } else {
        (None, text)
    };
    let mut option = DialogOption {
        label: cap(label, MAX_LINE_CHARS),
        description,
        checked,
        input: false,
        text: None,
    };
    if matches!(label, "Other" | "Type something." | "Type something") {
        option.label = "Other".into();
        option.input = true;
        option.text = Some(String::new());
    } else if let Some(rest) = ask_again_text(label) {
        option.label = ASK_AGAIN.into();
        option.input = true;
        option.text = Some(cap(rest, MAX_TEXT_CHARS));
    }
    option
}

fn ask_again_text(label: &str) -> Option<&str> {
    let rest = label
        .strip_prefix(ASK_AGAIN)
        .or_else(|| label.strip_prefix(&ASK_AGAIN.replace('’', "'")))?;
    Some(rest.strip_prefix(':')?.trim())
}

/// Removes and parses the `←  ☒ Color   ☐ Size   ✔ Submit  →` header from the head lines.
fn take_tabs(head: &mut Vec<String>) -> Vec<DialogTab> {
    let Some(at) = head
        .iter()
        .position(|l| (l.contains('☐') || l.contains('☒')) && l.contains("Submit"))
    else {
        return Vec::new();
    };
    let Some(tabs) = parse_tabs(&head[at]) else {
        return Vec::new();
    };
    head.remove(at);
    tabs
}

fn parse_tabs(header: &str) -> Option<Vec<DialogTab>> {
    let marks: Vec<(usize, char)> = header
        .char_indices()
        .filter(|(_, c)| matches!(c, '☐' | '☒' | '✔'))
        .collect();
    let mut tabs = Vec::new();
    let mut submit = false;
    for (i, (at, mark)) in marks.iter().enumerate() {
        let from = at + mark.len_utf8();
        let to = marks.get(i + 1).map_or(header.len(), |(next, _)| *next);
        let label =
            header[from..to].trim_matches(|c: char| c.is_whitespace() || c == '→' || c == '←');
        match mark {
            '✔' if label == "Submit" => submit = true,
            '✔' => return None,
            _ if label.is_empty() => return None,
            _ => tabs.push(DialogTab {
                label: cap(label, MAX_LINE_CHARS),
                answered: *mark == '☒',
            }),
        }
    }
    (submit && !tabs.is_empty()).then_some(tabs)
}

fn question_title(head: &[String]) -> Option<String> {
    let block: Vec<&str> = head
        .iter()
        .skip_while(|l| l.is_empty())
        .take_while(|l| !l.is_empty())
        .map(String::as_str)
        .collect();
    (!block.is_empty()).then(|| cap(&block.join(" "), MAX_LINE_CHARS))
}

/// Splits the head into the title, the lines between it and the question, and the question nearest the rows.
fn titled(head: &[String]) -> (Option<String>, Vec<String>, Option<String>) {
    let mut text = head.iter().filter(|l| !l.is_empty());
    let Some(title) = text.next() else {
        return (None, Vec::new(), None);
    };
    let rest: Vec<&String> = text.collect();
    let prompt = rest.iter().rposition(|l| l.ends_with('?'));
    let lines = rest
        .iter()
        .enumerate()
        .filter(|(at, _)| Some(*at) != prompt)
        .take(MAX_LINES)
        .map(|(_, l)| cap(l, MAX_LINE_CHARS))
        .collect();
    (
        Some(cap(title, MAX_LINE_CHARS)),
        lines,
        prompt.map(|at| cap(rest[at], MAX_LINE_CHARS)),
    )
}

fn unknown(block: &[Line], options: Vec<DialogOption>) -> Dialog {
    let (title, lines, prompt) = titled(&block.iter().map(|l| clean(l)).collect::<Vec<_>>());
    let key = key(
        DialogKind::Unknown,
        &[],
        title.as_deref(),
        prompt.as_deref(),
        &lines,
        &options,
    );
    Dialog {
        key,
        kind: DialogKind::Unknown,
        title,
        lines,
        prompt,
        tabs: Vec::new(),
        multi: false,
        options,
        submit: None,
        highlight: None,
        partial: true,
    }
}

/// Hashes what identifies the dialog and leaves out the highlight, checkboxes and current tab.
/// Without tabs the body lines and input text are in it, so the next permission prompt with the same labels is another dialog.
fn key(
    kind: DialogKind,
    tabs: &[DialogTab],
    title: Option<&str>,
    prompt: Option<&str>,
    lines: &[String],
    options: &[DialogOption],
) -> String {
    let kind = match kind {
        DialogKind::Select => "select",
        DialogKind::Question => "question",
        DialogKind::Review => "review",
        DialogKind::Unknown => "unknown",
    };
    let mut parts = vec![kind];
    if tabs.is_empty() {
        parts.push(title.unwrap_or_default());
        parts.push(prompt.unwrap_or_default());
        parts.extend(lines.iter().map(String::as_str));
        for option in options {
            parts.push(option.label.as_str());
            parts.extend(option.text.as_deref());
        }
    } else {
        parts.extend(tabs.iter().map(|t| t.label.as_str()));
        if kind == "review" {
            parts.extend(lines.iter().map(String::as_str));
        }
    }
    let mut hash: u32 = 0x811c_9dc5;
    for byte in parts.join("\0").bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

fn row_at(line: &[char], pc: usize) -> Option<Row> {
    let slot = *line.get(pc)?;
    if !matches!(slot, POINTER | '↑' | '↓' | ' ') || *line.get(pc + 1)? != ' ' {
        return None;
    }
    let rest = &line[pc + 2..];
    if rest.first().is_none_or(|c| *c == ' ') {
        return None;
    }
    let digits = rest.iter().take_while(|c| c.is_ascii_digit()).count();
    if (1..=3).contains(&digits)
        && rest.get(digits) == Some(&'.')
        && rest.get(digits + 1) == Some(&' ')
    {
        let index = text_of(&rest[..digits]).parse().ok()?;
        return Some(Row {
            slot,
            index: Some(index),
            text: text_of(&rest[digits + 2..]).trim().to_string(),
        });
    }
    Some(Row {
        slot,
        index: None,
        text: text_of(rest).trim().to_string(),
    })
}

/// The label of the unnumbered `Next` or `Submit` row that follows a multi-select's options.
fn submit_at(line: &[char], pc: usize) -> Option<String> {
    let slot = *line.get(pc)?;
    if !matches!(slot, POINTER | ' ') || *line.get(pc + 1)? != ' ' {
        return None;
    }
    let label = text_of(&line[pc + 2..]);
    let label = label.trim();
    matches!(label, "Next" | "Submit").then(|| label.to_string())
}

fn pointer_col(line: &[char]) -> Option<usize> {
    let at = line.iter().position(|c| !c.is_whitespace())?;
    (line[at] == POINTER && at <= MAX_POINTER_COL).then_some(at)
}

fn indent(line: &[char]) -> usize {
    line.iter().take_while(|c| c.is_whitespace()).count()
}

fn is_blank(line: &[char]) -> bool {
    line.iter().all(|c| c.is_whitespace())
}

fn is_border(c: char) -> bool {
    matches!(c, '─' | '━' | '═' | '│' | '┃' | '╭' | '╮' | '╰' | '╯')
}

/// Turns a box's side border into a space so columns stay put, and drops trailing border and blanks.
fn normalise(line: &str) -> Line {
    let mut chars: Line = line.chars().collect();
    if let Some(at) = chars.iter().take(3).position(|c| !c.is_whitespace())
        && matches!(chars[at], '│' | '┃')
    {
        chars[at] = ' ';
    }
    while chars
        .last()
        .is_some_and(|c| c.is_whitespace() || matches!(c, '│' | '┃'))
    {
        chars.pop();
    }
    chars
}

/// The dialog starts after the nearest horizontal rule above its last content line, else at the top.
fn block_bounds(screen: &[Line]) -> (usize, usize) {
    let mut end = screen.len();
    while end > 0
        && screen[end - 1]
            .iter()
            .all(|c| c.is_whitespace() || is_border(*c))
    {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && !is_rule(&screen[start - 1]) {
        start -= 1;
    }
    (start, end)
}

fn is_rule(line: &[char]) -> bool {
    let text = text_of(line);
    let text = text.trim();
    text.starts_with('╭')
        || (text.chars().count() >= 10 && text.chars().all(|c| matches!(c, '─' | '━' | '═' | ' ')))
}

fn clean(line: &[char]) -> String {
    text_of(line)
        .trim_matches(|c: char| c.is_whitespace() || is_border(c))
        .to_string()
}

fn text_of(chars: &[char]) -> String {
    chars.iter().collect()
}

fn cap(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // TODO: every `synthetic-*.txt` fixture is derived from the Claude Code 2.1.318 UI code, not captured; replace each with a real capture.
    const TRUST: &str = include_str!("dialogs/real-trust.txt");
    const BASH: &str = include_str!("dialogs/synthetic-bash.txt");
    const EDIT: &str = include_str!("dialogs/synthetic-edit.txt");
    const PLAN: &str = include_str!("dialogs/synthetic-plan.txt");
    const SINGLE: &str = include_str!("dialogs/synthetic-single-select.txt");
    const MULTI: &str = include_str!("dialogs/synthetic-multi-select.txt");
    const MULTI_SUBMIT: &str = include_str!("dialogs/synthetic-multi-select-submit.txt");
    const TABS: &str = include_str!("dialogs/synthetic-tabs.txt");
    const REVIEW: &str = include_str!("dialogs/synthetic-review.txt");
    const SCROLLED: &str = include_str!("dialogs/synthetic-scrolled.txt");

    fn dialog(screen: &str) -> Dialog {
        parse(&screen.lines().map(str::to_string).collect::<Vec<_>>()).expect("a dialog")
    }

    fn labels(d: &Dialog) -> Vec<&str> {
        d.options.iter().map(|o| o.label.as_str()).collect()
    }

    #[test]
    fn the_trust_dialog_is_a_select_focused_on_the_first_row() {
        let d = dialog(TRUST);
        assert_eq!(d.kind, DialogKind::Select);
        assert_eq!(labels(&d), ["No, exit", "Yes, I trust this folder"]);
        assert_eq!(d.highlight, Some(0));
        assert!(!d.partial && !d.multi && d.tabs.is_empty());
        assert!(d.title.unwrap().starts_with("Quick safety check"));
    }

    #[test]
    fn a_bash_prompt_reads_its_rows_and_the_editable_prefix() {
        let d = dialog(BASH);
        assert_eq!(d.kind, DialogKind::Select);
        assert_eq!(d.title.as_deref(), Some("Bash command"));
        assert_eq!(d.lines, ["npm test", "Run the test suite"]);
        assert_eq!(d.prompt.as_deref(), Some("Do you want to proceed?"));
        assert_eq!(
            labels(&d),
            [
                "Yes",
                "Yes, and don’t ask again for",
                "Yes, and switch to auto mode",
                "No"
            ]
        );
        assert_eq!(d.highlight, Some(0));
        let prefix = &d.options[1];
        assert!(prefix.input);
        assert_eq!(prefix.text.as_deref(), Some("npm test *"));
        assert!(!d.options[0].input && d.options[0].text.is_none());
        assert!(!d.partial);
    }

    #[test]
    fn an_ascii_apostrophe_still_marks_the_prefix_row() {
        let d = dialog(&BASH.replace('’', "'"));
        assert_eq!(d.options[1].label, "Yes, and don’t ask again for");
        assert_eq!(d.options[1].text.as_deref(), Some("npm test *"));
    }

    #[test]
    fn an_edit_prompt_and_a_plan_approval_are_selects() {
        let d = dialog(EDIT);
        assert_eq!(d.kind, DialogKind::Select);
        assert_eq!(d.title.as_deref(), Some("Edit file"));
        assert_eq!(
            d.prompt.as_deref(),
            Some("Do you want to make this edit to foo.rs?")
        );
        assert_eq!(d.options.len(), 3);

        let d = dialog(PLAN);
        assert_eq!(d.kind, DialogKind::Select);
        assert_eq!(d.prompt.as_deref(), Some("Would you like to proceed?"));
        assert_eq!(
            labels(&d),
            [
                "Yes, and auto-accept edits",
                "Yes, and manually approve edits",
                "No, keep planning"
            ]
        );
    }

    #[test]
    fn a_numbered_plan_above_the_options_is_not_read_as_options() {
        let d = dialog(PLAN);
        assert_eq!(d.options.len(), 3);
        assert!(d.lines.contains(&"1. Add the parser".to_string()));
        assert!(!d.partial);
    }

    #[test]
    fn a_single_select_has_descriptions_and_an_other_input() {
        let d = dialog(SINGLE);
        assert_eq!(d.kind, DialogKind::Question);
        assert_eq!(d.title.as_deref(), Some("Which colour?"));
        assert_eq!(labels(&d), ["Red", "Blue", "Other"]);
        assert_eq!(d.options[0].description.as_deref(), Some("The warm one"));
        assert!(d.options[2].input);
        assert_eq!(d.options[2].text.as_deref(), Some(""));
        assert!(!d.multi && d.submit.is_none());
        assert_eq!(d.highlight, Some(0));
    }

    #[test]
    fn a_multi_select_reads_checkboxes_and_the_submit_row() {
        let d = dialog(MULTI);
        assert_eq!(d.kind, DialogKind::Question);
        assert!(d.multi);
        let checked: Vec<_> = d.options.iter().map(|o| o.checked).collect();
        assert_eq!(checked, [Some(false), Some(true), Some(false)]);
        assert_eq!(d.submit.as_deref(), Some("Submit"));
        assert_eq!(d.highlight, Some(0));
        assert!(!d.partial);
    }

    #[test]
    fn a_focused_submit_row_puts_the_highlight_past_the_options() {
        let d = dialog(MULTI_SUBMIT);
        assert_eq!(d.options.len(), 3);
        assert_eq!(d.highlight, Some(3));
        assert_eq!(d.submit.as_deref(), Some("Submit"));
        assert!(!d.partial);
    }

    #[test]
    fn a_tabbed_question_reads_its_tabs_and_the_question_under_them() {
        let d = dialog(TABS);
        assert_eq!(d.kind, DialogKind::Question);
        let tabs: Vec<_> = d
            .tabs
            .iter()
            .map(|t| (t.label.as_str(), t.answered))
            .collect();
        assert_eq!(tabs, [("Color", true), ("Size", false)]);
        assert_eq!(d.title.as_deref(), Some("Which size?"));
        assert_eq!(labels(&d), ["Small", "Large", "Other"]);
        assert_eq!(d.options[0].description.as_deref(), Some("Fits most"));
        assert!(d.options[2].input);
    }

    #[test]
    fn the_review_screen_keeps_its_summary_and_its_two_rows() {
        let d = dialog(REVIEW);
        assert_eq!(d.kind, DialogKind::Review);
        assert_eq!(d.title.as_deref(), Some("Review your answers"));
        assert_eq!(d.prompt.as_deref(), Some("Ready to submit your answers?"));
        assert_eq!(
            d.lines,
            [
                "You have not answered all questions",
                "● Which colour?",
                "→ Blue"
            ]
        );
        assert_eq!(labels(&d), ["Submit answers", "Cancel"]);
        assert_eq!(d.tabs.len(), 2);
    }

    #[test]
    fn a_boxed_dialog_parses_like_the_plain_one() {
        let boxed = format!(
            "╭────────────────────╮\n{}╰────────────────────╯\n",
            BASH.lines()
                .filter(|l| !l.contains("Esc to cancel"))
                .map(|l| format!("│{l:<20}│\n"))
                .collect::<String>()
        );
        let screen = format!("{boxed} Esc to cancel · Tab to amend\n");
        let d = dialog(&screen);
        assert_eq!(d.kind, DialogKind::Select);
        assert_eq!(d.options.len(), 4);
        assert_eq!(d.highlight, Some(0));
    }

    #[test]
    fn the_key_ignores_the_highlight_and_checkboxes_but_not_labels() {
        let key = |s: &str| dialog(s).key;
        let moved = BASH
            .replace("❯ 1. Yes", "  1. Yes")
            .replace("  4. No", "❯ 4. No");
        assert_eq!(key(BASH), key(&moved));
        let toggled = MULTI.replace("[✔] B", "[ ] B").replace("[ ] A", "[✔] A");
        assert_eq!(key(MULTI), key(&toggled));
        let edited = BASH.replace("npm test *", "cargo test *");
        assert_ne!(key(BASH), key(&edited));
        let other_command = BASH.replace("   npm test\n", "   npm run build\n");
        assert_ne!(key(BASH), key(&other_command));
        let relabelled = BASH.replace("3. Yes, and switch", "3. Yes, and change");
        assert_ne!(key(BASH), key(&relabelled));
        assert_ne!(key(TRUST), key(BASH));
        assert_eq!(dialog(BASH).key.len(), 8);
    }

    #[test]
    fn a_review_key_follows_its_summary() {
        let other = REVIEW.replace("→ Blue", "→ Red");
        assert_ne!(dialog(REVIEW).key, dialog(&other).key);
    }

    #[test]
    fn a_tabbed_key_follows_the_tabs_not_the_current_question() {
        let other = TABS
            .replace("Which size?", "Which fit?")
            .replace("Small", "Tiny");
        assert_eq!(dialog(TABS).key, dialog(&other).key);
        let renamed = TABS.replace("☐ Size", "☐ Shade");
        assert_ne!(dialog(TABS).key, dialog(&renamed).key);
    }

    #[test]
    fn a_screen_without_a_footer_is_not_a_dialog() {
        let no_footer: Vec<String> = BASH
            .lines()
            .filter(|l| !l.contains("Esc to cancel"))
            .map(str::to_string)
            .collect();
        assert!(parse(&no_footer).is_none());
        assert!(parse(&[]).is_none());
    }

    #[test]
    fn two_pointer_rows_make_it_unknown_and_partial() {
        let d = dialog(&BASH.replace("  4. No", "❯ 4. No"));
        assert_eq!(d.kind, DialogKind::Unknown);
        assert!(d.partial && d.options.is_empty() && d.highlight.is_none());
    }

    #[test]
    fn a_footer_with_no_pointer_row_is_unknown() {
        let d = dialog(&BASH.replace("❯ 1. Yes", "  1. Yes"));
        assert_eq!(d.kind, DialogKind::Unknown);
        assert!(d.partial);
    }

    #[test]
    fn a_single_row_is_not_a_select() {
        let d = dialog(" Pick one\n\n ❯ 1. Only\n\n Esc to cancel\n");
        assert_eq!(d.kind, DialogKind::Unknown);
        assert!(d.partial);
    }

    #[test]
    fn a_gap_in_the_numbering_leaves_the_stray_row_out_and_marks_it_partial() {
        let d = dialog(&BASH.replace("  3. Yes, and switch", "  7. Yes, and switch"));
        assert_eq!(d.options.len(), 2);
        assert!(d.partial);
    }

    #[test]
    fn numbering_that_does_not_start_at_one_is_unknown_without_a_scroll_hint() {
        let d = dialog(&SCROLLED.replace(['↑', '↓'], " "));
        assert_eq!(d.kind, DialogKind::Unknown);
    }

    #[test]
    fn a_scroll_hint_marks_the_dialog_partial() {
        let d = dialog(SCROLLED);
        assert_eq!(d.kind, DialogKind::Select);
        assert!(d.partial);
        assert_eq!(labels(&d), ["feat/c", "feat/d", "feat/e"]);
        assert_eq!(d.highlight, Some(1));
    }

    #[test]
    fn an_unrecognised_line_among_the_rows_marks_the_dialog_partial() {
        let d = dialog(&BASH.replace("  4. No\n", "  4. No\n weird trailing text\n"));
        assert!(d.partial);
    }

    #[test]
    fn long_text_and_many_options_are_capped() {
        let long = "x".repeat(300);
        let rows: String = (1..=14).map(|i| format!("   {i}. row {i}\n")).collect();
        let d = dialog(&format!(
            " {long}\n\n ❯ 1. row 1\n{}\n Esc to cancel\n",
            &rows[rows.find("   2.").unwrap()..]
        ));
        assert_eq!(d.options.len(), 12);
        assert!(d.partial);
        assert_eq!(d.title.unwrap().chars().count(), 200);
    }
}
