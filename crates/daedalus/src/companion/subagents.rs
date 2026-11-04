//! Tracks the sub-agents a Claude session launched, from its transcript and the agents' own files.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::protocol::{Subagent, SubagentState};
use super::transcript::{SUMMARY_CHARS, clip_chars, flag, millis};
use crate::transcripts::next_line;

const MAX_SENT: usize = 20;
const MAX_TRACKED: usize = 256;
const NAME_CHARS: usize = 64;
const NOTIFICATION_OPEN: &str = "<task-notification>";
const NOTIFICATION_CLOSE: &str = "</task-notification>";

#[derive(Debug)]
struct Entry {
    sub: Subagent,
    offset: u64,
}

/// The sub-agents of one parent transcript, fed by its lines and by tails of the agents' files.
#[derive(Debug)]
pub(crate) struct Fleet {
    dir: PathBuf,
    entries: Vec<Entry>,
    by_tool: HashMap<String, usize>,
    by_agent: HashMap<String, usize>,
    seen: HashSet<u64>,
    changed: bool,
}

impl Fleet {
    pub(crate) fn new(transcript: &Path) -> Self {
        Fleet {
            dir: transcript.with_extension("").join("subagents"),
            entries: Vec::new(),
            by_tool: HashMap::new(),
            by_agent: HashMap::new(),
            seen: HashSet::new(),
            changed: false,
        }
    }

    /// Takes one parsed line of the parent transcript, whatever its type.
    pub(crate) fn line(&mut self, line: &Value) {
        if flag(line, "isSidechain") {
            return;
        }
        let at = line["timestamp"].as_str().map_or(0, millis);
        for text in carried_text(line) {
            for block in notifications(text) {
                self.notification(block, at);
            }
        }
        let content = line["message"]["content"].as_array();
        match line["type"].as_str() {
            Some("assistant") => {
                for block in content.into_iter().flatten() {
                    if block["type"] == "tool_use"
                        && matches!(block["name"].as_str(), Some("Agent" | "Task"))
                    {
                        self.launch(block, at);
                    }
                }
            }
            Some("user") => {
                for block in content.into_iter().flatten() {
                    if block["type"] == "tool_result" {
                        self.result(block, &line["toolUseResult"], at);
                    }
                }
            }
            _ => {}
        }
    }

    /// Reads what the running agents wrote to their own files since the last call.
    pub(crate) fn read_agents(&mut self) {
        for i in 0..self.entries.len() {
            let sub = &self.entries[i].sub;
            if sub.state != SubagentState::Running {
                continue;
            }
            let Some(id) = &sub.agent_id else {
                continue;
            };
            let path = self.dir.join(format!("agent-{id}.jsonl"));
            self.tail(i, &path);
        }
    }

    pub(crate) fn running(&self) -> bool {
        self.entries
            .iter()
            .any(|e| e.sub.state == SubagentState::Running)
    }

    /// Running agents first, newest start first, then ended ones, newest end first.
    pub(crate) fn list(&self) -> Vec<Subagent> {
        let mut subs: Vec<&Subagent> = self.entries.iter().map(|e| &e.sub).collect();
        subs.sort_by_key(|s| match s.state {
            SubagentState::Running => (false, std::cmp::Reverse(s.started_at)),
            _ => (true, std::cmp::Reverse(s.ended_at.unwrap_or(s.started_at))),
        });
        subs.into_iter().take(MAX_SENT).cloned().collect()
    }

