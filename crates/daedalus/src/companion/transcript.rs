//! Turns a Claude Code JSONL transcript into chat items, read on as the file grows.

use std::cell::Cell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::protocol::{
    FileChange, Item, ItemKind, ItemStatus, Permission, Question, QuestionOption, Subagent,
};
use super::subagents::Fleet;
use crate::transcripts::{Typed, next_line, typed_text};

const ASSISTANT_CAP: usize = 24 << 10;
const USER_CAP: usize = 8 << 10;
const DETAIL_CAP: usize = 1 << 10;
pub(super) const SUMMARY_CHARS: usize = 200;
const OUTPUT_LINES: usize = 6;
const OUTPUT_LINE_CHARS: usize = 200;
const MAX_FILES: usize = 5;
const NO_PREVIEW: [&str; 5] = ["Read", "Agent", "Task", "TodoWrite", "AskUserQuestion"];

/// One transcript file, parsed into items as far as it has been read.
#[derive(Debug)]
pub(crate) struct Feed {
    path: PathBuf,
    offset: u64,
    items: Vec<Item>,
    tools: HashMap<String, usize>,
    fleet: Fleet,
}

/// What a read found: items to upsert, or a file that shrank and was read again from the start.
#[derive(Debug, PartialEq)]
pub(crate) enum Read {
    Changed(Vec<usize>),
    Reset,
}

impl Feed {
    pub(crate) fn new(path: PathBuf) -> Self {
        Feed {
            fleet: Fleet::new(&path),
            path,
            offset: 0,
            items: Vec::new(),
            tools: HashMap::new(),
        }
    }

    /// Reads the lines written since the last call.
    pub(crate) fn read_on(&mut self) -> Read {
        let Ok(file) = File::open(&self.path) else {
            return Read::Changed(Vec::new());
        };
        let Ok(len) = file.metadata().map(|m| m.len()) else {
            return Read::Changed(Vec::new());
        };
        let reset = len < self.offset;
        if reset {
            self.offset = 0;
            self.items.clear();
            self.tools.clear();
            self.fleet.clear();
        }
        let mut input = BufReader::new(file);
        if input.seek(SeekFrom::Start(self.offset)).is_err() {
            return Read::Changed(Vec::new());
        }
        let mut changed = Vec::new();
        let mut buf = Vec::new();
        while let Some(n) = next_line(&mut input, &mut buf) {
            if let Ok(line) = serde_json::from_slice::<Value>(&buf) {
                self.line(&line, self.offset, &mut changed);
            }
            self.offset += n;
        }
        self.fleet.read_agents();
        if reset {
            return Read::Reset;
        }
        changed.sort_unstable();
        changed.dedup();
        Read::Changed(changed)
    }

    pub(crate) fn items(&self) -> &[Item] {
        &self.items
    }

    pub(crate) fn subagents(&self) -> Vec<Subagent> {
        self.fleet.list()
    }

    /// True while the sub-agents need polling even though the transcript file itself is idle.
    pub(crate) fn agents_running(&self) -> bool {
        self.fleet.running()
    }

    pub(crate) fn take_subagents_changed(&mut self) -> bool {
        self.fleet.take_changed()
    }

    /// The newest items strictly older than `before`, whole lines at a time, and the offset to
    /// ask for the rest from, or `None` when nothing older exists.
    ///
    /// `budget_bytes` covers only the items, so the caller subtracts the message envelope.
    pub(crate) fn page(
        &self,
        before: Option<u64>,
        limit: usize,
        budget_bytes: usize,
    ) -> (Vec<Item>, Option<u64>) {
        let end = match before {
            Some(b) => self.items.partition_point(|i| i.pos < b),
            None => self.items.len(),
        };
        let mut start = end.saturating_sub(limit.max(1));
        while start > 0 && start < end && self.items[start - 1].pos == self.items[start].pos {
            start -= 1;
        }
        let size = |i: &Item| serde_json::to_vec(i).map_or(0, |v| v.len() + 1);
        let mut total: usize = self.items[start..end].iter().map(size).sum();
        while total > budget_bytes && start < end {
            let pos = self.items[start].pos;
            let next = start + self.items[start..end].partition_point(|i| i.pos == pos);
            if next == end {
                break;
            }
            total -= self.items[start..next].iter().map(size).sum::<usize>();
            start = next;
        }
        let older = (start > 0).then(|| self.items[start].pos);
        (self.items[start..end].to_vec(), older)
    }

