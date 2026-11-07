//! Plans the keys that answer a Claude dialog, one key per screen read.
//!
//! Nothing here logs, and every abort reason is a fixed string, so a label or typed text cannot reach a log or the phone's error.

use daedalus_term::{ClaudeState, TerminalView};

use super::dialog;
use crate::companion::protocol::{AnswerPick, Dialog, DialogKind, DialogOption};

const MAX_KEYS: u32 = 80;
/// A wrapped or cut title is trusted only when this much of it matches.
const TITLE_PREFIX: usize = 40;
const SHOWN_CHARS: usize = 40;
const OTHER: &str = "Other";
const SUBMIT_ANSWERS: &str = "Submit answers";
const UNANSWERED: &str = "not answered all questions";

/// What the phone chose, and the dialog key it chose it on.
pub(in crate::shell) struct Goal {
    pub key: String,
    pub answers: Vec<AnswerPick>,
}

/// What the next screen read must show for the key just sent to have worked.
pub(in crate::shell) enum Expect {
    HighlightAt(usize),
    Checked(usize, bool),
    InputStartsWith(usize, String),
    TitleIs(String),
    ReviewOrNext,
    GoneOrKeyChanged,
}

pub(in crate::shell) enum Step {
    Key(Vec<u8>, Expect),
    Done,
    Abort(&'static str),
}

#[derive(Default)]
pub(in crate::shell) struct Progress {
    keys: u32,
    submitted: bool,
    question: Option<usize>,
    rewound: bool,
    tabs: Vec<String>,
    typed: Option<Typed>,
    /// Set by an Enter that ends a question, so a new question counts only right after one.
    entered: bool,
    after_enter: bool,
}

/// The Other row once text is typed into it: it then reads as a plain option, so it is found by position.
struct Typed {
    at: usize,
    title: Option<String>,
    others: Vec<String>,
}

impl Typed {
    fn of(d: &Dialog, at: usize) -> Self {
        Self {
            at,
            title: d.title.clone(),
            others: d
                .options
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != at)
                .map(|(_, o)| o.label.clone())
                .collect(),
        }
    }

    fn fits(&self, d: &Dialog) -> bool {
        let others = d
            .options
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != self.at)
            .map(|(_, o)| &o.label);
        // Typed text can turn a single select into a plain `Select`, with only its first title line.
        matches!(d.kind, DialogKind::Question | DialogKind::Select)
            && (d.title == self.title
                || self
                    .title
                    .as_deref()
                    .is_some_and(|t| title_matches(d.title.as_deref(), t)))
            && d.options.len() == self.others.len() + 1
            && others.eq(self.others.iter())
    }
}

impl Expect {
    /// Whether the screen now, `None` when no dialog is up, is what the key was meant to produce.
    pub(in crate::shell) fn met(&self, before: &Dialog, now: Option<&Dialog>) -> bool {
        match self {
            Self::GoneOrKeyChanged => now.is_none_or(|n| n.key != before.key),
            Self::ReviewOrNext => now.is_none_or(|n| {
                n.key != before.key
                    || n.kind == DialogKind::Review
                    || !title_matches(n.title.as_deref(), before.title.as_deref().unwrap_or(""))
            }),
            Self::HighlightAt(i) => now.is_some_and(|n| {
                n.kind == before.kind
                    && n.title == before.title
                    && n.options.len() == before.options.len()
                    && n.highlight == Some(*i as u32)
            }),
            Self::Checked(i, on) => now.is_some_and(|n| {
                n.title == before.title
                    && n.options.len() == before.options.len()
                    && n.options.get(*i).is_some_and(|o| o.checked == Some(*on))
            }),
            Self::InputStartsWith(i, text) => now.is_some_and(|n| {
                title_matches(n.title.as_deref(), before.title.as_deref().unwrap_or(""))
                    && n.options.get(*i).is_some_and(|o| shows(o, text))
            }),
            Self::TitleIs(q) => now.is_some_and(|n| {
                n.kind == DialogKind::Question && title_matches(n.title.as_deref(), q)
            }),
        }
    }
}

/// Plans the next key from the screen that follows the last one. Call it only once the previous `Expect` held.
pub(in crate::shell) fn step(d: &Dialog, goal: &Goal, p: &mut Progress, app_cursor: bool) -> Step {
    p.after_enter = std::mem::take(&mut p.entered);
    match plan(d, goal, p, app_cursor) {
        Ok(Step::Key(bytes, expect)) => {
            p.keys += 1;
            Step::Key(bytes, expect)
        }
        Ok(step) => step,
        Err(why) => Step::Abort(why),
    }
}

type Plan = Result<Step, &'static str>;

fn plan(d: &Dialog, goal: &Goal, p: &mut Progress, app: bool) -> Plan {
    if p.submitted {
        return Ok(Step::Done);
    }
    if p.keys >= MAX_KEYS {
        return Err("too many keys");
    }
    if d.partial || d.kind == DialogKind::Unknown {
        return Err("the dialog is not fully readable");
    }
    if !key_ok(d, goal, p) {
        return Err("The question changed");
    }
    match d.kind {
        DialogKind::Select if p.typed.as_ref().is_some_and(|t| t.fits(d)) => {
            question(d, goal, p, app)
        }
        DialogKind::Select => select(d, goal, p, app),
        DialogKind::Question => question(d, goal, p, app),
        DialogKind::Review => review(d, goal, p, app),
        DialogKind::Unknown => Err("the dialog is not fully readable"),
    }
}

/// The review screen has its own key, so it is accepted at the end of a form whose tabs it shares.
fn key_ok(d: &Dialog, goal: &Goal, p: &Progress) -> bool {
    d.key == goal.key
        || (d.kind == DialogKind::Review
            && !p.tabs.is_empty()
            && d.tabs.iter().map(|t| &t.label).eq(p.tabs.iter()))
        || p.typed.as_ref().is_some_and(|t| t.fits(d))
}

