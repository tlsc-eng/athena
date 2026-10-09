use std::rc::Rc;
use std::time::Instant;

use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point as GridPoint};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use athena_ui::{ActiveTheme, TerminalColors};
use gpui::{
    App, BorderStyle, Bounds, ContentMask, Element, ElementId, ElementInputHandler, Entity, Font,
    FontFallbacks, FontFeatures, FontStyle, FontWeight, GlobalElementId, Hsla, InspectorElementId,
    IntoElement, LayoutId, PaintQuad, Pixels, ShapedLine, SharedString, StrikethroughStyle, Style,
    TextRun, UnderlineStyle, Window, WindowTextSystem, fill, outline, point, px, relative, size,
};

use crate::colors;
use crate::glyphs;
use crate::marks;
use crate::search::Span;
use crate::terminal::{Damage, GridSize, Link, Terminal};
use crate::view::TerminalView;

const LINE_HEIGHT_RATIO: f32 = 1.4;

/// Logs each prepaint, its duration and the rows it rebuilt under `athena::render` when dropped.
struct PrepaintTimer {
    started: Instant,
    rebuilt: usize,
}

impl Drop for PrepaintTimer {
    fn drop(&mut self) {
        let us = self.started.elapsed().as_micros() as u64;
        let rebuilt = self.rebuilt;
        tracing::trace!(target: "athena::render", us, rebuilt, "terminal prepaint");
    }
}

pub struct TerminalElement {
    view: Entity<TerminalView>,
    focused: bool,
}

impl TerminalElement {
    pub fn new(view: Entity<TerminalView>, focused: bool) -> Self {
        Self { view, focused }
    }
}

pub struct Frame {
    origin: gpui::Point<Pixels>,
    rows: Vec<Rc<CachedRow>>,
    /// Search highlights, positioned like row quads.
    matches: Vec<PaintQuad>,
    link: Option<PaintQuad>,
    cursor: Option<PaintQuad>,
    cursor_glyph: Option<(gpui::Point<Pixels>, ShapedLine)>,
    cursor_bounds: Option<Bounds<Pixels>>,
    marked: Option<(PaintQuad, gpui::Point<Pixels>, ShapedLine)>,
    /// Dots left of prompt lines the shell marked, coloured by how the command ended.
    prompts: Vec<PaintQuad>,
    line_height: Pixels,
}

/// One row's quads and text, positioned relative to the grid's top-left corner.
#[derive(Default)]
struct CachedRow {
    backgrounds: Vec<PaintQuad>,
    glyphs: Vec<PaintQuad>,
    text: Vec<(gpui::Point<Pixels>, ShapedLine)>,
}

/// Everything besides cell contents that changes how rows look.
#[derive(Clone, PartialEq)]
struct RowKey {
    cols: usize,
    rows: usize,
    cell_width: Pixels,
    line_height: Pixels,
    font_size: Pixels,
    font: SharedString,
    palette: [Hsla; 23],
    display_offset: usize,
    selection: Option<SelectionRange>,
    focused: bool,
    hovered_link: Option<Link>,
}

/// Rows built by earlier prepaints, kept until the terminal reports them damaged.
#[derive(Default)]
pub struct RowCache {
    key: Option<RowKey>,
    rows: Vec<Option<Rc<CachedRow>>>,
}

impl RowCache {
    /// Drops the rows `damage` covers, or every row if anything in `key` changed, and returns
    /// the rows that need building.
    fn invalidate(&mut self, key: RowKey, damage: Damage) -> Vec<usize> {
        // Scrolled back, every new line shifts the view, so damage is not worth mapping.
        let full =
            damage == Damage::Full || key.display_offset != 0 || self.key.as_ref() != Some(&key);
        if full {
            self.rows = vec![None; key.rows];
        } else if let Damage::Rows(rows) = damage {
            for row in rows {
                if let Some(slot) = self.rows.get_mut(row) {
                    *slot = None;
                }
            }
        }
        self.key = Some(key);
        (0..self.rows.len())
            .filter(|&row| self.rows[row].is_none())
            .collect()
    }
}

