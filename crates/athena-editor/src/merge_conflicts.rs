//! Merge conflict markers: finding `<<<<<<<` … `>>>>>>>` blocks, resolving them, and drawing them
//! with VS Code's Accept and Compare actions.

use std::cell::RefCell;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use athena_ui::{ActiveTheme, Theme};
use gpui::{
    Action, AnyElement, Context, Hsla, MouseButton, Window, anchored, div, point, prelude::*, px,
};
use ropey::Rope;

use crate::view::EditorView;

/// Opens the file's current changes against its incoming changes, from its conflict markers.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = editor, no_json)]
pub struct CompareMergeConflicts {
    pub path: PathBuf,
}

/// One conflict, by zero-based lines of its markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConflictBlock {
    /// `<<<<<<< ours`
    pub start: usize,
    /// `||||||| base`, in diff3 style.
    pub base: Option<usize>,
    /// `=======`
    pub separator: usize,
    /// `>>>>>>> theirs`
    pub end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    Current,
    Incoming,
    Both,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Marker {
    Start,
    Base,
    Separator,
    End,
}

fn marker(line: &str) -> Option<Marker> {
    let line = line.trim_end_matches(['\n', '\r']);
    let (c, kind) = match line.as_bytes().first()? {
        b'<' => (b'<', Marker::Start),
        b'|' => (b'|', Marker::Base),
        b'=' => (b'=', Marker::Separator),
        b'>' => (b'>', Marker::End),
        _ => return None,
    };
    let bytes = line.as_bytes();
    let run = bytes.iter().take_while(|b| **b == c).count();
    // Exactly seven, then the end of the line or a space before the label.
    let ok = run == 7 && (bytes.len() == 7 || (kind != Marker::Separator && bytes[7] == b' '));
    let ok = ok || (kind == Marker::Separator && run == 7 && line[7..].trim().is_empty());
    ok.then_some(kind)
}

/// Blocks from markers in line order. A block with another `<<<<<<<` inside it is left out,
/// with the one inside: which marker closes which cannot be told.
fn blocks(markers: impl IntoIterator<Item = (usize, Marker)>) -> Vec<ConflictBlock> {
    let mut out = Vec::new();
    let mut open: Option<(usize, Option<usize>, Option<usize>)> = None;
    let mut depth = 0usize;
    for (line, kind) in markers {
        match kind {
            Marker::Start => {
                depth += 1;
                if depth == 1 {
                    open = Some((line, None, None));
                } else {
                    open = None;
                }
            }
            Marker::Base => {
                if let Some((_, base @ None, None)) = open.as_mut() {
                    *base = Some(line);
                }
            }
            Marker::Separator => {
                if let Some((_, _, sep @ None)) = open.as_mut() {
                    *sep = Some(line);
                }
            }
            Marker::End => {
                if depth == 0 {
                    continue;
                }
                depth -= 1;
                if depth == 0
                    && let Some((start, base, Some(separator))) = open.take()
                {
                    out.push(ConflictBlock {
                        start,
                        base,
                        separator,
                        end: line,
                    });
                }
            }
        }
    }
    out
}

/// Blocks from markers, and whether a `<<<<<<<`, `|||||||` or `>>>>>>>` line is left outside them.
/// A lone `=======` does not count: Markdown and reStructuredText underline headings with it.
fn scan(markers: impl IntoIterator<Item = (usize, Marker)>) -> (Vec<ConflictBlock>, bool) {
    let markers: Vec<(usize, Marker)> = markers.into_iter().collect();
    let found = blocks(markers.iter().copied());
    let stray = markers.iter().any(|&(line, kind)| {
        kind != Marker::Separator && !found.iter().any(|b| (b.start..=b.end).contains(&line))
    });
    (found, stray)
}

fn text_markers(text: &str) -> impl Iterator<Item = (usize, Marker)> + '_ {
    text.split_inclusive('\n')
        .enumerate()
        .filter_map(|(i, l)| Some((i, marker(l)?)))
}

/// The conflict blocks in `text`.
pub fn find_merge_conflicts(text: &str) -> Vec<ConflictBlock> {
    blocks(text_markers(text))
}

/// How many conflicts `text` still has: its blocks, plus one if marker lines are left outside
/// them, so an unclosed `<<<<<<<` never reads as resolved.
pub fn count_merge_conflicts(text: &str) -> usize {
    let (found, stray) = scan(text_markers(text));
    found.len() + usize::from(stray)
}

fn rope_conflicts(rope: &Rope) -> (Vec<ConflictBlock>, bool) {
    scan(rope.lines().enumerate().filter_map(|(i, line)| {
        let first = line.chars().next()?;
        if !"<|=>".contains(first) || line.chars().take(7).any(|c| c != first) {
            return None;
        }
        Some((i, marker(&line.to_string())?))
    }))
}

