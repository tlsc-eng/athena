//! Review comments in the window: which Claude session a file's comments belong to, the store
//! round trips, and keeping the editors and diffs that show them in step.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use athena_editor::{DiffView, EditorView, ReviewEvent, ReviewMark};
use gpui::{App, Context, Entity, EntityId, InteractiveElement as _, WeakEntity, actions};

use super::Shell;
use super::item::ItemView;
use crate::review_comments::{self as store, ReviewComment};
use crate::snapshots;

actions!(athena, [ClaudeReviewComments]);

/// The views showing comments, by entity.
#[derive(Default)]
pub(super) struct CommentsState {
    watched: HashMap<EntityId, Watched>,
    /// Terminals whose raw Claude state is `Some`, so only a first one asks for the column.
    claude_terminals: HashSet<EntityId>,
}

struct Watched {
    view: Target,
    session: String,
    path: PathBuf,
    /// The file's comments as stored, placed again on every edit.
    comments: Vec<ReviewComment>,
}

enum Target {
    Editor(WeakEntity<EditorView>),
    Diff(WeakEntity<DiffView>),
}

impl Target {
    fn alive(&self) -> bool {
        match self {
            Self::Editor(v) => v.upgrade().is_some(),
            Self::Diff(v) => v.upgrade().is_some(),
        }
    }

    /// The text the comments are placed in: the buffer, or the diff's new side.
    fn text(&self, cx: &App) -> Option<String> {
        match self {
            Self::Editor(v) => v.upgrade()?.read(cx).text(),
            Self::Diff(v) => v.upgrade()?.read(cx).new_text().map(str::to_string),
        }
    }

    fn saved(&self, cx: &mut App) {
        match self {
            Self::Editor(v) => {
                let _ = v.update(cx, |v, cx| v.review_saved(cx));
            }
            Self::Diff(v) => {
                let _ = v.update(cx, |v, cx| v.review_saved(cx));
            }
        }
    }

    fn show(&self, marks: Vec<ReviewMark>, cx: &mut App) {
        match self {
            Self::Editor(v) => {
                let _ = v.update(cx, |v, cx| v.set_review_comments(marks, cx));
            }
            Self::Diff(v) => {
                let _ = v.update(cx, |v, cx| v.set_review_comments(marks, cx));
            }
        }
    }
}

/// The session a file's comments go to: of those that changed it, the one that did anything last.
fn pick_session(bound: &[(String, SystemTime)], changed: impl Fn(&str) -> bool) -> Option<String> {
    bound
        .iter()
        .filter(|(session, _)| changed(session))
        .max_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)))
        .map(|(session, _)| session.clone())
}

fn marks(text: &str, comments: &[ReviewComment]) -> Vec<ReviewMark> {
    let lines = store::split_lines(text);
    comments
        .iter()
        .map(|c| {
            let placed = store::place(&lines, c);
            ReviewMark {
                id: c.id,
                lines: placed.lines,
                body: c.body.clone(),
                stale: placed.stale,
                sent: c.sent,
            }
        })
        .collect()
}

/// Whether a file gets the empty comment column: it is in one of the window's projects and
/// Claude runs in a terminal here or a session is bound to a pane here.
fn column_reserved(in_project: bool, claude_terminal: bool, bound_session: bool) -> bool {
    in_project && (claude_terminal || bound_session)
}

/// Stores what the popover asked for; true when it was text the box can let go of.
fn apply(
    dir: &Path,
    session: &str,
    path: &Path,
    text: Option<&str>,
    event: ReviewEvent,
) -> anyhow::Result<bool> {
    match event {
        ReviewEvent::Add { lines, body } => {
            let anchor = text
                .and_then(|t| store::anchor_of(t, &lines))
                .ok_or_else(|| anyhow::anyhow!("Those lines are no longer in the file."))?;
            store::add(dir, session, path, lines, anchor, body)?;
            Ok(true)
        }
        ReviewEvent::Edit { id, body } => {
            if !store::update_body(dir, session, id, body)? {
                anyhow::bail!("That comment no longer exists.");
            }
            Ok(true)
        }
        ReviewEvent::Delete { id } => {
            store::delete(dir, session, id)?;
            Ok(false)
        }
        ReviewEvent::Moved => Ok(false),
    }
}

/// One row of the "Review Comments" palette.
pub(super) struct CommentRow {
    pub(super) path: PathBuf,
    /// Zero-based, where the comment is in the file now.
    pub(super) line: u32,
    pub(super) label: String,
    pub(super) detail: String,
}