/// Edge of a prompt dot, centred in the padding left of the grid.
const PROMPT_DOT: f32 = 6.;
/// Padding left of the grid, wide enough for a prompt dot with room on both sides.
pub(crate) const PROMPT_GUTTER: f32 = 14.;

/// A dot beside each visible prompt line: filled once its command finished (success or failure
/// colour), an outline while it runs, as VS Code marks commands.
fn prompt_dots(
    terminal: &Terminal,
    origin: gpui::Point<Pixels>,
    line_height: Pixels,
    (success, failure, running): (Hsla, Hsla, Hsla),
) -> Vec<PaintQuad> {
    let term = terminal.term();
    let offset = term.grid().display_offset() as i32;
    (0..term.screen_lines())
        .filter_map(|row| {
            let command = terminal.command_at(Line(row as i32 - offset))?;
            let at = origin
                + point(
                    px(-(PROMPT_GUTTER + PROMPT_DOT) / 2.),
                    line_height * row as f32 + (line_height - px(PROMPT_DOT)) / 2.,
                );
            let dot = Bounds::new(at, size(px(PROMPT_DOT), px(PROMPT_DOT)));
            let round = |quad: PaintQuad| quad.corner_radii(px(PROMPT_DOT / 2.));
            Some(match command.exit {
                Some(Some(0) | None) => round(fill(dot, success)),
                Some(Some(_)) => round(fill(dot, failure)),
                None => round(outline(dot, running, BorderStyle::Solid)),
            })
        })
        .collect()
}

/// Search highlights for one frame; drawn over the cached rows, so they never invalidate them.
fn match_quads(
    spans: &[Span],
    cell_width: Pixels,
    line_height: Pixels,
    color: Hsla,
) -> Vec<PaintQuad> {
    spans
        .iter()
        .map(|span| {
            let bounds = Bounds::new(
                point(
                    cell_width * span.start as f32,
                    line_height * span.row as f32,
                ),
                size(cell_width * (span.end - span.start) as f32, line_height),
            );
            if span.current {
                fill(bounds, color.opacity(0.55))
                    .border_widths(px(1.))
                    .border_color(color)
            } else {
                fill(bounds, color.opacity(0.25))
            }
        })
        .collect()
}

fn palette_key(p: &TerminalColors) -> [Hsla; 23] {
    let mut key = [p.foreground; 23];
    key[1..7].copy_from_slice(&[
        p.bright_foreground,
        p.dim_foreground,
        p.background,
        p.cursor,
        p.cursor_text,
        p.selection,
    ]);
    key[7..].copy_from_slice(&p.ansi);
    key
}

#[derive(Clone, Copy, PartialEq)]
enum Underline {
    Single,
    Double,
    Curly,
}

#[derive(Clone, Copy, PartialEq)]
struct CellStyle {
    fg: Hsla,
    bold: bool,
    underline: Option<Underline>,
    underline_color: Option<Hsla>,
    strike: bool,
}

struct Run {
    col: usize,
    cells: usize,
    text: String,
    style: CellStyle,
    grid_aligned: bool,
}

/// Turns one viewport row of the grid into quads and shaped text.
struct RowBuilder<'a> {
    grid: &'a Grid<Cell>,
    cols: usize,
    offset: i32,
    selection: Option<SelectionRange>,
    overrides: &'a Colors,
    palette: &'a TerminalColors,
    cell_width: Pixels,
    line_height: Pixels,
    font_size: Pixels,
    text_system: &'a WindowTextSystem,
    text_run: &'a dyn Fn(usize, CellStyle) -> TextRun,
}

