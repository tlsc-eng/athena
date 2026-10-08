use athena_ui::{ActiveTheme, SyntaxColors};
use gpui::{
    App, Bounds, ContentMask, Element, ElementId, ElementInputHandler, Entity, Font, FontFeatures,
    FontStyle, FontWeight, GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId,
    PaintQuad, Pixels, Point, ShapedLine, SharedString, Style, TextRun, UnderlineStyle, Window,
    fill, point, px, relative, size,
};

use crate::Lang;
use crate::buffer::Buffer;
use crate::display::{DisplayLine, Guides};
use crate::syntax::Token;
use crate::view::{EditorLayout, EditorView};

/// How a token is drawn: colour, weight and whether it is underlined.
#[derive(Clone, Copy, PartialEq)]
struct TokenStyle {
    color: Hsla,
    weight: FontWeight,
    underline: bool,
}

impl TokenStyle {
    fn plain(color: Hsla) -> Self {
        Self {
            color,
            weight: FontWeight::NORMAL,
            underline: false,
        }
    }
}

fn style_for(token: Token, syntax: &SyntaxColors) -> TokenStyle {
    let color = match token {
        Token::Keyword => syntax.keyword,
        Token::Function | Token::Tag | Token::Link => syntax.function,
        Token::Property => syntax.property,
        Token::Type | Token::Namespace => syntax.type_,
        Token::Attribute => syntax.attribute,
        Token::String => syntax.string,
        Token::StringSpecial | Token::Number => syntax.string_special,
        Token::Constant | Token::Escape | Token::Embedded | Token::Label => syntax.constant,
        Token::VariableBuiltin => syntax.variable_builtin,
        Token::Operator => syntax.operator,
        Token::Punctuation => syntax.punctuation,
        Token::PunctuationSpecial => syntax.punctuation_special,
        Token::Comment => syntax.comment,
        Token::Heading => syntax.heading,
        Token::Error => syntax.error,
        Token::Variable => syntax.text,
    };
    TokenStyle {
        weight: if token == Token::Heading {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        },
        underline: token == Token::Link,
        ..TokenStyle::plain(color)
    }
}

const LINE_HEIGHT_RATIO: f32 = 1.5;
pub(crate) const GUTTER_PAD: f32 = 16.;
/// Width of the gutter column right of the line numbers that holds fold chevrons.
const FOLD_COLUMN: f32 = 20.;
const TEXT_PAD: f32 = 8.;

pub struct EditorElement {
    view: Entity<EditorView>,
    focused: bool,
}

impl EditorElement {
    pub fn new(view: Entity<EditorView>, focused: bool) -> Self {
        Self { view, focused }
    }
}

