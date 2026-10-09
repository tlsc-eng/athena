use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use athena_ui::SyntaxColors;
use gpui::{
    Bounds, Context, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent, PaintQuad, Pixels, fill,
    point, px, size,
};

use crate::buffer::Buffer;
use crate::display::{TAB_WIDTH, wrap_breaks};
use crate::syntax::Token;
use crate::view::EditorView;

/// Height of one row, as VS Code's minimap at scale 1.
const PITCH: f32 = 2.;
/// Width of one column of text.
const COLUMN: f32 = 1.;
/// Columns drawn; text past them is left out.
const COLUMNS: u16 = 80;
const WIDTH: f32 = COLUMNS as f32 * COLUMN + 16.;
/// Panes narrower than this keep the room for text.
const MIN_PANE: f32 = 520.;

/// The minimap's state in a view: whether it is on, its cached rows and its last layout.
pub(crate) struct Minimap {
    enabled: bool,
    cache: RefCell<Cache>,
    /// Where it was drawn and how, for mouse handling between frames.
    shown: Cell<Option<(Bounds<Pixels>, Geometry)>>,
    /// While the slider is dragged, how far below the slider's top it was grabbed.
    grab: Option<f32>,
}

impl Default for Minimap {
    fn default() -> Self {
        Self {
            enabled: true,
            cache: RefCell::default(),
            shown: Cell::new(None),
            grab: None,
        }
    }
}

/// A run of non-blank characters of one token, in display columns.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Block {
    cols: (u16, u16),
    token: Option<Token>,
}

/// One line's blocks and the column each wrapped row of it starts at.
#[derive(Debug, Default, PartialEq)]
struct LineBlocks {
    blocks: Vec<Block>,
    row_starts: Vec<u16>,
}

/// Lines' blocks, valid for one text version, parse and wrap width.
#[derive(Default)]
struct Cache {
    key: (u64, u64, Option<usize>),
    lines: HashMap<usize, Rc<LineBlocks>>,
}

/// Where the minimap's rows and slider sit for a scroll position, all in rows except pixels noted.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Geometry {
    rows: f32,
    visible: f32,
    height: f32,
    /// The display row drawn at the minimap's top.
    first: f32,
    /// The slider's top and height in pixels.
    slider_top: f32,
    slider_height: f32,
}

impl Geometry {
    /// A document of `rows` display rows, `visible` of them on screen from `scroll`, in a
    /// minimap `height` pixels tall; a document taller than the minimap scrolls in step with it.
    pub(crate) fn new(rows: usize, visible: f32, scroll: f32, height: f32) -> Self {
        let rows = rows.max(1) as f32;
        let slider_height = (visible * PITCH).min(height);
        let ratio = if rows > 1. {
            (scroll / (rows - 1.)).clamp(0., 1.)
        } else {
            0.
        };
        // The editor scrolls until the last row is at the top, so the view can reach past the end.
        let max_first = (rows - 1. + visible - height / PITCH).max(0.);
        let first = (scroll - ratio * (height - slider_height) / PITCH).clamp(0., max_first);
        Self {
            rows,
            visible,
            height,
            first,
            slider_top: (scroll - first) * PITCH,
            slider_height,
        }
    }

    fn row_at(&self, y: f32) -> f32 {
        self.first + y / PITCH
    }

    /// The scroll that puts the slider's top at `top` pixels.
    fn scroll_for_slider(&self, top: f32) -> f32 {
        let scroll = if (self.rows - 1. + self.visible) * PITCH <= self.height {
            top / PITCH
        } else {
            top / (self.height - self.slider_height).max(1.) * (self.rows - 1.)
        };
        scroll.clamp(0., (self.rows - 1.).max(0.))
    }

    fn on_slider(&self, y: f32) -> bool {
        (self.slider_top..self.slider_top + self.slider_height).contains(&y)
    }
}