    /// True once since the last call if the list would now read differently.
    pub(crate) fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.by_tool.clear();
        self.by_agent.clear();
        self.seen.clear();
        self.changed = false;
    }

    fn launch(&mut self, block: &Value, at: u64) {
        let Some(id) = block["id"].as_str() else {
            return;
        };
        if self.by_tool.contains_key(id) {
            return;
        }
        let input = &block["input"];
        let (description, _) = clip_chars(
            input["description"].as_str().unwrap_or_default(),
            SUMMARY_CHARS,
        );
        let (agent_type, _) = clip_chars(
            input["subagent_type"].as_str().unwrap_or_default(),
            NAME_CHARS,
        );
        if self.entries.len() >= MAX_TRACKED {
            self.evict();
        }
        self.by_tool.insert(id.to_string(), self.entries.len());
        self.entries.push(Entry {
            sub: Subagent {
                tool_id: id.to_string(),
                agent_id: None,
                agent_type,
                description,
                state: SubagentState::Running,
                started_at: at,
                ended_at: None,
                active_at: None,
                tools: 0,
                tool: None,
            },
            offset: 0,
        });
        self.changed = true;
    }

    fn evict(&mut self) {
        let oldest = self
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, e)| {
                let s = &e.sub;
                (
                    s.state == SubagentState::Running,
                    s.ended_at.unwrap_or(s.started_at),
                )
            })
            .map(|(i, _)| i);
        let Some(i) = oldest else {
            return;
        };
        self.entries.remove(i);
        self.by_tool.clear();
        self.by_agent.clear();
        for (i, e) in self.entries.iter().enumerate() {
            self.by_tool.insert(e.sub.tool_id.clone(), i);
            if let Some(agent) = &e.sub.agent_id {
                self.by_agent.insert(agent.clone(), i);
            }
        }
    }

    fn result(&mut self, block: &Value, outcome: &Value, at: u64) {
        let tool_id = block["tool_use_id"].as_str().unwrap_or_default();
        if let Some(&i) = self.by_tool.get(tool_id) {
            if flag(block, "is_error") {
                self.finish(i, SubagentState::Failed, at, 0);
            } else if outcome["status"] == "completed" {
                let tools = count(&outcome["totalToolUseCount"]);
                self.finish(i, SubagentState::Done, at, tools);
            }
            if let Some(agent) = agent_id(&outcome["agentId"]) {
                self.bind(i, agent);
            }
        }
        if let Some(agent) = agent_id(&outcome["resumedAgentId"])
            && let Some(&i) = self.by_agent.get(agent)
        {
            self.edit(i, |s| {
                s.state = SubagentState::Running;
                s.started_at = at;
                s.ended_at = None;
                s.tool = None;
                s.tools = 0;
            });
            self.entries[i].offset = 0;
        }
    }

    fn notification(&mut self, block: &str, at: u64) {
        let mut hasher = DefaultHasher::new();
        block.hash(&mut hasher);
        let hash = hasher.finish();
        if self.seen.contains(&hash) {
            return;
        }
        let found = tag(block, "task-id")
            .and_then(|id| self.by_agent.get(id))
            .or_else(|| tag(block, "tool-use-id").and_then(|id| self.by_tool.get(id)))
            .copied();
        let Some(i) = found else {
            return;
        };
        if self.entries[i].sub.state != SubagentState::Running {
            return;
        }
        let state = match tag(block, "status") {
            Some("completed") => SubagentState::Done,
            Some("failed") => SubagentState::Failed,
            Some("killed" | "stopped") => SubagentState::Stopped,
            _ => return,
        };
        let usage = block.rfind("</result>").map_or(block, |end| &block[end..]);
        let tools = tag(usage, "tool_uses")
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        self.seen.insert(hash);
        self.finish(i, state, at, tools);
    }

    fn finish(&mut self, i: usize, state: SubagentState, at: u64, tools: u32) {
        self.edit(i, |s| {
            s.state = state;
            s.ended_at = Some(at);
            s.tool = None;
            s.tools = s.tools.max(tools);
        });
    }

    fn bind(&mut self, i: usize, agent: &str) {
        self.by_agent.insert(agent.to_string(), i);
        self.edit(i, |s| s.agent_id = Some(agent.to_string()));
    }

    fn edit(&mut self, i: usize, change: impl FnOnce(&mut Subagent)) {
        let sub = &mut self.entries[i].sub;
        let before = sub.clone();
        change(sub);
        let differs = *sub != before;
        self.changed |= differs;
    }

    fn tail(&mut self, i: usize, path: &Path) {
        let Ok(file) = File::open(path) else {
            return;
        };
        let Ok(len) = file.metadata().map(|m| m.len()) else {
            return;
        };
        let mut sub = self.entries[i].sub.clone();
        let mut offset = self.entries[i].offset;
        if len < offset {
            offset = 0;
            sub.tools = 0;
            sub.tool = None;
            sub.active_at = None;
        }
        let mut input = BufReader::new(file);
        if input.seek(SeekFrom::Start(offset)).is_err() {
            return;
        }
        let mut buf = Vec::new();
        while let Some(n) = next_line(&mut input, &mut buf) {
            offset += n;
            let Ok(line) = serde_json::from_slice::<Value>(&buf) else {
                continue;
            };
            if line["type"] != "assistant" {
                continue;
            }
            if let Some(at) = line["timestamp"].as_str().map(millis).filter(|&at| at > 0) {
                sub.active_at = Some(at);
            }
            for block in line["message"]["content"].as_array().into_iter().flatten() {
                if block["type"] == "tool_use" {
                    sub.tools = sub.tools.saturating_add(1);
                    if let Some(name) = block["name"].as_str() {
                        sub.tool = Some(clip_chars(name, NAME_CHARS).0);
                    }
                }
            }
        }
        self.changed |= sub != self.entries[i].sub;
        self.entries[i] = Entry { sub, offset };
    }
}

