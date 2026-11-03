use std::ops::Range;

use gpui::{Context, KeyBinding, actions};

use crate::buffer::{Buffer, Cursor, Selection};
use crate::lsp_ui::LspRequest;
use crate::view::{EditorEvent, EditorView};

actions!(editor, [ExpandSelection, ShrinkSelection]);

pub(crate) fn init(cx: &mut gpui::App) {
    let ctx = Some("Editor");
    cx.bind_keys([
        KeyBinding::new("ctrl-shift-cmd-right", ExpandSelection, ctx),
        KeyBinding::new("ctrl-shift-cmd-left", ShrinkSelection, ctx),
    ]);
}

/// A start and end as zero-based lines and UTF-16 columns.
pub type Utf16Range = ((u32, u32), (u32, u32));

/// Expand and Shrink Selection's state, as VS Code's smart select keeps it.
#[derive(Default)]
pub(crate) struct SmartSelect {
    /// The selections before each expansion, the latest last, for Shrink to step back through.
    history: Vec<Vec<Selection>>,
    /// The buffer version and selections the last step left; anything else starts afresh.
    last: Option<(u64, Vec<Selection>)>,
    /// Each caret's ranges, innermost first, for one buffer version.
    chains: Option<(u64, Vec<Vec<Range<usize>>>)>,
    /// The request in flight and the buffer version it was asked for.
    pending: Option<(u64, u64)>,
    requests: u64,
}

/// Each selection grown to the smallest range of its chain that holds it and is larger; `None`
/// when no selection can grow.
fn grow(chains: &[Vec<Range<usize>>], selections: &[Selection]) -> Option<Vec<Selection>> {
    let mut grew = false;
    let out = selections
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let have = s.range();
            let next = chains.get(i).and_then(|chain| {
                chain
                    .iter()
                    .find(|r| r.start <= have.start && have.end <= r.end && r.len() > have.len())
            });
            match next {
                Some(r) => {
                    grew = true;
                    Selection {
                        anchor: r.start,
                        head: r.end,
                    }
                }
                None => *s,
            }
        })
        .collect();
    grew.then_some(out)
}

/// What selection grows through without a language server: the word, the line's text, the
/// whole line, then the document.
fn fallback_chain(b: &Buffer, at: usize) -> Vec<Range<usize>> {
    let mut chain = Vec::new();
    chain.extend(b.word_at(at));
    let line = b.line_of(at);
    let start = b.line_start(line);
    let text = b.line(line);
    let indent = text.chars().take_while(|c| c.is_whitespace()).count();
    let content = text.trim_end_matches(['\n', '\r']).chars().count();
    chain.push(start + indent..start + content);
    let end = match line + 1 < b.len_lines() {
        true => b.line_start(line + 1),
        false => b.len_chars(),
    };
    chain.push(start..end);
    chain.push(0..b.len_chars());
    chain.retain(|r| r.start <= at && at <= r.end);
    chain
}

impl EditorView {
    fn selections(&self) -> Vec<Selection> {
        self.cursor.all().iter().map(|c| c.selection).collect()
    }

    /// Ctrl+Shift+Cmd+Right: grows every selection to the syntax around it.
    pub(crate) fn expand_selection(&mut self, cx: &mut Context<Self>) {
        self.follow_edits();
        let Some(version) = self.version() else {
            return;
        };
        let now = self.selections();
        if self.smart.last.as_ref() != Some(&(version, now.clone())) {
            self.smart.history.clear();
            self.smart.chains = None;
        }
        if self
            .smart
            .chains
            .as_ref()
            .is_some_and(|(v, _)| *v == version)
        {
            return self.grow_selection(cx);
        }
        let Some(positions) = self.buf().map(|b| {
            self.cursor
                .all()
                .iter()
                .map(|c| b.utf16_position(c.head()))
                .collect()
        }) else {
            return;
        };
        self.smart.requests += 1;
        let request = self.smart.requests;
        self.smart.pending = Some((request, version));
        cx.emit(EditorEvent::Lsp(LspRequest::SelectionRanges {
            request,
            positions,
        }));
    }

