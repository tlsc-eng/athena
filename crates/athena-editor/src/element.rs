use athena_ui::ActiveTheme;
use gpui::{
    App, Bounds, ContentMask, Element, ElementId, ElementInputHandler, Entity, Font, FontFeatures,
    FontStyle, FontWeight, GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId,
    PaintQuad, Pixels, Point, ShapedLine, SharedString, Style, TextRun, UnderlineStyle, Window,
    fill, point, px, relative, size,
};

use crate::display::DisplayLine;
use crate::syntax::Token;
use crate::view::{EditorLayout, EditorView};

const LINE_HEIGHT_RATIO: f32 = 1.5;
const GUTTER_PAD: f32 = 16.;
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

        let mut frame = Frame {
            backgrounds: Vec::new(),
            text: Vec::new(),
            gutter: Vec::new(),
            gutter_bounds: bounds,
            text_bounds: bounds,
            overlay: Vec::new(),
            marked: None,
            line_height: lh,
            gutter_bg: theme.color.surface_sunken,
        };

        // Keep the cursor's line on screen after edits and keyboard moves.
        self.view.update(cx, |view, _| {
            view.viewport = bounds.size;
            let Some(buffer) = view.buffer.as_ref() else {
                return;
            };
            if view.autoscroll {
                let line = buffer.line_of(buffer.selection.head) as f32 * f32::from(lh);
                let height = f32::from(bounds.size.height);
                if line < view.scroll.y {
                    view.scroll.y = line;
                } else if line + f32::from(lh) > view.scroll.y + height {
                    view.scroll.y = line + f32::from(lh) - height;
                }
            }
        });

        let view = self.view.read(cx);
        let Some(buffer) = view.buffer.as_ref() else {
            return frame;
        };
        let total = buffer.len_lines();
        let digits = total.to_string().len().max(3);
        let gutter_w = cell * digits as f32 + px(GUTTER_PAD * 2.);
        let text_left = bounds.left() + gutter_w + px(TEXT_PAD);
        frame.gutter_bounds = Bounds::new(bounds.origin, size(gutter_w, bounds.size.height));
        frame.text_bounds = Bounds::from_corners(
            point(bounds.left() + gutter_w, bounds.top()),
            bounds.bottom_right(),
        );

        let first = (view.scroll.y / f32::from(lh)).floor().max(0.) as usize;
        let visible = (f32::from(bounds.size.height) / f32::from(lh)).ceil() as usize + 1;
        let last = (first + visible).min(total);
        let tokens = buffer.highlights(first..last);
        let rope = buffer.rope();
        let selection = buffer.selection.range();
        let head = buffer.selection.head;
        let head_line = buffer.line_of(head);

        // Horizontal autoscroll needs the cursor line shaped, so it is settled before painting.
        let mut scroll_x = view.scroll.x;
        let mut lines = Vec::new();
        for line in first..last {
            let raw = buffer.line(line);
            let display = DisplayLine::new(&raw);
            let start_char = buffer.line_start(line);
            let n = raw.chars().count();
            let mut colors = vec![syntax.text; n];
            for (bytes, token) in &tokens {
                let a = rope.byte_to_char(bytes.start).max(start_char);
                let b = rope.byte_to_char(bytes.end).min(start_char + n);
                let color = match token {
                    Token::Keyword | Token::Function => syntax.keyword,
                    Token::Type => syntax.type_,
                    Token::String | Token::Number => syntax.string,
                    Token::Comment => syntax.comment,
                    Token::Punctuation => syntax.punctuation,
                    Token::Property | Token::Variable => syntax.text,
                };
                for c in colors
                    .iter_mut()
                    .take(b.saturating_sub(start_char))
                    .skip(a.saturating_sub(start_char))
                {
                    *c = color;
                }
            }
            let mut runs: Vec<TextRun> = Vec::new();
            for (i, color) in colors.iter().enumerate() {
                let len = display.char_to_byte[i + 1] - display.char_to_byte[i];
                match runs.last_mut() {
                    Some(r) if r.color == *color => r.len += len,
                    _ => runs.push(run(len, *color)),
                }
            }
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
        let marks: Vec<(std::ops::Range<usize>, crate::MarkerSeverity)> = view
            .markers
            .iter()
            .map(|m| {
                let a = buffer.char_at_utf16(m.start.0, m.start.1);
                let b = buffer.char_at_utf16(m.end.0, m.end.1);
                (a..b.max(a), m.severity)
            })
            .collect();
        let y_of = |line: usize| bounds.top() + lh * line as f32 - px(view.scroll.y);
        let x0 = text_left - px(scroll_x);
        for (line, start, n, display, shaped) in &lines {
            let y = y_of(*line);
            let end = start + n;
            if *line == head_line && selection.is_empty() && self.focused {
                frame.backgrounds.push(fill(
                    Bounds::new(point(bounds.left(), y), size(bounds.size.width, lh)),
                    syntax.current_line,
                ));
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
            if !selection.is_empty()
                && let Some(b) = span(&selection)
            {
                frame.backgrounds.push(fill(b, theme.color.surface_accent));
            }
            frame.text.push((point(x0, y), shaped.clone()));

            let mut worst = None;
            for (range, severity) in &marks {
                if range.start > end || range.end < *start {
                    continue;
                }
                worst = worst.min(Some(*severity)).or(Some(*severity));
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
            let x = bounds.left() + gutter_w - px(GUTTER_PAD) - label.width;
            frame.gutter.push((point(x, y), label));

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
            for (origin, line) in &frame.gutter {
                let _ = line.paint(*origin, lh, window, cx);
            }
        });
    }
}
