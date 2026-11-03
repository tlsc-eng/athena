use std::path::PathBuf;

use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    pub id: i64,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackFrame {
    pub id: i64,
    pub name: String,
    /// The source file, when the frame has one on disk.
    pub path: Option<PathBuf>,
    /// One-based, as the client asked for in `initialize`.
    pub line: u32,
    pub column: u32,
    /// `subtle` for runtime frames a call stack shows dimmed, as VS Code does.
    pub hint: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub name: String,
    pub variables_reference: i64,
    /// Costly to fetch, so it is left closed until asked for.
    pub expensive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variable {
    pub name: String,
    pub value: String,
    pub type_name: Option<String>,
    /// Non-zero when the value has children to fetch with `variables`.
    pub variables_reference: i64,
}

/// A breakpoint as `setBreakpoints` sends it; lines are one-based.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceBreakpoint {
    pub line: u32,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    /// Set for a logpoint, which prints this instead of stopping.
    pub log_message: Option<String>,
}

/// What the adapter made of a breakpoint it was sent, in the order they were sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BreakpointStatus {
    pub id: Option<i64>,
    pub verified: bool,
    pub line: Option<u32>,
    pub message: Option<String>,
}

/// Why execution paused, from a `stopped` event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stopped {
    /// `breakpoint`, `step`, `pause`, `exception`, `entry`, …
    pub reason: String,
    pub thread_id: Option<i64>,
    pub all_threads_stopped: bool,
    pub description: Option<String>,
    pub text: Option<String>,
}

/// The value of an expression evaluated in a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evaluated {
    pub result: String,
    pub type_name: Option<String>,
    pub variables_reference: i64,
}

fn string(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn int(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(Value::as_i64)
}

fn list<'a>(body: &'a Value, key: &str) -> &'a [Value] {
    body.get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

pub(crate) fn parse_threads(body: &Value) -> Vec<Thread> {
    list(body, "threads")
        .iter()
        .filter_map(|t| {
            Some(Thread {
                id: int(t, "id")?,
                name: string(t, "name").unwrap_or_default(),
            })
        })
        .collect()
}

pub(crate) fn parse_frames(body: &Value) -> Vec<StackFrame> {
    list(body, "stackFrames")
        .iter()
        .filter_map(|f| {
            let source = f.get("source");
            Some(StackFrame {
                id: int(f, "id")?,
                name: string(f, "name").unwrap_or_default(),
                path: source
                    .and_then(|s| string(s, "path"))
                    .filter(|p| !p.is_empty())
                    .map(PathBuf::from),
                line: int(f, "line").unwrap_or(0).max(0) as u32,
                column: int(f, "column").unwrap_or(0).max(0) as u32,
                hint: string(f, "presentationHint")
                    .or_else(|| source.and_then(|s| string(s, "presentationHint"))),
            })
        })
        .collect()
}

pub(crate) fn parse_scopes(body: &Value) -> Vec<Scope> {
    list(body, "scopes")
        .iter()
        .filter_map(|s| {
            Some(Scope {
                name: string(s, "name")?,
                variables_reference: int(s, "variablesReference")?,
                expensive: s.get("expensive").and_then(Value::as_bool) == Some(true),
            })
        })
        .collect()
}

pub(crate) fn parse_variables(body: &Value) -> Vec<Variable> {
    list(body, "variables")
        .iter()
        .filter_map(|v| {
            Some(Variable {
                name: string(v, "name")?,
                value: string(v, "value").unwrap_or_default(),
                type_name: string(v, "type").filter(|t| !t.is_empty()),
                variables_reference: int(v, "variablesReference").unwrap_or(0),
            })
        })
        .collect()
}

pub(crate) fn parse_breakpoints(body: &Value) -> Vec<BreakpointStatus> {
    list(body, "breakpoints")
        .iter()
        .map(parse_breakpoint)
        .collect()
}

pub(crate) fn parse_breakpoint(b: &Value) -> BreakpointStatus {
    BreakpointStatus {
        id: int(b, "id"),
        verified: b.get("verified").and_then(Value::as_bool) == Some(true),
        line: int(b, "line").map(|l| l.max(0) as u32),
        message: string(b, "message"),
    }
}