impl RowBuilder<'_> {
    fn build(&self, row: usize) -> CachedRow {
        let palette = self.palette;
        let (cell_width, line_height) = (self.cell_width, self.line_height);
        let at = |col: usize| point(cell_width * col as f32, line_height * row as f32);
        let mut out = CachedRow::default();
        let mut runs: Vec<Run> = Vec::new();
        let mut bg_run: Option<(usize, usize, Hsla)> = None;
        let push_bg = |run: Option<(usize, usize, Hsla)>, out: &mut Vec<PaintQuad>| {
            if let Some((start, end, color)) = run {
                let b = Bounds::new(
                    at(start),
                    size(cell_width * (end - start) as f32, line_height),
                );
                out.push(fill(b, color));
            }
        };

        let line = Line(row as i32 - self.offset);
        let cells = &self.grid[line];
        for col in 0..self.cols {
            let cell = &cells[Column(col)];
            let flags = cell.flags;

            let mut fg_color = cell.fg;
            if flags.contains(Flags::BOLD) {
                fg_color = match fg_color {
                    Color::Named(name) if (name as usize) < 8 || name == NamedColor::Foreground => {
                        Color::Named(name.to_bright())
                    }
                    Color::Indexed(i) if i < 8 => Color::Indexed(i + 8),
                    other => other,
                };
            }
            let mut fg = colors::resolve(fg_color, self.overrides, palette);
            let mut bg = colors::resolve(cell.bg, self.overrides, palette);
            if flags.contains(Flags::DIM) {
                fg = colors::dim(fg, palette);
            }
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if flags.contains(Flags::HIDDEN) {
                fg = bg;
            }
            if self
                .selection
                .is_some_and(|s| s.contains(GridPoint::new(line, Column(col))))
            {
                bg = palette.selection;
            }

            match &mut bg_run {
                Some((_, end, color)) if *end == col && *color == bg => *end = col + 1,
                _ => {
                    push_bg(bg_run.take(), &mut out.backgrounds);
                    if bg != palette.background {
                        bg_run = Some((col, col + 1, bg));
                    }
                }
            }

            if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let underline = if flags.contains(Flags::UNDERCURL) {
                Some(Underline::Curly)
            } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
                Some(Underline::Double)
            } else if flags.intersects(Flags::ALL_UNDERLINES) {
                Some(Underline::Single)
            } else {
                None
            };
            let style = CellStyle {
                fg,
                bold: flags.contains(Flags::BOLD),
                underline,
                underline_color: underline
                    .and(cell.underline_color())
                    .map(|c| colors::resolve(c, self.overrides, palette)),
                strike: flags.contains(Flags::STRIKEOUT),
            };
            if cell.c == ' ' && style.underline.is_none() && !style.strike {
                continue;
            }
            if style.underline.is_none()
                && !style.strike
                && let Some(quads) = glyphs::quads(
                    cell.c,
                    Bounds::new(at(col), size(cell_width, line_height)),
                    fg,
                )
            {
                out.glyphs.extend(quads);
                continue;
            }
            // Every single-width glyph snaps to its cell, so fallback-font symbols keep the grid.
            let mut extra = cell
                .zerowidth()
                .into_iter()
                .flatten()
                .copied()
                .filter(|&c| !marks::is_athenas(c))
                .peekable();
            let simple = !flags.contains(Flags::WIDE_CHAR) && extra.peek().is_none();
            if simple
                && let Some(run) = runs.last_mut()
                && run.grid_aligned
                && run.col + run.cells == col
                && run.style == style
            {
                run.text.push(cell.c);
                run.cells += 1;
                continue;
            }
            let mut text = String::from(cell.c);
            text.extend(extra);
            runs.push(Run {
                col,
                cells: if flags.contains(Flags::WIDE_CHAR) {
                    2
                } else {
                    1
                },
                text,
                style,
                grid_aligned: simple,
            });
        }
        push_bg(bg_run.take(), &mut out.backgrounds);

        for run in runs {
            let force = run.grid_aligned.then_some(cell_width);
            let text_runs = [(self.text_run)(run.text.len(), run.style)];
            let shaped = self.text_system.shape_line(
                SharedString::from(run.text),
                self.font_size,
                &text_runs,
                force,
            );
            out.text.push((at(run.col), shaped));
        }
        out
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = Frame;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let mut timer = PrepaintTimer {
            started: Instant::now(),
            rebuilt: 0,
        };
        let theme = cx.theme();
        let palette = theme.terminal.clone();
        let match_color = theme.color.warning;
        let prompt_colors = (
            theme.color.success,
            theme.color.danger,
            theme.color.content_muted,
        );
        let font_size = theme.typography.code;
        let font = Font {
            family: theme.typography.mono.clone(),
            // Ligatures would merge glyphs and break the one-glyph-per-cell grid.
            features: FontFeatures::disable_ligatures(),
            // Geist Mono lacks most symbols TUIs draw; these hold box drawing, braille and emoji.
            fallbacks: Some(FontFallbacks::from_fonts(vec![
                "Menlo".into(),
                "Apple Symbols".into(),
                "Apple Color Emoji".into(),
            ])),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        };
        let bold_font = Font {
            weight: FontWeight::SEMIBOLD,
            ..font.clone()
        };

        let text_system = window.text_system().clone();
        let font_id = text_system.resolve_font(&font);
        let cell_width = text_system
            .advance(font_id, font_size, 'm')
            .map(|s| s.width)
            .unwrap_or(px(8.));
        let line_height = px((f32::from(font_size) * LINE_HEIGHT_RATIO).round());

        let grid = GridSize {
            cols: ((bounds.size.width / cell_width).floor() as u16).max(2),
            rows: ((bounds.size.height / line_height).floor() as u16).max(1),
            cell_width: cell_width.into(),
            cell_height: line_height.into(),
        };
        // Damage is read after the resize, which damages everything itself.
        let (damage, mut cache) = self.view.update(cx, |view, cx| {
            view.resize(grid, cx);
            let damage = view.terminal_mut().map(Terminal::take_damage);
            (damage, std::mem::take(&mut view.rows))
        });

        let view = self.view.read(cx);
        let mut frame = Frame {
            origin: bounds.origin,
            rows: Vec::new(),
            matches: Vec::new(),
            link: None,
            cursor: None,
            cursor_glyph: None,
            cursor_bounds: None,
            marked: None,
            prompts: Vec::new(),
            line_height,
        };
        let (Some(terminal), Some(damage)) = (view.terminal(), damage) else {
            self.view.update(cx, |view, _| view.rows = cache);
            return frame;
        };
        let term = terminal.term();
        let content = term.renderable_content();
        let offset = content.display_offset as i32;
        let origin = |col: usize, row: usize| {
            bounds.origin + point(cell_width * col as f32, line_height * row as f32)
        };
        let text_run = |len: usize, style: CellStyle| TextRun {
            len,
            font: if style.bold {
                bold_font.clone()
            } else {
                font.clone()
            },
            color: style.fg,
            background_color: None,
            underline: style.underline.map(|kind| UnderlineStyle {
                thickness: px(if kind == Underline::Double { 2. } else { 1. }),
                color: Some(style.underline_color.unwrap_or(style.fg)),
                wavy: kind == Underline::Curly,
            }),
            strikethrough: style.strike.then(|| StrikethroughStyle {
                thickness: px(1.),
                color: Some(style.fg),
            }),
        };

        let key = RowKey {
            cols: term.columns(),
            rows: term.screen_lines(),
            cell_width,
            line_height,
            font_size,
            font: font.family.clone(),
            palette: palette_key(&palette),
            display_offset: content.display_offset,
            selection: content.selection,
            focused: self.focused,
            hovered_link: view.hovered_link.clone(),
        };
        let stale = cache.invalidate(key, damage);
        timer.rebuilt = stale.len();
        let builder = RowBuilder {
            grid: term.grid(),
            cols: term.columns(),
            offset,
            selection: content.selection,
            overrides: content.colors,
            palette: &palette,
            cell_width,
            line_height,
            font_size,
            text_system: &text_system,
            text_run: &text_run,
        };
        for row in stale {
            cache.rows[row] = Some(Rc::new(builder.build(row)));
        }
        frame.rows = cache.rows.iter().flatten().cloned().collect();
        let spans = view.search_spans(content.display_offset, term.screen_lines(), term.columns());
        frame.matches = match_quads(&spans, cell_width, line_height, match_color);
        frame.prompts = prompt_dots(terminal, bounds.origin, line_height, prompt_colors);

        if let Some(link) = &view.hovered_link {
            let at = origin(link.start, link.row);
            let underline = Bounds::new(
                at + point(px(0.), line_height - px(2.)),
                size(cell_width * (link.end - link.start) as f32, px(1.)),
            );
            frame.link = Some(fill(underline, palette.foreground));
        }

        let cursor = content.cursor;
        let cursor_row = cursor.point.line.0 + offset;
        if cursor.shape != CursorShape::Hidden && (0..grid.rows as i32).contains(&cursor_row) {
            let row = cursor_row as usize;
            let col = cursor.point.column.0;
            let cell = &term.grid()[GridPoint::new(cursor.point.line, cursor.point.column)];
            let width = if cell.flags.contains(Flags::WIDE_CHAR) {
                2.
            } else {
                1.
            };
            let block = Bounds::new(origin(col, row), size(cell_width * width, line_height));
            frame.cursor_bounds = Some(block);
            frame.cursor = Some(match cursor.shape {
                CursorShape::Beam => fill(
                    Bounds::new(block.origin, size(px(2.), line_height)),
                    palette.cursor,
                ),
                CursorShape::Underline => fill(
                    Bounds::new(
                        block.origin + point(px(0.), line_height - px(2.)),
                        size(block.size.width, px(2.)),
                    ),
                    palette.cursor,
                ),
                CursorShape::Block if self.focused => fill(block, palette.cursor),
                _ => outline(block, palette.cursor, BorderStyle::Solid),
            });
            if cursor.shape == CursorShape::Block && self.focused && cell.c != ' ' {
                let style = CellStyle {
                    fg: palette.cursor_text,
                    bold: cell.flags.contains(Flags::BOLD),
                    underline: None,
                    underline_color: None,
                    strike: false,
                };
                let runs = [text_run(cell.c.len_utf8(), style)];
                let shaped =
                    text_system.shape_line(cell.c.to_string().into(), font_size, &runs, None);
                frame.cursor_glyph = Some((block.origin, shaped));
            }
        }

        if !view.marked.is_empty()
            && let Some(at) = frame.cursor_bounds
        {
            let style = CellStyle {
                fg: palette.foreground,
                bold: false,
                underline: Some(Underline::Single),
                underline_color: None,
                strike: false,
            };
            let runs = [text_run(view.marked.len(), style)];
            let shaped = text_system.shape_line(view.marked.clone().into(), font_size, &runs, None);
            let backdrop = fill(
                Bounds::new(at.origin, size(shaped.width, line_height)),
                palette.background,
            );
            frame.marked = Some((backdrop, at.origin, shaped));
            frame.cursor = None;
            frame.cursor_glyph = None;
        }
        self.view.update(cx, |view, _| view.rows = cache);
        frame
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        frame: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.view.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );
        let line_height = frame.line_height;
        let origin = frame.origin;
        let shifted = |quad: &PaintQuad| PaintQuad {
            bounds: quad.bounds + origin,
            ..quad.clone()
        };
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for row in &frame.rows {
                for quad in &row.backgrounds {
                    window.paint_quad(shifted(quad));
                }
            }
            for quad in &frame.matches {
                window.paint_quad(shifted(quad));
            }
            if let Some(link) = frame.link.take() {
                window.paint_quad(link);
            }
            for row in &frame.rows {
                for quad in &row.glyphs {
                    window.paint_quad(shifted(quad));
                }
            }
            for row in &frame.rows {
                for (at, line) in &row.text {
                    let _ = line.paint(origin + *at, line_height, window, cx);
                }
            }
            if let Some(cursor) = frame.cursor.take() {
                window.paint_quad(cursor);
            }
            if let Some((origin, glyph)) = &frame.cursor_glyph {
                let _ = glyph.paint(*origin, line_height, window, cx);
            }
            if let Some((backdrop, origin, line)) = frame.marked.take() {
                window.paint_quad(backdrop);
                let _ = line.paint(origin, line_height, window, cx);
            }
        });
        // The dots sit in the padding left of the grid, outside its content mask.
        for dot in frame.prompts.drain(..) {
            window.paint_quad(dot);
        }
        let cursor_bounds = frame.cursor_bounds;
        self.view.update(cx, |view, _| {
            view.cursor_bounds = cursor_bounds;
            view.origin = bounds.origin;
        });
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::index::{Column, Line, Point};
    use athena_ui::Theme;

    use super::*;

    fn key() -> RowKey {
        RowKey {
            cols: 80,
            rows: 4,
            cell_width: px(8.),
            line_height: px(18.),
            font_size: px(13.),
            font: "Geist Mono".into(),
            palette: palette_key(&Theme::dark(false).terminal),
            display_offset: 0,
            selection: None,
            focused: true,
            hovered_link: None,
        }
    }

    /// A cache whose rows were all built for `key`.
    fn warm(key: RowKey) -> RowCache {
        let mut cache = RowCache::default();
        for row in cache.invalidate(key, Damage::Full) {
            cache.rows[row] = Some(Rc::default());
        }
        cache
    }

    fn rebuilt(cache: &mut RowCache, key: RowKey, damage: Damage) -> Vec<usize> {
        let stale = cache.invalidate(key, damage);
        for &row in &stale {
            cache.rows[row] = Some(Rc::default());
        }
        stale
    }

    #[test]
    fn the_first_frame_builds_every_row() {
        let mut cache = RowCache::default();
        assert_eq!(cache.invalidate(key(), Damage::Rows(vec![])), [0, 1, 2, 3]);
    }

    #[test]
    fn an_unchanged_frame_rebuilds_only_damaged_rows() {
        let mut cache = warm(key());
        assert!(rebuilt(&mut cache, key(), Damage::Rows(vec![])).is_empty());
        assert_eq!(rebuilt(&mut cache, key(), Damage::Rows(vec![2, 0])), [0, 2]);
        assert!(
            rebuilt(&mut cache, key(), Damage::Rows(vec![9])).is_empty(),
            "rows past the screen are ignored"
        );
    }

    #[test]
    fn full_damage_rebuilds_everything() {
        let mut cache = warm(key());
        assert_eq!(rebuilt(&mut cache, key(), Damage::Full), [0, 1, 2, 3]);
    }

    #[test]
    fn any_change_besides_cell_contents_rebuilds_everything() {
        let palette = {
            let mut p = Theme::dark(false).terminal;
            p.ansi[3] = p.ansi[4];
            palette_key(&p)
        };
        let selection = SelectionRange::new(
            Point::new(Line(0), Column(1)),
            Point::new(Line(1), Column(3)),
            false,
        );
        let link = Link {
            row: 1,
            start: 0,
            end: 4,
            uri: "https://tlsc.io".into(),
        };
        let changes: Vec<(&str, RowKey)> = vec![
            ("resize", RowKey { rows: 5, ..key() }),
            ("columns", RowKey { cols: 81, ..key() }),
            (
                "cell width",
                RowKey {
                    cell_width: px(9.),
                    ..key()
                },
            ),
            (
                "line height",
                RowKey {
                    line_height: px(20.),
                    ..key()
                },
            ),
            (
                "font size",
                RowKey {
                    font_size: px(14.),
                    ..key()
                },
            ),
            (
                "font",
                RowKey {
                    font: "Menlo".into(),
                    ..key()
                },
            ),
            ("palette", RowKey { palette, ..key() }),
            (
                "scrollback",
                RowKey {
                    display_offset: 3,
                    ..key()
                },
            ),
            (
                "selection",
                RowKey {
                    selection: Some(selection),
                    ..key()
                },
            ),
            (
                "focus",
                RowKey {
                    focused: false,
                    ..key()
                },
            ),
            (
                "hovered link",
                RowKey {
                    hovered_link: Some(link),
                    ..key()
                },
            ),
        ];
        for (what, changed) in changes {
            let mut cache = warm(key());
            let rows = changed.rows;
            assert_eq!(
                rebuilt(&mut cache, changed.clone(), Damage::Rows(vec![])).len(),
                rows,
                "{what} changed"
            );
            assert_eq!(
                rebuilt(&mut cache, key(), Damage::Rows(vec![])).len(),
                4,
                "{what} changed back"
            );
        }
    }

    #[test]
    fn search_highlights_draw_over_cached_rows_without_rebuilding_them() {
        let mut cache = warm(key());
        let spans = [
            Span {
                row: 1,
                start: 2,
                end: 5,
                current: false,
            },
            Span {
                row: 3,
                start: 0,
                end: 1,
                current: true,
            },
        ];
        let color = Theme::dark(false).color.warning;
        let quads = match_quads(&spans, px(8.), px(18.), color);
        assert_eq!(
            quads[0].bounds,
            Bounds::new(point(px(16.), px(18.)), size(px(24.), px(18.)))
        );
        assert_eq!(quads[0].border_widths, Default::default());
        assert_eq!(
            quads[1].border_color, color,
            "the current match is outlined"
        );
        assert!(
            rebuilt(&mut cache, key(), Damage::Rows(vec![])).is_empty(),
            "highlights are not part of the row key"
        );
    }

    #[test]
    fn scrolled_back_rebuilds_every_frame() {
        let back = RowKey {
            display_offset: 10,
            ..key()
        };
        let mut cache = warm(back.clone());
        assert_eq!(rebuilt(&mut cache, back, Damage::Rows(vec![])).len(), 4);
    }

    struct Silent;

    impl crate::terminal::Transport for Silent {
        fn write(&self, _: Vec<u8>) {}
        fn resize(&self, _: u16, _: u16) {}
    }

    #[test]
    fn prompt_lines_get_a_dot_in_the_padding_coloured_by_their_exit() {
        let size = GridSize {
            cols: 20,
            rows: 6,
            cell_width: 8.,
            cell_height: 18.,
        };
        let mut terminal = Terminal::new(size, Box::new(Silent));
        let palette = Theme::dark(false).terminal;
        for bytes in [
            &b"\x1b]133;A\x07$ \x1b]133;B\x07ok\r\n\x1b]133;C\x07\x1b]133;D;0\x07"[..],
            b"\x1b]133;A\x07$ \x1b]133;B\x07no\r\n\x1b]133;C\x07\x1b]133;D;1\x07",
            b"\x1b]133;A\x07$ \x1b]133;B\x07",
        ] {
            terminal.handle(crate::terminal::PaneEvent::Output(bytes.to_vec()), &palette);
        }
        let (ok, bad, running) = (gpui::green(), gpui::red(), gpui::blue());
        let origin = point(px(100.), px(50.));
        let dots = prompt_dots(&terminal, origin, px(18.), (ok, bad, running));
        assert_eq!(dots.len(), 3);
        assert_eq!(dots[0].background, ok.into());
        assert_eq!(dots[1].background, bad.into());
        assert_eq!(dots[2].border_color, running);
        // As far from the first character as from the pane's edge, PROMPT_GUTTER left of it.
        for dot in &dots {
            assert_eq!(origin.x - dot.bounds.right(), px(4.));
            assert_eq!(dot.bounds.left() - (origin.x - px(PROMPT_GUTTER)), px(4.));
        }
        assert_eq!(dots[1].bounds.top(), origin.y + px(18. + 6.));
    }
}
