use alacritty_terminal::index::Point as GridPoint;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use athena_ui::ActiveTheme;
use gpui::{
    App, BorderStyle, Bounds, ContentMask, Element, ElementId, ElementInputHandler, Entity, Font,
    FontFallbacks, FontFeatures, FontStyle, FontWeight, GlobalElementId, Hsla, InspectorElementId,
    IntoElement, LayoutId, PaintQuad, Pixels, ShapedLine, SharedString, StrikethroughStyle, Style,
    TextRun, UnderlineStyle, Window, fill, outline, point, px, relative, size,
};

use crate::colors;
use crate::glyphs;
use crate::terminal::GridSize;
use crate::view::TerminalView;

const LINE_HEIGHT_RATIO: f32 = 1.4;

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
    backgrounds: Vec<PaintQuad>,
    glyphs: Vec<PaintQuad>,
    text: Vec<(gpui::Point<Pixels>, ShapedLine)>,
    cursor: Option<PaintQuad>,
    cursor_glyph: Option<(gpui::Point<Pixels>, ShapedLine)>,
    cursor_bounds: Option<Bounds<Pixels>>,
    marked: Option<(PaintQuad, gpui::Point<Pixels>, ShapedLine)>,
    line_height: Pixels,
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
    row: usize,
    col: usize,
    cells: usize,
    text: String,
    style: CellStyle,
    grid_aligned: bool,
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
        let theme = cx.theme();
        let palette = theme.terminal.clone();
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
        self.view.update(cx, |view, _| view.resize(grid));

        let view = self.view.read(cx);
        let mut frame = Frame {
            backgrounds: Vec::new(),
            glyphs: Vec::new(),
            text: Vec::new(),
            cursor: None,
            cursor_glyph: None,
            cursor_bounds: None,
            marked: None,
            line_height,
        };
        let Some(terminal) = view.terminal() else {
            return frame;
        };
        let term = terminal.term();
        let content = term.renderable_content();
        let overrides = content.colors;
        let offset = content.display_offset as i32;
        let origin = |col: usize, row: usize| {
            bounds.origin + point(cell_width * col as f32, line_height * row as f32)
        };

        let mut runs: Vec<Run> = Vec::new();
        let mut bg_run: Option<(usize, usize, usize, Hsla)> = None;
        let push_bg = |run: Option<(usize, usize, usize, Hsla)>, out: &mut Vec<PaintQuad>| {
            if let Some((row, start, end, color)) = run {
                let b = Bounds::new(
                    origin(start, row),
                    size(cell_width * (end - start) as f32, line_height),
                );
                out.push(fill(b, color));
            }
        };

        for cell in content.display_iter {
            let row = (cell.point.line.0 + offset) as usize;
            let col = cell.point.column.0;
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
            let mut fg = colors::resolve(fg_color, overrides, &palette);
            let mut bg = colors::resolve(cell.bg, overrides, &palette);
            if flags.contains(Flags::DIM) {
                fg = colors::dim(fg, &palette);
            }
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if flags.contains(Flags::HIDDEN) {
                fg = bg;
            }
            if content.selection.is_some_and(|s| s.contains(cell.point)) {
                bg = palette.selection;
            }

            match &mut bg_run {
                Some((r, _, end, color)) if *r == row && *end == col && *color == bg => {
                    *end = col + 1
                }
                _ => {
                    push_bg(bg_run.take(), &mut frame.backgrounds);
                    if bg != palette.background {
                        bg_run = Some((row, col, col + 1, bg));
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
                    .map(|c| colors::resolve(c, overrides, &palette)),
                strike: flags.contains(Flags::STRIKEOUT),
            };
            if cell.c == ' ' && style.underline.is_none() && !style.strike {
                continue;
            }
            if style.underline.is_none()
                && !style.strike
                && let Some(quads) = glyphs::quads(
                    cell.c,
                    Bounds::new(origin(col, row), size(cell_width, line_height)),
                    fg,
                )
            {
                frame.glyphs.extend(quads);
                continue;
            }
            // Every single-width glyph snaps to its cell, so fallback-font symbols keep the grid.
            let simple = !flags.contains(Flags::WIDE_CHAR) && cell.zerowidth().is_none();
            if simple
                && let Some(run) = runs.last_mut()
                && run.grid_aligned
                && run.row == row
                && run.col + run.cells == col
                && run.style == style
            {
                run.text.push(cell.c);
                run.cells += 1;
                continue;
            }
            let mut text = String::from(cell.c);
            text.extend(cell.zerowidth().into_iter().flatten());
            runs.push(Run {
                row,
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
        push_bg(bg_run.take(), &mut frame.backgrounds);

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
        for run in runs {
            let force = run.grid_aligned.then_some(cell_width);
            let runs = [text_run(run.text.len(), run.style)];
            let shaped =
                text_system.shape_line(SharedString::from(run.text), font_size, &runs, force);
            frame.text.push((origin(run.col, run.row), shaped));
        }

        if let Some(link) = &view.hovered_link {
            let at = origin(link.start, link.row);
            let underline = Bounds::new(
                at + point(px(0.), line_height - px(2.)),
                size(cell_width * (link.end - link.start) as f32, px(1.)),
            );
            frame.backgrounds.push(fill(underline, palette.foreground));
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
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for quad in frame.backgrounds.drain(..).chain(frame.glyphs.drain(..)) {
                window.paint_quad(quad);
            }
            for (origin, line) in &frame.text {
                let _ = line.paint(*origin, line_height, window, cx);
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
        let cursor_bounds = frame.cursor_bounds;
        self.view.update(cx, |view, _| {
            view.cursor_bounds = cursor_bounds;
            view.origin = bounds.origin;
        });
    }
}
