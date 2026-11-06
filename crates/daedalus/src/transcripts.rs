//! Claude Code's session transcripts (`~/.claude*/projects/<folder>/<session>.jsonl`), read for
//! the session list: title, activity and message count.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;

const TITLE_CHARS: usize = 200;
/// A transcript line longer than this is skipped unread, as a pasted file or image can make one.
const MAX_LINE: u64 = 8 << 20;
/// Claude Code shortens longer folder names and adds a hash, so those are matched by prefix.
const MAX_FOLDER: usize = 200;

/// A Claude Code config folder: `~/.claude`, or a `~/.claude-*` one chosen with
/// `CLAUDE_CONFIG_DIR`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// `claude` or the folder name, such as `claude-tlsc`.
    pub name: String,
    /// What `CLAUDE_CONFIG_DIR` must be to use it; `None` for the default folder.
    pub config_dir: Option<PathBuf>,
    pub dir: PathBuf,
}

/// Maps each pid in `pids` to the session id and folder Claude Code recorded for that process.
pub fn running_sessions(home: &Path, pids: &[i32]) -> HashMap<i32, (String, PathBuf)> {
    #[derive(Deserialize)]
    struct Running {
        pid: i32,
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(default)]
        cwd: PathBuf,
        #[serde(default, rename = "startedAt")]
        started_at: u64,
    }
    let mut out: HashMap<i32, (u64, String, PathBuf)> = HashMap::new();
    for profile in profiles(home) {
        for &pid in pids {
            let path = profile.dir.join("sessions").join(format!("{pid}.json"));
            let Some(found) = fs::read(path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Running>(&b).ok())
            else {
                continue;
            };
            if found.pid != pid || !crate::snapshots::valid_session(&found.session_id) {
                continue;
            }
            if out.get(&pid).is_none_or(|(at, ..)| found.started_at >= *at) {
                out.insert(pid, (found.started_at, found.session_id, found.cwd));
            }
        }
    }
    out.into_iter()
        .map(|(pid, (_, id, cwd))| (pid, (id, cwd)))
        .collect()
}

/// Pids with a `<pid>.json` in `sessions_dir` whose process is alive.
pub(crate) fn live_session_pids(sessions_dir: &Path) -> Vec<i32> {
    let Ok(entries) = fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension()? != "json" {
                return None;
            }
            path.file_stem()?.to_str()?.parse::<i32>().ok()
        })
        .filter(|&pid| {
            // EPERM means the process exists but belongs to someone else.
            pid > 0
                && (unsafe { libc::kill(pid, 0) } == 0
                    || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
        })
        .collect()
}

/// Whether a live Claude process under any profile of `home` runs session `id`.
pub(crate) fn session_running_elsewhere(home: &Path, id: &str) -> bool {
    let pids: Vec<i32> = profiles(home)
        .iter()
        .flat_map(|profile| live_session_pids(&profile.dir.join("sessions")))
        .collect();
    running_sessions(home, &pids).values().any(|(s, _)| s == id)
}

/// Config folders under `home` that hold transcripts.
pub fn profiles(home: &Path) -> Vec<Profile> {
    let mut out = Vec::new();
    let default = home.join(".claude");
    if default.join("projects").is_dir() {
        out.push(Profile {
            name: "claude".into(),
            config_dir: None,
            dir: default,
        });
    }
    let mut others: Vec<Profile> = fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let dir = e.path();
            // Its path is typed into a terminal to resume a session there.
            let typable = !name.chars().any(char::is_control);
            (name.starts_with(".claude-") && typable && dir.join("projects").is_dir()).then(|| {
                Profile {
                    name: name.trim_start_matches('.').into(),
                    config_dir: Some(dir.clone()),
                    dir,
                }
            })
        })
        .collect();
    others.sort_by(|a, b| a.name.cmp(&b.name));
    out.extend(others);
    out
}

/// The folder name Claude Code files a project's transcripts under.
pub fn project_folder(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// What a transcript says about its session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    /// The title Claude Code generated for the session, if any.
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    /// Prompts typed plus replies, each streamed reply counted once.
    pub messages: usize,
    /// Lines that were not JSON, such as one cut off mid-write.
    pub malformed: usize,
}

