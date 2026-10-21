//! Sending a session's unsent review comments to its Claude Code terminal as one pasted message.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use athena_proto::PaneId;
use athena_term::{ClaudeState, TerminalView};
use gpui::{Context, Entity};

use super::{ReviewComment, store};
use crate::review_comments::Placement;
use crate::shell::item::ItemView;
use crate::shell::notices::ToastAction;
use crate::shell::{Shell, windows};
use crate::snapshots;

/// Claude Code reads a paste and an Enter that arrive together as one multi-line paste, so the
/// Enter waits for the paste to be taken in.
const SUBMIT_DELAY: Duration = Duration::from_millis(80);

const NOT_RUNNING: &str = "Claude isn't running for that session. Resume it first.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    Send,
    Wait,
    NotRunning,
}

/// Whether Enter may be pressed in Claude's prompt now: not while it asks a question, which Enter
/// would answer, and not over text the user may have typed.
fn gate(state: Option<ClaudeState>, prompt_may_have_text: bool) -> Gate {
    match state {
        None => Gate::NotRunning,
        Some(ClaudeState::NeedsInput) => Gate::Wait,
        Some(_) if prompt_may_have_text => Gate::Wait,
        Some(ClaudeState::Running | ClaudeState::Finished) => Gate::Send,
    }
}

/// The gate for a terminal as it is now; a Claude that has sent no hook yet is not ready.
fn terminal_gate(view: &TerminalView) -> Gate {
    if !view.is_claude() {
        return Gate::NotRunning;
    }
    let Some(reported) = view.claude_hook_state() else {
        return Gate::Wait;
    };
    gate(
        view.claude_state().or(Some(reported)),
        view.prompt_may_have_text(),
    )
}

/// A send waiting for one Claude process to be ready; the message is built when it goes out.
#[derive(Debug, PartialEq, Eq)]
struct Pending {
    session: String,
    pid: i32,
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Send(Pending),
}

/// The send each session is waiting on, one at most.
#[derive(Default)]
pub(super) struct Queue {
    pending: HashMap<String, Pending>,
}

impl Queue {
    /// Queues `pending` in place of the session's older one.
    fn enqueue(&mut self, pending: Pending) {
        self.pending.insert(pending.session.clone(), pending);
    }

    fn get(&self, session: &str) -> Option<&Pending> {
        self.pending.get(session)
    }

    fn cancel(&mut self, session: &str) -> Option<Pending> {
        self.pending.remove(session)
    }

    /// What to do after the session's Claude changed: send once the gate opens.
    fn on_change(&mut self, session: &str, gate: Gate) -> Option<Action> {
        match gate {
            Gate::Send => self.pending.remove(session).map(Action::Send),
            Gate::Wait | Gate::NotRunning => None,
        }
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// Why a message cannot be pasted into a terminal.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    NotClaude,
    Restarted,
    NoBracketedPaste,
}

impl Refusal {
    fn notice(&self) -> &'static str {
        match self {
            Self::NotClaude => "Claude exited; queued comments were not sent.",
            Self::Restarted => "Claude was restarted; the comments were not sent.",
            Self::NoBracketedPaste => {
                "This terminal isn't taking pastes safely. Try again once Claude has redrawn."
            }
        }
    }
}

/// Whether the terminal still runs the Claude the send was for, and can take a paste.
fn refusal(
    is_claude: bool,
    expected_pid: i32,
    pid: Option<i32>,
    bracketed: bool,
) -> Option<Refusal> {
    if !is_claude {
        Some(Refusal::NotClaude)
    } else if pid != Some(expected_pid) {
        Some(Refusal::Restarted)
    } else if !bracketed {
        Some(Refusal::NoBracketedPaste)
    } else {
        None
    }
}

fn refusal_of(view: &TerminalView, expected_pid: i32) -> Option<Refusal> {
    refusal(
        view.is_claude(),
        expected_pid,
        view.foreground_pid(),
        view.bracketed_paste(),
    )
}

struct Batch {
    text: String,
    ids: Vec<u64>,
}

