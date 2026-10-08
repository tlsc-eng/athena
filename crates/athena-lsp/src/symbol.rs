use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::protocol::{Range, path_from_uri};

/// A named thing in the code: a function, type, field and so on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    /// The protocol's SymbolKind number.
    pub kind: u32,
    /// What holds it, such as the type of a method; shown dimmed beside the name.
    pub container: Option<String>,
    pub path: PathBuf,
    /// Where to put the cursor: the name itself when the server says.
    pub range: Range,
}

/// Reads `DocumentSymbol[]` (flattened, parents first) or `SymbolInformation[]`.
pub(crate) fn parse_symbols(result: &Value, path: Option<&Path>) -> Vec<Symbol> {
    let mut out = Vec::new();
    for item in result.as_array().map_or(&[][..], Vec::as_slice) {
        walk(item, path, None, &mut out);
    }
    out
}

fn walk(item: &Value, path: Option<&Path>, parent: Option<&str>, out: &mut Vec<Symbol>) {
    let Some(name) = item.get("name").and_then(Value::as_str) else {
        return;
    };
    let kind = item.get("kind").and_then(Value::as_u64).unwrap_or(0) as u32;
    let location = item.get("location");
    let file = location
        .and_then(|l| l.get("uri"))
        .and_then(Value::as_str)
        .and_then(path_from_uri)
        .or_else(|| path.map(Path::to_path_buf));
    let range = item
        .get("selectionRange")
        .or_else(|| location.and_then(|l| l.get("range")))
        .or_else(|| item.get("range"))
        .and_then(|r| serde_json::from_value(r.clone()).ok());
    let container = item
        .get("containerName")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .or(parent)
        .map(str::to_string);
    if let (Some(path), Some(range)) = (file, range) {
        out.push(Symbol {
            name: name.to_string(),
            kind,
            container,
            path,
            range,
        });
    }
    for child in item
        .get("children")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
    {
        walk(child, path, Some(name), out);
    }
}

/// A short word for a SymbolKind, as the palette shows beside each name.
pub fn symbol_kind_label(kind: u32) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type parameter",
        _ => "symbol",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn document_symbols_flatten_with_their_parent_as_container() {
        let list = parse_symbols(
            &json!([{
                "name": "Server", "kind": 23,
                "range": {"start": {"line": 2, "character": 0}, "end": {"line": 9, "character": 1}},
                "selectionRange": {"start": {"line": 2, "character": 5}, "end": {"line": 2, "character": 11}},
                "children": [{
                    "name": "addr", "kind": 8,
                    "range": {"start": {"line": 3, "character": 1}, "end": {"line": 3, "character": 12}},
                    "selectionRange": {"start": {"line": 3, "character": 1}, "end": {"line": 3, "character": 5}}
                }]
            }]),
            Some(Path::new("/p/s.go")),
        );
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "Server");
        assert_eq!(
            list[0].range.start.character, 5,
            "the name, not the whole body"
        );
        assert_eq!(list[1].container.as_deref(), Some("Server"));
        assert_eq!(list[1].path, Path::new("/p/s.go"));
    }

    #[test]
    fn symbol_information_carries_its_own_file() {
        let list = parse_symbols(
            &json!([{"name": "Run", "kind": 12, "containerName": "main",
                     "location": {"uri": "file:///p/main.go",
                                  "range": {"start": {"line": 4, "character": 5},
                                            "end": {"line": 4, "character": 8}}}},
                    {"name": "broken", "kind": 12}]),
            None,
        );
        assert_eq!(list.len(), 1, "an entry without a place is skipped");
        assert_eq!(list[0].path, Path::new("/p/main.go"));
        assert_eq!(list[0].container.as_deref(), Some("main"));
        assert_eq!(symbol_kind_label(list[0].kind), "function");
    }
}