/// The text that replaces a block's lines, from `lines` (each with its line break) of the file.
pub fn resolve_block(lines: &[&str], block: &ConflictBlock, how: Resolution) -> String {
    let current = &lines[block.start + 1..block.base.unwrap_or(block.separator)];
    let incoming = &lines[block.separator + 1..block.end];
    let kept: Vec<&str> = match how {
        Resolution::Current => current.to_vec(),
        Resolution::Incoming => incoming.to_vec(),
        Resolution::Both => current.iter().chain(incoming).copied().collect(),
    };
    let mut out = kept.concat();
    // A block at the very end of a file without a final line break keeps it that way.
    if !lines[block.end].ends_with(['\n', '\r']) {
        if out.ends_with("\r\n") {
            out.truncate(out.len() - 2);
        } else if out.ends_with(['\n', '\r']) {
            out.pop();
        }
    }
    out
}

/// The char range a block covers in `rope` and the text that replaces it.
fn resolve_in_rope(rope: &Rope, block: &ConflictBlock, how: Resolution) -> (Range<usize>, String) {
    let from = rope.line_to_char(block.start);
    let to = if block.end + 1 < rope.len_lines() {
        rope.line_to_char(block.end + 1)
    } else {
        rope.len_chars()
    };
    // The rope's own lines, so a lone `\r` counts as a break here as it did when finding blocks.
    let owned: Vec<String> = rope.slice(from..to).lines().map(String::from).collect();
    let lines: Vec<&str> = owned.iter().map(String::as_str).collect();
    let local = ConflictBlock {
        start: 0,
        base: block.base.map(|l| l - block.start),
        separator: block.separator - block.start,
        end: block.end - block.start,
    };
    (from..to, resolve_block(&lines, &local, how))
}

/// `text` with every block resolved the same way, for comparing the two sides whole.
pub fn resolve_all(text: &str, how: Resolution) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut out = String::with_capacity(text.len());
    let mut next = 0;
    for block in find_merge_conflicts(text) {
        out.extend(lines[next..block.start].iter().copied());
        out.push_str(&resolve_block(&lines, &block, how));
        next = block.end + 1;
    }
    out.extend(lines[next..].iter().copied());
    out
}

/// Which part of a block a line is in, for its background.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Band {
    CurrentHeader,
    Current,
    BaseHeader,
    Base,
    Separator,
    Incoming,
    IncomingHeader,
}

fn band(blocks: &[ConflictBlock], line: usize) -> Option<Band> {
    let i = blocks.partition_point(|b| b.end < line);
    let b = blocks.get(i).filter(|b| b.start <= line)?;
    Some(match line {
        l if l == b.start => Band::CurrentHeader,
        l if Some(l) == b.base => Band::BaseHeader,
        l if l == b.separator => Band::Separator,
        l if l == b.end => Band::IncomingHeader,
        l if l > b.separator => Band::Incoming,
        l if b.base.is_some_and(|base| l > base) => Band::Base,
        _ => Band::Current,
    })
}

/// Blocks found in one version of the buffer, and whether stray markers were left outside them.
#[derive(Default)]
pub(crate) struct MergeCache(RefCell<Option<(u64, Scan)>>);

type Scan = (Rc<Vec<ConflictBlock>>, bool);

impl EditorView {
    fn merge_scan(&self) -> Scan {
        let Some(b) = self.buf() else {
            return Default::default();
        };
        let version = b.version();
        let mut cache = self.merge.0.borrow_mut();
        match cache.as_ref() {
            Some((v, scan)) if *v == version => scan.clone(),
            _ => {
                let (blocks, stray) = rope_conflicts(b.rope());
                let scan = (Rc::new(blocks), stray);
                *cache = Some((version, scan.clone()));
                scan
            }
        }
    }

    pub(crate) fn merge_blocks(&self) -> Rc<Vec<ConflictBlock>> {
        self.merge_scan().0
    }

    /// How many conflicts the text has now, counted as [`count_merge_conflicts`] does.
    pub fn merge_conflict_count(&self) -> usize {
        let (blocks, stray) = self.merge_scan();
        blocks.len() + usize::from(stray)
    }

    /// The tint behind a line inside a conflict: current changes green, incoming blue, the
    /// common base grey, as VS Code colours them; marker lines stronger.
    pub(crate) fn merge_band(&self, line: usize, t: &Theme) -> Option<Hsla> {
        let (current, incoming, base) = (
            t.color.success,
            t.terminal.ansi[4],
            t.color.content_disabled,
        );
        Some(match band(&self.merge_blocks(), line)? {
            Band::CurrentHeader => current.opacity(0.4),
            Band::Current => current.opacity(0.16),
            Band::BaseHeader => base.opacity(0.4),
            Band::Base => base.opacity(0.16),
            Band::Separator => return None,
            Band::Incoming => incoming.opacity(0.16),
            Band::IncomingHeader => incoming.opacity(0.4),
        })
    }