fn select(d: &Dialog, goal: &Goal, p: &mut Progress, app: bool) -> Plan {
    let label = goal
        .answers
        .first()
        .and_then(|a| a.pick.first())
        .ok_or("no option was chosen")?;
    let to = find(d, label, |_| false)?;
    if let Some(key) = toward(d, to, app)? {
        return Ok(key);
    }
    Ok(enter(p, true))
}

fn review(d: &Dialog, goal: &Goal, p: &mut Progress, app: bool) -> Plan {
    if d.lines.iter().any(|l| norm(l).contains(UNANSWERED)) {
        return Err("not every question is answered");
    }
    if !goal.answers.is_empty() && p.question.is_none_or(|q| q + 1 != goal.answers.len()) {
        return Err("the review came before the last question");
    }
    let to = find(d, SUBMIT_ANSWERS, |_| false)?;
    if let Some(key) = toward(d, to, app)? {
        return Ok(key);
    }
    Ok(enter(p, true))
}

fn question(d: &Dialog, goal: &Goal, p: &mut Progress, app: bool) -> Plan {
    let tabbed = !d.tabs.is_empty();
    if goal.answers.len() != d.tabs.len().max(1) {
        return Err("the answers do not fit the questions");
    }
    let at = answer_index(d, &goal.answers)?;
    if p.question.is_none() {
        p.tabs = d.tabs.iter().map(|t| t.label.clone()).collect();
    }
    match p.question {
        None => {}
        Some(q) if q == at => {}
        Some(q) if p.rewound && p.after_enter && at == q + 1 => p.typed = None,
        Some(q) if !p.rewound && at + 1 == q => {}
        Some(_) => return Err("the questions came in another order"),
    }
    p.question = Some(at);
    p.rewound |= at == 0;
    if !p.rewound {
        return back(d, goal, at, app);
    }
    // The parser reads an Other row with desk-typed text as a plain option, so a missing input row means text we must not type into.
    if p.typed.is_none() && !d.options.iter().any(|o| o.input) {
        return Err("the Other row holds text from the desk");
    }
    let pick = &goal.answers[at];
    if d.multi {
        multi(d, pick, p, app, tabbed)
    } else {
        single(d, pick, p, app, tabbed)
    }
}

/// Steps one tab left, which the binary binds to `tabs:previous`, unless the cursor sits in a text field.
fn back(d: &Dialog, goal: &Goal, at: usize, app: bool) -> Plan {
    let focused = d
        .highlight
        .and_then(|h| d.options.get(h as usize))
        .is_some_and(|o| o.input);
    if focused {
        return Err("the cursor is in a text field");
    }
    let previous = goal.answers[at - 1]
        .question
        .clone()
        .ok_or("an answer lacks its question")?;
    Ok(Step::Key(left(app), Expect::TitleIs(previous)))
}

fn single(d: &Dialog, pick: &AnswerPick, p: &mut Progress, app: bool, tabbed: bool) -> Plan {
    let other = typed_text(pick);
    if other.is_some() && !pick.pick.is_empty() {
        return Err("an option and typed text for one question");
    }
    if let Some(text) = other {
        if let Some(key) = type_other(d, text, p, app)? {
            return Ok(key);
        }
        let at = p.typed.as_ref().map_or(0, |t| t.at);
        if let Some(key) = toward(d, at, app)? {
            return Ok(key);
        }
        return Ok(enter(p, !tabbed));
    }
    let [label] = &pick.pick[..] else {
        return Err("one option is needed for a single choice");
    };
    let to = find(d, label, |_| false)?;
    if let Some(key) = toward(d, to, app)? {
        return Ok(key);
    }
    Ok(enter(p, !tabbed))
}

fn multi(d: &Dialog, pick: &AnswerPick, p: &mut Progress, app: bool, tabbed: bool) -> Plan {
    let skip =
        |i: usize, p: &Progress| d.options[i].input || p.typed.as_ref().is_some_and(|t| t.at == i);
    let mut wanted = vec![false; d.options.len()];
    for label in &pick.pick {
        wanted[find(d, label, |i| skip(i, p))?] = true;
    }
    let desk_ticked = d.options.iter().any(|o| o.input && o.checked == Some(true));
    if desk_ticked && typed_text(pick).is_none() {
        return Err("the Other row is ticked at the desk");
    }
    for (i, option) in d.options.iter().enumerate() {
        let on = wanted[i];
        if skip(i, p) || option.checked.unwrap_or(false) == on {
            continue;
        }
        return Ok(match toward(d, i, app)? {
            Some(key) => key,
            None => Step::Key(b" ".to_vec(), Expect::Checked(i, on)),
        });
    }
    if let Some(text) = typed_text(pick) {
        if let Some(key) = type_other(d, text, p, app)? {
            return Ok(key);
        }
        let at = p.typed.as_ref().map_or(0, |t| t.at);
        if d.options[at].checked != Some(true) {
            return Err("the Other row is not checked");
        }
    }
    if d.submit.is_none() {
        return Err("no submit row");
    }
    if let Some(key) = toward(d, d.options.len(), app)? {
        return Ok(key);
    }
    Ok(enter(p, !tabbed))
}

fn typed_text(pick: &AnswerPick) -> Option<&str> {
    pick.other.as_deref().filter(|o| !o.trim().is_empty())
}

/// Types into the Other row once; `None` means the text is already in place.
fn type_other(
    d: &Dialog,
    text: &str,
    p: &mut Progress,
    app: bool,
) -> Result<Option<Step>, &'static str> {
    if text.chars().any(char::is_control) {
        return Err("the text has control characters");
    }
    if let Some(t) = &p.typed {
        return if shows(&d.options[t.at], text) {
            Ok(None)
        } else {
            Err("the typed text is not on screen")
        };
    }
    let mut rows = d
        .options
        .iter()
        .enumerate()
        .filter(|(_, o)| o.input && o.label == OTHER);
    let at = match (rows.next(), rows.next()) {
        (Some((at, _)), None) => at,
        _ => return Err("no single Other row"),
    };
    if let Some(key) = toward(d, at, app)? {
        return Ok(Some(key));
    }
    let present = d.options[at].text.as_deref().unwrap_or_default();
    if present.is_empty() {
        p.typed = Some(Typed::of(d, at));
        return Ok(Some(Step::Key(
            text.as_bytes().to_vec(),
            Expect::InputStartsWith(at, text.to_string()),
        )));
    }
    if shows(&d.options[at], text) {
        p.typed = Some(Typed::of(d, at));
        return Ok(None);
    }
    Err("the text field already holds other text")
}

