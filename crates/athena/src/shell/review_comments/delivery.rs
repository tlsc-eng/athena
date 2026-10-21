//! Sending a session's unsent review comments to its Claude Code terminal as one pasted message.

use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    Send,
    Wait,
    NotRunning,
}

/// Whether Enter may be pressed in Claude's prompt now: not while it asks a question, which Enter
/// would answer, and not over text the user is typing.
fn gate(state: Option<ClaudeState>, typed_since_hook: bool) -> Gate {
    match state {
        None => Gate::NotRunning,
        Some(ClaudeState::NeedsInput) => Gate::Wait,
        Some(_) if typed_since_hook => Gate::Wait,
        Some(ClaudeState::Running | ClaudeState::Finished) => Gate::Send,
    }
}

struct Batch {
    session: String,
    text: String,
    ids: Vec<u64>,
}

enum Action {
    Send(Batch),
}

/// The batch each session is waiting to send, one at most.
#[derive(Default)]
pub(super) struct Queue {
    batches: HashMap<String, Batch>,
}

impl Queue {
    /// Queues `batch` in place of the session's older one.
    fn enqueue(&mut self, batch: Batch) {
        self.batches.insert(batch.session.clone(), batch);
    }

    fn cancel(&mut self, session: &str) -> Option<Batch> {
        self.batches.remove(session)
    }

    /// What to do after the session's Claude changed: send its queued batch once the gate opens.
    fn on_change(&mut self, session: &str, gate: Gate) -> Option<Action> {
        match gate {
            Gate::Send => self.batches.remove(session).map(Action::Send),
            Gate::Wait | Gate::NotRunning => None,
        }
    }

    fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }
}

