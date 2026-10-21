use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use athena_lsp::Position;
use athena_ui::{ActiveTheme, Badge, Button, ButtonKind, Space, StatusDot, Theme, empty_state};
use athena_workspace::{Axis, DiffBase, ItemKind, git};
use gpui::{AnyElement, Context, FontWeight, Task, Window, div, prelude::*};

use super::list::{bare_row, dot_state, row_text};
use super::{FileChange, ROW_HEIGHT, detail_line};
use crate::review_comments::{self as store, Placement, ReviewComment};
use crate::shell::Shell;
use crate::shell::item::{ItemView, SessionTab};
use crate::shell::review_comments::delivery::place_all;
use crate::snapshots;
use crate::worktree_forks::worktree_fork;

/// Past this many files a worktree's change list is cut off: it is a list to read, not a diffstat.
const MAX_FORK_FILES: usize = 500;

/// Element ids of this panel's rows start here, clear of the Sessions list's, which count from zero.
const ID_BASE: usize = 1_000_000_000;

/// What the background read of a session found.
#[derive(Clone, Debug, Default, PartialEq)]
struct Loaded {
    /// The snapshot files plus those changed since the worktree branched; `None` outside a
    /// worktree with a fork record, where the snapshot files stand alone.
    changes: Option<Vec<FileChange>>,
    comments: Vec<SessionComment>,
}

#[derive(Default)]
struct Slot {
    loaded: Option<Loaded>,
    task: Option<Task<()>>,
    /// Something changed while the read ran, so it runs once more.
    again: bool,
}

/// The open session panels' data, by session id.
#[derive(Default)]
pub(super) struct Views {
    slots: HashMap<String, Slot>,
}

/// One review comment as the panel lists it.
#[derive(Clone, Debug, PartialEq)]
struct SessionComment {
    path: PathBuf,
    /// One-based, where the comment sits in its file now.
    line: u32,
    /// `src/a.rs:12`.
    label: String,
    /// The first line of the body.
    title: String,
    sent: bool,
    status: Option<&'static str>,
}

fn comment_row(comment: &ReviewComment, placed: &Placement, root: &Path) -> SessionComment {
    let line = *placed.lines.start();
    let shown = comment.path.strip_prefix(root).unwrap_or(&comment.path);
    SessionComment {
        path: comment.path.clone(),
        line,
        label: format!("{}:{line}", shown.display()),
        title: comment.body.lines().next().unwrap_or_default().to_string(),
        sent: comment.sent,
        status: match (comment.sent, placed.stale) {
            (true, true) => Some("sent · stale"),
            (false, true) => Some("stale"),
            (true, false) => Some("sent"),
            (false, false) => None,
        },
    }
}

/// A file's real location, so the spellings a snapshot and git give of it compare equal; a file
/// that is gone is named by its folder.
fn real(path: &Path) -> PathBuf {
    path.canonicalize()
        .ok()
        .or_else(|| Some(path.parent()?.canonicalize().ok()?.join(path.file_name()?)))
        .unwrap_or_else(|| path.to_path_buf())
}

/// The snapshot files, then every other file changed since commit `rev` (`changed`, then
/// `untracked`, named from the repository's top), which a diff against `rev` shows.
fn union(
    top: &Path,
    rev: &str,
    changed: &[String],
    untracked: &[String],
    snapshot: &[FileChange],
) -> Vec<FileChange> {
    let known: HashSet<PathBuf> = snapshot.iter().map(|f| real(&f.path)).collect();
    let mut extra: BTreeMap<&str, bool> = BTreeMap::new();
    extra.extend(changed.iter().map(|rel| (rel.as_str(), false)));
    extra.extend(untracked.iter().map(|rel| (rel.as_str(), true)));
    let mut out = snapshot.to_vec();
    out.extend(
        extra
            .into_iter()
            .map(|(rel, created)| (top.join(rel), rel, created))
            .filter(|(path, ..)| !known.contains(&real(path)))
            .take(MAX_FORK_FILES)
            .map(|(path, rel, created)| FileChange {
                path,
                counts: None,
                created,
                base: Some(DiffBase::Revision {
                    rev: rev.to_string(),
                    at: rel.to_string(),
                }),
            }),
    );
    out
}

