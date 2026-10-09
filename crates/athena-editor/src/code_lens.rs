use std::ops::Range;
use std::time::Duration;

use athena_ui::ActiveTheme;
use gpui::{AnyElement, Context, MouseButton, anchored, div, point, prelude::*, px};

use crate::lsp_ui::{Anchored, LspRequest};
use crate::view::{EditorEvent, EditorView};

/// Typing or scrolling pauses this long before code lenses are asked for again.
const LENS_DELAY: Duration = Duration::from_millis(300);
/// Lenses are asked for this many lines above and below the screen.
const LENS_MARGIN: usize = 100;

/// A command a language server shows with a line, such as "run go generate" or "3 references".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lens {
    /// The zero-based line it belongs to.
    pub line: u32,
    pub title: String,
    /// What a click sends back in [`LspRequest::RunCodeLens`]; `None` for a lens Athena cannot
    /// run, which is shown but not clickable.
    pub id: Option<usize>,
}

/// A lens as the editor keeps it: its title and the id a click sends.
type Shown = (String, Option<usize>);

#[derive(Default)]
pub(crate) struct CodeLenses {
    enabled: bool,
    /// Each lens's title and id, at the start of its line.
    shown: Anchored<Shown>,
    /// The request whose answer is shown, which a click names.
    shown_request: u64,
    /// The buffer version and lines last asked about, or waited on.
    asked: Option<(u64, Range<usize>)>,
    /// The request in flight and the buffer version it was asked for.
    pending: Option<(u64, u64)>,
    requests: u64,
    timer: Option<gpui::Task<()>>,
}

/// The lenses on each line, in the order given, as (line, [(title, id)]).
fn by_line(found: Vec<(usize, Shown)>) -> Vec<(usize, Vec<Shown>)> {
    let mut out: Vec<(usize, Vec<Shown>)> = Vec::new();
    for (line, lens) in found {
        match out.iter_mut().find(|(l, _)| *l == line) {
            Some((_, lenses)) => lenses.push(lens),
            None => out.push((line, vec![lens])),
        }
    }
    out.sort_by_key(|(line, _)| *line);
    out
}

impl EditorView {
    /// VS Code's `editor.codeLens`: show the commands language servers offer with lines.
    pub fn set_code_lens(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.lenses.enabled != enabled {
            self.lenses = CodeLenses {
                enabled,
                ..CodeLenses::default()
            };
            cx.notify();
        }
    }

    /// Asks for code lenses again, as after the server said they are out of date.
    pub fn refresh_code_lens(&mut self, cx: &mut Context<Self>) {
        self.lenses.asked = None;
        cx.notify();
    }