/// Each comment with where it is in its file now; one whose file cannot be read is stale at its
/// old lines.
fn place_all(
    comments: Vec<ReviewComment>,
    read: impl Fn(&Path) -> Option<String>,
) -> Vec<(ReviewComment, Placement)> {
    let mut texts: HashMap<std::path::PathBuf, Option<String>> = HashMap::new();
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

impl Shell {
    /// Sends the unsent comments of the session the focused tab belongs to.
    pub(in crate::shell) fn send_current_review_comments(&mut self, cx: &mut Context<Self>) {
        let session = snapshots::store()
            .ok()
            .and_then(|dir| self.current_comment_session(&dir));
        match session {
            Some(session) => self.send_review_comments(session, cx),
            None => self.transient_notice(
                "Comments not sent",
                "No Claude session in this window has review comments.",
                cx,
            ),
        }
    }

    /// Drops the batch waiting for Claude to be ready, if any.
    pub(in crate::shell) fn cancel_queued_review_comments(&mut self, cx: &mut Context<Self>) {
        let session = snapshots::store()
            .ok()
            .and_then(|dir| self.current_comment_session(&dir));
        let batch = session.and_then(|s| self.review.comments.queue.cancel(&s));
        let (title, body) = match batch {
            Some(b) => ("Canceled", format!("{} comments stay unsent.", b.ids.len())),
            None => ("Nothing to cancel", "No comments are waiting.".to_string()),
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
        let unsent = store::unsent(&dir, &session).unwrap_or_default();
        if unsent.is_empty() {
            return self.transient_notice(
                "Comments not sent",
                "There are no unsent review comments.",
                cx,
            );
        }
        let Some(pane) = self.review.sessions.pane_of(&session) else {
            let elsewhere = windows::session_bound_elsewhere(&session, self.window_handle, cx);
            let body = match elsewhere {
                true => "That session is in another window.",
                false => "Claude isn't running for that session. Resume it first.",
            };
            return self.transient_notice("Comments not sent", body, cx);
        };
        let Some((project, item)) = self.find_session(pane) else {
            return self.transient_notice(
                "Comments not sent",
                "Claude isn't running for that session. Resume it first.",
                cx,
            );
        };
        let Some(ItemView::Terminal(view)) = self.items.get(&(project.clone(), item)).cloned()
        else {
            return;
        };
        let root = self
            .review
            .sessions
            .cwd_of(&session)
            .map_or(project, Path::to_path_buf);
        let placed = place_all(unsent, |p| std::fs::read_to_string(p).ok());
        let batch = Batch {
            text: format_batch(&placed, &root),
            ids: placed.iter().map(|(c, _)| c.id).collect(),
            session,
        };
        match terminal_gate(&view, cx) {
            Gate::Send => self.deliver(batch, view, cx),
            Gate::NotRunning => self.transient_notice(
                "Comments not sent",
                "Claude isn't running for that session. Resume it first.",
                cx,
            ),
            Gate::Wait => {
                let (count, session) = (batch.ids.len(), batch.session.clone());
                self.review.comments.queue.enqueue(batch);
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

    /// A terminal's Claude changed or reported a hook; sends what waits for it once it is ready.
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
        let gate = terminal_gate(view, cx);
        if let Some(Action::Send(batch)) = self.review.comments.queue.on_change(&session, gate) {
            self.deliver(batch, view.clone(), cx);
        }
    }

    /// Pastes the waiting batch without pressing Enter, for the user to send themselves.
    fn paste_queued(&mut self, session: &str, cx: &mut Context<Self>) {
        let Some(batch) = self.review.comments.queue.cancel(session) else {
            return;
        };
        let view = self
            .review
            .sessions
            .pane_of(session)
            .and_then(|pane| self.find_session(pane))
            .and_then(|key| self.items.get(&key).cloned());
        let Some(ItemView::Terminal(view)) = view else {
            return;
        };
        view.update(cx, |v, cx| v.paste_text(&batch.text, cx));
        self.mark_comments_sent(&batch, cx);
        self.transient_notice("Pasted", "Press Enter in Claude to send.", cx);
    }

    /// Pastes the batch, and presses Enter a moment later unless Claude stopped being ready.
    fn deliver(&mut self, batch: Batch, view: Entity<TerminalView>, cx: &mut Context<Self>) {
        view.update(cx, |v, cx| v.paste_text(&batch.text, cx));
        let pasted_at = view.read(cx).last_input();
        // Marked now rather than after the Enter, so a second click in the delay cannot send it twice.
        self.mark_comments_sent(&batch, cx);
        let count = batch.ids.len();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SUBMIT_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                let typed_since_paste = view.read(cx).last_input() != pasted_at;
                let state = view.read(cx).claude_state();
                if gate(state, typed_since_paste) == Gate::Send {
                    view.update(cx, |v, cx| v.submit(cx));
                    this.transient_notice("Sent", format!("Sent {count} comments to Claude"), cx);
                } else {
                    this.transient_notice("Pasted", "Press Enter in Claude to send.", cx);
                }
            });
        })
        .detach();
    }

    fn mark_comments_sent(&mut self, batch: &Batch, cx: &mut Context<Self>) {
        let Ok(dir) = snapshots::store() else {
            return;
        };
        if let Err(err) = store::mark_sent(&dir, &batch.session, &batch.ids) {
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
            .filter(|w| w.session == batch.session)
            .map(|w| w.path.clone())
            .collect();
        for path in paths {
            self.reload_comments(&dir, &batch.session, &path, cx);
        }
    }
}

fn terminal_gate(view: &Entity<TerminalView>, cx: &gpui::App) -> Gate {
    let view = view.read(cx);
    gate(view.claude_state(), view.typed_since_hook())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review_comments::ReviewComment;
    use std::ops::RangeInclusive;
    use std::path::PathBuf;

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

    fn batch(session: &str, text: &str) -> Batch {
        Batch {
            session: session.into(),
            text: text.into(),
            ids: vec![1],
        }
    }

    #[test]
    fn a_queued_batch_waits_then_sends_once() {
        let mut q = Queue::default();
        q.enqueue(batch("s", "one"));
        assert!(q.on_change("s", Gate::Wait).is_none());
        assert!(q.on_change("s", Gate::NotRunning).is_none());
        assert!(!q.is_empty(), "still queued");
        assert!(q.on_change("other", Gate::Send).is_none());
        let Some(Action::Send(sent)) = q.on_change("s", Gate::Send) else {
            panic!("the open gate sends");
        };
        assert_eq!(sent.text, "one");
        assert!(q.on_change("s", Gate::Send).is_none(), "sent only once");
        assert!(q.is_empty());
    }

    #[test]
    fn a_newer_request_replaces_the_queued_batch() {
        let mut q = Queue::default();
        q.enqueue(batch("s", "old"));
        q.enqueue(batch("s", "new"));
        let Some(Action::Send(sent)) = q.on_change("s", Gate::Send) else {
            panic!("the open gate sends");
        };
        assert_eq!(sent.text, "new");
    }

    #[test]
    fn a_canceled_batch_never_sends() {
        let mut q = Queue::default();
        q.enqueue(batch("s", "one"));
        assert!(q.cancel("s").is_some());
        assert!(q.cancel("s").is_none());
        assert!(q.on_change("s", Gate::Send).is_none());
    }
}
