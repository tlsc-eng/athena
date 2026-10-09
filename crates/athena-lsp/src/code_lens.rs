use serde_json::Value;

use crate::code_action::{Command, parse_command};
use crate::protocol::{Location, Range, parse_locations};

/// A command shown above a line of code, such as "run go generate" or "3 references".
#[derive(Clone, Debug, PartialEq)]
pub struct CodeLens {
    pub range: Range,
    /// Left out until `codeLens/resolve` fills it in.
    pub command: Option<Command>,
    /// The lens as the server sent it, for `codeLens/resolve`.
    pub raw: Value,
}

impl CodeLens {
    /// The places a client-side "show references" lens lists, VS Code's
    /// `editor.action.showReferences(uri, position, locations)`; empty for any other command.
    pub fn locations(&self) -> Vec<Location> {
        let Some(command) = &self.command else {
            return Vec::new();
        };
        let shows_references = command.command.ends_with("showReferences")
            || command.command.ends_with("peekLocations");
        match (shows_references, command.arguments.as_ref()) {
            (true, Some(Value::Array(args))) => args.get(2).map_or_else(Vec::new, parse_locations),
            _ => Vec::new(),
        }
    }
}

pub(crate) fn parse_code_lens(item: &Value) -> Option<CodeLens> {
    Some(CodeLens {
        range: serde_json::from_value(item.get("range")?.clone()).ok()?,
        command: item.get("command").and_then(parse_command),
        raw: item.clone(),
    })
}

pub(crate) fn parse_code_lenses(result: &Value) -> Vec<CodeLens> {
    result
        .as_array()
        .map(|list| list.iter().filter_map(parse_code_lens).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn range(line: u32) -> Value {
        json!({"start": {"line": line, "character": 0}, "end": {"line": line, "character": 4}})
    }

    #[test]
    fn lenses_read_with_and_without_their_command() {
        let lenses = parse_code_lenses(&json!([
            {"range": range(3), "command": {"title": "run go generate", "command": "gopls.generate",
                                            "arguments": [{"Dir": "file:///p", "Recursive": false}]}},
            {"range": range(7), "data": {"kind": "references"}},
            {"command": {"title": "no range", "command": "x"}}
        ]));
        assert_eq!(lenses.len(), 2);
        assert_eq!(lenses[0].range.start.line, 3);
        let command = lenses[0].command.as_ref().unwrap();
        assert_eq!(
            (command.title.as_str(), command.command.as_str()),
            ("run go generate", "gopls.generate")
        );
        assert!(lenses[1].command.is_none());
        assert_eq!(lenses[1].raw["data"]["kind"], "references");
        assert!(lenses[0].locations().is_empty());
    }

    #[test]
    fn a_references_lens_carries_the_places_it_lists() {
        let lens = parse_code_lens(&json!({
            "range": range(1),
            "command": {"title": "2 references", "command": "editor.action.showReferences",
                        "arguments": ["file:///p/a.ts", {"line": 1, "character": 9}, [
                            {"uri": "file:///p/a.ts", "range": range(4)},
                            {"uri": "file:///p/b.ts", "range": range(8)}
                        ]]}
        }))
        .unwrap();
        let places = lens.locations();
        assert_eq!(places.len(), 2);
        assert_eq!(places[1].path, std::path::Path::new("/p/b.ts"));
        assert_eq!(places[1].range.start.line, 8);
    }
}