    /// Replaces the block starting at `start` with its resolution, as one undo step.
    fn accept_conflict(&mut self, start: usize, how: Resolution, cx: &mut Context<Self>) {
        let Some(block) = self
            .merge_blocks()
            .iter()
            .find(|b| b.start == start)
            .copied()
        else {
            return;
        };
        self.with_buffer(cx, |b, c| {
            let (range, resolved) = resolve_in_rope(b.rope(), &block, how);
            b.edit_primary(c, |b, c| b.replace_range(c, range, &resolved));
        });
    }

    /// VS Code's CodeLens row for each block on screen, after its `<<<<<<<` line's text.
    pub(crate) fn render_merge_actions(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let blocks = self.merge_blocks();
        let Some(layout) = self.layout.as_ref() else {
            return Vec::new();
        };
        if blocks.is_empty() {
            return Vec::new();
        }
        let t = cx.theme().clone();
        let lh = layout.line_height;
        let top = layout.origin.y + lh * layout.sticky.len() as f32;
        let bottom = layout.origin.y + self.viewport.height - lh;
        let mut out = Vec::new();
        for block in blocks.iter() {
            let Some(end) = self
                .buf()
                .map(|b| b.line_start(block.start) + b.line_len(block.start))
            else {
                break;
            };
            let Some(origin) = self.char_origin(end) else {
                continue;
            };
            if origin.y < top || origin.y > bottom {
                continue;
            }
            let start = block.start;
            let path = self.path().to_path_buf();
            let link = |id: &'static str, label: &'static str| {
                div()
                    .id((id, start))
                    .cursor_pointer()
                    .text_color(t.color.content_muted)
                    .hover(|s| s.text_color(t.color.content).underline())
                    .child(label)
            };
            let sep = || div().text_color(t.color.content_disabled).child("|");
            let accept = |how: Resolution| {
                cx.listener(move |this: &mut Self, _: &gpui::ClickEvent, _, cx| {
                    this.accept_conflict(start, how, cx)
                })
            };
            let row = div()
                .id(("merge-actions", start))
                .occlude()
                .h(lh)
                .flex()
                .items_center()
                .gap(px(6.))
                .font_family(t.typography.ui.clone())
                .text_size(t.typography.caption)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    link("merge-current", "Accept Current Change")
                        .on_click(accept(Resolution::Current)),
                )
                .child(sep())
                .child(
                    link("merge-incoming", "Accept Incoming Change")
                        .on_click(accept(Resolution::Incoming)),
                )
                .child(sep())
                .child(link("merge-both", "Accept Both Changes").on_click(accept(Resolution::Both)))
                .child(sep())
                .child(
                    link("merge-compare", "Compare Changes").on_click(cx.listener(
                        move |this: &mut Self, _: &gpui::ClickEvent, window: &mut Window, cx| {
                            window.focus(&this.focus);
                            let action = CompareMergeConflicts { path: path.clone() };
                            window.dispatch_action(Box::new(action), cx);
                        },
                    )),
                );
            // Drawn in tree order, under the palette and menus, and cut off at the editor's edge.
            let x = origin.x + layout.cell * 3.;
            let room = layout.origin.x + self.viewport.width - x;
            if room > px(0.) {
                out.push(
                    anchored()
                        .position(point(x, origin.y))
                        .child(row.max_w(room).overflow_hidden())
                        .into_any_element(),
                );
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_WAY: &str = "a\n<<<<<<< HEAD\nmine\n=======\ntheirs\nmore\n>>>>>>> feat\nz\n";

    #[test]
    fn blocks_are_found_with_and_without_a_base() {
        assert_eq!(
            find_merge_conflicts(TWO_WAY),
            [ConflictBlock {
                start: 1,
                base: None,
                separator: 3,
                end: 6
            }]
        );
        let diff3 = "<<<<<<< ours\nx\n||||||| base\nb\n=======\ny\n>>>>>>> theirs\n";
        assert_eq!(find_merge_conflicts(diff3)[0].base, Some(2));
        assert_eq!(
            find_merge_conflicts(&Rope::from_str(diff3).to_string()),
            rope_conflicts(&Rope::from_str(diff3)).0
        );
    }

    #[test]
    fn accepting_keeps_each_side_or_both() {
        let lines: Vec<&str> = TWO_WAY.split_inclusive('\n').collect();
        let b = find_merge_conflicts(TWO_WAY)[0];
        assert_eq!(resolve_block(&lines, &b, Resolution::Current), "mine\n");
        assert_eq!(
            resolve_block(&lines, &b, Resolution::Incoming),
            "theirs\nmore\n"
        );
        assert_eq!(
            resolve_block(&lines, &b, Resolution::Both),
            "mine\ntheirs\nmore\n"
        );
        assert_eq!(resolve_all(TWO_WAY, Resolution::Current), "a\nmine\nz\n");
        assert_eq!(
            resolve_all(TWO_WAY, Resolution::Incoming),
            "a\ntheirs\nmore\nz\n"
        );
    }

    #[test]
    fn crlf_files_keep_their_line_breaks() {
        let text =
            "<<<<<<< HEAD\r\nmine\r\n||||||| base\r\nold\r\n=======\r\ntheirs\r\n>>>>>>> x\r\nz";
        assert_eq!(resolve_all(text, Resolution::Both), "mine\r\ntheirs\r\nz");
        let tail = "a\r\n<<<<<<< HEAD\r\nmine\r\n=======\r\ntheirs\r\n>>>>>>> x";
        assert_eq!(resolve_all(tail, Resolution::Incoming), "a\r\ntheirs");
    }

    #[test]
    fn nested_markers_are_refused_but_later_blocks_still_count() {
        let nested = "<<<<<<< a\n<<<<<<< b\nx\n=======\ny\n>>>>>>> b\n=======\nz\n>>>>>>> a\n\
                      <<<<<<< c\n1\n=======\n2\n>>>>>>> c\n";
        let found = find_merge_conflicts(nested);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].start, 9);
        assert_eq!(
            resolve_all(nested, Resolution::Current)
                .matches("<<<<<<<")
                .count(),
            2
        );
    }