/// Text that may hold task notifications: the queue entry, the queued-command attachment or a user turn.
fn carried_text(line: &Value) -> Vec<&str> {
    match line["type"].as_str() {
        Some("queue-operation") if line["operation"] == "enqueue" => {
            line["content"].as_str().into_iter().collect()
        }
        Some("attachment") if line["attachment"]["type"] == "queued_command" => {
            line["attachment"]["prompt"].as_str().into_iter().collect()
        }
        Some("user") => match &line["message"]["content"] {
            Value::String(text) => vec![text],
            Value::Array(blocks) => blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn notifications(mut text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    while let Some(start) = text.find(NOTIFICATION_OPEN) {
        let rest = &text[start..];
        let first_close = rest.find(NOTIFICATION_CLOSE);
        let quoted = match (rest.find("<result>"), first_close) {
            (Some(open), Some(close)) if open < close => rest[open..]
                .find("</result>")
                .map_or(0, |end| open + end + "</result>".len()),
            _ => 0,
        };
        let Some(end) = rest[quoted..]
            .find(NOTIFICATION_CLOSE)
            .map(|end| quoted + end)
        else {
            break;
        };
        let end = end + NOTIFICATION_CLOSE.len();
        found.push(&rest[..end]);
        text = &rest[end..];
    }
    found
}

fn tag<'a>(block: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = block.find(&open)? + open.len();
    let len = block[start..].find(&format!("</{name}>"))?;
    Some(block[start..start + len].trim())
}

/// An agent id is a file stem, so anything outside this charset is refused.
fn agent_id(value: &Value) -> Option<&str> {
    value.as_str().filter(|id| {
        (1..=64).contains(&id.len())
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

fn count(value: &Value) -> u32 {
    value
        .as_u64()
        .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::io::Write;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("daedalus-fleet-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn stamp(sec: u32) -> String {
        format!("2026-01-01T00:00:{sec:02}.000Z")
    }

    fn millis_at(sec: u32) -> u64 {
        millis(&stamp(sec))
    }

    fn launch(tool_id: &str, name: &str, sec: u32) -> Value {
        json!({
            "type": "assistant",
            "timestamp": stamp(sec),
            "message": {"content": [{
                "type": "tool_use", "id": tool_id, "name": name,
                "input": {
                    "description": "Check the parser",
                    "subagent_type": "builder",
                    "prompt": "SECRET-PROMPT do the thing",
                },
            }]},
        })
    }

    fn result(tool_id: &str, outcome: Value, is_error: bool, sec: u32) -> Value {
        let mut block =
            json!({"type": "tool_result", "tool_use_id": tool_id, "content": "SECRET-BLOCK"});
        if is_error {
            block["is_error"] = json!(true);
        }
        json!({
            "type": "user",
            "timestamp": stamp(sec),
            "message": {"content": [block]},
            "toolUseResult": outcome,
        })
    }

    fn launched(tool_id: &str, agent: &str, sec: u32) -> Value {
        result(
            tool_id,
            json!({"status": "async_launched", "agentId": agent, "prompt": "SECRET-PROMPT"}),
            false,
            sec,
        )
    }

    fn notification(task: &str, tool_id: &str, status: &str, uses: u32) -> String {
        format!(
            "<task-notification>\n<task-id>{task}</task-id>\n<tool-use-id>{tool_id}</tool-use-id>\n\
             <output-file>/SECRET-PATH</output-file>\n<status>{status}</status>\n\
             <summary>SECRET-SUMMARY</summary>\n<result>SECRET-RESULT</result>\n\
             <usage><tool_uses>{uses}</tool_uses></usage>\n</task-notification>"
        )
    }

    fn enqueue(text: &str, sec: u32) -> Value {
        json!({"type": "queue-operation", "operation": "enqueue", "timestamp": stamp(sec), "content": text})
    }

    fn fleet(name: &str) -> (Fleet, PathBuf) {
        let dir = temp(name);
        (Fleet::new(&dir.join("s1.jsonl")), dir)
    }

    fn only(fleet: &Fleet) -> Subagent {
        let list = fleet.list();
        assert_eq!(list.len(), 1);
        list[0].clone()
    }

    fn agent_line(tool: &str, sec: u32) -> Value {
        json!({
            "type": "assistant",
            "isSidechain": true,
            "timestamp": stamp(sec),
            "message": {"content": [
                {"type": "text", "text": "SECRET-AGENT-TEXT"},
                {"type": "tool_use", "id": "x", "name": tool, "input": {}},
            ]},
        })
    }

    fn append(path: &Path, lines: &[Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    #[test]
    fn a_background_agent_runs_until_its_notification_arrives() {
        let (mut fleet, _dir) = fleet("lifecycle");
        fleet.line(&launch("toolu_1", "Agent", 1));
        let sub = only(&fleet);
        assert_eq!(sub.state, SubagentState::Running);
        assert_eq!(sub.started_at, millis_at(1));
        assert_eq!(
            (sub.agent_type.as_str(), sub.description.as_str()),
            ("builder", "Check the parser")
        );
        assert!(fleet.take_changed());

        fleet.line(&launched("toolu_1", "a0123456789abcdef", 2));
        let sub = only(&fleet);
        assert_eq!(sub.state, SubagentState::Running);
        assert_eq!(sub.agent_id.as_deref(), Some("a0123456789abcdef"));
        assert!(fleet.running());

        fleet.take_changed();
        fleet.line(&enqueue(
            &notification("a0123456789abcdef", "toolu_1", "completed", 9),
            5,
        ));
        let sub = only(&fleet);
        assert_eq!(sub.state, SubagentState::Done);
        assert_eq!(sub.ended_at, Some(millis_at(5)));
        assert_eq!(sub.tools, 9);
        assert!(fleet.take_changed());
        assert!(!fleet.running());
    }

    #[test]
    fn a_delivered_copy_of_a_notification_changes_nothing() {
        let (mut fleet, _dir) = fleet("dedupe");
        fleet.line(&launch("toolu_1", "Agent", 1));
        fleet.line(&launched("toolu_1", "a1", 2));
        let text = notification("a1", "toolu_1", "completed", 4);
        fleet.line(&enqueue(&text, 5));
        assert!(fleet.take_changed());
        let before = only(&fleet);

        fleet.line(&json!({
            "type": "attachment", "timestamp": stamp(6),
            "attachment": {"type": "queued_command", "prompt": text, "origin": {"kind": "task-notification"}},
        }));
        fleet.line(&json!({
            "type": "user", "timestamp": stamp(7), "origin": {"kind": "task-notification"},
            "message": {"content": [{"type": "text", "text": text}]},
        }));
        fleet.line(&json!({
            "type": "user", "timestamp": stamp(8), "message": {"content": text},
        }));
        assert!(!fleet.take_changed());
        assert_eq!(only(&fleet), before);
    }

    #[test]
    fn a_notification_from_a_delivered_line_alone_still_ends_the_agent() {
        let (mut fleet, _dir) = fleet("carriers");
        fleet.line(&launch("toolu_1", "Agent", 1));
        fleet.line(&launched("toolu_1", "a1", 2));
        fleet.line(&json!({
            "type": "attachment", "timestamp": stamp(6),
            "attachment": {"type": "queued_command", "prompt": notification("a1", "toolu_1", "completed", 2)},
        }));
        assert_eq!(only(&fleet).state, SubagentState::Done);
    }

    #[test]
    fn a_foreground_result_ends_the_agent_at_once() {
        let (mut fleet, _dir) = fleet("foreground");
        fleet.line(&launch("toolu_1", "Agent", 1));
        fleet.line(&result(
            "toolu_1",
            json!({"status": "completed", "agentId": "a1", "totalToolUseCount": 6, "content": "SECRET-CONTENT"}),
            false,
            4,
        ));
        let sub = only(&fleet);
        assert_eq!(sub.state, SubagentState::Done);
        assert_eq!(sub.tools, 6);
        assert_eq!(sub.ended_at, Some(millis_at(4)));
        assert_eq!(sub.agent_id.as_deref(), Some("a1"));

        fleet.line(&launch("toolu_2", "Agent", 5));
        fleet.line(&result("toolu_2", json!("denied: SECRET-ERROR"), true, 6));
        let failed = fleet
            .list()
            .into_iter()
            .find(|s| s.tool_id == "toolu_2")
            .unwrap();
        assert_eq!(failed.state, SubagentState::Failed);
        assert_eq!(failed.agent_id, None);
    }

    #[test]
    fn notification_statuses_map_to_states_and_unknown_ones_are_ignored() {
        for (status, want) in [
            ("failed", SubagentState::Failed),
            ("killed", SubagentState::Stopped),
            ("stopped", SubagentState::Stopped),
            ("paused", SubagentState::Running),
        ] {
            let (mut fleet, _dir) = fleet("statuses");
            fleet.line(&launch("toolu_1", "Agent", 1));
            fleet.line(&launched("toolu_1", "a1", 2));
            fleet.take_changed();
            fleet.line(&enqueue(&notification("a1", "toolu_1", status, 0), 3));
            assert_eq!(only(&fleet).state, want, "{status}");
            assert_eq!(fleet.take_changed(), want != SubagentState::Running);
        }
    }

    #[test]
    fn a_notification_finds_its_agent_by_tool_use_id_when_the_task_id_is_unknown() {
        let (mut fleet, _dir) = fleet("fallback");
        fleet.line(&launch("toolu_1", "Agent", 1));
        fleet.line(&enqueue(&notification("a9", "toolu_1", "failed", 0), 3));
        assert_eq!(only(&fleet).state, SubagentState::Failed);
    }

    #[test]
    fn a_resumed_agent_runs_again_and_a_later_notification_ends_it() {
        let (mut fleet, _dir) = fleet("resume");
        fleet.line(&launch("toolu_1", "Agent", 1));
        fleet.line(&launched("toolu_1", "a1", 2));
        fleet.line(&enqueue(&notification("a1", "toolu_1", "completed", 3), 3));
        assert_eq!(only(&fleet).state, SubagentState::Done);

        fleet.line(&result(
            "toolu_send",
            json!({"resumedAgentId": "a1", "revivedAgentId": "a1"}),
            false,
            10,
        ));
        let sub = only(&fleet);
        assert_eq!(sub.state, SubagentState::Running);
        assert_eq!(sub.started_at, millis_at(10));
        assert_eq!(sub.ended_at, None);

        fleet.line(&result(
            "toolu_send2",
            json!({"resumedAgentId": null}),
            false,
            11,
        ));
        assert_eq!(only(&fleet).state, SubagentState::Running);

        fleet.line(&enqueue(
            &notification("a1", "toolu_send", "completed", 5),
            20,
        ));
        let sub = only(&fleet);
        assert_eq!(sub.state, SubagentState::Done);
        assert_eq!(sub.ended_at, Some(millis_at(20)));
        assert_eq!(sub.tools, 5);
    }

    #[test]
    fn shell_notifications_are_ignored_and_the_task_alias_is_tracked() {
        let (mut fleet, _dir) = fleet("shell");
        fleet.line(&launch("toolu_1", "Task", 1));
        fleet.line(&launch("toolu_b", "Bash", 1));
        fleet.take_changed();
        fleet.line(&enqueue(
            "<task-notification><task-id>b7x</task-id><tool-use-id>toolu_b</tool-use-id>\
             <status>completed</status></task-notification>",
            2,
        ));
        assert!(!fleet.take_changed());
        assert_eq!(only(&fleet).state, SubagentState::Running);
    }

    #[test]
    fn sidechain_lines_and_other_tool_names_are_not_agents() {
        let (mut fleet, _dir) = fleet("sidechain");
        let mut nested = launch("toolu_1", "Agent", 1);
        nested["isSidechain"] = json!(true);
        fleet.line(&nested);
        fleet.line(&launch("toolu_2", "Read", 1));
        assert!(fleet.list().is_empty());
        assert!(!fleet.take_changed());
    }

    #[test]
    fn the_tail_counts_tool_calls_and_restarts_when_the_file_shrinks() {
        let (mut fleet, dir) = fleet("tail");
        let file = dir.join("s1/subagents/agent-a1.jsonl");
        append(&file, &[agent_line("Grep", 3), agent_line("Read", 4)]);
        fleet.line(&launch("toolu_1", "Agent", 1));
        fleet.line(&launched("toolu_1", "a1", 2));
        fleet.take_changed();

        fleet.read_agents();
        let sub = only(&fleet);
        assert_eq!((sub.tools, sub.tool.as_deref()), (2, Some("Read")));
        assert_eq!(sub.active_at, Some(millis_at(4)));
        assert!(fleet.take_changed());

        fleet.read_agents();
        assert!(!fleet.take_changed());

        append(&file, &[agent_line("Bash", 5)]);
        fleet.read_agents();
        assert_eq!(only(&fleet).tools, 3);

        fs::write(&file, format!("{}\n", agent_line("Edit", 9))).unwrap();
        fleet.read_agents();
        let sub = only(&fleet);
        assert_eq!((sub.tools, sub.tool.as_deref()), (1, Some("Edit")));
        assert_eq!(sub.active_at, Some(millis_at(9)));

        fleet.line(&enqueue(&notification("a1", "toolu_1", "completed", 4), 12));
        let sub = only(&fleet);
        assert_eq!((sub.tools, sub.tool), (4, None));
        append(&file, &[agent_line("Bash", 13)]);
        fleet.read_agents();
        assert_eq!(only(&fleet).tools, 4);
    }

    #[test]
    fn an_agent_id_outside_the_file_stem_charset_is_ignored() {
        let (mut fleet, dir) = fleet("path");
        append(&dir.join("s1/x.jsonl"), &[agent_line("Read", 3)]);
        fleet.line(&launch("toolu_1", "Agent", 1));
        fleet.line(&launched("toolu_1", "../x", 2));
        fleet.line(&launched("toolu_1", &"a".repeat(65), 2));
        assert_eq!(only(&fleet).agent_id, None);
        fleet.read_agents();
        assert_eq!(only(&fleet).tools, 0);

        fleet.line(&result(
            "toolu_x",
            json!({"resumedAgentId": "../x"}),
            false,
            3,
        ));
        assert_eq!(only(&fleet).state, SubagentState::Running);
    }

    #[test]
    fn long_names_are_clipped_and_the_list_is_capped_with_running_agents_first() {
        let (mut fleet, _dir) = fleet("caps");
        let mut long = launch("toolu_long", "Agent", 1);
        long["message"]["content"][0]["input"]["description"] = json!("d".repeat(300));
        long["message"]["content"][0]["input"]["subagent_type"] = json!("t".repeat(100));
        fleet.line(&long);
        let sub = only(&fleet);
        assert_eq!(
            (sub.description.len(), sub.agent_type.len()),
            (SUMMARY_CHARS, NAME_CHARS)
        );

        for n in 0..24 {
            fleet.line(&launch(&format!("toolu_{n}"), "Agent", 2 + n % 30));
        }
        for n in 0..10 {
            fleet.line(&enqueue(
                &notification("none", &format!("toolu_{n}"), "completed", 1),
                40 + n,
            ));
        }
        let list = fleet.list();
        assert_eq!(list.len(), MAX_SENT);
        let running = list
            .iter()
            .take_while(|s| s.state == SubagentState::Running)
            .count();
        assert_eq!(running, 15);
        assert!(
            list[running..]
                .iter()
                .all(|s| s.state == SubagentState::Done)
        );
    }

    #[test]
    fn ended_agents_follow_running_ones() {
        let (mut fleet, _dir) = fleet("order");
        for n in 0..3 {
            fleet.line(&launch(&format!("toolu_{n}"), "Agent", 1 + n));
        }
        fleet.line(&enqueue(&notification("x", "toolu_0", "completed", 1), 10));
        fleet.line(&enqueue(&notification("x", "toolu_1", "completed", 1), 11));
        let ids: Vec<String> = fleet.list().into_iter().map(|s| s.tool_id).collect();
        assert_eq!(ids, ["toolu_2", "toolu_1", "toolu_0"]);
    }

    #[test]
    fn tracking_is_bounded_and_drops_ended_agents_first() {
        let (mut fleet, _dir) = fleet("bound");
        fleet.line(&launch("toolu_old", "Agent", 1));
        fleet.line(&enqueue(&notification("x", "toolu_old", "completed", 1), 2));
        for n in 0..MAX_TRACKED {
            fleet.line(&launch(&format!("toolu_{n}"), "Agent", 3));
        }
        assert_eq!(fleet.entries.len(), MAX_TRACKED);
        assert!(!fleet.by_tool.contains_key("toolu_old"));
        assert!(
            fleet
                .entries
                .iter()
                .all(|e| fleet.by_tool[&e.sub.tool_id] < MAX_TRACKED)
        );
    }

    #[test]
    fn a_result_quoting_notification_markup_does_not_set_the_count_or_end_the_block() {
        let (mut fleet, _dir) = fleet("quoted");
        fleet.line(&launch("toolu_1", "Agent", 1));
        fleet.line(&launched("toolu_1", "a1", 2));
        let text = "<task-notification><task-id>a1</task-id><tool-use-id>toolu_1</tool-use-id>\
            <status>completed</status><result>quoted </task-notification>\
            <tool_uses>99</tool_uses> end</result><usage><tool_uses>3</tool_uses></usage>\
            </task-notification>";
        fleet.line(&enqueue(text, 5));
        let sub = only(&fleet);
        assert_eq!((sub.state, sub.tools), (SubagentState::Done, 3));
    }

    #[test]
    fn a_resumed_agent_counts_its_whole_file_live_and_on_replay() {
        let (mut live, dir) = fleet("recount");
        let file = dir.join("s1/subagents/agent-a1.jsonl");
        let first = [launch("toolu_1", "Agent", 1), launched("toolu_1", "a1", 2)];
        let done = enqueue(&notification("a1", "toolu_1", "completed", 2), 5);
        let resume = result("toolu_send", json!({"resumedAgentId": "a1"}), false, 10);

        live.line(&first[0]);
        live.line(&first[1]);
        append(&file, &[agent_line("Read", 3), agent_line("Grep", 4)]);
        live.read_agents();
        live.line(&done);
        live.line(&resume);
        append(
            &file,
            &[
                agent_line("Bash", 11),
                agent_line("Edit", 12),
                agent_line("Read", 13),
            ],
        );
        live.read_agents();

        let mut replayed = Fleet::new(&dir.join("s1.jsonl"));
        for line in [&first[0], &first[1], &done, &resume] {
            replayed.line(line);
        }
        replayed.read_agents();

        assert_eq!(only(&live).tools, 5);
        assert_eq!(only(&replayed), only(&live));
    }

    #[test]
    fn when_every_tracked_agent_is_running_the_oldest_is_dropped() {
        let (mut fleet, _dir) = fleet("all-running");
        for n in 0..=MAX_TRACKED {
            let mut line = launch(&format!("toolu_{n}"), "Agent", 0);
            line["timestamp"] = json!(format!("2026-01-01T00:{:02}:{:02}.000Z", n / 60, n % 60));
            fleet.line(&line);
        }
        assert_eq!(fleet.entries.len(), MAX_TRACKED);
        assert!(!fleet.by_tool.contains_key("toolu_0"));
        assert!(fleet.by_tool.contains_key(&format!("toolu_{MAX_TRACKED}")));
    }

    #[test]
    fn clearing_forgets_everything_including_seen_notifications() {
        let (mut fleet, _dir) = fleet("clear");
        let text = notification("a1", "toolu_1", "completed", 1);
        fleet.line(&launch("toolu_1", "Agent", 1));
        fleet.line(&enqueue(&text, 2));
        fleet.clear();
        assert!(fleet.list().is_empty());
        assert!(!fleet.take_changed());
        fleet.line(&launch("toolu_1", "Agent", 1));
        fleet.line(&enqueue(&text, 2));
        assert_eq!(only(&fleet).state, SubagentState::Done);
    }

    #[test]
    fn no_prompt_result_or_agent_text_reaches_the_wire() {
        let (mut fleet, dir) = fleet("privacy");
        let file = dir.join("s1/subagents/agent-a1.jsonl");
        append(&file, &[agent_line("Read", 3)]);
        fleet.line(&launch("toolu_1", "Agent", 1));
        fleet.line(&launched("toolu_1", "a1", 2));
        fleet.read_agents();
        fleet.line(&enqueue(&notification("a1", "toolu_1", "completed", 1), 5));
        fleet.line(&launch("toolu_2", "Agent", 6));
        fleet.line(&result("toolu_2", json!("SECRET-ERROR"), true, 7));
        let wire = serde_json::to_string(&fleet.list()).unwrap();
        assert!(wire.contains("Check the parser"));
        assert!(!wire.contains("SECRET"), "{wire}");
    }
}
