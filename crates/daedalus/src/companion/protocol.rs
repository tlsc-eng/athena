use serde::{Deserialize, Serialize};

pub const PROTO: u32 = 1;

/// Optional capabilities the desktop lists in `welcome`.
pub const FEATURES: &[&str] = &[
    "transcript",
    "usage",
    "act",
    "dialog",
    "answer",
    "subagents",
    "fit",
    "tool_output",
    "suggestion",
];

/// Largest payload of one binary frame.
pub const MAX_PAYLOAD: usize = 64 * 1024;

const HEADER_LEN: usize = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Output = 1,
    Input = 2,
}

/// Builds a binary frame; the caller keeps `payload` within `MAX_PAYLOAD`.
pub fn frame(kind: Kind, pane: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.push(kind as u8);
    out.extend_from_slice(&pane.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Splits a binary frame, returning `None` for a short header, an unknown kind or an oversized payload.
pub fn parse(bytes: &[u8]) -> Option<(Kind, u64, &[u8])> {
    if bytes.len() < HEADER_LEN {
        return None;
    }
    let kind = match bytes[0] {
        1 => Kind::Output,
        2 => Kind::Input,
        _ => return None,
    };
    let pane = u64::from_be_bytes(bytes[1..HEADER_LEN].try_into().ok()?);
    let payload = &bytes[HEADER_LEN..];
    (payload.len() <= MAX_PAYLOAD).then_some((kind, pane, payload))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        proto: u32,
        app: String,
        device: String,
    },
    Attach {
        pane: u64,
        tail: u64,
    },
    Detach {
        pane: u64,
    },
    SpawnClaude {
        id: String,
        project: String,
    },
    Refresh,
    TranscriptOpen {
        id: String,
        session: String,
        limit: u32,
    },
    TranscriptMore {
        id: String,
        session: String,
        before: u64,
        limit: u32,
    },
    TranscriptClose {
        session: String,
    },
    Prompt {
        id: String,
        pane: u64,
        text: String,
    },
    Interrupt {
        id: String,
        pane: u64,
    },
    Answer {
        id: String,
        pane: u64,
        key: String,
        answers: Vec<AnswerPick>,
    },
    UsageRefresh {
        id: String,
    },
    Fit {
        id: String,
        pane: u64,
        rows: u16,
        cols: u16,
    },
    Unfit {
        pane: u64,
    },
}

/// What a phone chose for one question; a plain select dialog takes the label alone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnswerPick {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pick: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        proto: u32,
        server: String,
        name: String,
        app: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        features: Vec<String>,
    },
    Snapshot {
        seq: u64,
        projects: Vec<Project>,
        sessions: Vec<Session>,
        terminals: Vec<Terminal>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        usage: Vec<UsageProfile>,
    },
    Attached {
        pane: u64,
        rows: u16,
        cols: u16,
    },
    Fitted {
        id: String,
        pane: u64,
        rows: u16,
        cols: u16,
    },
    ReplayDone {
        pane: u64,
    },
    Exited {
        pane: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
    },
    Dropped {
        pane: u64,
        reason: DropReason,
    },
    Spawned {
        id: String,
        pane: u64,
    },
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        message: String,
    },
    Notice {
        pane: u64,
        kind: NoticeKind,
        message: String,
        at: u64,
    },
    Transcript {
        session: String,
        kind: TranscriptKind,
        items: Vec<Item>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<u64>,
    },
    Sent {
        id: String,
    },
    Subagents {
        session: String,
        agents: Vec<Subagent>,
    },
}

/// Sent by the phone on the pair endpoint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PairRequest {
    Pair { device: String, proto: u32 },
}

/// The desktop's answer on the pair endpoint, followed by a close.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PairReply {
    Paired {
        token: String,
        server: String,
        name: String,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    Lagging,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    NeedsInput,
    Stopped,
}

