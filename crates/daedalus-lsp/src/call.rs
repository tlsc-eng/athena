use std::path::PathBuf;

use serde_json::Value;

use crate::protocol::{Range, path_from_uri};

/// A function or method in a call hierarchy; `raw` is sent back to ask for its calls.
#[derive(Clone, Debug, PartialEq)]
pub struct CallItem {
    pub name: String,
    /// The protocol's SymbolKind number.
    pub kind: u32,
    /// What the server adds, such as the package or the signature.
    pub detail: Option<String>,
    pub path: PathBuf,
    /// Where its name is.
    pub selection: Range,
    pub raw: Value,
}

/// A caller or a callee, and where the calls are: in the caller's file for incoming calls, and in
/// the asked item's file for outgoing ones.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub item: CallItem,
    pub ranges: Vec<Range>,
}

pub(crate) fn parse_item(value: &Value) -> Option<CallItem> {
    let range = |key: &str| serde_json::from_value(value.get(key)?.clone()).ok();
    Some(CallItem {
        name: value.get("name")?.as_str()?.to_string(),
        kind: value.get("kind").and_then(Value::as_u64).unwrap_or(0) as u32,
        detail: value
            .get("detail")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
            .map(str::to_string),
        path: path_from_uri(value.get("uri")?.as_str()?)?,
        selection: range("selectionRange").or_else(|| range("range"))?,
        raw: value.clone(),
    })
}

pub(crate) fn parse_items(result: &Value) -> Vec<CallItem> {
    result
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(parse_item)
        .collect()
}

/// Reads `CallHierarchyIncomingCall[]` (`key` "from") or `CallHierarchyOutgoingCall[]` ("to").
pub(crate) fn parse_calls(result: &Value, key: &str) -> Vec<Call> {
    result
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(|call| {
            Some(Call {
                item: parse_item(call.get(key)?)?,
                ranges: call
                    .get("fromRanges")
                    .and_then(|r| serde_json::from_value(r.clone()).ok())
                    .unwrap_or_default(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(name: &str, line: u32) -> Value {
        let at = json!({"start": {"line": line, "character": 5},
                        "end": {"line": line, "character": 9}});
        json!({"name": name, "kind": 12, "detail": "example.com/p • main.go",
               "uri": "file:///p/main.go", "range": at, "selectionRange": at})
    }

    #[test]
    fn incoming_and_outgoing_calls_keep_their_call_sites() {
        let site =
            json!({"start": {"line": 7, "character": 2}, "end": {"line": 7, "character": 8}});
        let incoming = parse_calls(
            &json!([{"from": item("main", 6), "fromRanges": [site]}]),
            "from",
        );
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].item.name, "main");
        assert_eq!(incoming[0].item.path, PathBuf::from("/p/main.go"));
        assert_eq!(
            incoming[0].item.detail.as_deref(),
            Some("example.com/p • main.go")
        );
        assert_eq!(incoming[0].ranges[0].start.line, 7);
        assert_eq!(incoming[0].item.raw["name"], "main");
        let outgoing = parse_calls(
            &json!([{"to": item("helper", 2), "fromRanges": []},
                                           {"to": {"name": "broken"}}]),
            "to",
        );
        assert_eq!(outgoing.len(), 1, "an item without a file is skipped");
        assert!(outgoing[0].ranges.is_empty());
        assert!(parse_items(&Value::Null).is_empty());
    }
}