fn fork_changes(top: &Path, rev: &str, snapshot: &[FileChange]) -> anyhow::Result<Vec<FileChange>> {
    let (changed, untracked) = git::changed_since(top, rev)?;
    Ok(union(top, rev, &changed, &untracked, snapshot))
}

/// Reads what the panel shows that is not in memory: the worktree's changes and the comments.
fn read_session(session: &str, cwd: &Path, snapshot: &[FileChange], store_dir: &Path) -> Loaded {
    let top = git::work_tree(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let fork = git::main_worktree(&top).and_then(|_| worktree_fork(&top));
    let changes = fork.and_then(|f| fork_changes(&top, &f.fork_sha, snapshot).ok());
    let all = store::load(store_dir, session).unwrap_or_default();
    let mut comments: Vec<SessionComment> = place_all(all, |p| std::fs::read_to_string(p).ok())
        .iter()
        .map(|(c, placed)| comment_row(c, placed, &top))
        .collect();
    comments.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    Loaded { changes, comments }
}

fn section_header(
    id: &'static str,
    title: &'static str,
    count: usize,
    action: Option<AnyElement>,
    t: &Theme,
) -> AnyElement {
    div()
        .id(id)
        .min_h(t.ui(ROW_HEIGHT))
        .mt(t.space(Space::Md))
        .px(t.space(Space::Lg))
        .flex()
        .items_center()
        .gap(t.space(Space::Sm))
        .child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.color.content)
                .child(title),
        )
        .child(Badge::new(count))
        .child(div().flex_1())
        .children(action)
        .into_any_element()
}