pub struct Frame {
    backgrounds: Vec<PaintQuad>,
    text: Vec<(Point<Pixels>, ShapedLine)>,
    gutter: Vec<(Point<Pixels>, ShapedLine)>,
    gutter_bounds: Bounds<Pixels>,
    gutter_marks: Vec<PaintQuad>,
    text_bounds: Bounds<Pixels>,
    overlay: Vec<PaintQuad>,
    marked: Option<(PaintQuad, Point<Pixels>, ShapedLine)>,
    line_height: Pixels,
    gutter_bg: Hsla,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// Guides for the shown lines, reaching to the cursor's line when it is near enough to matter.
fn guides_for(buffer: &Buffer, shown: &[usize], cursor_line: usize) -> Guides {
    const NEAR: usize = 1000;
    let (Some(&top), Some(&bottom)) = (shown.first(), shown.last()) else {
        return Guides::new(0..0, 0, 1, false, 0, |_| String::new());
    };
    let near = cursor_line + NEAR >= top && cursor_line <= bottom + NEAR;
    let lines = if near {
        top.min(cursor_line)..bottom.max(cursor_line) + 1
    } else {
        top..bottom + 1
    };
    let offside = matches!(buffer.lang(), Some(Lang::Python | Lang::Yaml));
    Guides::new(
        lines,
        buffer.len_lines(),
        buffer.indent.size(),
        offside,
        cursor_line,
        |l| buffer.line(l),
    )
}

impl Element for EditorElement {
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
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Frame {
        let theme = cx.theme().clone();
        let syntax = theme.syntax.clone();
        let font_size = theme.typography.code;
        let font = Font {
            family: theme.typography.mono.clone(),
            features: FontFeatures::disable_ligatures(),
            fallbacks: None,
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        };
        let text_system = window.text_system().clone();
        let cell = text_system
            .advance(text_system.resolve_font(&font), font_size, 'm')
            .map(|s| s.width)
            .unwrap_or(px(8.));
        let lh = px((f32::from(font_size) * LINE_HEIGHT_RATIO).round());
        let run = |len: usize, color: Hsla| TextRun {
            len,
            font: font.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let styled = |len: usize, style: TokenStyle| TextRun {
            font: Font {
                weight: style.weight,
                ..font.clone()
            },
            underline: style.underline.then_some(UnderlineStyle {
                thickness: px(1.),
                color: Some(style.color),
                wavy: false,
            }),
            ..run(len, style.color)
        };

        let mut frame = Frame {
            backgrounds: Vec::new(),
            text: Vec::new(),
            gutter: Vec::new(),
            gutter_bounds: bounds,
            gutter_marks: Vec::new(),
            text_bounds: bounds,
            overlay: Vec::new(),
            marked: None,
            line_height: lh,
            gutter_bg: theme.color.surface_sunken,
        };

        // Keep the cursor's line on screen after edits and keyboard moves.
        self.view.update(cx, |view, _| {
            view.viewport = bounds.size;
            view.follow_edits();
            let Some(head_line) = view.buf().map(|b| b.line_of(view.cursor.head())) else {
                return;
            };
            if let Some(top) = view.pending_top.take() {
                view.scroll.y = view.display.row_of(top) as f32 * f32::from(lh);
            }
            if view.autoscroll {
                let row = view.display.row_of(head_line);
                let line = row as f32 * f32::from(lh);
                let height = f32::from(bounds.size.height);
                if std::mem::take(&mut view.center_cursor) {
                    view.scroll.y = (line - (height - f32::from(lh)) / 2.).max(0.);
                } else if line < view.scroll.y {
                    view.scroll.y = line;
                } else if line + f32::from(lh) > view.scroll.y + height {
                    view.scroll.y = line + f32::from(lh) - height;
                }
            }
        });

        let view = self.view.read(cx);
        let Some(shared) = view.buffer.clone() else {
            return frame;
        };
        let buffer = shared.buffer.borrow();
        let total = buffer.len_lines();
        let digits = total.to_string().len().max(3);
        let numbers_right = bounds.left() + px(GUTTER_PAD) + cell * digits as f32;
        let fold_column = (numbers_right, numbers_right + px(FOLD_COLUMN));
        let gutter_w = fold_column.1 - bounds.left();
        let text_left = bounds.left() + gutter_w + px(TEXT_PAD);
        frame.gutter_bounds = Bounds::new(bounds.origin, size(gutter_w, bounds.size.height));
        frame.text_bounds = Bounds::from_corners(
            point(bounds.left() + gutter_w, bounds.top()),
            bounds.bottom_right(),
        );

        let first = (view.scroll.y / f32::from(lh)).floor().max(0.) as usize;
        let visible = (f32::from(bounds.size.height) / f32::from(lh)).ceil() as usize + 1;
        let last = (first + visible).min(view.display.row_count(total));
        let shown: Vec<usize> = (first..last).map(|r| view.display.line_of(r)).collect();
        // One highlight query per unbroken run of lines, so folded text is never queried.
        let mut tokens = Vec::new();
        for run in shown.chunk_by(|a, b| a + 1 == *b) {
            tokens.extend(buffer.highlights(run[0]..run[run.len() - 1] + 1));
        }
        let rope = buffer.rope();
        let selection = view.cursor.selection.range();
        let head = view.cursor.head();
        let head_line = buffer.line_of(head);

        // Horizontal autoscroll needs the cursor line shaped, so it is settled before painting.
        let mut scroll_x = view.scroll.x;
        let mut lines = Vec::new();
        for &line in &shown {
            let raw = buffer.line(line);
            let display = DisplayLine::new(&raw);
            let start_char = buffer.line_start(line);
            let line_bytes = rope.line_to_byte(line)..rope.line_to_byte(line + 1);
            let n = raw.chars().count();
            let mut styles = vec![TokenStyle::plain(syntax.text); n];
            for (bytes, token) in &tokens {
                if bytes.end <= line_bytes.start || bytes.start >= line_bytes.end {
                    continue;
                }
                let a = rope.byte_to_char(bytes.start).max(start_char);
                let b = rope.byte_to_char(bytes.end).min(start_char + n);
                let style = style_for(*token, &syntax);
                for s in styles
                    .iter_mut()
                    .take(b.saturating_sub(start_char))
                    .skip(a.saturating_sub(start_char))
                {
                    *s = style;
                }
            }
            let mut runs: Vec<(TextRun, TokenStyle)> = Vec::new();
            for (i, style) in styles.iter().enumerate() {
                let len = display.char_to_byte[i + 1] - display.char_to_byte[i];
                match runs.last_mut() {
                    Some((r, s)) if s == style => r.len += len,
                    _ => runs.push((styled(len, *style), *style)),
                }
            }
            let runs: Vec<TextRun> = runs.into_iter().map(|(r, _)| r).collect();
            let shaped = text_system.shape_line(
                SharedString::from(display.text.clone()),
                font_size,
                &runs,
                None,
            );
            lines.push((line, start_char, n, display, shaped));
        }
        if view.autoscroll
            && let Some((_, start, _, display, shaped)) =
                lines.iter().find(|(l, ..)| *l == head_line)
        {
            let x = f32::from(shaped.x_for_index(display.char_to_byte[head - start]));
            let width = f32::from(bounds.right() - text_left) - f32::from(cell) * 2.;
            if x < scroll_x {
                scroll_x = (x - f32::from(cell) * 4.).max(0.);
            } else if x > scroll_x + width {
                scroll_x = x - width + f32::from(cell) * 4.;
            }
        }

        let finds = view.find_matches().to_vec();
        let brackets = selection
            .is_empty()
            .then(|| buffer.matching_bracket(head))
            .flatten()
            .map_or(Vec::new(), |(a, b)| vec![a..a + 1, b..b + 1]);
        let marks: Vec<(std::ops::Range<usize>, crate::MarkerSeverity)> = view
            .markers
            .iter()
            .map(|m| {
                let a = buffer.char_at_utf16(m.start.0, m.start.1);
                let b = buffer.char_at_utf16(m.end.0, m.end.1);
                (a..b.max(a), m.severity)
            })
            .collect();
        let x0 = text_left - px(scroll_x);
        let guides = guides_for(&buffer, &shown, head_line);
        let guide_step = cell * buffer.indent.size() as f32;
        let mut tab_mark = None;
        for (row, (line, start, n, display, shaped)) in lines.iter().enumerate() {
            let y = bounds.top() + lh * (first + row) as f32 - px(view.scroll.y);
            let end = start + n;
            let folded = view.display.folded_at(*line);
            if *line == head_line && selection.is_empty() && self.focused {
                frame.backgrounds.push(fill(
                    Bounds::new(point(bounds.left(), y), size(bounds.size.width, lh)),
                    syntax.current_line,
                ));
            }
            for guide in 0..guides.level(*line) {
                let color = if guides.is_active(guide, *line) {
                    syntax.indent_guide_active
                } else {
                    syntax.indent_guide
                };
                let x = x0 + guide_step * guide as f32;
                frame
                    .backgrounds
                    .push(fill(Bounds::new(point(x, y), size(px(1.), lh)), color));
            }
            let span = |r: &std::ops::Range<usize>| -> Option<Bounds<Pixels>> {
                let a = r.start.max(*start);
                let b = r.end.min(end);
                let past_end = r.end > end && r.start <= end;
                if a > b || (a == b && !past_end) {
                    return None;
                }
                let xa = shaped.x_for_index(display.char_to_byte[a - start]);
                let xb = shaped.x_for_index(display.char_to_byte[b - start])
                    + if past_end { cell } else { px(0.) };
                Some(Bounds::new(point(x0 + xa, y), size(xb - xa, lh)))
            };
            for m in &finds {
                if let Some(b) = span(m) {
                    frame.backgrounds.push(fill(b, theme.color.surface_active));
                }
            }
            for r in &brackets {
                if let Some(b) = span(r) {
                    frame.backgrounds.push(fill(b, syntax.bracket_match));
                }
            }
            if !selection.is_empty()
                && let Some(b) = span(&selection)
            {
                frame.backgrounds.push(fill(b, theme.color.surface_accent));
            }
            frame.text.push((point(x0, y), shaped.clone()));
            // As VS Code's default `renderWhitespace: selection`: blanks show only where selected.
            let (a, b) = (selection.start.max(*start), selection.end.min(end));
            for (i, ch) in rope.slice(a.min(b)..b).chars().enumerate() {
                let x = x0 + shaped.x_for_index(display.char_to_byte[a + i - start]);
                match ch {
                    ' ' => frame.overlay.push(
                        fill(
                            Bounds::new(
                                point(x + cell / 2. - px(1.), y + lh / 2. - px(1.)),
                                size(px(2.), px(2.)),
                            ),
                            syntax.whitespace,
                        )
                        .corner_radii(px(1.)),
                    ),
                    '\t' => {
                        let mark = tab_mark.get_or_insert_with(|| {
                            text_system.shape_line(
                                "→".into(),
                                font_size,
                                &[run("→".len(), syntax.whitespace)],
                                None,
                            )
                        });
                        frame.text.push((point(x, y), mark.clone()));
                    }
                    _ => {}
                }
            }
            if folded.is_some() {
                let dots =
                    text_system.shape_line("⋯".into(), font_size, &[run(3, syntax.comment)], None);
                let x = x0 + shaped.width + cell;
                let pad = cell / 2.;
                frame.backgrounds.push(
                    fill(
                        Bounds::new(
                            point(x - pad, y + px(3.)),
                            size(dots.width + pad * 2., lh - px(6.)),
                        ),
                        theme.color.surface_active,
                    )
                    .corner_radii(theme.shape.radius_control),
                );
                frame.text.push((point(x, y), dots));
            }

            // A folded header's number also reports diagnostics hidden under it.
            let marks_end =
                folded.map_or(end, |f| buffer.line_start(f.end) + buffer.line_len(f.end));
            let mut worst = None;
            for (range, severity) in &marks {
                if range.start > marks_end || range.end < *start {
                    continue;
                }
                worst = worst.min(Some(*severity)).or(Some(*severity));
                if range.start > end {
                    continue;
                }
                let a = range.start.clamp(*start, end);
                let b = range.end.clamp(*start, end);
                let xa = shaped.x_for_index(display.char_to_byte[a - start]);
                let xb = shaped.x_for_index(display.char_to_byte[b - start]);
                let width = if b > a { xb - xa } else { cell };
                frame.overlay.push(fill(
                    Bounds::new(point(x0 + xa, y + lh - px(2.)), size(width, px(1.))),
                    crate::view::marker_color(*severity, &theme),
                ));
            }

            let number = (line + 1).to_string();
            let color = match worst {
                Some(
                    severity @ (crate::MarkerSeverity::Error | crate::MarkerSeverity::Warning),
                ) => crate::view::marker_color(severity, &theme),
                _ if *line == head_line => syntax.line_number_active,
                _ => syntax.line_number,
            };
            let label = text_system.shape_line(
                number.clone().into(),
                font_size,
                &[run(number.len(), color)],
                None,
            );
            let x = numbers_right - label.width;
            frame.gutter.push((point(x, y), label));
            if view.lightbulb == Some(*line) {
                // A painted dot, because the editor font has no emoji fallback for 💡.
                let d = px(6.);
                let origin = point(bounds.left() + (px(GUTTER_PAD) - d) / 2., y + (lh - d) / 2.);
                frame.gutter_marks.push(
                    fill(Bounds::new(origin, size(d, d)), theme.color.warning).corner_radii(d / 2.),
                );
            }
            // Marks come from the saved file, so unsaved line inserts shift them until the next save.
            for mark in &view.gutter_marks {
                let (top, height, color) = match *mark {
                    crate::GutterMark::Added { start, len }
                        if (start..start + len).contains(line) =>
                    {
                        (y, lh, theme.color.success)
                    }
                    crate::GutterMark::Modified { start, len }
                        if (start..start + len).contains(line) =>
                    {
                        (y, lh, theme.color.warning)
                    }
                    crate::GutterMark::Removed { before } if before == *line => {
                        (y - px(3.), px(6.), theme.color.danger)
                    }
                    _ => continue,
                };
                let width = if matches!(mark, crate::GutterMark::Removed { .. }) {
                    px(4.)
                } else {
                    px(2.)
                };
                frame.gutter_marks.push(fill(
                    Bounds::new(point(fold_column.0 + px(2.), top), size(width, height)),
                    color,
                ));
            }
            let chevron = match folded {
                Some(_) => Some(("▸", syntax.line_number_active)),
                None if view.gutter_hover && view.fold_at(*line).is_some() => {
                    Some(("▾", syntax.line_number))
                }
                None => None,
            };
            if let Some((glyph, color)) = chevron {
                let shaped = text_system.shape_line(
                    glyph.into(),
                    font_size,
                    &[run(glyph.len(), color)],
                    None,
                );
                let x = fold_column.0 + (px(FOLD_COLUMN) - shaped.width) / 2.;
                frame.gutter.push((point(x, y), shaped));
            }

            if let Some((_, caption)) = view
                .blame
                .as_ref()
                .filter(|(l, _)| *l == *line && *line == head_line && self.focused)
                .filter(|_| folded.is_none())
            {
                let text = text_system.shape_line(
                    caption.clone().into(),
                    font_size,
                    &[run(caption.len(), syntax.comment)],
                    None,
                );
                frame
                    .text
                    .push((point(x0 + shaped.width + cell * 3., y), text));
            }

            if *line == head_line {
                let x = x0 + shaped.x_for_index(display.char_to_byte[head - start]);
                if let Some(marked) = &view.marked {
                    let underline = UnderlineStyle {
                        thickness: px(1.),
                        color: Some(syntax.text),
                        wavy: false,
                    };
                    let runs = [TextRun {
                        underline: Some(underline),
                        ..run(marked.len(), syntax.text)
                    }];
                    let text =
                        text_system.shape_line(marked.clone().into(), font_size, &runs, None);
                    let backdrop = fill(
                        Bounds::new(point(x, y), size(text.width, lh)),
                        theme.color.surface_sunken,
                    );
                    frame.marked = Some((backdrop, point(x, y), text));
                } else if self.focused {
                    frame.overlay.push(fill(
                        Bounds::new(point(x, y), size(px(2.), lh)),
                        theme.color.accent,
                    ));
                }
            }
        }

        let stored = lines
            .into_iter()
            .map(|(line, _, _, display, shaped)| (line, display, shaped))
            .collect();
        self.view.update(cx, |view, _| {
            view.scroll.x = scroll_x;
            view.autoscroll = false;
            view.layout = Some(EditorLayout {
                origin: bounds.origin,
                text_left,
                line_height: lh,
                lines: stored,
                fold_column,
            });
        });
        frame
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        frame: &mut Frame,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.view.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );
        let lh = frame.line_height;
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for quad in frame.backgrounds.drain(..) {
                window.paint_quad(quad);
            }
            window.with_content_mask(
                Some(ContentMask {
                    bounds: frame.text_bounds,
                }),
                |window| {
                    for (origin, line) in &frame.text {
                        let _ = line.paint(*origin, lh, window, cx);
                    }
                    for quad in frame.overlay.drain(..) {
                        window.paint_quad(quad);
                    }
                    if let Some((backdrop, origin, line)) = frame.marked.take() {
                        window.paint_quad(backdrop);
                        let _ = line.paint(origin, lh, window, cx);
                    }
                },
            );
            window.paint_quad(fill(frame.gutter_bounds, frame.gutter_bg));
            for quad in frame.gutter_marks.drain(..) {
                window.paint_quad(quad);
            }
            for (origin, line) in &frame.gutter {
                let _ = line.paint(*origin, lh, window, cx);
            }
        });
    }
}