    fn line(&mut self, line: &Value, pos: u64, changed: &mut Vec<usize>) {
        self.fleet.line(line);
        let kind = line["type"].as_str().unwrap_or_default();
        if !matches!(kind, "user" | "assistant")
            || flag(line, "isSidechain")
            || flag(line, "isMeta")
        {
            return;
        }
        let at = line["timestamp"].as_str().map_or(0, millis);
        let uuid = line["uuid"]
            .as_str()
            .map_or_else(|| format!("p{pos}"), str::to_string);
        let cwd = line["cwd"].as_str().map(Path::new);
        let content = &line["message"]["content"];
        if kind == "user" && flag(line, "isCompactSummary") {
            let note = text_item(format!("{uuid}#0"), pos, at, ItemKind::Note, "");
            self.push(
                Item {
                    text: Some("Conversation compacted".into()),
                    ..note
                },
                changed,
            );
            return;
        }
        if flag(line, "isVisibleInTranscriptOnly") {
            return;
        }
        if kind == "assistant" {
            for (n, block) in blocks(content).iter().enumerate() {
                match block["type"].as_str() {
                    Some("text") => {
                        let text = block["text"].as_str().unwrap_or_default().trim();
                        if !text.is_empty() {
                            let id = format!("{uuid}#{n}");
                            self.push(text_item(id, pos, at, ItemKind::Assistant, text), changed);
                        }
                    }
                    Some("tool_use") => self.tool_use(block, pos, at, cwd, changed),
                    _ => {}
                }
            }
            return;
        }
        if let Some(results) = content.as_array() {
            for block in results.iter().filter(|b| b["type"] == "tool_result") {
                self.tool_result(block, line, pos, changed);
            }
        }
        let id = format!("{uuid}#0");
        match typed_text(content) {
            Some(Typed::Command(cmd)) => {
                self.push(
                    Item {
                        text: Some(cmd),
                        ..text_item(id, pos, at, ItemKind::Note, "")
                    },
                    changed,
                );
            }
            Some(Typed::Text(text)) if text.starts_with("[Request interrupted by user") => {
                self.push(
                    Item {
                        text: Some("Interrupted".into()),
                        ..text_item(id, pos, at, ItemKind::Note, "")
                    },
                    changed,
                );
            }
            Some(Typed::Text(text)) => {
                self.push(text_item(id, pos, at, ItemKind::User, &text), changed);
            }
            None => {}
        }
    }

    fn tool_use(
        &mut self,
        block: &Value,
        pos: u64,
        at: u64,
        cwd: Option<&Path>,
        changed: &mut Vec<usize>,
    ) {
        let Some(id) = block["id"].as_str() else {
            return;
        };
        let name = block["name"].as_str().unwrap_or_default();
        let input = &block["input"];
        let (summary, summary_cut) = clip_chars(&summary(name, input, cwd), SUMMARY_CHARS);
        let question = name == "AskUserQuestion";
        let (questions, questions_cut) = if question {
            questions(input)
        } else {
            (Vec::new(), false)
        };
        let item = Item {
            id: id.to_string(),
            pos,
            at,
            kind: if question {
                ItemKind::Question
            } else {
                ItemKind::Tool
            },
            text: None,
            cut: summary_cut || questions_cut,
            tool: Some(name.to_string()),
            tool_id: Some(id.to_string()),
            summary: (!summary.is_empty()).then_some(summary),
            status: Some(ItemStatus::Pending),
            detail: None,
            questions,
            answer: None,
            output: None,
            lines: None,
            files: Vec::new(),
            more_files: None,
            permission: None,
            result_pos: None,
        };
        self.push(item, changed);
    }

    fn tool_result(&mut self, block: &Value, line: &Value, pos: u64, changed: &mut Vec<usize>) {
        let Some(&idx) = block["tool_use_id"]
            .as_str()
            .and_then(|id| self.tools.get(id))
        else {
            return;
        };
        let failed = flag(block, "is_error");
        let (text, cut) = clip(&result_text(&block["content"]), DETAIL_CAP);
        let item = &mut self.items[idx];
        item.status = Some(if failed {
            ItemStatus::Error
        } else {
            ItemStatus::Ok
        });
        if item.kind == ItemKind::Question {
            item.answer = Some(text);
        } else if failed {
            item.detail = Some(text);
        } else {
            let tool = item.tool.as_deref().unwrap_or_default();
            let result = &line["toolUseResult"];
            if !NO_PREVIEW.contains(&tool) {
                let (output, lines) = output_preview(&output_source(result, &block["content"]));
                item.output = (!output.is_empty()).then_some(output);
                item.lines = (lines > 0).then_some(lines);
            }
            if tool == "Read" {
                item.lines = result["file"]["numLines"]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok());
            }
            let cwd = line["cwd"].as_str().map(Path::new);
            let (files, more) = file_changes(result, cwd);
            item.more_files = (more > 0).then_some(more);
            item.files = files;
        }
        item.permission = permission(line);
        item.result_pos = Some(pos);
        item.cut |= cut;
        changed.push(idx);
    }

    fn push(&mut self, item: Item, changed: &mut Vec<usize>) {
        if item.tool_id.is_some() {
            if self.tools.contains_key(&item.id) {
                return;
            }
            self.tools.insert(item.id.clone(), self.items.len());
        }
        changed.push(self.items.len());
        self.items.push(item);
    }
}