    /// The answer to [`LspRequest::SelectionRanges`]: each caret's ranges from the server,
    /// innermost first, or `None` to grow by words and lines instead.
    pub fn show_selection_ranges(
        &mut self,
        request: u64,
        chains: Option<Vec<Vec<Utf16Range>>>,
        cx: &mut Context<Self>,
    ) {
        let Some((asked, version)) = self.smart.pending else {
            return;
        };
        if asked != request || self.version() != Some(version) {
            return;
        }
        self.smart.pending = None;
        let Some(chains) = self.buf().map(|b| {
            self.cursor
                .all()
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let mut chain: Vec<Range<usize>> = chains
                        .as_ref()
                        .and_then(|all| all.get(i))
                        .into_iter()
                        .flatten()
                        .map(|&(a, z)| b.char_at_utf16(a.0, a.1)..b.char_at_utf16(z.0, z.1))
                        .collect();
                    if chain.is_empty() {
                        chain = fallback_chain(&b, c.head());
                    }
                    chain.push(0..b.len_chars());
                    chain
                })
                .collect()
        }) else {
            return;
        };
        self.smart.chains = Some((version, chains));
        self.grow_selection(cx);
    }

    fn grow_selection(&mut self, cx: &mut Context<Self>) {
        let Some((version, chains)) = &self.smart.chains else {
            return;
        };
        let version = *version;
        let now = self.selections();
        let Some(grown) = grow(chains, &now) else {
            return;
        };
        self.smart.history.push(now);
        self.set_selections(grown, cx);
        self.smart.last = Some((version, self.selections()));
    }

    /// Ctrl+Shift+Cmd+Left: steps back to the selections before the last expansion.
    pub(crate) fn shrink_selection(&mut self, cx: &mut Context<Self>) {
        self.follow_edits();
        let Some(version) = self.version() else {
            return;
        };
        if self.smart.last.as_ref() != Some(&(version, self.selections())) {
            return;
        }
        let Some(before) = self.smart.history.pop() else {
            return;
        };
        self.set_selections(before, cx);
        self.smart.last = Some((version, self.selections()));
    }

    fn set_selections(&mut self, selections: Vec<Selection>, cx: &mut Context<Self>) {
        let primary = self.cursor.primary_index();
        self.with_buffer(cx, |_, c| {
            let all = selections
                .into_iter()
                .map(|selection| Cursor {
                    selection,
                    ..Cursor::default()
                })
                .collect();
            c.set(all, primary);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(anchor: usize, head: usize) -> Selection {
        Selection { anchor, head }
    }

    #[test]
    fn each_selection_grows_to_the_next_larger_range_around_it() {
        let chains = vec![vec![4..9, 0..20, 0..40], vec![30..31, 28..35]];
        let first = grow(&chains, &[sel(5, 5), sel(30, 30)]).unwrap();
        assert_eq!(first, [sel(4, 9), sel(30, 31)]);
        let second = grow(&chains, &first).unwrap();
        assert_eq!(second, [sel(0, 20), sel(28, 35)]);
        let third = grow(&chains, &second).unwrap();
        assert_eq!(
            third,
            [sel(0, 40), sel(28, 35)],
            "the second has nowhere to go"
        );
        assert_eq!(grow(&chains, &third), None);
        assert_eq!(
            grow(&chains, &[sel(4, 9), sel(30, 30)]).unwrap()[0],
            sel(0, 20),
            "a range equal to the selection is skipped"
        );
    }

    #[test]
    fn vs_codes_keys_parse() {
        for source in ["ctrl-shift-cmd-right", "ctrl-shift-cmd-left"] {
            let k = gpui::Keystroke::parse(source).unwrap();
            assert!(k.modifiers.control && k.modifiers.shift && k.modifiers.platform);
        }
        assert!(gpui::Keystroke::parse("cmd-f").is_ok());
    }

    #[test]
    fn without_a_server_selection_grows_by_word_line_text_line_and_document() {
        let b = Buffer::new("fn a() {\n    let value = 1;\n}\n", None);
        let at = 18;
        assert_eq!(b.text(b.word_at(at).unwrap()), "value");
        let chain = fallback_chain(&b, at);
        let texts: Vec<String> = chain.iter().map(|r| b.text(r.clone())).collect();
        assert_eq!(
            texts,
            [
                "value",
                "let value = 1;",
                "    let value = 1;\n",
                "fn a() {\n    let value = 1;\n}\n"
            ]
        );
    }
}