/// Each comment with where it is in its file now; one whose file cannot be read is stale at its
/// old lines.
fn place_all(
    comments: Vec<ReviewComment>,
    read: impl Fn(&Path) -> Option<String>,
) -> Vec<(ReviewComment, Placement)> {
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::new();
    comments
        .into_iter()
        .map(|c| {
            let text = texts
                .entry(c.path.clone())
                .or_insert_with(|| read(&c.path))
                .as_deref();
            let placement = match text {
                Some(text) => store::place(&store::split_lines(text), &c),
                None => Placement {
                    lines: c.lines.clone(),
                    stale: true,
                },
            };
            (c, placement)
        })
        .collect()
}

/// The message pasted into Claude: each comment with its place, the line it is about and its body.
fn format_batch(comments: &[(ReviewComment, Placement)], root: &Path) -> String {
    let mut sorted: Vec<&(ReviewComment, Placement)> = comments.iter().collect();
    sorted.sort_by_key(|(c, p)| (c.path.clone(), *p.lines.start(), c.id));
    let mut out = String::from("Review comments on your changes:\n");
    for (n, (c, placed)) in sorted.into_iter().enumerate() {
        let shown = c.path.strip_prefix(root).unwrap_or(&c.path);
        let (first, last) = (placed.lines.start(), placed.lines.end());
        let lines = if first == last {
            first.to_string()
        } else {
            format!("{first}-{last}")
        };
        let note = if placed.stale {
            " (code changed since)"
        } else {
            ""
        };
        out.push_str(&format!("\n{}. {}:{lines}{note}\n", n + 1, shown.display()));
        let quoted = c.anchor.lines().next().unwrap_or_default().trim_end();
        if !quoted.is_empty() {
            out.push_str(&format!("   > {quoted}\n"));
        }
        for line in c.body.trim_end().lines() {
            match line.trim_end() {
                "" => out.push('\n'),
                line => out.push_str(&format!("   {line}\n")),
            }
        }
    }
    out.push_str("\nPlease address each one.");
    out
}

/// The message for what the store holds unsent for `session` right now; `None` when nothing is.
fn batch_from_store(
    dir: &Path,
    session: &str,
    root: &Path,
    read: impl Fn(&Path) -> Option<String>,
) -> Option<Batch> {
    let unsent = store::unsent(dir, session).ok()?;
    if unsent.is_empty() {
        return None;
    }
    let placed = place_all(unsent, read);
    Some(Batch {
        text: format_batch(&placed, root),
        ids: placed.iter().map(|(c, _)| c.id).collect(),
    })
}

impl Shell {
    /// The project and terminal that run `session` in this window.
    fn session_terminal(&self, session: &str) -> Option<(PathBuf, Entity<TerminalView>)> {
        let pane = self.review.sessions.pane_of(session)?;
        let key = self.find_session(pane)?;
        match self.items.get(&key)? {
            ItemView::Terminal(view) => Some((key.0, view.clone())),
            _ => None,
        }
    }

    fn current_session_here(&self, cx: &gpui::App) -> Option<String> {
        let dir = snapshots::store().ok()?;
        self.current_comment_session(&dir, cx)
    }

    /// Sends the unsent comments of the session the focused tab belongs to.
    pub(in crate::shell) fn send_current_review_comments(&mut self, cx: &mut Context<Self>) {
        match self.current_session_here(cx) {
            Some(session) => self.send_review_comments(session, cx),
            None => self.transient_notice(
                "Comments not sent",
                "No Claude session in this window has review comments.",
                cx,
            ),
        }
    }

    /// Drops the send waiting for Claude to be ready, if any; the comments stay unsent.
    pub(in crate::shell) fn cancel_queued_review_comments(&mut self, cx: &mut Context<Self>) {
        let queued = self
            .current_session_here(cx)
            .and_then(|s| self.review.comments.queue.cancel(&s));
        let (title, body) = match queued {
            Some(_) => ("Canceled", "The comments stay unsent."),
            None => ("Nothing to cancel", "No comments are waiting."),
        };
        self.transient_notice(title, body, cx);
    }