pub(super) fn flag(value: &Value, key: &str) -> bool {
    value[key].as_bool().unwrap_or(false)
}

fn blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::Array(blocks) => blocks.clone(),
        Value::String(text) => vec![serde_json::json!({ "type": "text", "text": text })],
        _ => Vec::new(),
    }
}

fn text_item(id: String, pos: u64, at: u64, kind: ItemKind, text: &str) -> Item {
    let cap = if kind == ItemKind::User {
        USER_CAP
    } else {
        ASSISTANT_CAP
    };
    let (text, cut) = clip(text, cap);
    Item {
        id,
        pos,
        at,
        kind,
        text: Some(text),
        cut,
        tool: None,
        tool_id: None,
        summary: None,
        status: None,
        detail: None,
        questions: Vec::new(),
        answer: None,
        output: None,
        lines: None,
        files: Vec::new(),
        more_files: None,
        permission: None,
        result_pos: None,
    }
}

fn summary(tool: &str, input: &Value, cwd: Option<&Path>) -> String {
    let field = |key: &str| input[key].as_str().unwrap_or_default().trim();
    match tool {
        "Bash" => {
            let first = field("command").lines().next().unwrap_or_default().trim();
            if first.is_empty() {
                field("description")
            } else {
                first
            }
            .to_string()
        }
        "Read" | "Edit" | "Write" | "MultiEdit" => {
            let path = Path::new(field("file_path"));
            let shown = cwd.and_then(|c| path.strip_prefix(c).ok()).unwrap_or(path);
            shown.to_string_lossy().into_owned()
        }
        "Grep" | "Glob" => field("pattern").to_string(),
        "WebFetch" => field("url").to_string(),
        "WebSearch" => field("query").to_string(),
        "Agent" | "Task" => field("description").to_string(),
        "TodoWrite" => "Updated todos".to_string(),
        "ExitPlanMode" => "Plan ready for review".to_string(),
        _ => String::new(),
    }
}

fn questions(input: &Value) -> (Vec<Question>, bool) {
    let cut = Cell::new(false);
    let text = |v: &Value, key: &str| {
        let (text, clipped) = clip(v[key].as_str().unwrap_or_default(), DETAIL_CAP);
        cut.set(cut.get() || clipped);
        text
    };
    let questions = input["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|q| Question {
            question: text(q, "question"),
            header: text(q, "header"),
            multi: flag(q, "multiSelect"),
            options: q["options"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|o| QuestionOption {
                    label: text(o, "label"),
                    description: text(o, "description"),
                })
                .collect(),
        })
        .collect();
    (questions, cut.get())
}