    #[test]
    fn look_alike_lines_are_not_markers() {
        let text = "<<<<<<<< eight\n======== x\n<<<<<<<x\nheading\n=======\n>>>>>>>\n";
        assert!(find_merge_conflicts(text).is_empty());
        assert_eq!(marker("=======  \n"), Some(Marker::Separator));
        assert_eq!(marker(">>>>>>>\n"), Some(Marker::End));
        assert_eq!(marker("<<<<<<< HEAD\r\n"), Some(Marker::Start));
    }

    #[test]
    fn bands_follow_the_parts_of_a_block() {
        let diff3 = "pre\n<<<<<<< ours\nx\n||||||| base\nb\n=======\ny\n>>>>>>> theirs\npost\n";
        let blocks = find_merge_conflicts(diff3);
        let bands: Vec<Option<Band>> = (0..9).map(|l| band(&blocks, l)).collect();
        assert_eq!(
            bands,
            [
                None,
                Some(Band::CurrentHeader),
                Some(Band::Current),
                Some(Band::BaseHeader),
                Some(Band::Base),
                Some(Band::Separator),
                Some(Band::Incoming),
                Some(Band::IncomingHeader),
                None
            ]
        );
    }

    #[test]
    fn a_lone_carriage_return_inside_a_block_is_a_line_of_its_own() {
        let rope = Rope::from_str("<<<<<<< HEAD\nmine\rmore\n=======\ntheirs\n>>>>>>> x\nz\n");
        let block = rope_conflicts(&rope).0[0];
        let (range, text) = resolve_in_rope(&rope, &block, Resolution::Incoming);
        assert_eq!((range, text.as_str()), (0..48, "theirs\n"));
        let (_, text) = resolve_in_rope(&rope, &block, Resolution::Current);
        assert_eq!(text, "mine\rmore\n");
        let tail = Rope::from_str("<<<<<<< HEAD\nmine\n=======\ntheirs\r>>>>>>> x");
        let block = rope_conflicts(&tail).0[0];
        assert_eq!(
            resolve_in_rope(&tail, &block, Resolution::Incoming).1,
            "theirs"
        );
    }

    #[test]
    fn an_unclosed_marker_keeps_the_file_unresolved() {
        let text = "<<<<<<< stray\nfoo\n<<<<<<< HEAD\na\n=======\nb\n>>>>>>> x\n";
        assert!(find_merge_conflicts(text).is_empty());
        assert_eq!(count_merge_conflicts(text), 1);
        assert_eq!(rope_conflicts(&Rope::from_str(text)), (Vec::new(), true));
        assert_eq!(
            count_merge_conflicts(&format!("{TWO_WAY}>>>>>>> left\n")),
            2
        );
        assert_eq!(count_merge_conflicts(TWO_WAY), 1);
        assert_eq!(count_merge_conflicts("Title\n=======\n\nbody\n"), 0);
    }
}
