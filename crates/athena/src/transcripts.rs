//! Claude Code's session transcripts (`~/.claude*/projects/<folder>/<session>.jsonl`), read for
//! the session list: title, activity, message count and token usage.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;

const TITLE_CHARS: usize = 200;
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
            (name.starts_with(".claude-") && dir.join("projects").is_dir()).then(|| Profile {
                name: name.trim_start_matches('.').into(),
                config_dir: Some(dir.clone()),
                dir,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub cache_read: u64,
}

impl Tokens {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_write_5m + self.cache_write_1h + self.cache_read
    }

    fn add(&mut self, other: &Tokens) {
        self.input += other.input;
        self.output += other.output;
        self.cache_write_5m += other.cache_write_5m;
        self.cache_write_1h += other.cache_write_1h;
        self.cache_read += other.cache_read;
    }
}

/// What a transcript says about its session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    /// The title Claude Code generated for the session, if any.
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    /// Prompts typed plus replies, each streamed reply counted once.
    pub messages: usize,
    /// Token usage by model id, subagents included.
    pub tokens: BTreeMap<String, Tokens>,
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
    /// Each reply's model and usage; a streamed reply repeats them on every line.
    replies: HashMap<String, (String, Tokens)>,
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
    model: Option<String>,
    usage: Option<Usage>,
    content: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    cache_creation: Option<CacheCreation>,
}

#[derive(Deserialize)]
struct CacheCreation {
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
}

impl Usage {
    fn tokens(&self) -> Tokens {
        let (five, hour) = match &self.cache_creation {
            Some(c) if c.ephemeral_5m_input_tokens + c.ephemeral_1h_input_tokens > 0 => {
                (c.ephemeral_5m_input_tokens, c.ephemeral_1h_input_tokens)
            }
            _ => (self.cache_creation_input_tokens, 0),
        };
        Tokens {
            input: self.input_tokens,
            output: self.output_tokens,
            cache_write_5m: five,
            cache_write_1h: hour,
            cache_read: self.cache_read_input_tokens,
        }
    }
}

#[derive(Deserialize)]
struct WithContent {
    message: ContentOnly,
}

#[derive(Deserialize)]
struct ContentOnly {
    content: Value,
}

/// The text a person typed, or `None` for a tool result or a line the harness added.
fn prompt_text(content: &Value) -> Option<String> {
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
                .join(" ")
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
        return Some(one_line(&format!("{name} {args}")));
    }
    // Hook output, reminders and local command echoes are wrapped in tags; typed text is not.
    if text.is_empty() || text.starts_with('<') {
        return None;
    }
    Some(one_line(text))
}

fn one_line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match joined.char_indices().nth(TITLE_CHARS) {
        Some((cut, _)) => format!("{}…", &joined[..cut]),
        None => joined,
    }
}