impl Shell {
    /// The sessions bound to a pane here, each with the last time it or its snapshots changed.
    fn bound_sessions(&self, store: &Path) -> Vec<(String, SystemTime)> {
        self.review
            .sessions
            .bound_sessions()
            .into_iter()
            .map(|(session, at)| {
                let snapshotted = std::fs::metadata(store.join(&session))
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH);
                (session, at.max(snapshotted))
            })
            .collect()
    }

    fn review_session_for(&self, path: &Path) -> Option<String> {
        let store = snapshots::store().ok()?;
        pick_session(&self.bound_sessions(&store), |session| {
            snapshots::files(&store, session).iter().any(|f| f == path)
        })
    }

    /// Turns comments on for an editor whose file a session of this window changed, else off.
    pub(super) fn watch_editor_comments(
        &mut self,
        editor: &Entity<EditorView>,
        cx: &mut Context<Self>,
    ) {
        let path = editor.read(cx).path().to_path_buf();
        let session = self.review_session_for(&path);
        editor.update(cx, |e, cx| e.set_review(session.is_some(), cx));
        match session {
            Some(session) => self.watch(
                editor.entity_id(),
                Target::Editor(editor.downgrade()),
                session,
                path,
                cx,
            ),
            None => {
                self.review.comments.watched.remove(&editor.entity_id());
            }
        }
    }

    fn review_reserved(&self, path: &Path, cx: &App) -> bool {
        column_reserved(
            self.workspace
                .projects
                .iter()
                .any(|p| path.starts_with(&p.root)),
            self.items.values().any(|v| v.claude_state(cx).is_some()),
            !self.review.sessions.bound_sessions().is_empty(),
        )
    }

    /// Sets an opened editor's comment column from scratch.
    pub(super) fn reserve_review_column(
        &mut self,
        editor: &Entity<EditorView>,
        cx: &mut Context<Self>,
    ) {
        let reserved = self.review_reserved(editor.read(cx).path(), cx);
        editor.update(cx, |e, cx| e.set_review_reserved(reserved, cx));
    }

    /// Decides again, for every open editor, whether its file is under review and by which
    /// session, after a session's home changed.
    pub(super) fn rewatch_comment_editors(&mut self, cx: &mut Context<Self>) {
        let editors: Vec<Entity<EditorView>> = self
            .items
            .values()
            .filter_map(|v| match v {
                ItemView::Editor(e) => Some(e.clone()),
                _ => None,
            })
            .collect();
        for editor in editors {
            self.watch_editor_comments(&editor, cx);
        }
    }

    /// A terminal's Claude state changed; the first time it is `Some`, editors get the column.
    pub(super) fn note_claude_terminal(
        &mut self,
        terminal: EntityId,
        running: bool,
        cx: &mut Context<Self>,
    ) {
        let terminals = &mut self.review.comments.claude_terminals;
        let started = match running {
            true => terminals.insert(terminal),
            false => {
                terminals.remove(&terminal);
                false
            }
        };
        if started {
            self.sync_review_columns(cx);
        }
    }

    /// Gives open editors the comment column once Claude shows up. It is never taken back, so
    /// the text moves at most once, however the sessions come and go.
    pub(super) fn sync_review_columns(&mut self, cx: &mut Context<Self>) {
        let editors: Vec<Entity<EditorView>> = self
            .items
            .values()
            .filter_map(|v| match v {
                ItemView::Editor(e) => Some(e.clone()),
                _ => None,
            })
            .collect();
        for editor in editors {
            if self.review_reserved(editor.read(cx).path(), cx) {
                editor.update(cx, |e, cx| e.set_review_reserved(true, cx));
            }
        }
    }

    /// Turns comments on for a diff of a session's changes.
    pub(super) fn watch_diff_comments(
        &mut self,
        view: &Entity<DiffView>,
        session: &str,
        cx: &mut Context<Self>,
    ) {
        view.update(cx, |v, cx| v.set_review(true, cx));
        let path = view.read(cx).path().to_path_buf();
        let target = Target::Diff(view.downgrade());
        self.watch(view.entity_id(), target, session.to_string(), path, cx);
    }

    fn watch(
        &mut self,
        id: EntityId,
        view: Target,
        session: String,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let comments = snapshots::store()
            .ok()
            .and_then(|s| store::for_file(&s, &session, &path).ok())
            .unwrap_or_default();
        let watched = &mut self.review.comments.watched;
        watched.retain(|_, w| w.view.alive());
        watched.insert(
            id,
            Watched {
                view,
                session,
                path,
                comments,
            },
        );
        self.refresh_comment_marks(id, cx);
    }

    /// Re-evaluates every open editor of `path` after a session touched it.
    pub(super) fn refresh_comment_editors(&mut self, path: &Path, cx: &mut Context<Self>) {
        let editors: Vec<Entity<EditorView>> = self
            .items
            .values()
            .filter_map(|v| match v {
                ItemView::Editor(e) if e.read(cx).path() == path => Some(e.clone()),
                _ => None,
            })
            .collect();
        for editor in editors {
            self.watch_editor_comments(&editor, cx);
        }
    }

    /// Places the comments again in the view's current text, so the dots follow their lines.
    pub(super) fn refresh_comment_marks(&mut self, id: EntityId, cx: &mut Context<Self>) {
        let Some(w) = self.review.comments.watched.get(&id) else {
            return;
        };
        if let Some(text) = w.view.text(cx) {
            w.view.show(marks(&text, &w.comments), cx);
        }
    }

    /// An editor's text changed; reading it only pays when it has comments to move.
    pub(super) fn comments_edited(&mut self, editor: &Entity<EditorView>, cx: &mut Context<Self>) {
        let id = editor.entity_id();
        if self
            .review
            .comments
            .watched
            .get(&id)
            .is_some_and(|w| !w.comments.is_empty())
        {
            self.refresh_comment_marks(id, cx);
        }
    }

    pub(super) fn review_comment_event(
        &mut self,
        id: EntityId,
        event: ReviewEvent,
        cx: &mut Context<Self>,
    ) {
        if event == ReviewEvent::Moved {
            return self.refresh_comment_marks(id, cx);
        }
        let Some(w) = self.review.comments.watched.get(&id) else {
            return;
        };
        let (session, path) = (w.session.clone(), w.path.clone());
        let Ok(dir) = snapshots::store() else {
            return;
        };
        let text = w.view.text(cx);
        let released = match apply(&dir, &session, &path, text.as_deref(), event) {
            Ok(released) => released,
            // The box keeps what was typed. A comment gone from the store, deleted in another
            // view, must also go from the views so the next save adds a new one.
            Err(err) => {
                self.reload_comments(&dir, &session, &path, cx);
                return self.transient_notice("Could not save the comment", format!("{err:#}"), cx);
            }
        };
        if released && let Some(w) = self.review.comments.watched.get(&id) {
            w.view.saved(cx);
        }
        self.reload_comments(&dir, &session, &path, cx);
    }

    /// Reads a file's comments again and shows them in every view of it; an unreadable store
    /// leaves the views as they are.
    fn reload_comments(&mut self, dir: &Path, session: &str, path: &Path, cx: &mut Context<Self>) {
        let Ok(fresh) = store::for_file(dir, session, path) else {
            return;
        };
        let sharing: Vec<EntityId> = self
            .review
            .comments
            .watched
            .iter()
            .filter(|(_, w)| w.session == session && w.path == path)
            .map(|(id, _)| *id)
            .collect();
        for id in sharing {
            if let Some(w) = self.review.comments.watched.get_mut(&id) {
                w.comments = fresh.clone();
            }
            self.refresh_comment_marks(id, cx);
        }
    }

    /// The unsent comments of the session the focused tab belongs to, else of the window's
    /// most recently active session.
    pub(super) fn comment_rows(&self) -> Vec<CommentRow> {
        let Ok(dir) = snapshots::store() else {
            return Vec::new();
        };
        let focused = match self.focused_item_view() {
            Some(ItemView::Editor(e)) => Some(e.entity_id()),
            Some(ItemView::Diff(d)) => Some(d.entity_id()),
            _ => None,
        };
        let session = focused
            .and_then(|id| self.review.comments.watched.get(&id))
            .map(|w| w.session.clone())
            .or_else(|| pick_session(&self.bound_sessions(&dir), |_| true));
        let Some(session) = session else {
            return Vec::new();
        };
        let root = self.active_root();
        let mut comments = store::unsent(&dir, &session).unwrap_or_default();
        comments.sort_by(|a, b| (&a.path, a.lines.start()).cmp(&(&b.path, b.lines.start())));
        comments
            .into_iter()
            .map(|c| {
                let line = std::fs::read_to_string(&c.path).map_or(*c.lines.start(), |text| {
                    *store::place(&store::split_lines(&text), &c).lines.start()
                });
                let shown = root
                    .as_ref()
                    .and_then(|r| c.path.strip_prefix(r).ok())
                    .unwrap_or(&c.path);
                CommentRow {
                    label: format!("{}:{line}", shown.display()),
                    detail: c.body.lines().next().unwrap_or_default().to_string(),
                    line: line.saturating_sub(1),
                    path: c.path,
                }
            })
            .collect()
    }
}