/// `line`'s non-blank runs by token, in display columns, cut at [`COLUMNS`].
fn line_blocks(
    line: &str,
    tokens: &[(Range<usize>, Token)],
    line_byte: usize,
    wrap: Option<usize>,
) -> LineBlocks {
    let text = line.trim_end_matches(['\n', '\r']);
    let breaks = wrap.map_or(Vec::new(), |cols| wrap_breaks(text, cols));
    let mut out = LineBlocks {
        row_starts: vec![0],
        ..LineBlocks::default()
    };
    let mut token_at: Vec<Option<Token>> = vec![None; text.len()];
    // Later tokens are the inner ones, which win, as the editor draws them.
    for (r, token) in tokens {
        let a = r.start.saturating_sub(line_byte).min(text.len());
        let b = r.end.saturating_sub(line_byte).min(text.len());
        token_at[a..b].fill(Some(*token));
    }
    let mut col = 0usize;
    for (i, (byte, c)) in text.char_indices().enumerate() {
        if breaks.contains(&i) {
            out.row_starts.push(col.min(u16::MAX as usize) as u16);
        }
        let width = if c == '\t' {
            TAB_WIDTH - col % TAB_WIDTH
        } else {
            1
        };
        if !c.is_whitespace() {
            let token = token_at[byte];
            let at = col.min(u16::MAX as usize - 1) as u16;
            match out.blocks.last_mut() {
                Some(b) if b.cols.1 == at && b.token == token => b.cols.1 = at + 1,
                _ => out.blocks.push(Block {
                    cols: (at, at + 1),
                    token,
                }),
            }
        }
        col += width;
    }
    out
}

impl Minimap {
    /// Blocks for `lines`, from the cache or worked out with one highlight query per gap in it.
    fn blocks(
        &self,
        buffer: &Buffer,
        lines: Range<usize>,
        wrap: Option<usize>,
    ) -> Vec<Rc<LineBlocks>> {
        let mut cache = self.cache.borrow_mut();
        let key = (buffer.version(), buffer.parses(), wrap);
        if cache.key != key {
            *cache = Cache {
                key,
                lines: HashMap::new(),
            };
        }
        let missing: Vec<usize> = lines
            .clone()
            .filter(|l| !cache.lines.contains_key(l))
            .collect();
        for run in missing.chunk_by(|a, b| a + 1 == *b) {
            let (a, b) = (run[0], run[run.len() - 1] + 1);
            let tokens = buffer.highlights(a..b);
            for line in a..b {
                let text = buffer.rope().line(line).to_string();
                let byte = buffer.rope().line_to_byte(line);
                let end = byte + text.len();
                let upto = tokens.partition_point(|(r, _)| r.start < end);
                let here: Vec<_> = tokens[..upto]
                    .iter()
                    .filter(|(r, _)| r.end > byte)
                    .cloned()
                    .collect();
                cache
                    .lines
                    .insert(line, Rc::new(line_blocks(&text, &here, byte, wrap)));
            }
        }
        lines.map(|l| cache.lines[&l].clone()).collect()
    }
}

/// The minimap's width in a pane `width` wide: none when it is off or the pane too narrow.
pub(crate) fn width(view: &EditorView, pane: Pixels) -> Pixels {
    if view.minimap.enabled && view.buffer.is_some() && f32::from(pane) >= MIN_PANE {
        px(WIDTH)
    } else {
        px(0.)
    }
}

/// Colours for the minimap: each token's, the slider's and the caret line's.
pub(crate) struct Palette<'a> {
    pub syntax: &'a SyntaxColors,
    pub token: fn(Token, &SyntaxColors) -> Hsla,
    pub background: Hsla,
    pub border: Hsla,
    pub slider: Hsla,
    pub caret: Hsla,
}