impl Reader {
    fn line(&mut self, text: &str, count_messages: bool) {
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
                let model = message.model.unwrap_or_default();
                let tokens = message.usage.map(|u| u.tokens()).unwrap_or_default();
                if count_messages && !self.replies.contains_key(&id) {
                    self.summary.messages += 1;
                }
                if id.is_empty() {
                    let key = format!("line-{}", self.replies.len());
                    self.replies.insert(key, (model, tokens));
                } else {
                    self.replies.insert(id, (model, tokens));
                }
            }
            Some("user") if count_messages && !line.is_meta && !line.is_compact_summary => {
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

    fn tokens(&self) -> BTreeMap<String, Tokens> {
        let mut out: BTreeMap<String, Tokens> = BTreeMap::new();
        for (model, tokens) in self.replies.values() {
            if tokens.total() > 0 {
                out.entry(model.clone()).or_default().add(tokens);
            }
        }
        out
    }
}

/// Token usage of a subagent's transcript, which counts toward its session.
fn subagent_tokens(path: &Path) -> BTreeMap<String, Tokens> {
    let Ok(file) = fs::File::open(path) else {
        return BTreeMap::new();
    };
    let mut reader = Reader::default();
    for line in BufReader::new(file).split(b'\n').map_while(Result::ok) {
        reader.line(&String::from_utf8_lossy(&line), false);
    }
    reader.tokens()
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
    subagents: BTreeMap<String, Tokens>,
}

static CACHE: Mutex<Option<HashMap<PathBuf, Cached>>> = Mutex::new(None);

/// Reads `path` from where the last read stopped, or from the start if it shrank.
fn read_cached(path: &Path, len: u64, modified: SystemTime) -> Summary {
    let mut guard = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    let cache = guard.get_or_insert_with(HashMap::new);
    let mut entry = match cache.remove(path) {
        Some(c) if c.len == len && c.modified == modified => {
            let summary = summarize(&c);
            cache.insert(path.to_path_buf(), c);
            return summary;
        }
        Some(c) if c.offset <= len => c,
        _ => Cached {
            len: 0,
            modified,
            offset: 0,
            reader: Reader::default(),
            subagents: BTreeMap::new(),
        },
    };
    drop(guard);
    if let Ok(mut file) = fs::File::open(path)
        && file.seek(SeekFrom::Start(entry.offset)).is_ok()
    {
        let mut input = BufReader::new(file);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match input.read_until(b'\n', &mut buf) {
                // A line still being written is read again next time.
                Ok(n) if n > 0 && buf.ends_with(b"\n") => {
                    entry.offset += n as u64;
                    entry.reader.line(&String::from_utf8_lossy(&buf), true);
                }
                _ => break,
            }
        }
    }
    entry.len = len;
    entry.modified = modified;
    entry.subagents = BTreeMap::new();
    let dir = path.with_extension("").join("subagents");
    for file in fs::read_dir(dir).into_iter().flatten().flatten() {
        if file.path().extension().is_some_and(|e| e == "jsonl") {
            for (model, tokens) in subagent_tokens(&file.path()) {
                entry.subagents.entry(model).or_default().add(&tokens);
            }
        }
    }
    let summary = summarize(&entry);
    let mut guard = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    guard
        .get_or_insert_with(HashMap::new)
        .insert(path.to_path_buf(), entry);
    summary
}

fn summarize(cached: &Cached) -> Summary {
    let mut summary = cached.reader.summary.clone();
    summary.tokens = cached.reader.tokens();
    for (model, tokens) in &cached.subagents {
        summary.tokens.entry(model.clone()).or_default().add(tokens);
    }
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

/// USD per million tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
}

impl Price {
    /// A model's price with the usual cache multiples of its input price.
    pub const fn of(input: f64, output: f64) -> Self {
        Self {
            input,
            output,
            cache_write_5m: input * 1.25,
            cache_write_1h: input * 2.,
            cache_read: input * 0.1,
        }
    }

    fn cost(&self, t: &Tokens) -> f64 {
        (t.input as f64 * self.input
            + t.output as f64 * self.output
            + t.cache_write_5m as f64 * self.cache_write_5m
            + t.cache_write_1h as f64 * self.cache_write_1h
            + t.cache_read as f64 * self.cache_read)
            / 1_000_000.
    }
}

/// List prices as Claude Code's own cost tally applied them when this was written; an estimate.
const PRICES: &[(&str, Price)] = &[
    (
        "claude-opus-5-5",
        Price {
            cache_read: 0.2,
            ..Price::of(4., 20.)
        },
    ),
    ("claude-opus-5", Price::of(5., 25.)),
    ("claude-haiku-5-5", Price::of(0.1, 0.5)),
    ("claude-opus-4-5", Price::of(5., 25.)),
    ("claude-opus-4-1", Price::of(15., 75.)),
    ("claude-opus-4", Price::of(15., 75.)),
    ("claude-sonnet-4-5", Price::of(3., 15.)),
    ("claude-sonnet-4", Price::of(3., 15.)),
    ("claude-haiku-4-5", Price::of(1., 5.)),
    ("claude-3-7-sonnet", Price::of(3., 15.)),
    ("claude-3-5-haiku", Price::of(0.8, 4.)),
];