/// Binds "Claude: Review Comments for Session…" on the shell's root element.
pub(super) fn bind_comment_actions(el: gpui::Div, cx: &mut Context<Shell>) -> gpui::Div {
    el.on_action(cx.listener(|this, _: &ClaudeReviewComments, window, cx| {
        this.open_palette(super::palette::Mode::ReviewComments, window, cx)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn the_latest_session_that_changed_the_file_is_the_one() {
        let bound = [
            ("old".to_string(), at(10)),
            ("new".to_string(), at(30)),
            ("other-file".to_string(), at(50)),
        ];
        let changed = |s: &str| s != "other-file";
        assert_eq!(pick_session(&bound, changed).as_deref(), Some("new"));
        assert_eq!(pick_session(&bound, |_| false), None, "none changed it");
        assert_eq!(pick_session(&[], |_| true), None, "none in this window");
    }

    #[test]
    fn a_failed_save_stores_nothing_and_tells_the_box_to_keep_its_text() {
        let dir = std::env::temp_dir().join(format!("athena-rc-apply-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = Path::new("/p/a.rs");
        let add = |lines| ReviewEvent::Add {
            lines,
            body: "why".into(),
        };
        let run = |text: Option<&str>, event| apply(&dir, "s-1", path, text, event);
        assert!(
            run(Some("a\nb\n"), add(3..=3)).unwrap(),
            "the trailing empty line"
        );
        assert!(
            run(Some("a\nb\n"), add(9..=9)).is_err(),
            "a line that is gone"
        );
        assert!(run(None, add(1..=1)).is_err(), "a view with no text");
        let edit = |id| ReviewEvent::Edit {
            id,
            body: "x".into(),
        };
        assert!(run(None, edit(99)).is_err(), "a comment deleted meanwhile");
        assert!(run(None, edit(0)).unwrap());
        assert!(!run(None, ReviewEvent::Delete { id: 0 }).unwrap());
        assert_eq!(store::load(&dir, "s-1").unwrap().len(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn placing_every_comment_in_one_split_gives_each_its_own_result() {
        let comment = |id, line, anchor: &str, sent| ReviewComment {
            id,
            session: "s".into(),
            path: "/p/a.rs".into(),
            lines: line..=line,
            anchor: anchor.into(),
            body: String::new(),
            sent,
            created: 0,
        };
        let text = "new\nnew\na\nb\nc\n";
        let all = [
            comment(1, 1, "a", false),
            comment(2, 2, "b", true),
            comment(3, 5, "gone", false),
        ];
        let shown = marks(text, &all);
        for (c, m) in all.iter().zip(&shown) {
            let alone = store::place(&store::split_lines(text), c);
            assert_eq!(
                (alone.lines, alone.stale, c.sent),
                (m.lines.clone(), m.stale, m.sent)
            );
        }
        assert_eq!(shown[0].lines, 3..=3);
        assert!(shown[2].stale);
    }

    #[test]
    fn the_column_needs_a_project_file_and_claude_running_or_bound_here() {
        assert!(
            column_reserved(true, true, false),
            "a restored Claude terminal"
        );
        assert!(column_reserved(true, false, true));
        assert!(!column_reserved(true, false, false));
        assert!(
            !column_reserved(false, true, true),
            "a file outside the projects"
        );
    }

    #[test]
    fn equal_times_pick_the_same_session_every_time() {
        let bound = [("b".to_string(), at(5)), ("a".to_string(), at(5))];
        assert_eq!(pick_session(&bound, |_| true).as_deref(), Some("b"));
    }

    #[test]
    fn comments_are_placed_in_the_text_they_are_shown_in() {
        let c = ReviewComment {
            id: 3,
            session: "s".into(),
            path: "/p/a.rs".into(),
            lines: 2..=2,
            anchor: "b".into(),
            body: "why".into(),
            sent: false,
            created: 0,
        };
        let shown = marks("x\ny\nb\n", std::slice::from_ref(&c));
        assert_eq!(shown[0].lines, 3..=3);
        assert!(!shown[0].stale);
        assert!(marks("x\ny\n", &[c])[0].stale);
    }
}