/// A key that ends a question or the dialog; `last` says nothing follows it but the dialog closing.
fn enter(p: &mut Progress, last: bool) -> Step {
    if last {
        p.submitted = true;
        Step::Key(b"\r".to_vec(), Expect::GoneOrKeyChanged)
    } else {
        p.entered = true;
        Step::Key(b"\r".to_vec(), Expect::ReviewOrNext)
    }
}

fn rows(d: &Dialog) -> usize {
    d.options.len() + usize::from(d.submit.is_some())
}

/// One arrow towards row `to`, or `None` when the highlight is already there.
fn toward(d: &Dialog, to: usize, app: bool) -> Result<Option<Step>, &'static str> {
    let at = d.highlight.ok_or("no row is highlighted")? as usize;
    if at >= rows(d) || to >= rows(d) {
        return Err("the highlight is off the list");
    }
    Ok(match at.cmp(&to) {
        std::cmp::Ordering::Less => Some(Step::Key(down(app), Expect::HighlightAt(at + 1))),
        std::cmp::Ordering::Greater => Some(Step::Key(up(app), Expect::HighlightAt(at - 1))),
        std::cmp::Ordering::Equal => None,
    })
}

fn down(app: bool) -> Vec<u8> {
    if app { b"\x1bOB" } else { b"\x1b[B" }.to_vec()
}

fn up(app: bool) -> Vec<u8> {
    if app { b"\x1bOA" } else { b"\x1b[A" }.to_vec()
}

fn left(app: bool) -> Vec<u8> {
    if app { b"\x1bOD" } else { b"\x1b[D" }.to_vec()
}

fn norm(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(['’', '‘'], "'")
}

/// A screen label cut with `…` matches by prefix.
fn label_matches(screen: &str, want: &str) -> bool {
    let (screen, want) = (norm(screen), norm(want));
    screen == want
        || screen
            .strip_suffix('…')
            .map(str::trim_end)
            .is_some_and(|cut| !cut.is_empty() && want.starts_with(cut))
}

fn find(d: &Dialog, label: &str, skip: impl Fn(usize) -> bool) -> Result<usize, &'static str> {
    let mut hits = d
        .options
        .iter()
        .enumerate()
        .filter(|(i, o)| !skip(*i) && label_matches(&o.label, label))
        .map(|(i, _)| i);
    match (hits.next(), hits.next()) {
        (Some(at), None) => Ok(at),
        (None, _) => Err("no row has that label"),
        (Some(_), Some(_)) => Err("several rows have that label"),
    }
}

fn title_matches(title: Option<&str>, want: &str) -> bool {
    let Some(title) = title else { return false };
    let (title, want) = (norm(title), norm(want));
    let shorter = title.chars().count().min(want.chars().count());
    title == want
        || (shorter >= TITLE_PREFIX && (title.starts_with(&want) || want.starts_with(&title)))
}

fn answer_index(d: &Dialog, answers: &[AnswerPick]) -> Result<usize, &'static str> {
    let mut hits = answers
        .iter()
        .enumerate()
        .filter(|(_, a)| {
            a.question
                .as_deref()
                .is_some_and(|q| title_matches(d.title.as_deref(), q))
        })
        .map(|(i, _)| i);
    match (hits.next(), hits.next()) {
        (Some(at), None) => Ok(at),
        (None, _) => Err("no answer is for this question"),
        (Some(_), Some(_)) => Err("several answers are for this question"),
    }
}

/// Whether the row holds the start of `text`, however it renders once typed into.
fn shows(option: &DialogOption, text: &str) -> bool {
    let start: String = norm(text).chars().take(SHOWN_CHARS).collect();
    // An input row's label is a name, not what was typed.
    let row = if option.input {
        norm(option.text.as_deref().unwrap_or_default())
    } else {
        norm(&option.label)
    };
    row.contains(&start)
}

/// What the driver found after the last Enter, for the one log line that records it.
pub(in crate::shell) fn outcome(before: &Dialog, now: Option<&Dialog>) -> &'static str {
    match now {
        None => "gone",
        Some(n) if n.key != before.key => "another dialog",
        Some(_) => "same dialog",
    }
}

/// A running answer: the planner plus the check of each key's effect.
pub(in crate::shell) struct Run {
    goal: Goal,
    progress: Progress,
    pending: Option<(Expect, Dialog)>,
}

pub(in crate::shell) enum Next {
    Send(Vec<u8>),
    Done,
    Abort(String),
}

impl Run {
    pub(in crate::shell) fn new(goal: Goal) -> Self {
        Self {
            goal,
            progress: Progress::default(),
            pending: None,
        }
    }

    /// Checks the screen against what the last key should have done, then plans the next key.
    pub(in crate::shell) fn advance(&mut self, screen: Option<Dialog>, app_cursor: bool) -> Next {
        if let Some((expect, before)) = self.pending.take()
            && !expect.met(&before, screen.as_ref())
        {
            return self.abort("the screen did not match the key just sent");
        }
        let Some(d) = screen else {
            return if self.progress.submitted {
                Next::Done
            } else {
                self.abort("no dialog is on screen")
            };
        };
        match step(&d, &self.goal, &mut self.progress, app_cursor) {
            Step::Key(bytes, expect) => {
                self.pending = Some((expect, d));
                Next::Send(bytes)
            }
            Step::Done => Next::Done,
            Step::Abort(why) => self.abort(why),
        }
    }

    fn abort(&self, why: &str) -> Next {
        Next::Abort(self.refusal(why))
    }

    pub(in crate::shell) fn refusal(&self, why: &str) -> String {
        format!(
            "Couldn't confirm the screen, open the terminal (step {}: {why})",
            self.progress.keys
        )
    }

