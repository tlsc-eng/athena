use serde_json::Value;

use crate::edit::{WorkspaceEdit, parse_workspace_edit};

/// A command the server runs itself through `workspace/executeCommand`.
#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    pub title: String,
    pub command: String,
    pub arguments: Option<Value>,
}

/// A fix or refactoring offered at a place in the code.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeAction {
    pub title: String,
    /// Such as `quickfix`, `refactor.extract` or `source.organizeImports`.
    pub kind: Option<String>,
    pub preferred: bool,
    /// Why the action cannot run here; such actions are shown but not run.
    pub disabled: Option<String>,
    pub edit: Option<WorkspaceEdit>,
    pub command: Option<Command>,
    /// The action as the server sent it, for `codeAction/resolve`.
    pub raw: Value,
}

impl CodeAction {
    pub fn is_quickfix(&self) -> bool {
        self.kind
            .as_deref()
            .is_some_and(|k| k == "quickfix" || k.starts_with("quickfix."))
    }

    /// Whether resolving could add the edit the server left out.
    pub fn needs_resolve(&self) -> bool {
        self.edit.is_none() && self.raw.get("data").is_some()
    }
}

fn command(value: &Value) -> Option<Command> {
    Some(Command {
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        command: value.get("command")?.as_str()?.to_string(),
        arguments: value.get("arguments").cloned(),
    })
}

/// One entry of a `textDocument/codeAction` reply: a CodeAction literal or a bare Command.
pub(crate) fn parse_code_action(item: &Value) -> Option<CodeAction> {
    let title = item.get("title")?.as_str()?.to_string();
    if item.get("command").is_some_and(Value::is_string) {
        return Some(CodeAction {
            title,
            kind: None,
            preferred: false,
            disabled: None,
            edit: None,
            command: Some(command(item)?),
            raw: item.clone(),
        });
    }
    let edit = match item.get("edit") {
        Some(edit) => Some(parse_workspace_edit(edit)?),
        None => None,
    };
    Some(CodeAction {
        title,
        kind: item.get("kind").and_then(Value::as_str).map(str::to_string),
        preferred: item
            .get("isPreferred")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        disabled: item
            .pointer("/disabled/reason")
            .and_then(Value::as_str)
            .map(str::to_string),
        edit,
        command: item.get("command").and_then(command),
        raw: item.clone(),
    })
}

pub(crate) fn parse_code_actions(result: &Value) -> Vec<CodeAction> {
    result
        .as_array()
        .map(|list| list.iter().filter_map(parse_code_action).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_actions_with_edits_commands_and_bare_commands() {
        let list = parse_code_actions(&json!([
            {"title": "Add import: \"strings\"", "kind": "quickfix", "isPreferred": true,
             "edit": {"changes": {"file:///p/a.go": []}}},
            {"title": "Extract function", "kind": "refactor.extract",
             "command": {"title": "Extract function", "command": "gopls.apply_fix",
                         "arguments": [{"Fix": "extract_function"}]}},
            {"title": "Run tests", "command": "gopls.test", "arguments": []},
            {"title": "Inline", "kind": "refactor.inline", "data": {"id": 3},
             "disabled": {"reason": "not a call"}},
            {"kind": "quickfix"}
        ]));
        assert_eq!(list.len(), 4, "an action without a title is skipped");
        assert!(list[0].is_quickfix() && list[0].preferred && list[0].edit.is_some());
        assert_eq!(list[1].command.as_ref().unwrap().command, "gopls.apply_fix");
        assert_eq!(list[2].command.as_ref().unwrap().title, "Run tests");
        assert!(list[2].kind.is_none());
        assert_eq!(list[3].disabled.as_deref(), Some("not a call"));
        assert!(list[3].needs_resolve());
        assert!(parse_code_actions(&json!(null)).is_empty());
    }
}