pub(crate) fn parse_stopped(body: &Value) -> Stopped {
    Stopped {
        reason: string(body, "reason").unwrap_or_default(),
        thread_id: int(body, "threadId"),
        all_threads_stopped: body.get("allThreadsStopped").and_then(Value::as_bool) == Some(true),
        description: string(body, "description"),
        text: string(body, "text"),
    }
}

pub(crate) fn parse_evaluated(body: &Value) -> Evaluated {
    Evaluated {
        result: string(body, "result").unwrap_or_default(),
        type_name: string(body, "type").filter(|t| !t.is_empty()),
        variables_reference: int(body, "variablesReference").unwrap_or(0),
    }
}

impl SourceBreakpoint {
    pub(crate) fn to_json(&self) -> Value {
        let mut b = json!({"line": self.line});
        let set = |b: &mut Value, key: &str, value: &Option<String>| {
            if let Some(v) = value.as_ref().filter(|v| !v.trim().is_empty()) {
                b[key] = json!(v);
            }
        };
        set(&mut b, "condition", &self.condition);
        set(&mut b, "hitCondition", &self.hit_condition);
        set(&mut b, "logMessage", &self.log_message);
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delve_replies_parse_into_frames_scopes_and_variables() {
        let frames = parse_frames(&json!({"stackFrames": [
            {"id": 1000, "name": "main.main", "line": 11, "column": 3,
             "source": {"name": "main.go", "path": "/m/main.go"}},
            {"id": 1001, "name": "runtime.main", "line": 283, "column": 0,
             "source": {"path": "/go/src/runtime/proc.go"}, "presentationHint": "subtle"},
            {"id": 1002, "name": "runtime.goexit", "line": 0, "source": {"path": ""}},
            {"name": "no id"}
        ], "totalFrames": 3}));
        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames[0].path.as_deref(),
            Some(std::path::Path::new("/m/main.go"))
        );
        assert_eq!((frames[0].line, frames[0].column), (11, 3));
        assert_eq!(frames[1].hint.as_deref(), Some("subtle"));
        assert_eq!(frames[2].path, None);

        let scopes = parse_scopes(&json!({"scopes": [
            {"name": "Locals", "variablesReference": 1000, "expensive": false},
            {"name": "Globals (package main)", "variablesReference": 1001, "expensive": true}
        ]}));
        assert!(!scopes[0].expensive && scopes[1].expensive);

        let vars = parse_variables(&json!({"variables": [
            {"name": "p", "value": "main.point {X: 1, Y: 2}", "type": "main.point",
             "variablesReference": 1002},
            {"name": "total", "value": "0", "type": "", "variablesReference": 0}
        ]}));
        assert_eq!(vars[0].type_name.as_deref(), Some("main.point"));
        assert_eq!(vars[1].type_name, None);
        assert_eq!(vars[0].variables_reference, 1002);
    }

    #[test]
    fn breakpoints_send_only_what_is_set_and_read_back_verification() {
        let plain = SourceBreakpoint {
            line: 7,
            ..Default::default()
        };
        assert_eq!(plain.to_json(), json!({"line": 7}));
        let logpoint = SourceBreakpoint {
            line: 9,
            condition: Some("i > 1".into()),
            hit_condition: Some(" ".into()),
            log_message: Some("i is {i}".into()),
        };
        assert_eq!(
            logpoint.to_json(),
            json!({"line": 9, "condition": "i > 1", "logMessage": "i is {i}"})
        );
        let status = parse_breakpoints(&json!({"breakpoints": [
            {"id": 1, "verified": true, "line": 7},
            {"verified": false, "message": "could not find file"}
        ]}));
        assert!(status[0].verified && !status[1].verified);
        assert_eq!(status[1].message.as_deref(), Some("could not find file"));
    }

    #[test]
    fn stopped_events_and_evaluations_read_their_fields() {
        let stopped = parse_stopped(&json!({"reason": "breakpoint", "threadId": 1,
                                           "allThreadsStopped": true}));
        assert_eq!(stopped.reason, "breakpoint");
        assert_eq!(stopped.thread_id, Some(1));
        assert!(stopped.all_threads_stopped);
        let evaluated = parse_evaluated(&json!({"result": "3", "type": "int",
                                               "variablesReference": 0}));
        assert_eq!(evaluated.result, "3");
        assert_eq!(
            parse_threads(&json!({"threads": [{"id": 1, "name": "* main"}]}))[0].id,
            1
        );
    }
}