impl Summary {
    pub fn label(&self) -> Option<&str> {
        self.title.as_deref().or(self.first_prompt.as_deref())
    }
}

/// A parse in progress, kept so a growing transcript is read on from where it stopped.
#[derive(Clone, Debug, Default)]
struct Reader {
    summary: Summary,
    /// A streamed reply repeats its id on every line.
    replies: HashSet<String>,
    prompts: usize,
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
    #[serde(rename = "isMeta", default)]
    is_meta: bool,
    #[serde(rename = "isCompactSummary", default)]
    is_compact_summary: bool,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    content: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct WithContent {
    message: ContentOnly,
}

#[derive(Deserialize)]
struct ContentOnly {
    content: Value,
}

/// What a person typed, before it is shortened for a title.
pub(crate) enum Typed {
    Text(String),
    Command(String),
}

/// The text a person typed, or `None` for a tool result or a line the harness added.
pub(crate) fn typed_text(content: &Value) -> Option<Typed> {
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            if blocks.iter().any(|b| b["type"] == "tool_result") {
                return None;
            }
            blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => return None,
    };
    let text = text.trim();
    if let Some(start) = text.find("<command-name>") {
        let rest = &text[start + "<command-name>".len()..];
        let name = rest.split("</command-name>").next()?.trim();
        let args = text
            .split("<command-args>")
            .nth(1)
            .and_then(|a| a.split("</command-args>").next())
            .map(str::trim)
            .unwrap_or_default();
        return Some(Typed::Command(format!("{name} {args}").trim().to_string()));
    }
    // Hook output, reminders and local command echoes are wrapped in tags; typed text is not.
    if text.is_empty() || text.starts_with('<') {
        return None;
    }
    Some(Typed::Text(text.to_string()))
}

fn prompt_text(content: &Value) -> Option<String> {
    match typed_text(content)? {
        Typed::Text(t) | Typed::Command(t) => Some(one_line(&t)),
    }
}

fn one_line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match joined.char_indices().nth(TITLE_CHARS) {
        Some((cut, _)) => format!("{}…", &joined[..cut]),
        None => joined,
    }
}