    /// Whether the last key was the one that submits, so the dialog closing is the expected change.
    pub(in crate::shell) fn settling(&self) -> bool {
        matches!(self.pending, Some((Expect::GoneOrKeyChanged, _)))
    }

    /// Whether the screen has moved on enough to be checked against the last key.
    pub(in crate::shell) fn ready(&self, now: Option<&Dialog>, last: Option<&Dialog>) -> bool {
        match &self.pending {
            Some((expect @ Expect::GoneOrKeyChanged, before)) => expect.met(before, now),
            _ => now != last,
        }
    }

    pub(in crate::shell) fn after(&self, now: Option<&Dialog>) -> Option<&'static str> {
        self.pending
            .as_ref()
            .filter(|(expect, _)| matches!(expect, Expect::GoneOrKeyChanged))
            .map(|(_, before)| outcome(before, now))
    }
}

/// The dialog on a pane's screen and whether its arrows are in application mode, read as the snapshot reads it.
/// `None` means the pane is not attached.
pub(in crate::shell) fn read(view: &TerminalView) -> Option<(Option<Dialog>, bool)> {
    if !view.is_attached() {
        return None;
    }
    let dialog = (view.claude_state() == Some(ClaudeState::NeedsInput))
        .then(|| dialog::parse(&view.text_lines(view.grid_size().0 as usize)))
        .flatten();
    Some((dialog, view.app_cursor()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::protocol::DialogTab;

    const BASH: &str = include_str!("dialogs/synthetic-bash.txt");
    const SINGLE: &str = include_str!("dialogs/synthetic-single-select.txt");
    const MULTI: &str = include_str!("dialogs/synthetic-multi-select.txt");
    const TABS: &str = include_str!("dialogs/synthetic-tabs.txt");
    const SINGLE_CHAT: &str = include_str!("dialogs/synthetic-single-select-chat.txt");
    const REVIEW: &str = include_str!("dialogs/synthetic-review.txt");
    const REAL_TABS_FIRST: &str = include_str!("dialogs/real-tabs-first.txt");
    const REAL_TABS: &str = include_str!("dialogs/real-tabs.txt");
    const REAL_MULTI: &str = include_str!("dialogs/real-multi-select.txt");
    const REAL_REVIEW: &str = include_str!("dialogs/real-review.txt");
    const REAL_REVIEW_UNANSWERED: &str = include_str!("dialogs/real-review-unanswered.txt");
    const REAL_REVIEW_MULTI: &str = include_str!("dialogs/real-review-multi-select.txt");

    const DOWN: &[u8] = b"\x1b[B";
    const UP: &[u8] = b"\x1b[A";
    const LEFT: &[u8] = b"\x1b[D";
    const ENTER: &[u8] = b"\r";

    fn parse(screen: &str) -> Dialog {
        let lines: Vec<String> = screen.lines().map(str::to_string).collect();
        dialog::parse(&lines).expect("a dialog")
    }

    fn at(mut d: Dialog, highlight: u32) -> Dialog {
        d.highlight = Some(highlight);
        d
    }

    fn checked(mut d: Dialog, flags: &[bool]) -> Dialog {
        for (o, on) in d.options.iter_mut().zip(flags) {
            o.checked = Some(*on);
        }
        d
    }

    fn pick(question: Option<&str>, labels: &[&str], other: Option<&str>) -> AnswerPick {
        AnswerPick {
            question: question.map(str::to_string),
            pick: labels.iter().map(|l| l.to_string()).collect(),
            other: other.map(str::to_string),
        }
    }

    fn goal(d: &Dialog, answers: Vec<AnswerPick>) -> Goal {
        Goal {
            key: d.key.clone(),
            answers,
        }
    }

    /// Feeds the screens in turn, each following the key before it, and returns the keys sent and how it ended.
    fn play(goal: Goal, screens: Vec<Option<Dialog>>, app: bool) -> (Vec<Vec<u8>>, Option<Next>) {
        let mut run = Run::new(goal);
        let mut keys = Vec::new();
        let mut end = None;
        for screen in screens {
            match run.advance(screen, app) {
                Next::Send(bytes) => keys.push(bytes),
                done => {
                    end = Some(done);
                    break;
                }
            }
        }
        for key in &keys {
            assert_ne!(key, b"\x1b", "a lone Esc would cancel the dialog");
        }
        (keys, end)
    }

    fn assert_done(end: Option<Next>) {
        assert!(matches!(end, Some(Next::Done)));
    }

    fn assert_aborted(end: Option<Next>, because: &str) {
        match end {
            Some(Next::Abort(why)) => assert!(why.contains(because), "{why}"),
            _ => panic!("expected an abort"),
        }
    }

    /// The row after text went into it: a plain option, so the dialog's key changes.
    fn typed(mut d: Dialog, row: usize, text: &str) -> Dialog {
        d.options[row].label = text.into();
        d.options[row].input = false;
        d.options[row].text = None;
        d.key = "typed".into();
        d
    }

    /// The single-select fixture after `text` went into its Other row, as the parser reads it.
    fn typed_single(text: &str) -> Dialog {
        let d = parse(&SINGLE.replace("3. Other", &format!("3. {text}")));
        assert_eq!(d.kind, DialogKind::Select);
        d
    }

    #[test]
    fn the_next_bash_prompt_with_the_same_labels_is_another_dialog() {
        let d = parse(BASH);
        let next = parse(&BASH.replace("   npm test\n", "   rm -rf build\n"));
        assert_eq!(
            d.options.iter().map(|o| &o.label).collect::<Vec<_>>(),
            next.options.iter().map(|o| &o.label).collect::<Vec<_>>()
        );
        let g = goal(&d, vec![pick(None, &["Yes"], None)]);
        let (keys, end) = play(g, vec![Some(next)], false);
        assert!(keys.is_empty());
        assert_aborted(end, "The question changed");
    }

    #[test]
    fn a_tabbed_form_with_other_questions_aborts_on_the_title_check() {
        let d = parse(TABS);
        let mut other_form = d.clone();
        other_form.title = Some("Which shoe?".into());
        let g = tabbed_goal(&d);
        let (keys, end) = play(g, vec![Some(other_form)], false);
        assert!(keys.is_empty());
        assert_aborted(end, "no answer is for this question");
    }

    #[test]
    fn a_review_of_other_answers_has_another_key() {
        let d = clean_review();
        let other = parse(&REVIEW.replace("→ Blue", "→ Red"));
        assert_ne!(d.key, other.key);
        let g = goal(&d, Vec::new());
        let (keys, end) = play(g, vec![Some(other)], false);
        assert!(keys.is_empty());
        assert_aborted(end, "The question changed");
    }

    #[test]
    fn a_short_text_is_not_found_in_the_name_of_an_empty_input_row() {
        let d = parse(SINGLE);
        let g = goal(&d, vec![pick(Some("Which colour?"), &[], Some("t"))]);
        let (keys, end) = play(g, vec![Some(at(d.clone(), 2)), Some(at(d, 2))], false);
        assert_eq!(keys, [b"t".to_vec()]);
        assert_aborted(end, "did not match");
    }

    #[test]
    fn typing_is_checked_against_the_same_title() {
        let d = parse(SINGLE);
        let g = goal(&d, vec![pick(Some("Which colour?"), &[], Some("hello"))]);
        let mut elsewhere = at(typed_single("hello"), 2);
        elsewhere.title = Some("Which shoe?".into());
        let (_, end) = play(g, vec![Some(at(d, 2)), Some(elsewhere)], false);
        assert_aborted(end, "did not match");
    }

    #[test]
    fn a_select_walks_up_to_the_target_then_enters() {
        let d = parse(BASH);
        let g = goal(&d, vec![pick(None, &["Yes"], None)]);
        let screens = vec![
            Some(at(d.clone(), 2)),
            Some(at(d.clone(), 1)),
            Some(at(d.clone(), 0)),
            None,
        ];
        let (keys, end) = play(g, screens, false);
        assert_eq!(keys, [UP, UP, ENTER]);
        assert_done(end);
    }

    #[test]
    fn a_select_walks_down_and_a_highlighted_target_goes_straight_to_enter() {
        let d = parse(BASH);
        let g = goal(&d, vec![pick(None, &["No"], None)]);
        let screens = vec![Some(at(d.clone(), 2)), Some(at(d.clone(), 3)), None];
        let (keys, end) = play(g, screens, false);
        assert_eq!(keys, [DOWN, ENTER]);
        assert_done(end);
        let g = goal(&d, vec![pick(None, &["Yes"], None)]);
        let (keys, _) = play(g, vec![Some(d)], false);
        assert_eq!(keys, [ENTER]);
    }

    #[test]
    fn curly_and_straight_apostrophes_both_match_the_ask_again_row() {
        let d = parse(BASH);
        for label in [
            "Yes, and don’t ask again for",
            "Yes, and don't ask again for",
        ] {
            let g = goal(&d, vec![pick(None, &[label], None)]);
            let (keys, _) = play(g, vec![Some(at(d.clone(), 1))], false);
            assert_eq!(keys, [ENTER], "{label}");
        }
    }

    #[test]
    fn a_screen_label_cut_with_an_ellipsis_matches_by_prefix_and_must_be_unique() {
        let mut d = parse(BASH);
        d.options[2].label = "Yes, and switch to…".into();
        let g = goal(
            &d,
            vec![pick(None, &["Yes, and switch to auto mode"], None)],
        );
        let (keys, _) = play(g, vec![Some(at(d.clone(), 2))], false);
        assert_eq!(keys, [ENTER]);
        d.options[3].label = "Yes, and…".into();
        d.options[2].label = "Yes, and…".into();
        let g = goal(
            &d,
            vec![pick(None, &["Yes, and switch to auto mode"], None)],
        );
        let (_, end) = play(g, vec![Some(d)], false);
        assert_aborted(end, "several rows");
    }

    #[test]
    fn app_cursor_changes_the_arrow_sequences() {
        let d = parse(BASH);
        let g = goal(&d, vec![pick(None, &["No"], None)]);
        let (keys, _) = play(g, vec![Some(d.clone())], true);
        assert_eq!(keys, [b"\x1bOB".to_vec()]);
        let g = goal(&d, vec![pick(None, &["Yes"], None)]);
        let (keys, _) = play(g, vec![Some(at(d, 1))], true);
        assert_eq!(keys, [b"\x1bOA".to_vec()]);
    }

    #[test]
    fn a_missing_label_aborts() {
        let d = parse(BASH);
        let g = goal(&d, vec![pick(None, &["Maybe"], None)]);
        let (keys, end) = play(g, vec![Some(d)], false);
        assert!(keys.is_empty());
        assert_aborted(end, "no row has that label");
    }

    #[test]
    fn a_changed_key_aborts_and_so_does_unknown_or_partial() {
        let d = parse(BASH);
        let g = goal(&d, vec![pick(None, &["Yes"], None)]);
        let (_, end) = play(g, vec![Some(parse(SINGLE))], false);
        assert_aborted(end, "The question changed");
        let mut unknown = d.clone();
        unknown.kind = DialogKind::Unknown;
        let g = goal(&d, vec![pick(None, &["Yes"], None)]);
        let (keys, end) = play(g, vec![Some(unknown)], false);
        assert!(keys.is_empty());
        assert_aborted(end, "not fully readable");
        let mut partial = d.clone();
        partial.partial = true;
        let g = goal(&d, vec![pick(None, &["Yes"], None)]);
        let (_, end) = play(g, vec![Some(partial)], false);
        assert_aborted(end, "not fully readable");
    }

    #[test]
    fn a_highlight_that_does_not_move_aborts() {
        let d = parse(BASH);
        let g = goal(&d, vec![pick(None, &["No"], None)]);
        let (keys, end) = play(g, vec![Some(d.clone()), Some(d)], false);
        assert_eq!(keys, [DOWN]);
        assert_aborted(end, "did not match");
    }

    #[test]
    fn an_enter_that_leaves_the_dialog_unchanged_aborts() {
        let d = parse(BASH);
        let g = goal(&d, vec![pick(None, &["Yes"], None)]);
        let (keys, end) = play(g, vec![Some(d.clone()), Some(d)], false);
        assert_eq!(keys, [ENTER]);
        assert_aborted(end, "did not match");
    }

    #[test]
    fn a_vanished_dialog_aborts_before_the_last_enter() {
        let d = parse(BASH);
        let g = goal(&d, vec![pick(None, &["No"], None)]);
        let (_, end) = play(g, vec![Some(d), None], false);
        assert_aborted(end, "did not match");
    }

    #[test]
    fn a_single_select_types_into_an_empty_other_row_once_then_enters() {
        let d = parse(SINGLE);
        let g = goal(
            &d,
            vec![pick(Some("Which colour?"), &[], Some("héllo wörld"))],
        );
        let screens = vec![
            Some(d.clone()),
            Some(at(d.clone(), 1)),
            Some(at(d.clone(), 2)),
            Some(at(typed_single("héllo wörld"), 2)),
            None,
        ];
        let (keys, end) = play(g, screens, false);
        assert_eq!(keys, [DOWN, DOWN, "héllo wörld".as_bytes(), ENTER]);
        assert_done(end);
    }

    #[test]
    fn the_highlight_is_put_back_on_the_typed_row_before_enter() {
        let d = parse(SINGLE);
        let g = goal(&d, vec![pick(Some("Which colour?"), &[], Some("hello"))]);
        let after = typed_single("hello");
        let screens = vec![
            Some(at(d, 2)),
            Some(at(after.clone(), 1)),
            Some(at(after, 2)),
            None,
        ];
        let (keys, end) = play(g, screens, false);
        assert_eq!(keys, [&b"hello"[..], DOWN, ENTER]);
        assert_done(end);
    }

    #[test]
    fn an_up_that_lands_on_another_tab_with_the_same_highlight_aborts() {
        let second = at(parse(TABS), 1);
        let g = tabbed_goal(&second);
        let mut other_tab = at(colour_question(), 0);
        other_tab.options.truncate(3);
        let first = at(colour_question(), 1);
        let screens = vec![Some(second), Some(first), Some(other_tab)];
        let (_, end) = play(g, screens, false);
        assert_aborted(end, "did not match");
    }

    #[test]
    fn a_space_that_lands_on_another_tab_with_the_same_boxes_aborts() {
        let d = parse(MULTI);
        let mut tabbed = d.clone();
        tabbed.tabs = vec![DialogTab {
            label: "A".into(),
            answered: false,
        }];
        let g = goal(&tabbed, vec![pick(Some("Which sizes?"), &["A", "B"], None)]);
        let mut next_tab = checked(tabbed.clone(), &[true, true, false]);
        next_tab.title = Some("Which else?".into());
        let screens = vec![Some(tabbed), Some(next_tab)];
        let (keys, end) = play(g, screens, false);
        assert_eq!(keys, [b" ".to_vec()]);
        assert_aborted(end, "did not match");
    }

    #[test]
    fn a_new_question_without_an_enter_before_it_aborts() {
        let second = parse(TABS);
        let first = colour_question();
        let g = tabbed_goal(&second);
        let mut p = Progress::default();
        assert!(matches!(step(&first, &g, &mut p, false), Step::Key(..)));
        assert!(matches!(step(&second, &g, &mut p, false), Step::Abort(_)));
    }

    #[test]
    fn an_other_row_ticked_at_the_desk_aborts_when_the_phone_typed_nothing() {
        let d = parse(MULTI);
        let mut ticked = checked(d, &[false, true, true]);
        ticked.options[2].input = true;
        let g = goal(&ticked, vec![pick(Some("Which sizes?"), &["B"], None)]);
        let (keys, end) = play(g, vec![Some(ticked)], false);
        assert!(keys.is_empty());
        assert_aborted(end, "ticked at the desk");
    }

    #[test]
    fn typed_text_that_the_row_does_not_show_aborts() {
        let d = parse(SINGLE);
        let g = goal(&d, vec![pick(Some("Which colour?"), &[], Some("hello"))]);
        let (keys, end) = play(g, vec![Some(at(d.clone(), 2)), Some(at(d, 2))], false);
        assert_eq!(keys, [b"hello".to_vec()]);
        assert_aborted(end, "did not match");
    }

    #[test]
    fn an_other_row_holding_desk_text_aborts_before_any_key() {
        let d = parse(&MULTI.replace("3. [ ] Other", "3. [ ] desk typing"));
        assert_eq!(d.kind, DialogKind::Question);
        assert!(!d.options.iter().any(|o| o.input));
        for answer in [
            pick(Some("Which sizes?"), &["A"], Some("hello")),
            pick(Some("Which sizes?"), &["A"], None),
            pick(Some("Which sizes?"), &["B"], None),
        ] {
            let g = goal(&d, vec![answer]);
            let (keys, end) = play(g, vec![Some(d.clone())], false);
            assert!(keys.is_empty());
            assert_aborted(end, "holds text from the desk");
        }
    }

    #[test]
    fn leading_spaces_in_the_text_do_not_make_an_empty_match() {
        let d = parse(SINGLE);
        let text = format!("{}hello", " ".repeat(40));
        let g = goal(&d, vec![pick(Some("Which colour?"), &[], Some(&text))]);
        let (keys, end) = play(g, vec![Some(at(d.clone(), 2)), Some(at(d, 2))], false);
        assert_eq!(keys, [text.into_bytes()]);
        assert_aborted(end, "did not match");
    }

    #[test]
    fn an_input_row_already_holding_the_text_goes_straight_to_enter() {
        let mut d = parse(SINGLE);
        d.options[2].text = Some("hello".into());
        let g = goal(&d, vec![pick(Some("Which colour?"), &[], Some("hello"))]);
        let (keys, _) = play(g, vec![Some(at(d, 2))], false);
        assert_eq!(keys, [ENTER]);
    }

    #[test]
    fn text_with_control_characters_is_never_typed() {
        let d = parse(SINGLE);
        for text in ["a\nb", "a\x1b[Bb", "a\tb", "a\x7fb"] {
            let g = goal(&d, vec![pick(Some("Which colour?"), &[], Some(text))]);
            let (keys, end) = play(g, vec![Some(at(d.clone(), 2))], false);
            assert!(keys.is_empty());
            assert_aborted(end, "control characters");
        }
    }

    #[test]
    fn a_single_select_pick_without_other_moves_then_enters() {
        let d = parse(SINGLE);
        let g = goal(&d, vec![pick(Some("Which colour?"), &["Blue"], None)]);
        let (keys, end) = play(g, vec![Some(d.clone()), Some(at(d, 1)), None], false);
        assert_eq!(keys, [DOWN, ENTER]);
        assert_done(end);
    }

    #[test]
    fn a_pick_together_with_other_text_aborts() {
        let d = parse(SINGLE);
        let g = goal(&d, vec![pick(Some("Which colour?"), &["Red"], Some("x"))]);
        let (keys, end) = play(g, vec![Some(d)], false);
        assert!(keys.is_empty());
        assert_aborted(end, "an option and typed text");
    }

    #[test]
    fn a_multi_select_toggles_only_the_differing_rows_then_submits() {
        let d = parse(MULTI);
        let g = goal(&d, vec![pick(Some("Which sizes?"), &["A"], None)]);
        let both = |on: [bool; 3], h: u32| Some(at(checked(d.clone(), &on), h));
        let screens = vec![
            Some(d.clone()),
            both([true, true, false], 0),
            both([true, true, false], 1),
            both([true, false, false], 1),
            both([true, false, false], 2),
            both([true, false, false], 3),
            None,
        ];
        let (keys, end) = play(g, screens, false);
        assert_eq!(keys, [&b" "[..], DOWN, b" ", DOWN, DOWN, ENTER]);
        assert_done(end);
    }

    #[test]
    fn a_multi_select_with_matching_boxes_goes_straight_to_submit() {
        let d = parse(MULTI);
        let g = goal(&d, vec![pick(Some("Which sizes?"), &["B"], None)]);
        let (keys, _) = play(g, vec![Some(at(d.clone(), 3))], false);
        assert_eq!(keys, [ENTER]);
    }

    #[test]
    fn a_multi_select_types_other_last_and_checks_it_by_position() {
        let d = parse(MULTI);
        let g = goal(&d, vec![pick(Some("Which sizes?"), &["B"], Some("XXL"))]);
        let after = at(typed(checked(d.clone(), &[false, true, true]), 2, "XXL"), 2);
        let screens = vec![
            Some(at(d.clone(), 2)),
            Some(after.clone()),
            Some(at(after, 3)),
            None,
        ];
        let (keys, end) = play(g, screens, false);
        assert_eq!(keys, [&b"XXL"[..], DOWN, ENTER]);
        assert_done(end);
    }

    #[test]
    fn a_multi_select_other_that_does_not_check_aborts() {
        let d = parse(MULTI);
        let g = goal(&d, vec![pick(Some("Which sizes?"), &["B"], Some("XXL"))]);
        let unchecked = at(typed(d.clone(), 2, "XXL"), 2);
        let (_, end) = play(g, vec![Some(at(d, 2)), Some(unchecked)], false);
        assert_aborted(end, "not checked");
    }

    fn colour_question() -> Dialog {
        let mut d = parse(TABS);
        d.title = Some("Which colour?".into());
        d.options[0].label = "Red".into();
        d.options[0].description = None;
        d.options[1].label = "Blue".into();
        d
    }

    fn tabbed_goal(first: &Dialog) -> Goal {
        goal(
            first,
            vec![
                pick(Some("Which colour?"), &["Blue"], None),
                pick(Some("Which size?"), &["Small"], None),
            ],
        )
    }

    fn clean_review() -> Dialog {
        let mut d = parse(REVIEW);
        d.lines.retain(|l| !l.contains("not answered"));
        d
    }

    #[test]
    fn a_form_started_on_the_second_question_goes_left_first_then_through_the_review() {
        let second = parse(TABS);
        let first = colour_question();
        let g = tabbed_goal(&second);
        let screens = vec![
            Some(second.clone()),
            Some(first.clone()),
            Some(at(first.clone(), 1)),
            Some(second.clone()),
            Some(clean_review()),
            None,
        ];
        let (keys, end) = play(g, screens, false);
        assert_eq!(keys, [LEFT, DOWN, ENTER, ENTER, ENTER]);
        assert_done(end);
    }

    #[test]
    fn app_cursor_changes_the_left_sequence_too() {
        let second = parse(TABS);
        let g = tabbed_goal(&second);
        let (keys, _) = play(g, vec![Some(second)], true);
        assert_eq!(keys, [b"\x1bOD".to_vec()]);
    }

    #[test]
    fn going_left_from_a_focused_input_row_aborts() {
        let second = at(parse(TABS), 2);
        let g = tabbed_goal(&second);
        let (keys, end) = play(g, vec![Some(second)], false);
        assert!(keys.is_empty());
        assert_aborted(end, "text field");
    }

    #[test]
    fn a_left_that_does_not_change_the_title_aborts() {
        let second = parse(TABS);
        let g = tabbed_goal(&second);
        let (keys, end) = play(g, vec![Some(second.clone()), Some(second)], false);
        assert_eq!(keys, [LEFT]);
        assert_aborted(end, "did not match");
    }

    #[test]
    fn a_title_cut_to_forty_characters_still_matches_but_a_short_stub_does_not() {
        let long = "Which of the following colours should the new banner use today?";
        assert!(title_matches(Some(&long[..45]), long));
        assert!(title_matches(Some(long), &long[..45]));
        assert!(!title_matches(Some("Which of"), "Which of the following"));
    }

    #[test]
    fn a_question_that_no_answer_names_aborts() {
        let d = parse(TABS);
        let answers = vec![
            pick(Some("Which colour?"), &["Blue"], None),
            pick(Some("Another?"), &["x"], None),
        ];
        let (_, end) = play(goal(&d, answers), vec![Some(d)], false);
        assert_aborted(end, "no answer is for this question");
    }

    #[test]
    fn a_form_with_a_different_number_of_answers_than_tabs_aborts() {
        let d = parse(TABS);
        let g = goal(&d, vec![pick(Some("Which size?"), &["Small"], None)]);
        let (_, end) = play(g, vec![Some(d)], false);
        assert_aborted(end, "do not fit");
    }

    #[test]
    fn a_review_that_warns_about_unanswered_questions_aborts() {
        let d = parse(REVIEW);
        let g = goal(&d, Vec::new());
        let (keys, end) = play(g, vec![Some(d)], false);
        assert!(keys.is_empty());
        assert_aborted(end, "not every question");
    }

    #[test]
    fn a_real_two_question_form_submits_through_the_footerless_review() {
        let first = parse(REAL_TABS_FIRST);
        let g = goal(
            &first,
            vec![
                pick(Some("Which colour?"), &["Red"], None),
                pick(Some("Which size?"), &["Small"], None),
            ],
        );
        let screens = vec![
            Some(first),
            Some(parse(REAL_TABS)),
            Some(parse(REAL_REVIEW)),
            None,
        ];
        let (keys, end) = play(g, screens, false);
        assert_eq!(keys, [ENTER, ENTER, ENTER]);
        assert_done(end);
    }

    #[test]
    fn a_real_multi_select_submits_through_the_footerless_review() {
        let m = parse(REAL_MULTI);
        let g = goal(&m, vec![pick(Some("Which sizes?"), &["A"], None)]);
        let on = checked(m.clone(), &[true, false, false]);
        let screens = vec![
            Some(m.clone()),
            Some(on.clone()),
            Some(at(on.clone(), 1)),
            Some(at(on.clone(), 2)),
            Some(at(on, 3)),
            Some(parse(REAL_REVIEW_MULTI)),
            None,
        ];
        let (keys, end) = play(g, screens, false);
        assert_eq!(
            keys,
            [
                b" ".to_vec(),
                DOWN.to_vec(),
                DOWN.to_vec(),
                DOWN.to_vec(),
                ENTER.to_vec(),
                ENTER.to_vec()
            ]
        );
        assert_done(end);
    }

    #[test]
    fn a_real_unanswered_review_aborts() {
        let d = parse(REAL_REVIEW_UNANSWERED);
        let g = goal(&d, Vec::new());
        let (keys, end) = play(g, vec![Some(d)], false);
        assert!(keys.is_empty());
        assert_aborted(end, "not every question");
    }

    #[test]
    fn a_review_without_the_warning_submits_and_a_cancel_highlight_goes_up_first() {
        let d = clean_review();
        let g = goal(&d, Vec::new());
        let (keys, end) = play(g, vec![Some(at(d.clone(), 1)), Some(d), None], false);
        assert_eq!(keys, [UP, ENTER]);
        assert_done(end);
    }

    #[test]
    fn a_review_that_comes_before_the_last_question_aborts() {
        let first = colour_question();
        let g = tabbed_goal(&first);
        let screens = vec![Some(at(first, 1)), Some(clean_review())];
        let (keys, end) = play(g, screens, false);
        assert_eq!(keys, [ENTER]);
        assert_aborted(end, "before the last question");
    }

    #[test]
    fn a_tabless_single_question_ends_when_the_dialog_is_gone() {
        let d = parse(SINGLE);
        let g = goal(&d, vec![pick(Some("Which colour?"), &["Red"], None)]);
        let (keys, end) = play(g, vec![Some(d), None], false);
        assert_eq!(keys, [ENTER]);
        assert_done(end);
    }

    #[test]
    fn a_lone_question_with_the_chat_row_ends_when_the_dialog_is_gone() {
        let d = parse(SINGLE_CHAT);
        let g = goal(&d, vec![pick(Some("Which colour?"), &["Red"], None)]);
        let (keys, end) = play(g, vec![Some(d), None], false);
        assert_eq!(keys, [ENTER]);
        assert_done(end);
    }

    #[test]
    fn a_lone_question_with_the_chat_row_moves_then_enters() {
        let d = parse(SINGLE_CHAT);
        let g = goal(&d, vec![pick(Some("Which colour?"), &["Blue"], None)]);
        let (keys, end) = play(g, vec![Some(d.clone()), Some(at(d, 1)), None], false);
        assert_eq!(keys, [DOWN, ENTER]);
        assert_done(end);
    }

    #[test]
    fn the_key_budget_stops_a_runaway() {
        let d = parse(BASH);
        let g = goal(&d, vec![pick(None, &["No"], None)]);
        let mut p = Progress {
            keys: MAX_KEYS,
            ..Progress::default()
        };
        assert!(matches!(
            step(&d, &g, &mut p, false),
            Step::Abort("too many keys")
        ));
    }

    #[test]
    fn an_empty_answer_for_a_select_aborts() {
        let d = parse(BASH);
        let g = goal(&d, Vec::new());
        let (_, end) = play(g, vec![Some(d)], false);
        assert_aborted(end, "no option was chosen");
    }

    #[test]
    fn abort_reasons_carry_no_dialog_text() {
        let d = parse(BASH);
        let g = goal(&d, vec![pick(None, &["secret-label"], Some("secret-text"))]);
        let (_, end) = play(g, vec![Some(d)], false);
        let Some(Next::Abort(why)) = end else {
            panic!("expected an abort")
        };
        assert!(!why.contains("secret") && !why.contains("Bash"), "{why}");
    }

    #[test]
    fn the_review_key_is_accepted_only_for_the_tabs_the_form_started_with() {
        let second = parse(TABS);
        let first = colour_question();
        let g = tabbed_goal(&second);
        let mut other_review = clean_review();
        other_review.tabs = vec![DialogTab {
            label: "Elsewhere".into(),
            answered: true,
        }];
        let screens = vec![
            Some(second.clone()),
            Some(first.clone()),
            Some(at(first, 1)),
            Some(second),
            Some(other_review),
        ];
        let (_, end) = play(g, screens, false);
        assert_aborted(end, "The question changed");
    }
}
