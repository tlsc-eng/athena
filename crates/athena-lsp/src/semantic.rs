use serde_json::Value;

/// The token types and modifiers a server names, by the indexes its token data uses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SemanticLegend {
    pub types: Vec<String>,
    pub modifiers: Vec<String>,
}

impl SemanticLegend {
    pub(crate) fn parse(legend: &Value) -> Option<Self> {
        let names = |key: &str| -> Vec<String> {
            legend
                .get(key)
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        };
        let types = names("tokenTypes");
        (!types.is_empty()).then(|| Self {
            types,
            modifiers: names("tokenModifiers"),
        })
    }

    /// The bit a modifier sets in [`SemanticToken::modifiers`]; 0 if the server never uses it.
    pub fn modifier_bit(&self, name: &str) -> u32 {
        self.modifiers
            .iter()
            .position(|m| m == name)
            .filter(|&i| i < 32)
            .map_or(0, |i| 1 << i)
    }
}

/// One classified range, in zero-based lines and UTF-16 columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticToken {
    pub line: u32,
    pub start: u32,
    pub length: u32,
    /// An index into [`SemanticLegend::types`].
    pub kind: u32,
    /// A bit set over [`SemanticLegend::modifiers`].
    pub modifiers: u32,
}

/// A file's tokens as the server encodes them, five numbers each, and the id a later delta
/// request names them by.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SemanticTokens {
    pub result_id: Option<String>,
    pub data: Vec<u32>,
}

/// Replace `delete` numbers at `start` of the previous data with `data`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticEdit {
    pub start: usize,
    pub delete: usize,
    pub data: Vec<u32>,
}

/// What a full or delta request brought back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SemanticReply {
    Full(SemanticTokens),
    Delta {
        result_id: Option<String>,
        edits: Vec<SemanticEdit>,
    },
}

/// A semantic tokens reply as received, read by [`SemanticAnswer::parse`] wherever that is
/// cheapest; a large file's data runs to hundreds of thousands of numbers.
#[derive(Debug)]
pub struct SemanticAnswer(pub(crate) Value);

impl SemanticAnswer {
    pub fn parse(self) -> Option<SemanticReply> {
        parse_reply(&self.0)
    }
}

fn numbers(value: Option<&Value>) -> Option<Vec<u32>> {
    value?
        .as_array()?
        .iter()
        .map(|n| n.as_u64().and_then(|n| u32::try_from(n).ok()))
        .collect()
}

fn parse_reply(result: &Value) -> Option<SemanticReply> {
    let result_id = result
        .get("resultId")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(edits) = result.get("edits").and_then(Value::as_array) {
        let edits = edits
            .iter()
            .map(|e| {
                Some(SemanticEdit {
                    start: e.get("start")?.as_u64()? as usize,
                    delete: e.get("deleteCount")?.as_u64()? as usize,
                    data: numbers(e.get("data")).unwrap_or_default(),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        return Some(SemanticReply::Delta { result_id, edits });
    }
    Some(SemanticReply::Full(SemanticTokens {
        result_id,
        data: numbers(result.get("data"))?,
    }))
}

/// The data a delta's edits make of `previous`; `None` if an edit reaches past its end.
pub fn apply_semantic_edits(previous: &[u32], edits: &[SemanticEdit]) -> Option<Vec<u32>> {
    let mut sorted: Vec<&SemanticEdit> = edits.iter().collect();
    sorted.sort_by_key(|e| e.start);
    let mut out = Vec::with_capacity(previous.len());
    let mut at = 0;
    for edit in sorted {
        if edit.start < at || edit.start + edit.delete > previous.len() {
            return None;
        }
        out.extend_from_slice(&previous[at..edit.start]);
        out.extend_from_slice(&edit.data);
        at = edit.start + edit.delete;
    }
    out.extend_from_slice(&previous[at..]);
    Some(out)
}

/// Absolute tokens from the protocol's relative encoding; a trailing partial token is ignored.
pub fn decode_semantic_tokens(data: &[u32]) -> Vec<SemanticToken> {
    let (mut line, mut start) = (0u32, 0u32);
    data.as_chunks::<5>()
        .0
        .iter()
        .map(|t| {
            if t[0] > 0 {
                line = line.saturating_add(t[0]);
                start = t[1];
            } else {
                start = start.saturating_add(t[1]);
            }
            SemanticToken {
                line,
                start,
                length: t[2],
                kind: t[3],
                modifiers: t[4],
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn relative_tokens_become_absolute_lines_and_columns() {
        let data = [2, 5, 3, 0, 3, 0, 5, 4, 1, 0, 3, 2, 7, 2, 0, 9];
        let tokens = decode_semantic_tokens(&data);
        assert_eq!(
            tokens,
            [
                SemanticToken {
                    line: 2,
                    start: 5,
                    length: 3,
                    kind: 0,
                    modifiers: 3
                },
                SemanticToken {
                    line: 2,
                    start: 10,
                    length: 4,
                    kind: 1,
                    modifiers: 0
                },
                SemanticToken {
                    line: 5,
                    start: 2,
                    length: 7,
                    kind: 2,
                    modifiers: 0
                },
            ]
        );
    }

    #[test]
    fn delta_edits_splice_the_previous_data_in_order() {
        let previous = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let edits = [
            SemanticEdit {
                start: 8,
                delete: 2,
                data: vec![],
            },
            SemanticEdit {
                start: 0,
                delete: 1,
                data: vec![20, 21],
            },
            SemanticEdit {
                start: 5,
                delete: 0,
                data: vec![50],
            },
        ];
        assert_eq!(
            apply_semantic_edits(&previous, &edits),
            Some(vec![20, 21, 2, 3, 4, 5, 50, 6, 7, 8])
        );
        let past_end = [SemanticEdit {
            start: 9,
            delete: 3,
            data: vec![],
        }];
        assert_eq!(apply_semantic_edits(&previous, &past_end), None);
    }

    #[test]
    fn replies_read_as_full_data_or_delta_edits() {
        let full = SemanticAnswer(json!({"resultId": "4", "data": [0, 1, 2, 3, 0]}));
        assert_eq!(
            full.parse(),
            Some(SemanticReply::Full(SemanticTokens {
                result_id: Some("4".into()),
                data: vec![0, 1, 2, 3, 0]
            }))
        );
        let delta = SemanticAnswer(
            json!({"resultId": "5", "edits": [{"start": 5, "deleteCount": 5, "data": [1, 0, 2, 1, 0]}]}),
        );
        assert_eq!(
            delta.parse(),
            Some(SemanticReply::Delta {
                result_id: Some("5".into()),
                edits: vec![SemanticEdit {
                    start: 5,
                    delete: 5,
                    data: vec![1, 0, 2, 1, 0]
                }]
            })
        );
        assert_eq!(SemanticAnswer(json!(null)).parse(), None);
        assert_eq!(SemanticAnswer(json!({"data": [1, -1]})).parse(), None);
    }

    #[test]
    fn the_legend_names_types_and_modifier_bits() {
        let legend = SemanticLegend::parse(&json!({
            "tokenTypes": ["namespace", "type", "parameter"],
            "tokenModifiers": ["declaration", "readonly", "defaultLibrary"]
        }))
        .unwrap();
        assert_eq!(legend.types[2], "parameter");
        assert_eq!(legend.modifier_bit("readonly"), 0b10);
        assert_eq!(legend.modifier_bit("defaultLibrary"), 0b100);
        assert_eq!(legend.modifier_bit("deprecated"), 0);
        assert!(SemanticLegend::parse(&json!({"tokenTypes": []})).is_none());
    }
}
