use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Zero-based line and UTF-16 column, as the protocol counts them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Severity,
    pub message: String,
    pub source: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Location {
    pub path: PathBuf,
    pub range: Range,
}

#[derive(Deserialize)]
struct RawDiagnostic {
    range: Range,
    severity: Option<u8>,
    message: String,
    source: Option<String>,
}

pub(crate) fn parse_diagnostics(params: &serde_json::Value) -> Option<(PathBuf, Vec<Diagnostic>)> {
    let path = path_from_uri(params.get("uri")?.as_str()?)?;
    let raw: Vec<RawDiagnostic> =
        serde_json::from_value(params.get("diagnostics")?.clone()).ok()?;
    let list = raw
        .into_iter()
        .map(|d| Diagnostic {
            range: d.range,
            severity: match d.severity {
                Some(2) => Severity::Warning,
                Some(3) => Severity::Information,
                Some(4) => Severity::Hint,
                _ => Severity::Error,
            },
            message: d.message,
            source: d.source,
        })
        .collect();
    Some((path, list))
}

/// A definition reply may be one Location, a list of them, or a list of LocationLinks.
pub(crate) fn parse_locations(result: &serde_json::Value) -> Vec<Location> {
    let items = match result {
        serde_json::Value::Array(items) => items.clone(),
        serde_json::Value::Object(_) => vec![result.clone()],
        _ => return Vec::new(),
    };
    items
        .iter()
        .filter_map(|item| {
            let uri = item
                .get("uri")
                .or_else(|| item.get("targetUri"))?
                .as_str()?;
            let range = item
                .get("range")
                .or_else(|| item.get("targetSelectionRange"))?;
            Some(Location {
                path: path_from_uri(uri)?,
                range: serde_json::from_value(range.clone()).ok()?,
            })
        })
        .collect()
}

pub fn uri_from_path(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

pub fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn uris_round_trip() {
        let path = Path::new("/Users/me/my repo/ü.go");
        let uri = uri_from_path(path);
        assert_eq!(uri, "file:///Users/me/my%20repo/%C3%BC.go");
        assert_eq!(path_from_uri(&uri).as_deref(), Some(path));
        assert_eq!(path_from_uri("https://x"), None);
    }

    #[test]
    fn reads_diagnostics_and_locations() {
        let (path, list) = parse_diagnostics(&json!({
            "uri": "file:///a/main.go",
            "diagnostics": [{"range": {"start": {"line": 2, "character": 1}, "end": {"line": 2, "character": 6}},
                             "severity": 1, "message": "\"os\" imported and not used", "source": "compiler"}]
        }))
        .unwrap();
        assert_eq!(path, Path::new("/a/main.go"));
        assert_eq!(list[0].severity, Severity::Error);
        assert_eq!(
            list[0].range.start,
            Position {
                line: 2,
                character: 1
            }
        );

        let link = json!([{"targetUri": "file:///b.ts", "targetRange": {}, "targetSelectionRange":
            {"start": {"line": 4, "character": 0}, "end": {"line": 4, "character": 3}}}]);
        assert_eq!(parse_locations(&link)[0].path, Path::new("/b.ts"));
        assert!(parse_locations(&json!(null)).is_empty());
    }
}