    /// Asks for the lenses around the lines on screen once typing or scrolling pauses; called
    /// every frame.
    pub(crate) fn schedule_code_lens(&mut self, cx: &mut Context<Self>) {
        if !self.lenses.enabled || !self.completing_attached() {
            return;
        }
        let (Some(version), Some(lines)) = (self.version(), self.buf().map(|b| b.len_lines()))
        else {
            return;
        };
        let Some((first, last)) = self.layout.as_ref().and_then(|l| {
            let first = l.rows.first()?.line;
            Some((first, l.rows.last()?.line))
        }) else {
            return;
        };
        if let Some((v, asked)) = &self.lenses.asked
            && *v == version
            && asked.start <= first
            && last < asked.end
        {
            return;
        }
        let window = first.saturating_sub(LENS_MARGIN)..(last + LENS_MARGIN + 1).min(lines);
        self.lenses.asked = Some((version, window.clone()));
        self.lenses.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LENS_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.lenses.timer = None;
                this.lenses.requests += 1;
                let request = this.lenses.requests;
                this.lenses.pending = Some((request, version));
                cx.emit(EditorEvent::Lsp(LspRequest::CodeLens {
                    request,
                    start_line: window.start as u32,
                    end_line: window.end as u32,
                }));
            });
        }));
    }

    /// The answer to [`LspRequest::CodeLens`]; dropped if the text changed since.
    pub fn show_code_lenses(&mut self, request: u64, lenses: Vec<Lens>, cx: &mut Context<Self>) {
        let Some((asked, version)) = self.lenses.pending else {
            return;
        };
        let Some(shared) = self.buffer.clone() else {
            return;
        };
        let b = shared.buffer.borrow();
        if asked != request || b.version() != version || !self.lenses.enabled {
            return;
        }
        self.lenses.pending = None;
        let items: Vec<(Range<usize>, Shown)> = lenses
            .into_iter()
            .filter(|l| (l.line as usize) < b.len_lines() && !l.title.trim().is_empty())
            .map(|l| {
                let at = b.line_start(l.line as usize);
                (at..at, (l.title.replace(['\n', '\t'], " "), l.id))
            })
            .collect();
        if items.is_empty() && self.lenses.shown.now(&b).is_empty() {
            return;
        }
        self.lenses.shown = Anchored::new(&b, items);
        self.lenses.shown_request = request;
        cx.notify();
    }

    /// Each line's lenses on screen, muted after its text as the merge conflict actions are;
    /// a click on one asks the shell to run it.
    pub(crate) fn render_code_lens(
        &self,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let (Some(layout), Some(b)) = (self.layout.as_ref(), self.buf()) else {
            return Vec::new();
        };
        if !self.lenses.enabled {
            return Vec::new();
        }
        let found: Vec<(usize, Shown)> = self
            .lenses
            .shown
            .now(&b)
            .into_iter()
            .map(|(at, lens)| (b.line_of(at.start), lens))
            .collect();
        if found.is_empty() {
            return Vec::new();
        }
        let t = cx.theme().clone();
        let lh = layout.line_height;
        let top = layout.origin.y + lh * layout.sticky.len() as f32;
        let bottom = layout.origin.y + self.viewport.height - lh;
        let head_line = b.line_of(self.cursor.head());
        let request = self.lenses.shown_request;
        let mut out = Vec::new();
        for (line, lenses) in by_line(found) {
            if self.display.fold_containing(line).is_some() {
                continue;
            }
            let end = b.line_start(line) + b.line_len(line);
            let Some(origin) = self.char_origin(end) else {
                continue;
            };
            if origin.y < top || origin.y > bottom {
                continue;
            }
            let mut row = div()
                .id(("code-lens", line))
                .occlude()
                .h(lh)
                .flex()
                .items_center()
                .gap(px(6.))
                .font_family(t.typography.ui.clone())
                .text_size(t.typography.caption)
                .text_color(t.color.content_muted)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
            for (i, (title, id)) in lenses.into_iter().enumerate() {
                if i > 0 {
                    row = row.child(div().text_color(t.color.content_disabled).child("|"));
                }
                let lens = div().id(("code-lens-item", line * 64 + i)).child(title);
                row = row.child(match id {
                    Some(id) => lens
                        .cursor_pointer()
                        .hover(|s| s.text_color(t.color.content).underline())
                        .on_click(cx.listener(move |_, _: &gpui::ClickEvent, _, cx| {
                            cx.emit(EditorEvent::Lsp(LspRequest::RunCodeLens { request, id }));
                        })),
                    None => lens,
                });
            }
            // After the inline blame caption when the cursor's line shows one.
            let caption = self
                .blame
                .as_ref()
                .filter(|(l, _)| focused && *l == line && line == head_line)
                .map_or(0, |(_, c)| c.chars().count() + 3);
            let x = origin.x + layout.cell * (3 + caption) as f32;
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
    use crate::buffer::{Buffer, Cursor};

    #[test]
    fn lenses_group_by_line_in_order_and_follow_edits_above_them() {
        let grouped = by_line(vec![
            (7, ("2 references".into(), Some(1))),
            (2, ("run go generate".into(), Some(0))),
            (7, ("1 implementation".into(), None)),
        ]);
        assert_eq!(
            grouped,
            [
                (2, vec![("run go generate".to_string(), Some(0))]),
                (
                    7,
                    vec![
                        ("2 references".to_string(), Some(1)),
                        ("1 implementation".to_string(), None)
                    ]
                ),
            ]
        );
        let mut b = Buffer::new("package main\n\nfunc a() {}\n", None);
        let at = b.line_start(2);
        let shown = Anchored::new(&b, vec![(at..at, ("1 reference".to_string(), Some(0)))]);
        let mut c = Cursor::at(0);
        b.insert(&mut c, "// doc\n");
        let now = shown.now(&b);
        assert_eq!(
            b.line_of(now[0].0.start),
            3,
            "a line added above moves it down"
        );
        let mut c = Cursor::at(b.line_start(3));
        b.insert(&mut c, "\t");
        assert_eq!(
            b.line_of(shown.now(&b)[0].0.start),
            3,
            "indenting keeps it on its line"
        );
    }
}