/// A model id without a trailing `-YYYYMMDD` release date.
fn base_model(model: &str) -> &str {
    match model.rsplit_once('-') {
        Some((base, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => model,
    }
}

/// The price of `model`: the user's from settings first, then the built-in table.
pub fn price_of(model: &str, overrides: &HashMap<String, Price>) -> Option<Price> {
    let base = base_model(model);
    [model, base].into_iter().find_map(|id| {
        overrides
            .get(id)
            .copied()
            .or_else(|| PRICES.iter().find(|(m, _)| *m == id).map(|(_, p)| *p))
    })
}

/// The estimated cost in USD of `tokens`, and the models it has no price for.
pub fn cost(
    tokens: &BTreeMap<String, Tokens>,
    overrides: &HashMap<String, Price>,
) -> (f64, Vec<String>) {
    let mut total = 0.;
    let mut unpriced = Vec::new();
    for (model, t) in tokens {
        match price_of(model, overrides) {
            Some(price) => total += price.cost(t),
            None => unpriced.push(model.clone()),
        }
    }
    (total, unpriced)
}

/// `12.3k`, `4.5M`: token counts as they fit in a list row.
pub fn short_count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(input: impl std::io::Read) -> Summary {
        let mut reader = Reader::default();
        for line in BufReader::new(input).split(b'\n').map_while(Result::ok) {
            reader.line(&String::from_utf8_lossy(&line), true);
        }
        reader.summary.tokens = reader.tokens();
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
    fn a_streamed_reply_is_counted_once_and_its_last_usage_wins() {
        let text = lines(&[
            prompt(json!("fix the parser")),
            reply("msg_1", "claude-opus-5", 10, 1),
            reply("msg_1", "claude-opus-5", 10, 50),
            reply("msg_2", "claude-haiku-4-5-20251001", 5, 5),
        ]);
        let s = parse(text.as_bytes());
        assert_eq!(s.messages, 3);
        let opus = s.tokens["claude-opus-5"];
        assert_eq!(
            opus,
            Tokens {
                input: 10,
                output: 50,
                cache_write_5m: 100,
                cache_write_1h: 200,
                cache_read: 1000
            }
        );
        assert_eq!(s.tokens["claude-haiku-4-5-20251001"].input, 5);
        let total: u64 = s.tokens.values().map(Tokens::total).sum();
        assert_eq!(total, opus.total() + 5 + 5 + 300 + 1000);
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
        assert_eq!(s.tokens.len(), 1);
    }

    #[test]
    fn folders_are_named_as_claude_code_names_them() {
        assert_eq!(
            project_folder(Path::new("/Users/me/code/hobby/athena")),
            "-Users-me-code-hobby-athena"
        );
        assert_eq!(
            project_folder(Path::new("/Users/me/code/tlsc/.github")),
            "-Users-me-code-tlsc--github"
        );
        assert_eq!(project_folder(Path::new("/a/b_c d")), "-a-b-c-d");
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("athena-tx-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sessions_come_from_every_profile_newest_first_with_subagents_counted() {
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
        let sub = work.join(new).join("subagents");
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            sub.join("agent-1.jsonl"),
            lines(&[reply("b", "claude-opus-5", 100, 0)]),
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
        assert_eq!(list[0].summary.tokens["claude-opus-5"].input, 110);
        assert_eq!(
            list[0].summary.messages, 2,
            "subagent replies are not messages"
        );
        assert_eq!(sessions(&home, root, 1).len(), 1);
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
        assert_eq!(s.tokens["claude-opus-5"].input, 7);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cost_uses_the_users_prices_first_and_names_models_without_one() {
        let mut tokens = BTreeMap::new();
        tokens.insert(
            "claude-haiku-4-5-20251001".to_string(),
            Tokens {
                input: 909,
                output: 11,
                ..Tokens::default()
            },
        );
        tokens.insert(
            "claude-new-9".to_string(),
            Tokens {
                input: 1,
                ..Tokens::default()
            },
        );
        let (usd, unpriced) = cost(&tokens, &HashMap::new());
        assert!((usd - 0.000964).abs() < 1e-9, "{usd}");
        assert_eq!(unpriced, ["claude-new-9"]);

        let mut mine = HashMap::new();
        mine.insert("claude-new-9".to_string(), Price::of(1_000_000., 0.));
        mine.insert("claude-haiku-4-5".to_string(), Price::of(0., 0.));
        let (usd, unpriced) = cost(&tokens, &mine);
        assert_eq!((usd, unpriced.len()), (1., 0));
        assert_eq!(short_count(999), "999");
        assert_eq!(short_count(12_345), "12.3k");
        assert_eq!(short_count(4_500_000), "4.5M");
    }
}