    /// Sends `session`'s unsent comments now if its Claude is ready, else once it is.
    pub(in crate::shell) fn send_review_comments(
        &mut self,
        session: String,
        cx: &mut Context<Self>,
    ) {
        let Ok(dir) = snapshots::store() else {
            return;
        };
        let count = store::unsent(&dir, &session).map_or(0, |u| u.len());
        if count == 0 {
            return self.transient_notice(
                "Comments not sent",
                "There are no unsent review comments.",
                cx,
            );
        }
        let Some((_, view)) = self.session_terminal(&session) else {
            let elsewhere = windows::session_bound_elsewhere(&session, self.window_handle, cx);
            let body = match elsewhere {
                true => "That session is in another window.",
                false => NOT_RUNNING,
            };
            return self.transient_notice("Comments not sent", body, cx);
        };
        let (gate, pid) = {
            let v = view.read(cx);
            (terminal_gate(v), v.foreground_pid())
        };
        let Some(pid) = pid.filter(|_| gate != Gate::NotRunning) else {
            return self.transient_notice("Comments not sent", NOT_RUNNING, cx);
        };
        let pending = Pending {
            session: session.clone(),
            pid,
        };
        match gate {
            Gate::Send => self.deliver(pending, view, true, cx),
            Gate::NotRunning => {}
            Gate::Wait => {
                self.review.comments.queue.enqueue(pending);
                let action = ToastAction {
                    label: "Paste Now (don't send)",
                    run: Rc::new(move |shell, _, cx| shell.paste_queued(&session, cx)),
                };
                self.action_toast(
                    format!("{count} comments will send when Claude is ready"),
                    "Cancel from the palette with Claude: Cancel Queued Review Comments.",
                    action,
                    cx,
                );
            }
        }
    }

    /// A terminal's Claude changed; drops what waits for a Claude that is gone, else sends it once
    /// the Claude is ready.
    pub(in crate::shell) fn retry_queued_comments(
        &mut self,
        view: &Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        if self.review.comments.queue.is_empty() {
            return;
        }
        let Some(session) = view
            .read(cx)
            .session()
            .and_then(|pane| self.review.sessions.session_of(pane))
            .map(str::to_string)
        else {
            return;
        };
        let Some(pid) = self.review.comments.queue.get(&session).map(|p| p.pid) else {
            return;
        };
        let (gone, gate) = {
            let v = view.read(cx);
            (
                refusal_of(v, pid).filter(|r| *r != Refusal::NoBracketedPaste),
                terminal_gate(v),
            )
        };
        if let Some(gone) = gone {
            self.review.comments.queue.cancel(&session);
            return self.transient_notice("Comments not sent", gone.notice(), cx);
        }
        if let Some(Action::Send(pending)) = self.review.comments.queue.on_change(&session, gate) {
            self.deliver(pending, view.clone(), true, cx);
        }
    }

    /// Pastes what the session has unsent without pressing Enter, for the user to send.
    fn paste_queued(&mut self, session: &str, cx: &mut Context<Self>) {
        let Some((_, view)) = self.session_terminal(session) else {
            return self.transient_notice(
                "Comments not sent",
                "That session is no longer in a terminal here. They stay queued.",
                cx,
            );
        };
        if let Some(pending) = self.review.comments.queue.cancel(session) {
            self.deliver(pending, view, false, cx);
        }
    }

