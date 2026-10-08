use std::path::PathBuf;

use serde_json::Value;

use crate::completion::{TextEdit, text_edit};
use crate::protocol::{Position, path_from_uri};

/// One step of a workspace edit, in the order the server listed them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileChange {
    /// Text edits to one file; `version` is the document version they were made for, if given.
    Edit {
        path: PathBuf,
        version: Option<i64>,
        edits: Vec<TextEdit>,
    },
    Create {
        path: PathBuf,
        overwrite: bool,
        ignore_if_exists: bool,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
        overwrite: bool,
        ignore_if_exists: bool,
    },
    Delete {
        path: PathBuf,
        ignore_if_not_exists: bool,
    },
}

/// Changes a server asks for across files, as rename and code actions answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceEdit {
    pub changes: Vec<FileChange>,
}

impl WorkspaceEdit {
    pub fn is_empty(&self) -> bool {
        self.changes.iter().all(|c| match c {
            FileChange::Edit { edits, .. } => edits.is_empty(),
            _ => false,
        })
    }
}

/// Reads `documentChanges` when present (it is ordered and versioned), else `changes`; `None` if
/// any entry is unreadable, so a half-understood edit is never applied.
pub(crate) fn parse_workspace_edit(value: &Value) -> Option<WorkspaceEdit> {
    if let Some(list) = value.get("documentChanges").and_then(Value::as_array) {
        let changes = list.iter().map(document_change).collect::<Option<_>>()?;
        return Some(WorkspaceEdit { changes });
    }
    let mut changes: Vec<FileChange> = match value.get("changes") {
        Some(Value::Object(map)) => map
            .iter()
            .map(|(uri, edits)| {
                Some(FileChange::Edit {
                    path: path_from_uri(uri)?,
                    version: None,
                    edits: text_edits(edits)?,
                })
            })
            .collect::<Option<_>>()?,
        Some(Value::Null) | None => Vec::new(),
        Some(_) => return None,
    };
    changes.sort_by(|a, b| change_path(a).cmp(change_path(b)));
    Some(WorkspaceEdit { changes })
}

fn change_path(change: &FileChange) -> &PathBuf {
    match change {
        FileChange::Edit { path, .. }
        | FileChange::Create { path, .. }
        | FileChange::Delete { path, .. } => path,
        FileChange::Rename { from, .. } => from,
    }
}

fn text_edits(value: &Value) -> Option<Vec<TextEdit>> {
    value.as_array()?.iter().map(text_edit).collect()
}

