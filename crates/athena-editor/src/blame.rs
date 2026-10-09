//! The whole-file blame gutter: who last changed each stretch of lines, and how long ago.

use std::ops::Range;

use athena_ui::{ActiveTheme, motion};
use gpui::{
    Animation, AnyElement, Context, EventEmitter, Pixels, Point, anchored, div, point, prelude::*,
    px,
};

use crate::view::EditorView;

/// Columns of the gutter the blame text takes.
pub(crate) const BLAME_COLUMNS: usize = 26;

/// What a commit shows in the blame gutter; the shell words it so the editor needs no git.
#[derive(Clone, Debug, PartialEq)]
pub struct BlameCommit {
    pub sha: String,
    pub author: String,
    /// "3 days ago".
    pub age: String,
    pub summary: String,
    /// 1 for the newest commit in the file down to 0 for the oldest, for the age colour.
    pub heat: f32,
}

/// Blame for every line of a file: its commits, and which one each zero-based line came from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GutterBlame {
    pub commits: Vec<BlameCommit>,
    pub lines: Vec<usize>,
}

/// Something the git gutters ask of whoever knows the repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitGutterEvent {
    /// The blame gutter was clicked on a commit's lines.
    OpenCommit { sha: String },
}

impl EventEmitter<GitGutterEvent> for EditorView {}

/// The blame gutter's state on one editor.
#[derive(Default)]
pub(crate) struct FileBlame {
    pub(crate) blame: Option<GutterBlame>,
    /// Stretches of lines from one commit, as (lines, commit).
    pub(crate) chunks: Vec<(Range<usize>, usize)>,
    /// The chunk under the pointer, and where the pointer was.
    hovered: Option<(usize, Point<Pixels>)>,
}

/// Consecutive lines from the same commit, merged into one stretch each.
pub(crate) fn chunks(lines: &[usize]) -> Vec<(Range<usize>, usize)> {
    let mut out: Vec<(Range<usize>, usize)> = Vec::new();
    for (line, &commit) in lines.iter().enumerate() {
        match out.last_mut() {
            Some((range, c)) if *c == commit && range.end == line => range.end = line + 1,
            _ => out.push((line..line + 1, commit)),
        }
    }
    out
}

/// The blame text for a stretch's first row: author, then age, cut to fit `columns`.
pub(crate) fn caption(commit: &BlameCommit, columns: usize) -> String {
    let age = commit.age.as_str();
    let room = columns.saturating_sub(age.chars().count() + 1);
    let mut author: String = commit.author.chars().take(room).collect();
    if author.chars().count() < commit.author.chars().count() && room > 0 {
        author.pop();
        author.push('…');
    }
    let pad = columns.saturating_sub(author.chars().count() + age.chars().count());
    let mut out = author;
    out.extend(std::iter::repeat_n(' ', pad.max(1)));
    out.push_str(age);
    out
}

impl EditorView {
    /// Shows or hides the blame gutter; `None` hides it.
    pub fn set_file_blame(&mut self, blame: Option<GutterBlame>, cx: &mut Context<Self>) {
        if self.file_blame.blame == blame {
            return;
        }
        self.file_blame.chunks = blame.as_ref().map(|b| chunks(&b.lines)).unwrap_or_default();
        self.file_blame.blame = blame;
        self.file_blame.hovered = None;
        cx.notify();
    }

    pub fn has_file_blame(&self) -> bool {
        self.file_blame.blame.is_some()
    }

    /// The blame stretch drawn at a window position, if the pointer is over the blame gutter.
    fn blame_chunk_at(&self, position: Point<Pixels>) -> Option<usize> {
        self.file_blame.blame.as_ref()?;
        let layout = self.layout.as_ref()?;
        if position.x < layout.fold_column.1 || position.x >= layout.text_left {
            return None;
        }
        let line = self
            .char_at_position(position)
            .and_then(|at| Some(self.buf()?.line_of(at)))?;
        self.file_blame
            .chunks
            .iter()
            .position(|(lines, _)| lines.contains(&line))
    }

    /// A click on the blame gutter opens that commit's changes; true if it was there.
    pub(crate) fn click_blame(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(chunk) = self.blame_chunk_at(position) else {
            return false;
        };
        let commit = self.file_blame.chunks[chunk].1;
        if let Some(c) = self
            .file_blame
            .blame
            .as_ref()
            .and_then(|b| b.commits.get(commit))
        {
            cx.emit(GitGutterEvent::OpenCommit { sha: c.sha.clone() });
        }
        true
    }

    /// Follows the pointer over the blame gutter for the commit popover.
    pub(crate) fn hover_blame(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let hovered = self.blame_chunk_at(position).map(|c| (c, position));
        if hovered.map(|h| h.0) != self.file_blame.hovered.map(|h| h.0) {
            self.file_blame.hovered = hovered;
            cx.notify();
        }
    }

    pub(crate) fn blame_hovered(&self) -> Option<usize> {
        self.file_blame.hovered.map(|h| h.0)
    }

    /// The hovered commit's summary, next to the pointer, as JetBrains annotations show it.
    pub(crate) fn render_blame_hover(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (chunk, at) = self.file_blame.hovered?;
        let blame = self.file_blame.blame.as_ref()?;
        let commit = blame.commits.get(self.file_blame.chunks.get(chunk)?.1)?;
        let t = cx.theme().clone();
        let short = &commit.sha[..commit.sha.len().min(7)];
        let head = format!("{} · {} · {short}", commit.author, commit.age);
        let panel = div()
            .id("blame-hover")
            .occlude()
            .max_w(px(420.))
            .px(px(10.))
            .py(px(6.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_panel)
            .shadow(vec![t.popover_shadow()])
            .font_family(t.typography.ui.clone())
            .text_size(t.typography.caption)
            .child(div().text_color(t.color.content_muted).child(head))
            .child(
                div()
                    .text_color(t.color.content)
                    .child(commit.summary.clone()),
            )
            .child(
                div()
                    .text_color(t.color.content_muted)
                    .child("Click to open this commit's changes"),
            );
        let panel = motion::animate_if(
            t.motion.reduced,
            panel,
            ("blame-hover-open", chunk),
            Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d),
        );
        Some(
            anchored()
                .position(point(at.x + px(12.), at.y + px(12.)))
                .snap_to_window()
                .child(panel)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(author: &str, age: &str) -> BlameCommit {
        BlameCommit {
            sha: "0123456789".into(),
            author: author.into(),
            age: age.into(),
            summary: String::new(),
            heat: 0.,
        }
    }

    #[test]
    fn neighbouring_lines_of_one_commit_form_one_stretch() {
        assert_eq!(
            chunks(&[0, 0, 1, 0, 0, 2]),
            vec![(0..2, 0), (2..3, 1), (3..5, 0), (5..6, 2)]
        );
        assert!(chunks(&[]).is_empty());
    }

    #[test]
    fn captions_keep_the_age_and_cut_long_authors() {
        let c = caption(&commit("Ann", "3 days ago"), 20);
        assert_eq!(c, "Ann       3 days ago");
        assert_eq!(c.chars().count(), 20);
        let long = caption(&commit("Zoë Ünal-Long-Name", "2 years ago"), 20);
        assert_eq!(long, "Zoë Üna… 2 years ago");
        assert_eq!(long.chars().count(), 20);
    }
}
