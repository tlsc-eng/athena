use std::ops::Range;

use serde_json::Value;

use crate::markup::{MarkupBlock, hover_blocks};

/// The call the cursor is in: its signature, the argument being typed, and its documentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureHelp {
    pub label: String,
    /// Bytes of `label` naming the active parameter.
    pub active: Option<Range<usize>>,
    /// The first paragraph of the signature's documentation.
    pub documentation: Option<String>,
}

pub(crate) fn parse_signature_help(result: &Value) -> Option<SignatureHelp> {
    let signatures = result.get("signatures")?.as_array()?;
    let index = result
        .get("activeSignature")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let signature = signatures.get(index).or_else(|| signatures.first())?;
    let label = signature.get("label")?.as_str()?.to_string();
    let parameter = signature
        .get("activeParameter")
        .or_else(|| result.get("activeParameter"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let active = signature
        .get("parameters")
        .and_then(Value::as_array)
        .and_then(|list| list.get(parameter))
        .and_then(|p| p.get("label"))
        .and_then(|l| parameter_span(&label, l));
    let documentation = signature
        .get("documentation")
        .map(hover_blocks)
        .and_then(|blocks| {
            blocks.into_iter().find_map(|b| match b {
                MarkupBlock::Text(text) => Some(text),
                MarkupBlock::Code(_) => None,
            })
        });
    Some(SignatureHelp {
        label,
        active,
        documentation,
    })
}

/// A parameter label is either its text, found inside the parentheses, or UTF-16 offsets.
fn parameter_span(label: &str, parameter: &Value) -> Option<Range<usize>> {
    match parameter {
        Value::String(text) if !text.is_empty() => {
            let open = label.find('(').map_or(0, |i| i + 1);
            let at = label[open..].find(text.as_str())? + open;
            Some(at..at + text.len())
        }
        Value::Array(pair) => {
            let unit = |i: usize| pair.get(i)?.as_u64().map(|n| n as usize);
            let (start, end) = (unit(0)?, unit(1)?);
            let byte = |units: usize| {
                let mut seen = 0;
                for (at, c) in label.char_indices() {
                    if seen >= units {
                        return Some(at);
                    }
                    seen += c.len_utf16();
                }
                (seen >= units).then_some(label.len())
            };
            Some(byte(start)?..byte(end)?)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_the_active_parameter_by_text_or_offsets() {
        let help = parse_signature_help(&json!({
            "signatures": [{"label": "Join(elems []string, sep string) string",
                            "documentation": {"kind": "markdown", "value": "Join concatenates.\n\nMore."},
                            "parameters": [{"label": "elems []string"}, {"label": "sep string"}]}],
            "activeSignature": 0, "activeParameter": 1
        }))
        .unwrap();
        assert_eq!(&help.label[help.active.clone().unwrap()], "sep string");
        assert_eq!(help.documentation.as_deref(), Some("Join concatenates."));

        let offsets = parse_signature_help(&json!({
            "signatures": [{"label": "f(é: int, b: int)", "parameters": [{"label": [2, 8]}, {"label": [10, 16]}],
                            "activeParameter": 1}]
        }))
        .unwrap();
        assert_eq!(&offsets.label[offsets.active.unwrap()], "b: int");
        assert!(parse_signature_help(&json!(null)).is_none());
        assert!(parse_signature_help(&json!({"signatures": []})).is_none());
    }
}