fn document_change(value: &Value) -> Option<FileChange> {
    let flag = |key: &str| {
        value
            .pointer(&format!("/options/{key}"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    let uri = |key: &str| path_from_uri(value.get(key)?.as_str()?);
    Some(match value.get("kind").and_then(Value::as_str) {
        Some("create") => FileChange::Create {
            path: uri("uri")?,
            overwrite: flag("overwrite"),
            ignore_if_exists: flag("ignoreIfExists"),
        },
        Some("rename") => FileChange::Rename {
            from: uri("oldUri")?,
            to: uri("newUri")?,
            overwrite: flag("overwrite"),
            ignore_if_exists: flag("ignoreIfExists"),
        },
        Some("delete") => FileChange::Delete {
            path: uri("uri")?,
            ignore_if_not_exists: flag("ignoreIfNotExists"),
        },
        Some(_) => return None,
        None => FileChange::Edit {
            path: path_from_uri(value.pointer("/textDocument/uri")?.as_str()?)?,
            version: value
                .pointer("/textDocument/version")
                .and_then(Value::as_i64),
            edits: text_edits(value.get("edits")?)?,
        },
    })
}

/// Why a list of text edits could not be applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditError {
    /// Two edits change the same text; the protocol forbids it, and no order of them is right.
    Overlap { line: u32 },
}

/// `text` with `edits` applied, all positions taken in the original text; same-place inserts keep
/// their order, as the protocol asks.
pub fn apply_text_edits(text: &str, edits: &[TextEdit]) -> Result<String, EditError> {
    let starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let offset = |p: Position| -> usize {
        let Some(&start) = starts.get(p.line as usize) else {
            return text.len();
        };
        let end = starts
            .get(p.line as usize + 1)
            .map_or(text.len(), |next| next - 1);
        let line = text[start..end]
            .strip_suffix('\r')
            .unwrap_or(&text[start..end]);
        let mut units = 0;
        for (i, c) in line.char_indices() {
            if units >= p.character as usize {
                return start + i;
            }
            units += c.len_utf16();
        }
        start + line.len()
    };
    let mut spans: Vec<(usize, usize, usize, u32)> = edits
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let start = offset(e.range.start);
            (start, offset(e.range.end).max(start), i, e.range.start.line)
        })
        .collect();
    // Inserts sort before a replace starting at the same place, as the protocol orders them.
    spans.sort_by_key(|&(start, end, i, _)| (start, end, i));
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end, i, line) in spans {
        if start < at {
            return Err(EditError::Overlap { line });
        }
        out.push_str(&text[at..start]);
        out.push_str(&edits[i].text);
        at = end;
    }
    out.push_str(&text[at..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Range;
    use serde_json::json;
    use std::path::Path;

    fn edit(start: (u32, u32), end: (u32, u32), text: &str) -> TextEdit {
        let pos = |(line, character)| Position { line, character };
        TextEdit {
            range: Range {
                start: pos(start),
                end: pos(end),
            },
            text: text.into(),
        }
    }

    #[test]
    fn edits_apply_in_utf16_columns_and_keep_insert_order() {
        let text = "héllo 😀 wörld\nsecond\n";
        let out = apply_text_edits(
            text,
            &[
                edit((0, 9), (0, 14), "earth"),
                edit((1, 0), (1, 0), "A"),
                edit((1, 0), (1, 0), "B"),
                edit((0, 0), (0, 5), "bye"),
            ],
        )
        .unwrap();
        assert_eq!(out, "bye 😀 earth\nABsecond\n");
    }

    #[test]
    fn positions_past_the_line_or_the_text_are_clamped() {
        let text = "ab\r\ncd";
        assert_eq!(
            apply_text_edits(text, &[edit((0, 99), (0, 99), "!")]).unwrap(),
            "ab!\r\ncd"
        );
        assert_eq!(
            apply_text_edits(text, &[edit((1, 1), (9, 0), "")]).unwrap(),
            "ab\r\nc"
        );
    }

    #[test]
    fn overlapping_edits_are_refused_but_touching_ones_apply() {
        let text = "abcdefghij";
        assert_eq!(
            apply_text_edits(
                text,
                &[edit((0, 0), (0, 5), "X"), edit((0, 3), (0, 8), "Y")]
            ),
            Err(EditError::Overlap { line: 0 })
        );
        assert_eq!(
            apply_text_edits(
                text,
                &[edit((0, 5), (0, 8), "Y"), edit((0, 0), (0, 5), "X")]
            )
            .unwrap(),
            "XYij"
        );
        assert_eq!(
            apply_text_edits(text, &[edit((0, 2), (0, 4), ""), edit((0, 2), (0, 2), "+")]).unwrap(),
            "ab+efghij",
            "an insert where a replace starts goes before it"
        );
    }

    #[test]
    fn reads_versioned_document_changes_in_order() {
        let parsed = parse_workspace_edit(&json!({
            "documentChanges": [
                {"textDocument": {"uri": "file:///p/a.go", "version": 7},
                 "edits": [{"range": {"start": {"line": 0, "character": 0},
                                      "end": {"line": 0, "character": 1}}, "newText": "x",
                            "annotationId": "rename"}]},
                {"kind": "rename", "oldUri": "file:///p/b.go", "newUri": "file:///p/c.go",
                 "options": {"overwrite": false, "ignoreIfExists": true}},
                {"kind": "create", "uri": "file:///p/d.go"},
                {"kind": "delete", "uri": "file:///p/e.go", "options": {"ignoreIfNotExists": true}}
            ]
        }))
        .unwrap();
        assert_eq!(parsed.changes.len(), 4);
        assert!(matches!(&parsed.changes[0],
            FileChange::Edit { path, version: Some(7), edits } if path == Path::new("/p/a.go") && edits[0].text == "x"));
        assert!(matches!(&parsed.changes[1],
            FileChange::Rename { from, to, overwrite: false, ignore_if_exists: true }
                if from == Path::new("/p/b.go") && to == Path::new("/p/c.go")));
        assert!(matches!(
            &parsed.changes[2],
            FileChange::Create {
                overwrite: false,
                ..
            }
        ));
        assert!(matches!(
            &parsed.changes[3],
            FileChange::Delete {
                ignore_if_not_exists: true,
                ..
            }
        ));
    }

    #[test]
    fn reads_plain_changes_and_refuses_what_it_cannot_read() {
        let parsed = parse_workspace_edit(&json!({"changes": {
            "file:///p/z.go": [{"range": {"start": {"line": 1, "character": 0},
                                          "end": {"line": 1, "character": 0}}, "newText": "a"}],
            "file:///p/a.go": []
        }}))
        .unwrap();
        let paths: Vec<_> = parsed.changes.iter().map(change_path).collect();
        assert_eq!(paths, [Path::new("/p/a.go"), Path::new("/p/z.go")]);
        assert!(parse_workspace_edit(&json!({})).unwrap().is_empty());
        assert!(
            parse_workspace_edit(
                &json!({"documentChanges": [{"kind": "chmod", "uri": "file:///x"}]})
            )
            .is_none()
        );
        assert!(
            parse_workspace_edit(&json!({"changes": {"https://x": []}})).is_none(),
            "a non-file URI is not silently skipped"
        );
    }
}