fn output_source(result: &Value, content: &Value) -> String {
    let Some(stdout) = result["stdout"].as_str() else {
        return result_text(content);
    };
    let stderr = result["stderr"].as_str().unwrap_or_default();
    [stdout, stderr]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first few non-empty lines of `text` within the preview budget, and the line count of the whole text.
fn output_preview(text: &str) -> (String, u32) {
    let text = strip_ansi(text);
    let total = text.trim_end().lines().count();
    let mut preview = String::new();
    for line in text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(OUTPUT_LINES)
    {
        let (line, _) = clip_chars(line, OUTPUT_LINE_CHARS);
        let sep = usize::from(!preview.is_empty());
        let room = DETAIL_CAP.saturating_sub(preview.len() + sep);
        let (line, cut) = clip(&line, room);
        if line.is_empty() {
            break;
        }
        if sep == 1 {
            preview.push('\n');
        }
        preview.push_str(&line);
        if cut {
            break;
        }
    }
    (preview, u32::try_from(total).unwrap_or(u32::MAX))
}

/// Drops CSI and OSC escape sequences and C0 controls other than tab and newline.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    for n in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(n) = chars.next() {
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(' '..='/') => {
                    chars.next();
                }
                _ => {}
            },
            '\t' | '\n' => out.push(c),
            c if c.is_control() && (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

fn file_changes(result: &Value, cwd: Option<&Path>) -> (Vec<FileChange>, u32) {
    let diff = &result["bashEditDiff"];
    let mut changes: Vec<FileChange> = if let Some(files) = diff["files"].as_array() {
        files
            .iter()
            .filter_map(|f| {
                let hunks = f["hunks"].as_array().map_or(&[][..], Vec::as_slice);
                Some(change(f["filePath"].as_str()?, hunks, false, cwd))
            })
            .collect()
    } else if let Some(path) = result["filePath"]
        .as_str()
        .filter(|_| result["structuredPatch"].is_array() || result["type"] == "create")
    {
        let hunks = result["structuredPatch"]
            .as_array()
            .map_or(&[][..], Vec::as_slice);
        let created = result["type"] == "create";
        let mut file = change(path, hunks, created, cwd);
        if created && file.added == 0 {
            let lines = result["content"]
                .as_str()
                .unwrap_or_default()
                .lines()
                .count();
            file.added = u32::try_from(lines).unwrap_or(u32::MAX);
        }
        vec![file]
    } else {
        Vec::new()
    };
    let moved = diff["moreFiles"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0);
    let rest = u32::try_from(changes.len().saturating_sub(MAX_FILES)).unwrap_or(u32::MAX);
    changes.truncate(MAX_FILES);
    (changes, rest.saturating_add(moved))
}

fn change(path: &str, hunks: &[Value], created: bool, cwd: Option<&Path>) -> FileChange {
    let path = Path::new(path);
    let shown = cwd.and_then(|c| path.strip_prefix(c).ok()).unwrap_or(path);
    let (path, _) = clip_chars(&shown.to_string_lossy(), SUMMARY_CHARS);
    let (mut added, mut removed) = (0u32, 0u32);
    for text in hunks.iter().filter_map(|h| h["lines"].as_array()).flatten() {
        match text.as_str().and_then(|t| t.chars().next()) {
            Some('+') => added = added.saturating_add(1),
            Some('-') => removed = removed.saturating_add(1),
            _ => {}
        }
    }
    FileChange {
        path,
        added,
        removed,
        created,
    }
}

fn permission(line: &Value) -> Option<Permission> {
    let decision = &line["permissionDecision"];
    let source = decision["source"].as_str().unwrap_or_default();
    let reason = decision["reasonType"].as_str().unwrap_or_default();
    match decision["decision"].as_str()? {
        "reject" if source == "hook" => Some(Permission::DeniedHook),
        "reject" if reason == "classifier" => Some(Permission::DeniedAuto),
        "reject" => Some(Permission::Denied),
        "accept" if source.starts_with("user") => Some(Permission::User),
        "accept" => match reason {
            "classifier" => Some(Permission::Auto),
            "mode" => Some(Permission::Mode),
            "rule" => Some(Permission::Rule),
            _ => None,
        },
        _ => None,
    }
}

fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// `text` cut to at most `max` bytes on a char boundary, and whether anything was cut.
fn clip(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

pub(super) fn clip_chars(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((end, _)) => (text[..end].to_string(), true),
        None => (text.to_string(), false),
    }
}

/// Milliseconds since the epoch for an RFC 3339 timestamp, 0 when it does not parse.
pub(super) fn millis(stamp: &str) -> u64 {
    let Some(secs) = crate::usage::parse_rfc3339(stamp) else {
        return 0;
    };
    let frac = stamp
        .split_once('.')
        .map(|(_, rest)| rest.bytes().take_while(u8::is_ascii_digit).take(3))
        .map_or(0, |digits| {
            let digits: Vec<u8> = digits.collect();
            digits
                .iter()
                .chain(b"000")
                .take(3)
                .fold(0, |n, d| n * 10 + u64::from(d - b'0'))
        });
    u64::try_from(secs).map_or(0, |s| s * 1000 + frac)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcripts::{find, project_folder};
    use serde_json::json;
    use std::fs;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("daedalus-feed-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, lines: &[Value]) {
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(path, text).unwrap();
    }

    fn append(path: &Path, text: &str) {
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn changed(read: Read) -> Vec<usize> {
        match read {
            Read::Changed(indices) => indices,
            Read::Reset => panic!("unexpected reset"),
        }
    }

    fn user(uuid: &str, content: Value) -> Value {
        json!({ "type": "user", "uuid": uuid, "cwd": "/work/app",
            "timestamp": "2026-01-01T00:00:00.250Z", "message": { "role": "user", "content": content } })
    }

    fn assistant(uuid: &str, content: Value) -> Value {
        json!({ "type": "assistant", "uuid": uuid, "cwd": "/work/app",
            "timestamp": "2026-01-01T00:00:01Z", "message": { "role": "assistant", "content": content } })
    }

    fn feed(lines: &[Value]) -> (Feed, PathBuf) {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = temp(&format!("feed{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        let path = dir.join("s.jsonl");
        write(&path, lines);
        (Feed::new(path.clone()), path)
    }

    #[test]
    fn every_kind_of_item_is_read() {
        let (mut feed, _) = feed(&[
            user("u1", json!("fix the bug\nplease")),
            assistant(
                "a1",
                json!([
                    { "type": "thinking", "thinking": "hmm" },
                    { "type": "text", "text": "On it." },
                    { "type": "tool_use", "id": "toolu_1", "name": "Bash",
                      "input": { "command": "cargo test\nmore" } },
                    { "type": "tool_use", "id": "toolu_2", "name": "Read",
                      "input": { "file_path": "/work/app/src/main.rs" } },
                    { "type": "tool_use", "id": "toolu_3", "name": "AskUserQuestion",
                      "input": { "questions": [{ "question": "Which?", "header": "Pick",
                        "multiSelect": false,
                        "options": [{ "label": "A", "description": "first" }] }] } },
                    { "type": "tool_use", "id": "toolu_4", "name": "ExitPlanMode", "input": {} },
                ]),
            ),
            user(
                "u2",
                json!("<command-name>/review</command-name>\n<command-args>main</command-args>"),
            ),
            json!({ "type": "user", "uuid": "u3", "isCompactSummary": true,
                "isVisibleInTranscriptOnly": true, "message": { "content": "summary" } }),
            user("u4", json!("[Request interrupted by user]")),
        ]);
        assert_eq!(changed(feed.read_on()).len(), 9);
        let items = feed.items();
        assert_eq!(items[0].kind, ItemKind::User);
        assert_eq!(items[0].text.as_deref(), Some("fix the bug\nplease"));
        assert_eq!(items[0].at, 1_767_225_600_250);
        assert_eq!(items[1].id, "a1#1");
        assert_eq!(items[1].kind, ItemKind::Assistant);
        assert_eq!(items[2].id, "toolu_1");
        assert_eq!(items[2].summary.as_deref(), Some("cargo test"));
        assert_eq!(items[2].status, Some(ItemStatus::Pending));
        assert_eq!(items[3].summary.as_deref(), Some("src/main.rs"));
        assert_eq!(items[4].kind, ItemKind::Question);
        assert_eq!(items[4].questions[0].options[0].label, "A");
        assert_eq!(items[5].summary.as_deref(), Some("Plan ready for review"));
        assert_eq!(items[6].text.as_deref(), Some("/review main"));
        assert_eq!(items[6].kind, ItemKind::Note);
        assert_eq!(items[7].text.as_deref(), Some("Conversation compacted"));
        assert_eq!(items[8].text.as_deref(), Some("Interrupted"));
        assert!(items.windows(2).all(|w| w[0].pos <= w[1].pos));
    }

    #[test]
    fn a_question_missing_its_header_or_descriptions_gets_empty_strings() {
        let (mut feed, _) = feed(&[assistant(
            "a1",
            json!([{ "type": "tool_use", "id": "toolu_1", "name": "AskUserQuestion",
                "input": { "questions": [{ "question": "Which?", "options": [{ "label": "A" }] }] } }]),
        )]);
        feed.read_on();
        let q = &feed.items()[0].questions[0];
        assert_eq!(q.header, "");
        assert!(!q.multi);
        assert_eq!(q.options[0].description, "");
    }

    #[test]
    fn a_tool_result_updates_its_item_and_reports_it() {
        let (mut feed, path) = feed(&[assistant(
            "a1",
            json!([
                { "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": { "command": "ls" } },
                { "type": "tool_use", "id": "toolu_2", "name": "AskUserQuestion",
                  "input": { "questions": [] } },
            ]),
        )]);
        assert_eq!(changed(feed.read_on()), vec![0, 1]);
        append(
            &path,
            &(user(
                "u1",
                json!([
                    { "type": "tool_result", "tool_use_id": "toolu_1", "is_error": true,
                      "content": "boom" },
                    { "type": "tool_result", "tool_use_id": "toolu_2",
                      "content": [{ "type": "text", "text": "A" }] },
                ]),
            )
            .to_string()
                + "\n"),
        );
        assert_eq!(changed(feed.read_on()), vec![0, 1]);
        assert_eq!(feed.items()[0].status, Some(ItemStatus::Error));
        assert_eq!(feed.items()[0].detail.as_deref(), Some("boom"));
        assert_eq!(feed.items()[1].status, Some(ItemStatus::Ok));
        assert_eq!(feed.items()[1].answer.as_deref(), Some("A"));
        assert_eq!(feed.items().len(), 2);
    }

    #[test]
    fn thinking_sidechain_meta_and_unknown_lines_are_skipped() {
        let mut side = user("u1", json!("side"));
        side["isSidechain"] = json!(true);
        let mut meta = user("u2", json!("meta"));
        meta["isMeta"] = json!(true);
        let mut hidden = user("u3", json!("hidden"));
        hidden["isVisibleInTranscriptOnly"] = json!(true);
        let mut hidden_reply = assistant("a2", json!([{ "type": "text", "text": "hidden" }]));
        hidden_reply["isVisibleInTranscriptOnly"] = json!(true);
        let (mut feed, path) = feed(&[
            side,
            meta,
            hidden,
            hidden_reply,
            json!({ "type": "summary", "summary": "x" }),
            assistant("a1", json!([{ "type": "thinking", "thinking": "" }])),
            user("u4", json!("<system-reminder>x</system-reminder>")),
        ]);
        append(&path, "not json\n");
        assert!(changed(feed.read_on()).is_empty());
    }

    #[test]
    fn a_half_written_last_line_is_read_on_the_next_call() {
        let (mut feed, path) = feed(&[user("u1", json!("one"))]);
        let second = user("u2", json!("two")).to_string();
        let (head, tail) = second.split_at(20);
        append(&path, head);
        assert_eq!(changed(feed.read_on()), vec![0]);
        append(&path, &format!("{tail}\n"));
        assert_eq!(changed(feed.read_on()), vec![1]);
        assert_eq!(feed.items()[1].text.as_deref(), Some("two"));
    }

    #[test]
    fn a_shrunk_file_is_read_again_from_the_start() {
        let (mut feed, path) = feed(&[user("u1", json!("one")), user("u2", json!("two"))]);
        feed.read_on();
        write(&path, &[user("u3", json!("new"))]);
        assert_eq!(feed.read_on(), Read::Reset);
        assert_eq!(feed.items().len(), 1);
        assert_eq!(feed.items()[0].text.as_deref(), Some("new"));
    }

    fn numbered(count: usize) -> Feed {
        let lines: Vec<Value> = (0..count)
            .map(|n| user(&format!("u{n}"), json!(format!("message {n}"))))
            .collect();
        let (mut feed, _) = feed(&lines);
        feed.read_on();
        feed
    }

    #[test]
    fn paging_goes_back_by_offset_to_the_start() {
        let feed = numbered(5);
        let (page, before) = feed.page(None, 2, usize::MAX);
        assert_eq!(page.len(), 2);
        assert_eq!(page[1].id, "u4#0");
        let before = before.unwrap();
        assert_eq!(before, page[0].pos);
        let (older, before) = feed.page(Some(before), 2, usize::MAX);
        assert_eq!(older[1].id, "u2#0");
        assert!(older.iter().all(|i| i.pos < page[0].pos));
        let (oldest, before) = feed.page(before, 2, usize::MAX);
        assert_eq!(oldest.len(), 1);
        assert_eq!(oldest[0].id, "u0#0");
        assert_eq!(before, None);
    }

    #[test]
    fn a_page_never_splits_the_items_of_one_line() {
        let (mut feed, _) = feed(&[assistant(
            "a1",
            json!([{ "type": "text", "text": "a" }, { "type": "text", "text": "b" },
                   { "type": "text", "text": "c" }]),
        )]);
        feed.read_on();
        assert_eq!(feed.page(None, 1, usize::MAX).0.len(), 3);
    }

    #[test]
    fn a_zero_limit_still_returns_the_newest_line() {
        let feed = numbered(3);
        let (page, before) = feed.page(None, 0, usize::MAX);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id, "u2#0");
        assert_eq!(before, Some(page[0].pos));
        let (page, before) = feed.page(Some(u64::MAX), 0, usize::MAX);
        assert_eq!(page[0].id, "u2#0");
        assert!(before.is_some());
        assert!(feed.page(Some(0), 0, usize::MAX).0.is_empty());
    }

    #[test]
    fn an_oversized_question_is_cut_and_flagged() {
        let big = "q".repeat(DETAIL_CAP * 2);
        let (mut feed, _) = feed(&[assistant(
            "a1",
            json!([{ "type": "tool_use", "id": "toolu_1", "name": "AskUserQuestion",
                "input": { "questions": [{ "question": big, "header": big,
                    "options": [{ "label": big, "description": big }] }] } }]),
        )]);
        feed.read_on();
        let item = &feed.items()[0];
        let q = &item.questions[0];
        assert!(item.cut);
        for field in [
            &q.question,
            &q.header,
            &q.options[0].label,
            &q.options[0].description,
        ] {
            assert_eq!(field.len(), DETAIL_CAP);
        }
    }

    #[test]
    fn the_byte_budget_drops_the_oldest_items() {
        let feed = numbered(5);
        let one = serde_json::to_vec(&feed.items()[4]).unwrap().len() + 1;
        let (page, before) = feed.page(None, 5, one * 2 + 1);
        assert_eq!(page.len(), 2);
        assert_eq!(page[1].id, "u4#0");
        assert_eq!(before, Some(page[0].pos));
        let (page, _) = feed.page(None, 5, 1);
        assert_eq!(page.len(), 1);
    }

    #[test]
    fn text_is_cut_on_a_char_boundary_and_flagged() {
        let long = "é".repeat(USER_CAP);
        let (mut feed, _) = feed(&[user("u1", json!(long))]);
        feed.read_on();
        let item = &feed.items()[0];
        assert!(item.cut);
        assert_eq!(item.text.as_ref().unwrap().len(), USER_CAP);
        let (short, cut) = clip_chars(&"x".repeat(300), SUMMARY_CHARS);
        assert!(cut);
        assert_eq!(short.len(), SUMMARY_CHARS);
    }

    #[test]
    fn timestamps_keep_their_milliseconds() {
        assert_eq!(millis("2026-01-01T00:00:00Z"), 1_767_225_600_000);
        assert_eq!(millis("2026-01-01T00:00:00.5Z"), 1_767_225_600_500);
        assert_eq!(millis("nope"), 0);
    }

    #[test]
    fn find_looks_in_every_profile_and_through_the_prefix_rule() {
        let home = temp("home");
        let root = Path::new("/work/app");
        let folder = project_folder(root);
        let work = home.join(".claude-work/projects").join(&folder);
        fs::create_dir_all(home.join(".claude/projects").join(&folder)).unwrap();
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("abc-1.jsonl"), "").unwrap();
        assert_eq!(find(&home, root, "abc-1"), Some(work.join("abc-1.jsonl")));
        assert_eq!(find(&home, root, "missing"), None);
        assert_eq!(find(&home, root, "../abc-1"), None);

        let long = Path::new("/work").join("x".repeat(300));
        let long_folder = project_folder(&long);
        let dir = home
            .join(".claude/projects")
            .join(format!("{}-hash", &long_folder[..200]));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("def-2.jsonl"), "").unwrap();
        assert_eq!(find(&home, &long, "def-2"), Some(dir.join("def-2.jsonl")));
    }

    fn tool_feed(name: &str, result: Value, extra: Value) -> Item {
        let mut done = user(
            "u2",
            json!([{ "type": "tool_result", "tool_use_id": "toolu_1", "content": "fallback" }]),
        );
        done["toolUseResult"] = result;
        if let Some(extra) = extra.as_object() {
            for (k, v) in extra {
                done[k] = v.clone();
            }
        }
        let (mut feed, _) = feed(&[
            assistant(
                "a1",
                json!([{ "type": "tool_use", "id": "toolu_1", "name": name, "input": {} }]),
            ),
            done,
        ]);
        feed.read_on();
        feed.items()[0].clone()
    }

    #[test]
    fn a_long_bash_output_gives_a_short_preview_and_a_line_count() {
        let stdout: String = (0..160)
            .map(|n| format!("line {n} {}\n", "x".repeat(300)))
            .collect();
        let item = tool_feed("Bash", json!({ "stdout": stdout, "stderr": "" }), json!({}));
        let output = item.output.unwrap();
        assert_eq!(output.lines().count(), 6);
        assert!(output.len() <= 1 << 10, "{}", output.len());
        assert_eq!(item.lines, Some(160));
        assert!(item.result_pos.is_some());
    }

    #[test]
    fn stderr_follows_stdout_and_content_is_the_fallback() {
        let both = tool_feed(
            "Bash",
            json!({ "stdout": "out", "stderr": "err" }),
            json!({}),
        );
        assert_eq!(both.output.as_deref(), Some("out\nerr"));
        assert_eq!(both.lines, Some(2));
        let only_err = tool_feed("Bash", json!({ "stdout": "", "stderr": "err" }), json!({}));
        assert_eq!(only_err.lines, Some(1));
        let plain = tool_feed("Grep", json!(["odd"]), json!({}));
        assert_eq!(plain.output.as_deref(), Some("fallback"));
    }

    #[test]
    fn ansi_sequences_and_controls_are_stripped() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("a\x1b]0;title\x07b\x08\tc\r"), "ab\tc");
        assert_eq!(strip_ansi("\x1b(Bplain"), "plain");
    }

    #[test]
    fn bash_edit_diff_files_are_counted_and_capped() {
        let files: Vec<Value> = (0..7)
            .map(|n| {
                json!({ "filePath": format!("/work/app/f{n}.rs"),
                    "hunks": [{ "lines": [" ctx", "-old", "+new", "+more"] }] })
            })
            .collect();
        let result = json!({ "stdout": "", "bashEditDiff": { "files": files, "moreFiles": 3 } });
        let item = tool_feed("Bash", result, json!({}));
        assert_eq!(item.files.len(), 5);
        assert_eq!(item.more_files, Some(5));
        assert_eq!(item.files[0].path, "f0.rs");
        assert_eq!((item.files[0].added, item.files[0].removed), (2, 1));
        assert_eq!(item.output, None);
    }

    #[test]
    fn edit_and_write_patches_give_one_file() {
        let edit = json!({ "filePath": "/work/app/a.rs",
            "structuredPatch": [{ "lines": ["-x", "+y"] }] });
        let item = tool_feed("Edit", edit, json!({}));
        assert_eq!(item.files.len(), 1);
        assert!(!item.files[0].created);
        let write = json!({ "type": "create", "filePath": "/work/app/b.rs",
            "structuredPatch": [], "content": "a\nb\nc\n" });
        let item = tool_feed("Write", write, json!({}));
        assert!(item.files[0].created);
        assert_eq!((item.files[0].added, item.files[0].removed), (3, 0));
    }

    #[test]
    fn permission_decisions_map_to_the_enum() {
        let of = |d: Value| permission(&json!({ "permissionDecision": d }));
        let accept = |reason: &str| json!({ "decision": "accept", "source": "config", "reasonType": reason });
        assert_eq!(of(accept("classifier")), Some(Permission::Auto));
        assert_eq!(of(accept("mode")), Some(Permission::Mode));
        assert_eq!(of(accept("rule")), Some(Permission::Rule));
        assert_eq!(of(accept("other")), None);
        assert_eq!(
            of(json!({ "decision": "accept", "source": "user_temporary" })),
            Some(Permission::User)
        );
        assert_eq!(
            of(json!({ "decision": "reject", "source": "hook", "reasonType": "hook" })),
            Some(Permission::DeniedHook)
        );
        assert_eq!(
            of(json!({ "decision": "reject", "source": "config", "reasonType": "classifier" })),
            Some(Permission::DeniedAuto)
        );
        assert_eq!(permission(&json!({})), None);
    }

    #[test]
    fn an_error_keeps_its_detail_and_has_no_output() {
        let (mut feed, _) = feed(&[
            assistant(
                "a1",
                json!([{ "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {} }]),
            ),
            {
                let mut line = user(
                    "u2",
                    json!([{ "type": "tool_result", "tool_use_id": "toolu_1",
                    "is_error": true, "content": "boom" }]),
                );
                line["toolUseResult"] = json!({ "stdout": "noise" });
                line
            },
        ]);
        feed.read_on();
        let item = &feed.items()[0];
        assert_eq!(item.detail.as_deref(), Some("boom"));
        assert_eq!(item.output, None);
    }

    #[test]
    fn a_result_without_tool_use_result_has_no_extra_fields() {
        let (mut feed, _) = feed(&[
            assistant(
                "a1",
                json!([{ "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {} }]),
            ),
            user(
                "u2",
                json!([{ "type": "tool_result", "tool_use_id": "toolu_1", "content": "" }]),
            ),
        ]);
        feed.read_on();
        let item = &feed.items()[0];
        assert_eq!(item.status, Some(ItemStatus::Ok));
        assert!(item.output.is_none() && item.lines.is_none() && item.files.is_empty());
        assert!(item.permission.is_none());
    }

    #[test]
    fn read_keeps_its_line_count_without_a_preview() {
        let item = tool_feed("Read", json!({ "file": { "numLines": 88 } }), json!({}));
        assert_eq!(item.lines, Some(88));
        assert_eq!(item.output, None);
    }
}