/// The minimap's quads at the right edge of `bounds`, for a view scrolled `scroll` rows with
/// `visible` rows on screen.
pub(crate) fn quads(
    view: &EditorView,
    buffer: &Buffer,
    bounds: Bounds<Pixels>,
    scroll: f32,
    visible: f32,
    palette: &Palette,
) -> Vec<PaintQuad> {
    let w = width(view, bounds.size.width);
    if w == px(0.) {
        view.minimap.shown.set(None);
        return Vec::new();
    }
    let area = Bounds::from_corners(
        point(bounds.right() - w, bounds.top()),
        bounds.bottom_right(),
    );
    let display = &view.display;
    let total = buffer.len_lines();
    let rows = display.row_count(total);
    let height = f32::from(area.size.height);
    let g = Geometry::new(rows, visible, scroll, height);
    view.minimap.shown.set(Some((area, g)));

    let mut out = vec![
        fill(area, palette.background),
        fill(
            Bounds::new(area.origin, size(px(1.), area.size.height)),
            palette.border,
        ),
    ];
    let first_row = g.first.floor() as usize;
    let last_row = ((g.first + height / PITCH).ceil() as usize).min(rows);
    if first_row >= last_row {
        return out;
    }
    let first_line = display.line_of(first_row);
    let last_line = display.line_of(last_row - 1);
    let blocks = view
        .minimap
        .blocks(buffer, first_line..last_line + 1, display.wrap_cols());
    let caret_line = buffer.line_of(view.cursor.head());
    let left = area.left() + px(8.);
    for row in first_row..last_row {
        let line = display.line_of(row);
        let sub = row - display.row_of(line);
        let lb = &blocks[line - first_line];
        let y = area.top() + px((row as f32 - g.first) * PITCH);
        if line == caret_line {
            out.push(fill(
                Bounds::new(point(area.left() + px(1.), y), size(w - px(1.), px(PITCH))),
                palette.caret,
            ));
        }
        let start = lb.row_starts.get(sub).copied().unwrap_or(u16::MAX);
        let end = lb.row_starts.get(sub + 1).copied().unwrap_or(u16::MAX);
        for b in lb
            .blocks
            .iter()
            .filter(|b| b.cols.1 > start && b.cols.0 < end)
        {
            let a = b.cols.0.max(start) - start;
            let z = (b.cols.1.min(end) - start).min(COLUMNS);
            if a >= z {
                continue;
            }
            let color = match b.token {
                Some(t) => (palette.token)(t, palette.syntax),
                None => palette.syntax.text,
            };
            out.push(fill(
                Bounds::new(
                    point(left + px(a as f32 * COLUMN), y),
                    size(px((z - a) as f32 * COLUMN), px(PITCH)),
                ),
                color.opacity(0.65),
            ));
        }
    }
    out.push(fill(
        Bounds::new(
            point(area.left() + px(1.), area.top() + px(g.slider_top)),
            size(w - px(1.), px(g.slider_height)),
        ),
        palette.slider,
    ));
    out
}

impl EditorView {
    /// Turns the minimap on or off, as `editor.minimap.enabled` says.
    pub fn set_minimap(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.minimap.enabled != on {
            self.minimap.enabled = on;
            self.minimap.shown.set(None);
            cx.notify();
        }
    }

    /// A press on the minimap: on the slider it starts a drag; elsewhere it centres that row
    /// and drags from there, as VS Code does.
    pub(crate) fn click_minimap(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) -> bool {
        let Some((area, g)) = self.minimap.shown.get() else {
            return false;
        };
        if event.button != MouseButton::Left || !area.contains(&event.position) {
            return false;
        }
        let y = f32::from(event.position.y - area.top());
        if g.on_slider(y) {
            self.minimap.grab = Some(y - g.slider_top);
        } else {
            let scroll = (g.row_at(y) - g.visible / 2.).clamp(0., (g.rows - 1.).max(0.));
            self.scroll_rows_to(scroll, cx);
            self.minimap.grab = Some(g.slider_height / 2.);
        }
        true
    }

    /// Follows the pointer while the slider is dragged; false once no drag is under way.
    pub(crate) fn drag_minimap(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) -> bool {
        let Some(grab) = self.minimap.grab else {
            return false;
        };
        let Some((area, g)) = self.minimap.shown.get() else {
            self.minimap.grab = None;
            return false;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.minimap.grab = None;
            return false;
        }
        let top = f32::from(event.position.y - area.top()) - grab;
        self.scroll_rows_to(g.scroll_for_slider(top), cx);
        true
    }