impl Reader {
    fn line(&mut self, text: &str) {
        let Ok(line) = serde_json::from_str::<Line>(text) else {
            if !text.trim().is_empty() {
                self.summary.malformed += 1;
            }
            return;
        };
        match line.kind.as_deref() {
            Some("ai-title") => {
                if let Some(title) = line.ai_title.filter(|t| !t.trim().is_empty()) {
                    self.summary.title = Some(one_line(&title));
                }
            }
            Some("assistant") => {
                let Some(message) = line.message else {
                    return;
                };
                let id = message.id.unwrap_or_default();
                if !self.replies.contains(&id) {
                    self.summary.messages += 1;
                }
                if id.is_empty() {
                    let key = format!("line-{}", self.replies.len());
                    self.replies.insert(key);
                } else {
                    self.replies.insert(id);
                }
            }
            Some("user") if !line.is_meta && !line.is_compact_summary => {
                if line.message.as_ref().is_none_or(|m| m.content.is_none()) {
                    return;
                }
                let Ok(full) = serde_json::from_str::<WithContent>(text) else {
                    return;
                };
                if let Some(prompt) = prompt_text(&full.message.content) {
                    self.prompts += 1;
                    self.summary.messages += 1;
                    if self.summary.first_prompt.is_none() {
                        self.summary.first_prompt = Some(prompt);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Reads the next whole line into `buf` and returns its length on disk, or `None` at the end or
/// at a line still being written. Past [`MAX_LINE`] only that much is kept, which reads as malformed.
pub(crate) fn next_line(input: &mut impl BufRead, buf: &mut Vec<u8>) -> Option<u64> {
    buf.clear();
    let n = input.by_ref().take(MAX_LINE).read_until(b'\n', buf).ok()? as u64;
    if buf.ends_with(b"\n") {
        return Some(n);
    }
    if n < MAX_LINE {
        return None;
    }
    let mut skipped = 0;
    loop {
        let chunk = input.fill_buf().ok()?;
        if chunk.is_empty() {
            return None;
        }
        let (len, end) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        input.consume(len);
        skipped += len as u64;
        if end {
            return Some(n + skipped);
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub id: String,
    pub profile: Profile,
    pub path: PathBuf,
    pub modified: SystemTime,
    pub summary: Summary,
}

struct Cached {
    len: u64,
    modified: SystemTime,
    /// Where the next unread line starts.
    offset: u64,
    reader: Reader,
}

static CACHE: Mutex<Option<HashMap<PathBuf, Cached>>> = Mutex::new(None);

/// Reads `path` from where the last read stopped, or from the start if it shrank.
fn read_cached(path: &Path, len: u64, modified: SystemTime) -> Summary {
    let mut guard = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    let cache = guard.get_or_insert_with(HashMap::new);
    let mut entry = match cache.remove(path) {
        Some(c) if c.len == len && c.modified == modified => {
            let summary = c.reader.summary.clone();
            cache.insert(path.to_path_buf(), c);
            return summary;
        }
        Some(c) if c.offset <= len => c,
        _ => Cached {
            len: 0,
            modified,
            offset: 0,
            reader: Reader::default(),
        },
    };
    drop(guard);
    if let Ok(mut file) = fs::File::open(path)
        && file.seek(SeekFrom::Start(entry.offset)).is_ok()
    {
        let mut input = BufReader::new(file);
        let mut buf = Vec::new();
        // A line still being written is read again next time.
        while let Some(n) = next_line(&mut input, &mut buf) {
            entry.offset += n;
            entry.reader.line(&String::from_utf8_lossy(&buf));
        }
    }
    entry.len = len;
    entry.modified = modified;
    let summary = entry.reader.summary.clone();
    let mut guard = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    guard
        .get_or_insert_with(HashMap::new)
        .insert(path.to_path_buf(), entry);
    summary
}

/// The transcript folders of the project at `root` in every profile.
fn project_dirs(home: &Path, root: &Path) -> Vec<(Profile, PathBuf)> {
    let folder = project_folder(root);
    profiles(home)
        .into_iter()
        .flat_map(|profile| {
            let projects = profile.dir.join("projects");
            let exact = projects.join(&folder);
            let dirs: Vec<PathBuf> = if folder.len() <= MAX_FOLDER {
                vec![exact]
            } else {
                fs::read_dir(&projects)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with(&folder[..MAX_FOLDER]))
                    })
                    .collect()
            };
            dirs.into_iter()
                .filter(|d| d.is_dir())
                .map(move |d| (profile.clone(), d))
        })
        .collect()
}

/// The transcript of session `id` in whichever profile holds it for the project at `cwd`.
pub fn find(home: &Path, cwd: &Path, id: &str) -> Option<PathBuf> {
    if !crate::snapshots::valid_session(id) {
        return None;
    }
    let name = format!("{id}.jsonl");
    project_dirs(home, cwd)
        .into_iter()
        .map(|(_, dir)| dir.join(&name))
        .find(|path| path.is_file())
}

/// The profile holding the project's most recently written transcript, from file times alone.
pub fn latest_profile(home: &Path, root: &Path) -> Option<Profile> {
    latest_session(home, root).map(|(_, profile)| profile)
}

/// The id and profile of the project's most recently written transcript, from file times alone.
pub fn latest_session(home: &Path, root: &Path) -> Option<(String, Profile)> {
    project_dirs(home, root)
        .into_iter()
        .flat_map(|(profile, dir)| {
            fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(move |e| {
                    let path = e.path();
                    if path.extension().is_none_or(|x| x != "jsonl") {
                        return None;
                    }
                    let id = path.file_stem()?.to_string_lossy().into_owned();
                    if !crate::snapshots::valid_session(&id) {
                        return None;
                    }
                    let modified = e.metadata().ok()?.modified().ok()?;
                    Some((modified, id, profile.clone()))
                })
        })
        .max_by_key(|(modified, ..)| *modified)
        .map(|(_, id, profile)| (id, profile))
}

/// The `limit` most recently active sessions of the project at `root`, newest first.
pub fn sessions(home: &Path, root: &Path, limit: usize) -> Vec<Session> {
    let mut found: Vec<(SystemTime, u64, String, Profile, PathBuf)> = Vec::new();
    for (profile, dir) in project_dirs(home, root) {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let Some(id) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            if !crate::snapshots::valid_session(&id) {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            found.push((modified, meta.len(), id, profile.clone(), path));
        }
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found
        .into_iter()
        .take(limit)
        .map(|(modified, len, id, profile, path)| Session {
            summary: read_cached(&path, len, modified),
            id,
            profile,
            path,
            modified,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(input: impl std::io::Read) -> Summary {
        let mut reader = Reader::default();
        for line in BufReader::new(input).split(b'\n').map_while(Result::ok) {
            reader.line(&String::from_utf8_lossy(&line));
        }
        reader.summary
    }

    fn lines(values: &[Value]) -> String {
        values.iter().map(|v| v.to_string() + "\n").collect()
    }

    fn reply(id: &str, model: &str, input: u64, output: u64) -> Value {
        json!({
            "type": "assistant",
            "message": {
                "id": id, "model": model, "role": "assistant",
                "content": [{ "type": "text", "text": "ok" }],
                "usage": {
                    "input_tokens": input, "output_tokens": output,
                    "cache_creation_input_tokens": 300, "cache_read_input_tokens": 1000,
                    "cache_creation": { "ephemeral_5m_input_tokens": 100, "ephemeral_1h_input_tokens": 200 }
                }
            }
        })
    }

    fn prompt(content: Value) -> Value {
        json!({ "type": "user", "message": { "role": "user", "content": content } })
    }

    #[test]
    fn a_streamed_reply_is_counted_once() {
        let text = lines(&[
            prompt(json!("fix the parser")),
            reply("msg_1", "claude-opus-5", 10, 1),
            reply("msg_1", "claude-opus-5", 10, 50),
            reply("msg_2", "claude-haiku-4-5-20251001", 5, 5),
        ]);
        let s = parse(text.as_bytes());
        assert_eq!(s.messages, 3);
    }

    #[test]
    fn the_title_prefers_claudes_own_then_the_first_typed_prompt() {
        let text = lines(&[
            json!({ "type": "user", "isMeta": true, "message": { "content": "<local-command-caveat>x" } }),
            prompt(json!([{ "type": "tool_result", "content": "output" }])),
            prompt(json!("<system-reminder>ignore me</system-reminder>")),
            prompt(json!([{ "type": "text", "text": "  add a\n session   list " }])),
            prompt(json!("second prompt")),
        ]);
        let s = parse(text.as_bytes());
        assert_eq!(s.first_prompt.as_deref(), Some("add a session list"));
        assert_eq!(s.label(), Some("add a session list"));
        assert_eq!(s.messages, 2);
        let titled = text + &lines(&[json!({ "type": "ai-title", "aiTitle": "Session list" })]);
        assert_eq!(parse(titled.as_bytes()).label(), Some("Session list"));

        let command = lines(&[prompt(json!(
            "<command-message>review</command-message>\n<command-name>/review</command-name>\n<command-args>main</command-args>"
        ))]);
        assert_eq!(
            parse(command.as_bytes()).first_prompt.as_deref(),
            Some("/review main")
        );
    }

    #[test]
    fn malformed_and_cut_off_lines_are_skipped() {
        let mut text = lines(&[prompt(json!("hello")), reply("m", "claude-opus-5", 1, 1)]);
        text.push_str("{ not json\n\n[1,2]\n");
        text.push_str(r#"{"type":"assistant","message":{"id":"m2","usage":{"input_"#);
        let s = parse(text.as_bytes());
        assert_eq!(s.malformed, 3);
        assert_eq!(s.messages, 2);
    }

    #[test]
    fn a_line_past_the_cap_is_skipped_whole_and_reading_goes_on() {
        let long = format!("{{\"x\":\"{}\"}}\n", "a".repeat(MAX_LINE as usize));
        let after = lines(&[reply("m", "claude-opus-5", 1, 1)]);
        let text = format!("{long}{after}");
        let mut input = BufReader::new(text.as_bytes());
        let mut buf = Vec::new();
        assert_eq!(next_line(&mut input, &mut buf), Some(long.len() as u64));
        assert_eq!(buf.len() as u64, MAX_LINE, "only the cap is kept");
        assert_eq!(next_line(&mut input, &mut buf), Some(after.len() as u64));
        assert_eq!(buf, after.as_bytes());
        assert_eq!(next_line(&mut input, &mut buf), None);
        let cut_off = &long[..long.len() - 1];
        assert_eq!(
            next_line(&mut BufReader::new(cut_off.as_bytes()), &mut buf),
            None
        );
    }

    #[test]
    fn a_config_folder_named_with_control_characters_is_not_a_profile() {
        let home = temp("control");
        fs::create_dir_all(home.join(".claude-ok/projects")).unwrap();
        fs::create_dir_all(home.join(".claude-x\n\x1b[2J/projects")).unwrap();
        let names: Vec<String> = profiles(&home).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["claude-ok"]);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn folders_are_named_as_claude_code_names_them() {
        assert_eq!(
            project_folder(Path::new("/Users/me/code/hobby/daedalus")),
            "-Users-me-code-hobby-daedalus"
        );
        assert_eq!(
            project_folder(Path::new("/Users/me/code/tlsc/.github")),
            "-Users-me-code-tlsc--github"
        );
        assert_eq!(project_folder(Path::new("/a/b_c d")), "-a-b-c-d");
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("daedalus-tx-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_pid_file(home: &Path, profile: &str, pid: i32, body: &str) {
        let dir = home.join(profile).join("sessions");
        fs::create_dir_all(home.join(profile).join("projects")).unwrap();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{pid}.json")), body).unwrap();
    }

    const SID: &str = "0b6c1e3a-1f2d-4c1b-9d55-1f0d2c3b4a5e";
    const OTHER: &str = "2d8e3a5c-3b4f-4e3d-9f77-3b2f4e5d6c7a";

    #[test]
    fn running_sessions_reads_a_non_default_profile() {
        let home = temp("running");
        let body = format!(r#"{{"pid":123,"sessionId":"{SID}","cwd":"/work/app","extra":1}}"#);
        write_pid_file(&home, ".claude-tlsc", 123, &body);
        let found = running_sessions(&home, &[123, 456]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[&123], (SID.to_string(), PathBuf::from("/work/app")));
    }

    #[test]
    fn running_sessions_skips_a_mismatched_pid_an_invalid_id_and_a_missing_file() {
        let home = temp("running-bad");
        let wrong_pid = format!(r#"{{"pid":999,"sessionId":"{SID}","cwd":"/a"}}"#);
        write_pid_file(&home, ".claude-tlsc", 1, &wrong_pid);
        write_pid_file(
            &home,
            ".claude-tlsc",
            2,
            r#"{"pid":2,"sessionId":"../x","cwd":"/a"}"#,
        );
        write_pid_file(&home, ".claude-tlsc", 3, "not json");
        assert!(running_sessions(&home, &[1, 2, 3, 4]).is_empty());
    }

    #[test]
    fn live_session_pids_keeps_only_live_numeric_pid_files() {
        let home = temp("live-pids");
        let own = std::process::id() as i32;
        write_pid_file(&home, ".claude", own, "{}");
        write_pid_file(&home, ".claude", 999_999_999, "{}");
        write_pid_file(&home, ".claude", 0, "{}");
        let dir = home.join(".claude/sessions");
        fs::write(dir.join("notapid.json"), "{}").unwrap();
        fs::write(dir.join(format!("{own}.txt")), "").unwrap();
        assert_eq!(live_session_pids(&dir), vec![own]);
        assert!(live_session_pids(&home.join("missing")).is_empty());
    }

    #[test]
    fn a_live_process_holding_the_session_is_running_elsewhere() {
        let home = temp("elsewhere");
        let own = std::process::id() as i32;
        let body = format!(r#"{{"pid":{own},"sessionId":"{SID}","cwd":"/a"}}"#);
        write_pid_file(&home, ".claude", own, &body);
        let dead = format!(r#"{{"pid":999999999,"sessionId":"{OTHER}","cwd":"/a"}}"#);
        write_pid_file(&home, ".claude", 999_999_999, &dead);
        assert!(session_running_elsewhere(&home, SID));
        assert!(!session_running_elsewhere(&home, OTHER));
    }

    #[test]
    fn running_sessions_parses_a_pid_file_without_a_cwd() {
        let home = temp("running-nocwd");
        write_pid_file(
            &home,
            ".claude",
            5,
            &format!(r#"{{"pid":5,"sessionId":"{SID}"}}"#),
        );
        assert_eq!(running_sessions(&home, &[5])[&5].1, PathBuf::new());
    }

    #[test]
    fn running_sessions_prefers_the_latest_start_across_profiles() {
        let home = temp("running-latest");
        let newer = "1c7d2f4b-2a3e-4d2c-8e66-2a1e3d4c5b6f";
        let old = format!(r#"{{"pid":7,"sessionId":"{SID}","cwd":"/a","startedAt":10}}"#);
        let new = format!(r#"{{"pid":7,"sessionId":"{newer}","cwd":"/b","startedAt":20}}"#);
        write_pid_file(&home, ".claude", 7, &new);
        write_pid_file(&home, ".claude-work", 7, &old);
        assert_eq!(running_sessions(&home, &[7])[&7].0, newer);
    }

    #[test]
    fn sessions_come_from_every_profile_newest_first() {
        let home = temp("home");
        let root = Path::new("/work/app");
        let folder = project_folder(root);
        let default = home.join(".claude/projects").join(&folder);
        let work = home.join(".claude-work/projects").join(&folder);
        fs::create_dir_all(&default).unwrap();
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(home.join(".claude-mem")).unwrap();
        let old = "0b6c1e3a-1f2d-4c1b-9d55-1f0d2c3b4a5e";
        let new = "1c7d2f4b-2a3e-4d2c-8e66-2a1e3d4c5b6f";
        fs::write(
            default.join(format!("{old}.jsonl")),
            lines(&[prompt(json!("old one"))]),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(
            work.join(format!("{new}.jsonl")),
            lines(&[
                prompt(json!("new one")),
                reply("a", "claude-opus-5", 10, 10),
            ]),
        )
        .unwrap();
        fs::write(work.join("not-a-session.txt"), "x").unwrap();

        let names: Vec<String> = profiles(&home).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["claude", "claude-work"]);
        let list = sessions(&home, root, 10);
        assert_eq!(
            list.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            [new, old]
        );
        assert_eq!(list[0].profile.config_dir, Some(home.join(".claude-work")));
        assert_eq!(list[1].profile.config_dir, None);
        assert_eq!(list[0].summary.messages, 2);
        assert_eq!(sessions(&home, root, 1).len(), 1);
        assert_eq!(
            latest_profile(&home, root).map(|p| p.name),
            Some("claude-work".into())
        );
        assert_eq!(latest_profile(&home, Path::new("/work/other")), None);
        assert_eq!(
            latest_session(&home, root).map(|(id, p)| (id, p.name)),
            Some((new.into(), "claude-work".into()))
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_growing_transcript_is_read_on_from_where_it_stopped() {
        use std::io::Write;
        let dir = temp("grow");
        let path = dir.join("s.jsonl");
        fs::write(&path, lines(&[prompt(json!("first"))])).unwrap();
        let meta = |p: &Path| {
            let m = fs::metadata(p).unwrap();
            (m.len(), m.modified().unwrap())
        };
        let (len, at) = meta(&path);
        assert_eq!(read_cached(&path, len, at).messages, 1);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        // A line still being written stays unread until it ends.
        let half = reply("r1", "claude-opus-5", 7, 7).to_string();
        file.write_all(&half.as_bytes()[..20]).unwrap();
        let (len, at) = meta(&path);
        assert_eq!(read_cached(&path, len, at).messages, 1);
        file.write_all(format!("{}\n", &half[20..]).as_bytes())
            .unwrap();
        let (len, at) = meta(&path);
        let s = read_cached(&path, len, at);
        assert_eq!((s.messages, s.malformed), (2, 0));
        fs::remove_dir_all(dir).unwrap();
    }
}