    /// Pastes the session's unsent comments, and presses Enter a moment later when `submit` and
    /// Claude is still ready. What is sent is read from the store now.
    fn deliver(
        &mut self,
        pending: Pending,
        view: Entity<TerminalView>,
        submit: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(why) = refusal_of(view.read(cx), pending.pid) {
            return self.transient_notice("Comments not sent", why.notice(), cx);
        }
        let Some(root) = self.comment_root(&pending.session) else {
            return;
        };
        let Ok(dir) = snapshots::store() else {
            return;
        };
        let read = |p: &Path| std::fs::read_to_string(p).ok();
        let Some(batch) = batch_from_store(&dir, &pending.session, &root, read) else {
            return self.transient_notice(
                "Comments not sent",
                "There are no unsent review comments.",
                cx,
            );
        };
        view.update(cx, |v, cx| v.paste_text(&batch.text, cx));
        let pasted_at = view.read(cx).last_input();
        // Marked now rather than after the Enter, so a second click in the delay cannot send it twice.
        self.mark_comments_sent(&pending.session, &batch.ids, cx);
        if !submit {
            return self.transient_notice("Pasted", "Press Enter in Claude to send.", cx);
        }
        let count = batch.ids.len();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SUBMIT_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                let ready = {
                    let v = view.read(cx);
                    let typed = v.last_input() != pasted_at;
                    let state = v.claude_state().or(v.claude_hook_state());
                    refusal_of(v, pending.pid).is_none() && gate(state, typed) == Gate::Send
                };
                if ready {
                    view.update(cx, |v, cx| v.submit(cx));
                    let sent = format!("Sent {count} comments to Claude");
                    this.transient_notice("Sent", sent, cx);
                } else {
                    this.transient_notice("Pasted", "Press Enter in Claude to send.", cx);
                }
            });
        })
        .detach();
    }

    /// Where paths in the message start: the session's folder, else its project's.
    fn comment_root(&self, session: &str) -> Option<PathBuf> {
        let cwd = self.review.sessions.cwd_of(session);
        cwd.map(Path::to_path_buf)
            .or_else(|| Some(self.session_terminal(session)?.0))
    }

    fn mark_comments_sent(&mut self, session: &str, ids: &[u64], cx: &mut Context<Self>) {
        let Ok(dir) = snapshots::store() else {
            return;
        };
        if let Err(err) = store::mark_sent(&dir, session, ids) {
            return self.transient_notice(
                "Could not mark the comments sent",
                format!("{err:#}"),
                cx,
            );
        }
        let paths: Vec<_> = self
            .review
            .comments
            .watched
            .values()
            .filter(|w| w.session == session)
            .map(|w| w.path.clone())
            .collect();
        for path in paths {
            self.reload_comments(&dir, session, &path, cx);
        }
    }

    /// A UserPromptSubmit hook came from `pane`'s Claude: its prompt box is empty now.
    pub(in crate::shell) fn claude_prompt_submitted(
        &mut self,
        pane: Option<PaneId>,
        cx: &mut Context<Self>,
    ) {
        let key = pane.and_then(|p| self.find_session(p));
        if let Some(ItemView::Terminal(view)) = key.and_then(|k| self.items.get(&k)).cloned() {
            view.update(cx, |v, cx| v.note_prompt_submit(cx));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review_comments::ReviewComment;
    use std::ops::RangeInclusive;

    fn comment(
        id: u64,
        path: &str,
        lines: RangeInclusive<u32>,
        anchor: &str,
        body: &str,
    ) -> ReviewComment {
        ReviewComment {
            id,
            session: "s".into(),
            path: PathBuf::from(path),
            lines,
            anchor: anchor.into(),
            body: body.into(),
            sent: false,
            created: 0,
        }
    }

    fn placed(c: ReviewComment, stale: bool) -> (ReviewComment, Placement) {
        let p = Placement {
            lines: c.lines.clone(),
            stale,
        };
        (c, p)
    }

    #[test]
    fn the_batch_lists_comments_with_relative_paths_ranges_quotes_and_indented_bodies() {
        let batch = [
            placed(
                comment(
                    2,
                    "/p/src/store.go",
                    41..=43,
                    "func (s *Store) Get() {\n\treturn nil\n}",
                    "Use RLock here?\n\nGets are hot.",
                ),
                false,
            ),
            placed(comment(1, "/p/README.md", 7..=7, "intro", "Typo."), false),
            placed(comment(3, "/elsewhere/x.rs", 2..=2, "", "Outside."), false),
        ];
        assert_eq!(
            format_batch(&batch, Path::new("/p")),
            "Review comments on your changes:\n\
             \n\
             1. /elsewhere/x.rs:2\n   Outside.\n\
             \n\
             2. README.md:7\n   > intro\n   Typo.\n\
             \n\
             3. src/store.go:41-43\n   > func (s *Store) Get() {\n   Use RLock here?\n\n   Gets are hot.\n\
             \n\
             Please address each one."
        );
    }

    #[test]
    fn a_stale_comment_keeps_its_old_lines_and_says_the_code_changed() {
        let batch = [placed(comment(1, "/p/a.rs", 4..=5, "x\ny", "why?"), true)];
        let text = format_batch(&batch, Path::new("/p"));
        assert!(text.contains("1. a.rs:4-5 (code changed since)\n   > x\n   why?\n"));
    }

    #[test]
    fn comments_are_placed_where_their_lines_are_now_or_stale_when_the_file_is_gone() {
        let moved = comment(1, "/p/a.rs", 1..=1, "b", "");
        let missing = comment(2, "/p/gone.rs", 3..=3, "b", "");
        let read = |p: &Path| (p == Path::new("/p/a.rs")).then(|| "a\nb\n".to_string());
        let got = place_all(vec![moved, missing], read);
        assert_eq!((got[0].1.lines.clone(), got[0].1.stale), (2..=2, false));
        assert_eq!((got[1].1.lines.clone(), got[1].1.stale), (3..=3, true));
    }

    #[test]
    fn the_message_is_built_from_the_store_as_it_is_when_it_goes_out() {
        let dir = std::env::temp_dir().join(format!("athena-delivery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = Path::new("/p/a.rs");
        let add = |line: u32, body: &str| {
            store::add(&dir, "s", path, line..=line, "x".into(), body.into()).unwrap()
        };
        let (first, second) = (add(1, "first"), add(2, "second"));
        let read = |_: &Path| Some("x\nx\nx\n".to_string());
        let build = || batch_from_store(&dir, "s", Path::new("/p"), read);
        let queued = build().unwrap();
        assert_eq!(queued.ids, vec![first.id, second.id]);

        store::update_body(&dir, "s", first.id, "first, reworded".into()).unwrap();
        store::delete(&dir, "s", second.id).unwrap();
        let third = add(3, "third");
        let sent = build().unwrap();
        assert_eq!(sent.ids, vec![first.id, third.id]);
        assert!(sent.text.contains("first, reworded") && sent.text.contains("third"));
        assert!(!sent.text.contains("second"));

        store::mark_sent(&dir, "s", &sent.ids).unwrap();
        assert!(build().is_none(), "nothing unsent is nothing to send");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_gate_opens_for_a_running_or_finished_claude_with_an_empty_prompt() {
        use ClaudeState::*;
        let cases = [
            (None, false, Gate::NotRunning),
            (None, true, Gate::NotRunning),
            (Some(Running), false, Gate::Send),
            (Some(Running), true, Gate::Wait),
            (Some(Finished), false, Gate::Send),
            (Some(Finished), true, Gate::Wait),
            (Some(NeedsInput), false, Gate::Wait),
            (Some(NeedsInput), true, Gate::Wait),
        ];
        for (state, typed, want) in cases {
            assert_eq!(gate(state, typed), want, "{state:?}, typed {typed}");
        }
    }

    fn pending(session: &str, pid: i32) -> Pending {
        Pending {
            session: session.into(),
            pid,
        }
    }

    #[test]
    fn a_queued_send_waits_then_goes_out_once() {
        let mut q = Queue::default();
        q.enqueue(pending("s", 7));
        assert!(q.on_change("s", Gate::Wait).is_none());
        assert!(q.on_change("s", Gate::NotRunning).is_none());
        assert!(!q.is_empty(), "still queued");
        assert!(q.on_change("other", Gate::Send).is_none());
        assert_eq!(
            q.on_change("s", Gate::Send),
            Some(Action::Send(pending("s", 7)))
        );
        assert!(q.on_change("s", Gate::Send).is_none(), "sent only once");
        assert!(q.is_empty());
    }

    #[test]
    fn a_newer_request_replaces_the_queued_one() {
        let mut q = Queue::default();
        q.enqueue(pending("s", 7));
        q.enqueue(pending("s", 8));
        assert_eq!(
            q.on_change("s", Gate::Send),
            Some(Action::Send(pending("s", 8)))
        );
    }

    #[test]
    fn a_canceled_send_never_goes_out() {
        let mut q = Queue::default();
        q.enqueue(pending("s", 7));
        assert!(q.cancel("s").is_some());
        assert!(q.cancel("s").is_none());
        assert!(q.on_change("s", Gate::Send).is_none());
    }

    #[test]
    fn only_the_same_claude_in_bracketed_paste_mode_gets_the_message() {
        assert_eq!(refusal(true, 7, Some(7), true), None);
        assert_eq!(refusal(false, 7, Some(7), true), Some(Refusal::NotClaude));
        assert_eq!(refusal(true, 7, Some(8), true), Some(Refusal::Restarted));
        assert_eq!(refusal(true, 7, None, true), Some(Refusal::Restarted));
        assert_eq!(
            refusal(true, 7, Some(7), false),
            Some(Refusal::NoBracketedPaste)
        );
    }
}