/// How the phone applies a `transcript` message to its copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptKind {
    Page,
    Older,
    Append,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentState {
    Running,
    Done,
    Failed,
    Stopped,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// A sub-agent Claude launched with an `Agent` call, joined to its chat item by `tool_id`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Subagent {
    pub tool_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub agent_type: String,
    pub description: String,
    pub state: SubagentState,
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_at: Option<u64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tools: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    User,
    Assistant,
    Tool,
    Question,
    Note,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Pending,
    Ok,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub multi: bool,
    pub options: Vec<QuestionOption>,
}

/// One entry of a session transcript, identified by `id` and located by its byte offset `pos`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub pos: u64,
    pub at: u64,
    pub kind: ItemKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cut: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<Question>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileChange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub more_files: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<Permission>,
    /// Byte offset of the result line, kept for fetching the full output later.
    #[serde(skip)]
    pub result_pos: Option<u64>,
}

/// One file a tool call changed, with its added and removed line counts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub added: u32,
    pub removed: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub created: bool,
}

/// How Claude Code decided whether a tool call could run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Auto,
    Mode,
    Rule,
    User,
    DeniedAuto,
    DeniedHook,
    Denied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Running,
    NeedsInput,
    Finished,
    Idle,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub root: String,
    pub name: String,
    pub window: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub cwd: String,
    pub main_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub state: SessionState,
    pub unread: bool,
    pub todos: Vec<Todo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs: Option<String>,
    pub files: u32,
    pub modified_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Terminal {
    pub pane: u64,
    pub project: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<SessionState>,
    pub rows: u16,
    pub cols: u16,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fitted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialog: Option<Dialog>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DialogKind {
    Select,
    Question,
    Review,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DialogTab {
    pub label: String,
    pub answered: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DialogOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub input: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Dialog {
    pub key: String,
    pub kind: DialogKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<DialogTab>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multi: bool,
    pub options: Vec<DialogOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlight: Option<u32>,
    #[serde(default)]
    pub partial: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageStatus {
    Ok,
    Stale,
    NoAccess,
    RateLimited,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    pub label: String,
    pub used: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
}

/// One Claude sign-in's plan limits; `windows` is empty unless `status` is `ok`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageProfile {
    pub profile: String,
    pub status: UsageStatus,
    pub windows: Vec<UsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde::de::DeserializeOwned;

    use super::*;

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../design/protocol")
    }

    /// Writes the fixture under `UPDATE_FIXTURES=1`, otherwise requires the committed file to equal `msg`.
    fn golden<T>(name: &str, msg: &T)
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let path = fixture_dir().join(format!("{name}.json"));
        let json = serde_json::to_string_pretty(msg).unwrap() + "\n";
        if std::env::var_os("UPDATE_FIXTURES").is_some() {
            std::fs::create_dir_all(fixture_dir()).unwrap();
            std::fs::write(&path, &json).unwrap();
        }
        let committed = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}; run with UPDATE_FIXTURES=1", path.display()));
        assert_eq!(
            committed, json,
            "{name} drifted; run with UPDATE_FIXTURES=1"
        );
        let parsed: T = serde_json::from_str(&committed).unwrap();
        assert_eq!(&parsed, msg);
    }

    const SESSION_ID: &str = "5d1f0c2e-8a47-4b9e-9c1a-3f6e2b7a9d10";

    fn option(label: &str) -> DialogOption {
        DialogOption {
            label: label.into(),
            description: None,
            checked: None,
            input: false,
            text: None,
        }
    }

    fn bash_dialog() -> Dialog {
        Dialog {
            key: "d41c9a07".into(),
            kind: DialogKind::Select,
            title: Some("Bash command".into()),
            lines: vec!["npm test".into(), "Run the test suite".into()],
            prompt: Some("Do you want to proceed?".into()),
            tabs: Vec::new(),
            multi: false,
            options: vec![
                option("Yes"),
                DialogOption {
                    input: true,
                    text: Some("npm test *".into()),
                    ..option("Yes, and don’t ask again for")
                },
                option("No"),
            ],
            submit: None,
            highlight: Some(0),
            partial: false,
        }
    }

    fn question_dialog() -> Dialog {
        Dialog {
            key: "5e0b2f11".into(),
            kind: DialogKind::Question,
            title: Some("Which sizes?".into()),
            lines: Vec::new(),
            prompt: None,
            tabs: vec![
                DialogTab {
                    label: "Color".into(),
                    answered: true,
                },
                DialogTab {
                    label: "Size".into(),
                    answered: false,
                },
            ],
            multi: true,
            options: vec![
                DialogOption {
                    description: Some("warm".into()),
                    checked: Some(true),
                    ..option("Red")
                },
                DialogOption {
                    checked: Some(false),
                    input: true,
                    text: Some(String::new()),
                    ..option("Other")
                },
            ],
            submit: Some("Submit".into()),
            highlight: Some(2),
            partial: false,
        }
    }

    fn item(id: &str, pos: u64, kind: ItemKind) -> Item {
        Item {
            id: id.into(),
            pos,
            at: 1_767_225_600_000,
            kind,
            text: None,
            cut: false,
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

    fn items() -> Vec<Item> {
        vec![
            Item {
                text: Some("run the tests".into()),
                ..item("u1", 1_024, ItemKind::User)
            },
            Item {
                text: Some("Running them now, and the output is long".into()),
                cut: true,
                ..item("a1", 1_300, ItemKind::Assistant)
            },
            Item {
                tool: Some("Bash".into()),
                tool_id: Some("toolu_01abc".into()),
                summary: Some("cargo test -p daedalus".into()),
                status: Some(ItemStatus::Error),
                detail: Some("error[E0432]: unresolved import".into()),
                ..item("t1", 1_800, ItemKind::Tool)
            },
            Item {
                tool: Some("Bash".into()),
                tool_id: Some("toolu_02abc".into()),
                summary: Some("cargo build".into()),
                status: Some(ItemStatus::Ok),
                output: Some("Compiling daedalus\nFinished dev profile".into()),
                lines: Some(42),
                files: vec![FileChange {
                    path: "src/main.rs".into(),
                    added: 3,
                    removed: 1,
                    created: true,
                }],
                more_files: Some(2),
                permission: Some(Permission::Auto),
                ..item("t2", 2_000, ItemKind::Tool)
            },
            Item {
                tool: Some("AskUserQuestion".into()),
                tool_id: Some("toolu_01def".into()),
                status: Some(ItemStatus::Ok),
                questions: vec![Question {
                    question: "Which database?".into(),
                    header: "Database".into(),
                    multi: false,
                    options: vec![
                        QuestionOption {
                            label: "SQLite".into(),
                            description: "Embedded, no server".into(),
                        },
                        QuestionOption {
                            label: "Postgres".into(),
                            description: "Hosted, shared".into(),
                        },
                    ],
                }],
                answer: Some("SQLite".into()),
                ..item("q1", 2_400, ItemKind::Question)
            },
            Item {
                text: Some("Conversation compacted".into()),
                ..item("n1", 3_000, ItemKind::Note)
            },
        ]
    }

    fn session() -> Session {
        Session {
            id: SESSION_ID.into(),
            label: "Add the phone bridge".into(),
            pane: Some(123),
            project: Some("/Users/jason/code/daedalus".into()),
            cwd: "/Users/jason/code/daedalus/.claude/worktrees/bridge".into(),
            main_root: "/Users/jason/code/daedalus".into(),
            branch: Some("feat/bridge".into()),
            state: SessionState::NeedsInput,
            unread: true,
            todos: vec![
                Todo {
                    content: "Write the protocol".into(),
                    status: "completed".into(),
                    active_form: None,
                },
                Todo {
                    content: "Add the server".into(),
                    status: "in_progress".into(),
                    active_form: Some("Adding the server".into()),
                },
            ],
            plan: Some("1. Types\n2. Server".into()),
            needs: Some("Run cargo test?".into()),
            files: 3,
            modified_ms: 1_767_225_600_000,
        }
    }

    #[test]
    fn the_phone_to_desktop_messages_match_their_fixtures() {
        golden(
            "hello",
            &ClientMsg::Hello {
                proto: PROTO,
                app: "0.1.0".into(),
                device: "Jason's iPhone".into(),
            },
        );
        golden(
            "attach",
            &ClientMsg::Attach {
                pane: 123,
                tail: 262_144,
            },
        );
        golden("detach", &ClientMsg::Detach { pane: 123 });
        golden(
            "fit",
            &ClientMsg::Fit {
                id: "r20".into(),
                pane: 123,
                rows: 44,
                cols: 57,
            },
        );
        golden("unfit", &ClientMsg::Unfit { pane: 123 });
        golden(
            "spawn_claude",
            &ClientMsg::SpawnClaude {
                id: "r1".into(),
                project: "/Users/jason/code/daedalus".into(),
            },
        );
        golden("refresh", &ClientMsg::Refresh);
        golden(
            "transcript_open",
            &ClientMsg::TranscriptOpen {
                id: "r5".into(),
                session: SESSION_ID.into(),
                limit: 60,
            },
        );
        golden(
            "transcript_more",
            &ClientMsg::TranscriptMore {
                id: "r6".into(),
                session: SESSION_ID.into(),
                before: 123_456,
                limit: 60,
            },
        );
        golden(
            "transcript_close",
            &ClientMsg::TranscriptClose {
                session: SESSION_ID.into(),
            },
        );
        golden(
            "prompt",
            &ClientMsg::Prompt {
                id: "r7".into(),
                pane: 123,
                text: "run the tests\nthen commit".into(),
            },
        );
        golden(
            "interrupt",
            &ClientMsg::Interrupt {
                id: "r10".into(),
                pane: 123,
            },
        );
    }

    #[test]
    fn an_answer_matches_its_fixture() {
        golden(
            "answer",
            &ClientMsg::Answer {
                id: "r12".into(),
                pane: 123,
                key: "3f2a9c10".into(),
                answers: vec![
                    AnswerPick {
                        question: Some("Which colour?".into()),
                        pick: vec!["Blue".into()],
                        other: None,
                    },
                    AnswerPick {
                        question: Some("Which sizes?".into()),
                        pick: vec!["A".into(), "B".into()],
                        other: Some("XXL".into()),
                    },
                ],
            },
        );
    }

    #[test]
    fn a_usage_refresh_matches_its_fixture() {
        golden(
            "usage_refresh",
            &ClientMsg::UsageRefresh { id: "r11".into() },
        );
    }

    #[test]
    fn a_snapshot_with_a_dialog_matches_its_fixture() {
        golden(
            "snapshot_dialog",
            &ServerMsg::Snapshot {
                seq: 9,
                projects: Vec::new(),
                sessions: Vec::new(),
                terminals: vec![Terminal {
                    pane: 123,
                    project: "/Users/jason/code/daedalus".into(),
                    title: "claude".into(),
                    program: Some("claude".into()),
                    cwd: None,
                    claude: Some(SessionState::NeedsInput),
                    rows: 40,
                    cols: 120,
                    fitted: false,
                    dialog: Some(question_dialog()),
                    suggestion: None,
                }],
                usage: Vec::new(),
            },
        );
    }

    #[test]
    fn a_snapshot_with_a_suggestion_matches_its_fixture() {
        golden(
            "snapshot_suggestion",
            &ServerMsg::Snapshot {
                seq: 10,
                projects: Vec::new(),
                sessions: Vec::new(),
                terminals: vec![Terminal {
                    pane: 123,
                    project: "/Users/jason/code/daedalus".into(),
                    title: "claude".into(),
                    program: Some("claude".into()),
                    cwd: None,
                    claude: Some(SessionState::Finished),
                    rows: 40,
                    cols: 120,
                    fitted: false,
                    dialog: None,
                    suggestion: Some("run the tests and commit".into()),
                }],
                usage: Vec::new(),
            },
        );
    }

    #[test]
    fn a_snapshot_with_usage_matches_its_fixture() {
        let window = |label: &str, used, resets_at| UsageWindow {
            label: label.into(),
            used,
            resets_at,
        };
        golden(
            "snapshot_usage",
            &ServerMsg::Snapshot {
                seq: 8,
                projects: Vec::new(),
                sessions: Vec::new(),
                terminals: Vec::new(),
                usage: vec![
                    UsageProfile {
                        profile: "claude".into(),
                        status: UsageStatus::Ok,
                        windows: vec![
                            window("5h", 42.5, Some(1_767_240_000)),
                            window("7d", 18., Some(1_767_700_000)),
                            window("Opus", 7., None),
                        ],
                        message: None,
                    },
                    UsageProfile {
                        profile: "claude-work".into(),
                        status: UsageStatus::Unavailable,
                        windows: Vec::new(),
                        message: Some("the usage service did not answer".into()),
                    },
                ],
            },
        );
    }

    #[test]
    fn the_desktop_to_phone_messages_match_their_fixtures() {
        golden(
            "welcome",
            &ServerMsg::Welcome {
                proto: PROTO,
                server: "a1b2c3d4e5f60718".into(),
                name: "Jason's Mac".into(),
                app: "2.0.1".into(),
                features: FEATURES.iter().map(|f| f.to_string()).collect(),
            },
        );
        golden(
            "transcript",
            &ServerMsg::Transcript {
                session: SESSION_ID.into(),
                kind: TranscriptKind::Page,
                items: items(),
                before: Some(1_024),
            },
        );
        golden("sent", &ServerMsg::Sent { id: "r7".into() });
        golden(
            "subagents",
            &ServerMsg::Subagents {
                session: SESSION_ID.into(),
                agents: vec![
                    Subagent {
                        tool_id: "toolu_01A".into(),
                        agent_id: Some("a1b2c3d4e5f607182".into()),
                        agent_type: "builder".into(),
                        description: "Build the transcript parser".into(),
                        state: SubagentState::Running,
                        started_at: 1_767_225_600_000,
                        ended_at: None,
                        active_at: Some(1_767_225_660_000),
                        tools: 12,
                        tool: Some("Bash".into()),
                    },
                    Subagent {
                        tool_id: "toolu_01B".into(),
                        agent_id: Some("a9f8e7d6c5b4a3921".into()),
                        agent_type: "Explore".into(),
                        description: "Find the callers".into(),
                        state: SubagentState::Done,
                        started_at: 1_767_225_500_000,
                        ended_at: Some(1_767_225_590_000),
                        active_at: None,
                        tools: 7,
                        tool: None,
                    },
                ],
            },
        );
        golden(
            "snapshot",
            &ServerMsg::Snapshot {
                seq: 7,
                projects: vec![Project {
                    root: "/Users/jason/code/daedalus".into(),
                    name: "daedalus".into(),
                    window: 1,
                }],
                sessions: vec![session()],
                terminals: vec![
                    Terminal {
                        pane: 123,
                        project: "/Users/jason/code/daedalus".into(),
                        title: "claude".into(),
                        program: Some("claude".into()),
                        cwd: Some("/Users/jason/code/daedalus/.claude/worktrees/bridge".into()),
                        claude: Some(SessionState::NeedsInput),
                        rows: 40,
                        cols: 120,
                        fitted: true,
                        dialog: Some(bash_dialog()),
                        suggestion: None,
                    },
                    Terminal {
                        pane: 124,
                        project: "/Users/jason/code/daedalus".into(),
                        title: "zsh".into(),
                        program: None,
                        cwd: None,
                        claude: None,
                        rows: 24,
                        cols: 80,
                        fitted: false,
                        dialog: None,
                        suggestion: None,
                    },
                ],
                usage: Vec::new(),
            },
        );
        golden(
            "attached",
            &ServerMsg::Attached {
                pane: 123,
                rows: 40,
                cols: 120,
            },
        );
        golden(
            "fitted",
            &ServerMsg::Fitted {
                id: "r20".into(),
                pane: 123,
                rows: 44,
                cols: 57,
            },
        );
        golden("replay_done", &ServerMsg::ReplayDone { pane: 123 });
        golden(
            "exited",
            &ServerMsg::Exited {
                pane: 123,
                code: Some(0),
            },
        );
        golden(
            "dropped",
            &ServerMsg::Dropped {
                pane: 123,
                reason: DropReason::Lagging,
            },
        );
        golden(
            "spawned",
            &ServerMsg::Spawned {
                id: "r1".into(),
                pane: 456,
            },
        );
        golden(
            "error",
            &ServerMsg::Error {
                id: Some("r1".into()),
                message: "project is not open in a window".into(),
            },
        );
        golden(
            "notice",
            &ServerMsg::Notice {
                pane: 123,
                kind: NoticeKind::NeedsInput,
                message: "Run cargo test?".into(),
                at: 1_767_225_600_000,
            },
        );
    }

    #[test]
    fn the_pair_messages_match_their_fixtures() {
        golden(
            "pair",
            &PairRequest::Pair {
                device: "Jason's iPhone".into(),
                proto: PROTO,
            },
        );
        golden(
            "paired",
            &PairReply::Paired {
                token: "0123456789abcdef".repeat(4),
                server: "a1b2c3d4e5f60718".into(),
                name: "Jason's Mac".into(),
            },
        );
    }

    #[test]
    fn an_exit_with_an_unknown_status_omits_the_code() {
        let msg = ServerMsg::Exited {
            pane: 5,
            code: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"exited","pane":5}"#);
        assert_eq!(serde_json::from_str::<ServerMsg>(&json).unwrap(), msg);
    }

    #[test]
    fn a_welcome_without_features_omits_the_field() {
        let msg = ServerMsg::Welcome {
            proto: PROTO,
            server: "a1b2c3d4e5f60718".into(),
            name: "Jason's Mac".into(),
            app: "2.0.1".into(),
            features: Vec::new(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("features"));
        assert_eq!(serde_json::from_str::<ServerMsg>(&json).unwrap(), msg);
    }

    #[test]
    fn an_error_without_an_id_omits_the_field() {
        let msg = ServerMsg::Error {
            id: None,
            message: "pairing was declined".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"error","message":"pairing was declined"}"#);
        assert_eq!(serde_json::from_str::<ServerMsg>(&json).unwrap(), msg);
        assert_eq!(
            serde_json::from_str::<PairReply>(&json).unwrap(),
            PairReply::Error {
                message: "pairing was declined".into()
            }
        );
    }

    #[test]
    fn a_message_with_an_unknown_type_is_rejected() {
        assert!(serde_json::from_str::<ClientMsg>(r#"{"type":"reboot"}"#).is_err());
        assert!(serde_json::from_str::<ServerMsg>(r#"{"type":"hello"}"#).is_err());
        assert!(
            serde_json::from_str::<PairRequest>(r#"{"type":"attach","pane":1,"tail":0}"#).is_err()
        );
    }

    #[test]
    fn a_frame_round_trips_through_parse() {
        let bytes = frame(Kind::Input, 0x0102_0304_0506_0708, b"ls\r");
        assert_eq!(bytes[..9], [2, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            parse(&bytes),
            Some((Kind::Input, 0x0102_0304_0506_0708, &b"ls\r"[..]))
        );
        assert_eq!(
            parse(&frame(Kind::Output, 9, b"")),
            Some((Kind::Output, 9, &b""[..]))
        );
    }

    #[test]
    fn a_malformed_frame_parses_to_none() {
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&[1, 0, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(parse(&[0, 0, 0, 0, 0, 0, 0, 0, 1]), None);
        assert_eq!(parse(&[3, 0, 0, 0, 0, 0, 0, 0, 1]), None);
    }

    #[test]
    fn a_frame_payload_over_the_limit_parses_to_none() {
        let at_limit = frame(Kind::Output, 1, &vec![0; MAX_PAYLOAD]);
        assert!(parse(&at_limit).is_some());
        let over = frame(Kind::Output, 1, &vec![0; MAX_PAYLOAD + 1]);
        assert_eq!(parse(&over), None);
    }
}
