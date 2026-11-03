use serde::{Deserialize, Serialize};

pub const PROTO: u32 = 1;

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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        proto: u32,
        server: String,
        name: String,
        app: String,
    },
    Snapshot {
        seq: u64,
        projects: Vec<Project>,
        sessions: Vec<Session>,
        terminals: Vec<Terminal>,
    },
    Attached {
        pane: u64,
        rows: u16,
        cols: u16,
    },
    ReplayDone {
        pane: u64,
    },
    Exited {
        pane: u64,
        code: i32,
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
    pub unread: u32,
    pub todos: Vec<Todo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs: Option<String>,
    pub files: Vec<String>,
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

    fn session() -> Session {
        Session {
            id: "5d1f0c2e-8a47-4b9e-9c1a-3f6e2b7a9d10".into(),
            label: "Add the phone bridge".into(),
            pane: Some(123),
            project: Some("/Users/jason/code/daedalus".into()),
            cwd: "/Users/jason/code/daedalus/.claude/worktrees/bridge".into(),
            main_root: "/Users/jason/code/daedalus".into(),
            branch: Some("feat/bridge".into()),
            state: SessionState::NeedsInput,
            unread: 2,
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
            files: vec!["crates/daedalus/src/companion/protocol.rs".into()],
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
            "spawn_claude",
            &ClientMsg::SpawnClaude {
                id: "r1".into(),
                project: "/Users/jason/code/daedalus".into(),
            },
        );
        golden("refresh", &ClientMsg::Refresh);
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
                        cwd: Some("/Users/jason/code/daedalus".into()),
                        claude: Some(SessionState::Running),
                        rows: 40,
                        cols: 120,
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
                    },
                ],
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
        golden("replay_done", &ServerMsg::ReplayDone { pane: 123 });
        golden("exited", &ServerMsg::Exited { pane: 123, code: 0 });
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
