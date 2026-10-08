use std::ops::Range as Span;

use serde_json::Value;

use crate::markup::expand_snippet;
use crate::protocol::Range;

/// Text to put in place of a range of the document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEdit {
    pub range: Range,
    pub text: String,
}

/// One suggestion, with any snippet already reduced to plain text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    /// The protocol's CompletionItemKind number.
    pub kind: Option<u32>,
    pub detail: Option<String>,
    pub filter_text: String,
    pub sort_text: String,
    pub text: String,
    /// What `text` replaces; without one, the word before the cursor.
    pub range: Option<Range>,
    /// The part of `text` to select once inserted, in chars; `None` puts the cursor after it.
    pub select: Option<Span<usize>>,
    /// Edits elsewhere in the file, such as an import gopls adds.
    pub additional_edits: Vec<TextEdit>,
    pub preselect: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompletionList {
    pub items: Vec<CompletionItem>,
    /// The server filtered for the current word; typing more should ask again.
    pub incomplete: bool,
}

pub(crate) fn parse_completions(result: &Value) -> CompletionList {
    let (items, incomplete) = match result {
        Value::Array(items) => (items.as_slice(), false),
        Value::Object(list) => (
            list.get("items")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice),
            list.get("isIncomplete")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
        _ => (&[][..], false),
    };
    CompletionList {
        items: items.iter().filter_map(parse_item).collect(),
        incomplete,
    }
}

/// A list of TextEdits, as formatting replies with; anything else is no edits.
pub(crate) fn parse_text_edits(result: &Value) -> Vec<TextEdit> {
    result
        .as_array()
        .map(|edits| edits.iter().filter_map(text_edit).collect())
        .unwrap_or_default()
}

fn text_edit(e: &Value) -> Option<TextEdit> {
    Some(TextEdit {
        range: serde_json::from_value(e.get("range")?.clone()).ok()?,
        text: e.get("newText")?.as_str()?.to_string(),
    })
}

fn parse_item(item: &Value) -> Option<CompletionItem> {
    let str_of = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_string);
    let label = str_of("label")?;
    let edit = item.get("textEdit");
    // An InsertReplaceEdit offers both; VS Code inserts by default.
    let range = edit
        .and_then(|e| e.get("range").or_else(|| e.get("insert")))
        .and_then(|r| serde_json::from_value(r.clone()).ok());
    let raw = edit
        .and_then(|e| e.get("newText"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| str_of("insertText"))
        .unwrap_or_else(|| label.clone());
    let snippet = item.get("insertTextFormat").and_then(Value::as_u64) == Some(2);
    let (text, select) = if snippet {
        expand_snippet(&raw)
    } else {
        (raw, None)
    };
    let detail = str_of("detail").filter(|d| !d.is_empty()).or_else(|| {
        item.pointer("/labelDetails/description")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let additional_edits = item
        .get("additionalTextEdits")
        .and_then(Value::as_array)
        .map(|edits| edits.iter().filter_map(text_edit).collect())
        .unwrap_or_default();
    Some(CompletionItem {
        kind: item.get("kind").and_then(Value::as_u64).map(|k| k as u32),
        detail,
        filter_text: str_of("filterText").unwrap_or_else(|| label.clone()),
        sort_text: str_of("sortText").unwrap_or_else(|| label.clone()),
        text,
        range,
        select,
        additional_edits,
        preselect: item
            .get("preselect")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        label,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Position;
    use serde_json::json;

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn reads_text_edits_snippets_and_plain_items() {
        let list = parse_completions(&json!({
            "isIncomplete": true,
            "items": [
                {"label": "Println", "kind": 3, "detail": "func(a ...any) (n int, err error)",
                 "insertTextFormat": 2, "sortText": "00000",
                 "textEdit": {"range": {"start": {"line": 4, "character": 5},
                                        "end": {"line": 4, "character": 7}},
                              "newText": "Println(${1:})"},
                 "additionalTextEdits": [{"range": {"start": {"line": 2, "character": 0},
                                                    "end": {"line": 2, "character": 0}},
                                          "newText": "import \"fmt\"\n"}]},
                {"label": "len", "kind": 3, "insertText": "len"},
                {"label": "x", "textEdit": {"insert": {"start": {"line": 0, "character": 1},
                                                       "end": {"line": 0, "character": 2}},
                                            "replace": {"start": {"line": 0, "character": 1},
                                                        "end": {"line": 0, "character": 4}},
                                            "newText": "xyz"}},
                {"kind": 3}
            ]
        }));
        assert!(list.incomplete);
        assert_eq!(list.items.len(), 3, "an item without a label is skipped");
        let println = &list.items[0];
        assert_eq!(println.text, "Println()");
        assert_eq!(println.select, Some(8..8));
        assert_eq!(println.range.unwrap().start, pos(4, 5));
        assert_eq!(println.additional_edits[0].range.start, pos(2, 0));
        assert_eq!(println.sort_text, "00000");
        let len = &list.items[1];
        assert_eq!(
            (len.text.as_str(), len.range, len.select.clone()),
            ("len", None, None)
        );
        assert_eq!(len.filter_text, "len");
        assert_eq!(list.items[2].range.unwrap().end, pos(0, 2));

        let bare = parse_completions(&json!([{"label": "a"}]));
        assert_eq!(bare.items[0].text, "a");
        assert!(!bare.incomplete);
        assert!(parse_completions(&json!(null)).items.is_empty());
    }
}