    fn scroll_rows_to(&mut self, rows: f32, cx: &mut Context<Self>) {
        let lh = self.layout.as_ref().map_or(px(20.), |l| l.line_height);
        self.scroll.y = rows * f32::from(lh);
        self.autoscroll = false;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_short_document_sits_at_the_top_with_the_slider_over_the_visible_rows() {
        let g = Geometry::new(100, 40., 10., 600.);
        assert_eq!(g.first, 0.);
        assert_eq!((g.slider_top, g.slider_height), (20., 80.));
        assert_eq!(g.scroll_for_slider(20.), 10.);
    }

    #[test]
    fn a_long_document_scrolls_the_minimap_so_the_slider_spans_top_to_bottom() {
        let (rows, visible, height) = (10_000, 50., 600.);
        let top = Geometry::new(rows, visible, 0., height);
        assert_eq!((top.first, top.slider_top), (0., 0.));
        let end = Geometry::new(rows, visible, rows as f32 - 1., height);
        assert_eq!(
            end.slider_top + end.slider_height,
            height,
            "slider at the bottom"
        );
        assert!(end.first + height / PITCH >= rows as f32, "last rows drawn");
        let mid = Geometry::new(rows, visible, 5000., height);
        assert!(
            (mid.row_at(mid.slider_top) - 5000.).abs() < 0.01,
            "slider over the view"
        );
        assert!((mid.scroll_for_slider(mid.slider_top) - 5000.).abs() < 0.5);
    }

    #[test]
    fn a_slightly_long_document_never_draws_above_its_first_row() {
        for scroll in [0., 10., 150., 309.] {
            let g = Geometry::new(310, 40., scroll, 600.);
            assert!(g.first >= 0.);
            assert!((g.row_at(g.slider_top) - scroll).abs() < 0.01);
        }
    }

    #[test]
    fn blocks_follow_tokens_skip_blanks_expand_tabs_and_start_wrapped_rows() {
        let tokens = vec![(10..13, Token::Keyword), (14..15, Token::Variable)];
        let lb = line_blocks("\tlet x = 1;\n", &tokens, 9, None);
        let cols: Vec<_> = lb.blocks.iter().map(|b| (b.cols, b.token)).collect();
        assert_eq!(
            cols,
            [
                ((4, 7), Some(Token::Keyword)),
                ((8, 9), Some(Token::Variable)),
                ((10, 11), None),
                ((12, 14), None),
            ]
        );
        let wrapped = line_blocks("aaaa bbbb cccc", &[], 0, Some(5));
        assert_eq!(wrapped.row_starts, [0, 5, 10]);
    }

    /// 10k lines of Rust: a frame from the cache, and one after an edit that misses it.
    #[test]
    fn a_frame_on_a_10k_line_file_stays_within_budget() {
        let text: String = (0..2000)
            .map(|i| {
                format!("fn f{i}(x: u32) -> u32 {{\n    let y = x * {i};\n    // note\n    y\n}}\n")
            })
            .collect();
        let mut b = Buffer::new(&text, Some("/x/a.rs".into()));
        assert!(b.len_lines() >= 10_000);
        let m = Minimap::default();
        let lines = 5000..5300;
        m.blocks(&b, lines.clone(), None);
        let t = Instant::now();
        for _ in 0..100 {
            m.blocks(&b, lines.clone(), None);
        }
        let cached = t.elapsed() / 100;
        b.insert(&mut crate::Cursor::at(b.line_start(5100)), "x");
        let t = Instant::now();
        m.blocks(&b, lines.clone(), None);
        let missed = t.elapsed();
        // Debug builds run several times slower than release, where the budget is 1 ms.
        let budget = if cfg!(debug_assertions) { 20. } else { 1. };
        assert!(
            cached.as_secs_f64() * 1e3 < budget / 10.,
            "cached {cached:?}"
        );
        assert!(
            missed.as_secs_f64() * 1e3 < budget * 3.,
            "after an edit {missed:?}"
        );
    }
}