impl Shell {
    /// The ids of the sessions this window has a panel open for.
    pub(in crate::shell) fn open_session_views(&self) -> Vec<String> {
        self.workspace
            .projects
            .iter()
            .flat_map(|p| p.items())
            .filter_map(|i| match &i.kind {
                ItemKind::Session { id } => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    pub(in crate::shell) fn session_tab_label(&self, id: &str) -> String {
        let label = self.review.sessions.row(id).map(|r| r.label.as_str());
        format!("Session: {}", label.unwrap_or(super::short_id(id)))
    }

    /// Reads the open panels' sessions again in the background.
    pub(in crate::shell) fn refresh_session_views(&mut self, cx: &mut Context<Self>) {
        let open = self.open_session_views();
        self.review
            .sessions
            .views
            .slots
            .retain(|id, _| open.contains(id));
        for id in open {
            self.refresh_session_view(&id, cx);
        }
    }

    pub(in crate::shell) fn refresh_session_view(&mut self, id: &str, cx: &mut Context<Self>) {
        let slots = &mut self.review.sessions.views.slots;
        let slot = slots.entry(id.to_string()).or_default();
        if slot.task.is_some() {
            slot.again = true;
            return;
        }
        let Ok(store_dir) = snapshots::store() else {
            return;
        };
        let Some(session) = self.live_sessions(cx).into_iter().find(|s| s.id == id) else {
            return;
        };
        let snapshot = self
            .review
            .sessions
            .row(id)
            .map(|r| r.files.clone())
            .unwrap_or_default();
        let id = id.to_string();
        let slot_id = id.clone();
        let task = cx.spawn(async move |this, cx| {
            let session_id = id.clone();
            let loaded = cx
                .background_executor()
                .spawn(
                    async move { read_session(&session_id, &session.cwd, &snapshot, &store_dir) },
                )
                .await;
            let _ = this.update(cx, |this, cx| {
                let again = match this.review.sessions.views.slots.get_mut(&id) {
                    Some(slot) => {
                        slot.task = None;
                        slot.loaded = Some(loaded);
                        std::mem::take(&mut slot.again)
                    }
                    None => false,
                };
                if again {
                    this.refresh_session_view(&id, cx);
                }
                cx.notify();
            });
        });
        if let Some(slot) = self.review.sessions.views.slots.get_mut(&slot_id) {
            slot.task = Some(task);
        }
    }

    /// Shows the panel for `session`: the open one, else a split right of the session's terminal
    /// when that is on screen, else a tab in the active pane.
    pub(in crate::shell) fn open_session_view(
        &mut self,
        session: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let kind = ItemKind::Session {
            id: session.clone(),
        };
        let open = self.workspace.projects.iter().find_map(|p| {
            let item = p.items().find(|i| i.kind == kind)?;
            Some((p.root.clone(), item.id))
        });
        if let Some((root, item)) = open {
            return self.focus_item(&root, item, window, cx);
        }
        let terminal = self
            .review
            .sessions
            .pane_of(&session)
            .and_then(|pane| self.find_session(pane))
            .filter(|(root, _)| self.active_root().as_ref() == Some(root))
            .and_then(|(_, item)| {
                let layout = self.workspace.active_project()?.layout.as_ref()?;
                let (pane, at) = layout.find_item(item)?;
                (layout.pane(pane)?.active == at).then_some(pane)
            });
        let Some(pane) = terminal else {
            return self.add_item(kind, window, cx);
        };
        self.zoomed = None;
        let split = self
            .workspace
            .active
            .and_then(|i| self.workspace.projects[i].layout.as_mut())
            .and_then(|l| l.split(pane, Axis::Horizontal, kind));
        self.entering = split;
        self.after_layout_change(window, cx);
    }

    /// The palette command: the focused terminal's session, else the most recent one.
    pub(in crate::shell) fn open_current_session_view(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focused = match self.focused_item_view() {
            Some(ItemView::Terminal(t)) => t
                .read(cx)
                .session()
                .and_then(|pane| self.review.sessions.session_of(pane))
                .map(str::to_string),
            Some(ItemView::Session(tab)) => Some(tab.id),
            _ => None,
        };
        let session = focused.or_else(|| {
            let all = self.live_sessions(cx);
            all.into_iter().max_by_key(|s| s.modified).map(|s| s.id)
        });
        match session {
            Some(id) => self.open_session_view(id, window, cx),
            None => self.transient_notice(
                "No Claude session",
                "Start Claude in a terminal to open its session view.",
                cx,
            ),
        }
    }

    pub(in crate::shell) fn render_session_view(
        &mut self,
        tab: &SessionTab,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let id = tab.id.clone();
        if let Some(root) = self.active_root() {
            self.refresh_sessions(&root, cx);
        }
        let Some(session) = self.live_sessions(cx).into_iter().find(|s| s.id == id) else {
            return div()
                .track_focus(&tab.focus)
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(empty_state(
                    "Session not found",
                    "This window no longer knows the session.",
                    None,
                    cx,
                ))
                .into_any_element();
        };
        let state = &self.review.sessions;
        let loaded = state.views.slots.get(&id).and_then(|s| s.loaded.clone());
        let mut row = state
            .row(&id)
            .cloned()
            .unwrap_or_else(|| bare_row(&session));
        if let Some(changes) = loaded.as_ref().and_then(|l| l.changes.clone()) {
            row.files = changes;
        }
        let comments = loaded.map(|l| l.comments).unwrap_or_default();
        let unsent = comments.iter().filter(|c| !c.sent).count();
        let reviewing = state.review.as_ref().is_some_and(|r| r.session == id);
        let prices = self.settings.file.claude_prices.clone();
        let text = row_text(&session, &prices);

        let focus = session.pane.map(|pane| {
            let id = id.clone();
            Button::new(
                "session-view-focus",
                "Focus Terminal",
                ButtonKind::Secondary,
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_live_session(&id, Some(pane), window, cx)
            }))
        });
        let chip = |text: String| {
            div()
                .flex_none()
                .px(t.space(Space::Sm))
                .rounded(t.shape.radius_control)
                .bg(t.color.surface_active)
                .text_color(t.color.content_secondary)
                .child(text)
        };
        let header = div()
            .flex_none()
            .px(t.space(Space::Lg))
            .py(t.space(Space::Md))
            .flex()
            .flex_col()
            .gap(t.space(Space::Sm))
            .border_b_1()
            .border_color(t.color.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(t.space(Space::Md))
                    .child(StatusDot::new(dot_state(session.state)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .text_size(t.typography.body)
                            .text_color(t.color.content)
                            .child(session.label.clone()),
                    )
                    .children(focus),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(t.space(Space::Sm))
                    .text_color(t.color.content_muted)
                    .children(text.branch.map(chip))
                    .children(text.cost)
                    .children(text.todos.map(|todos| format!("{todos} todos")))
                    .children(text.files),
            );

        let mut out: Vec<AnyElement> = Vec::new();
        out.push(section_header("session-plan", "Plan", 0, None, &t));
        let plan = self.render_live_plan(ID_BASE / 10_000, &row, &t);
        if plan.is_empty() {
            let hint = "No todos or plan reported yet.";
            out.push(detail_line(("session-plan-none", 0), hint, &t).into_any_element());
        }
        out.extend(plan);

        let review_all = (!row.files.is_empty() && !reviewing).then(|| {
            let review = row.clone();
            Button::new("session-review-all", "Review All", ButtonKind::Ghost)
                .on_click(
                    cx.listener(move |this, _, window, cx| this.start_review(&review, window, cx)),
                )
                .into_any_element()
        });
        out.push(section_header(
            "session-changes",
            "Changes",
            row.files.len(),
            review_all,
            &t,
        ));
        if reviewing && let Some(bar) = self.render_review_bar(true, &t, cx) {
            out.push(bar);
        }
        if row.files.is_empty() {
            let hint = "No changes recorded for this session.";
            out.push(detail_line(("session-changes-none", 0), hint, &t).into_any_element());
        }
        for (j, file) in row.files.iter().enumerate() {
            out.push(self.render_file_row(ID_BASE + j, &row.id, file, &session.cwd, true, &t, cx));
        }

        let send = (unsent > 0).then(|| {
            let id = id.clone();
            Button::new(
                "session-send-comments",
                athena_editor::send_label(unsent),
                ButtonKind::Secondary,
            )
            .on_click(cx.listener(move |this, _, _, cx| this.send_review_comments(id.clone(), cx)))
            .into_any_element()
        });
        out.push(section_header(
            "session-comments",
            "Comments",
            comments.len(),
            send,
            &t,
        ));
        if comments.is_empty() {
            let hint = "No review comments. Comment on a line of a change to start.";
            out.push(detail_line(("session-comments-none", 0), hint, &t).into_any_element());
        }
        for (j, c) in comments.iter().enumerate() {
            out.push(self.render_session_comment(j, c, &t, cx));
        }
        out.push(div().h(t.ui(ROW_HEIGHT)).flex_none().into_any_element());

        div()
            .id(("session-view", ID_BASE))
            .track_focus(&tab.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(t.color.surface)
            .text_size(t.typography.caption)
            .text_color(t.color.content_secondary)
            .child(header)
            .child(
                div()
                    .id("session-view-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(out),
            )
            .into_any_element()
    }

    fn render_session_comment(
        &self,
        j: usize,
        comment: &SessionComment,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (path, line) = (comment.path.clone(), comment.line);
        let stale = comment.status.is_some_and(|s| s.ends_with("stale"));
        div()
            .id(("session-comment", j))
            .px(t.space(Space::Lg))
            .py(t.space(Space::Xs))
            .flex()
            .flex_col()
            .cursor_pointer()
            .hover(|s| s.bg(t.color.surface_hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                let at = Position {
                    line: line.saturating_sub(1),
                    character: 0,
                };
                this.lsp.jump = Some((path.clone(), at));
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(t.space(Space::Md))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(match comment.sent {
                                true => t.color.content_muted,
                                false => t.color.content,
                            })
                            .child(comment.label.clone()),
                    )
                    .children(comment.status.map(|status| {
                        div()
                            .flex_none()
                            .text_color(match stale {
                                true => t.color.warning,
                                false => t.color.content_muted,
                            })
                            .child(status)
                    })),
            )
            .child(
                div()
                    .truncate()
                    .text_color(t.color.content_muted)
                    .child(comment.title.clone()),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sh(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("/usr/bin/git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=T", "-c", "user.email=t@x"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    }

    fn head(dir: &Path) -> String {
        git::head_sha(dir).unwrap()
    }

    fn snapshot_file(path: PathBuf) -> FileChange {
        FileChange {
            path,
            counts: Some((1, 0)),
            created: false,
            base: None,
        }
    }

    #[test]
    fn a_worktree_lists_what_changed_since_the_fork_beside_what_the_snapshots_track() {
        if !git::available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("athena-fork-diff-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let top = dir.canonicalize().unwrap();
        for name in ["committed", "uncommitted", "snapshot-only", "both"] {
            fs::write(top.join(name), "one\n").unwrap();
        }
        fs::write(top.join(".gitignore"), "ignored.log\n").unwrap();
        sh(&top, &["init", "-q", "-b", "main"]);
        sh(&top, &["add", "-A"]);
        sh(&top, &["commit", "-qm", "base"]);
        let fork = head(&top);

        fs::write(top.join("committed"), "two\n").unwrap();
        fs::write(top.join("both"), "two\n").unwrap();
        sh(&top, &["commit", "-qam", "work"]);
        fs::write(top.join("uncommitted"), "two\n").unwrap();
        fs::write(top.join("untracked"), "new\n").unwrap();
        fs::write(top.join("ignored.log"), "x\n").unwrap();

        let snapshot = [
            snapshot_file(top.join("snapshot-only")),
            snapshot_file(top.join("both")),
        ];
        let found = fork_changes(&top, &fork, &snapshot).unwrap();
        let at = |rel: &str| DiffBase::Revision {
            rev: fork.clone(),
            at: rel.to_string(),
        };
        let seen: Vec<(String, Option<DiffBase>, bool)> = found
            .iter()
            .map(|f| {
                let name = f.path.strip_prefix(&top).unwrap().display().to_string();
                (name, f.base.clone(), f.created)
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("snapshot-only".to_string(), None, false),
                ("both".to_string(), None, false),
                ("committed".to_string(), Some(at("committed")), false),
                ("uncommitted".to_string(), Some(at("uncommitted")), false),
                ("untracked".to_string(), Some(at("untracked")), true),
            ]
        );
        fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn a_path_a_snapshot_spells_another_way_is_still_one_file() {
        let dir = std::env::temp_dir().join(format!("athena-fork-spell-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/a"), "").unwrap();
        let top = dir.canonicalize().unwrap();
        let roundabout = dir.join("sub/../sub/a");
        let found = union(
            &top,
            "abcd",
            &["sub/a".into()],
            &[],
            &[snapshot_file(roundabout)],
        );
        assert_eq!(found.len(), 1);
        fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn a_comment_row_names_the_file_line_first_line_and_whether_it_was_sent_or_went_stale() {
        let comment = |sent| ReviewComment {
            id: 1,
            session: "s".into(),
            path: "/p/src/a.rs".into(),
            lines: 3..=4,
            anchor: "x".into(),
            body: "Rename this\nbecause it clashes".into(),
            sent,
            created: 0,
        };
        let placed = |stale| Placement {
            lines: 12..=13,
            stale,
        };
        let row = comment_row(&comment(false), &placed(false), Path::new("/p"));
        assert_eq!(row.label, "src/a.rs:12");
        assert_eq!(row.title, "Rename this");
        assert_eq!(row.status, None);
        let status =
            |sent, stale| comment_row(&comment(sent), &placed(stale), Path::new("/p")).status;
        assert_eq!(status(true, false), Some("sent"));
        assert_eq!(status(false, true), Some("stale"));
        assert_eq!(status(true, true), Some("sent · stale"));
        let outside = comment_row(&comment(false), &placed(false), Path::new("/q"));
        assert_eq!(outside.label, "/p/src/a.rs:12");
    }
}
