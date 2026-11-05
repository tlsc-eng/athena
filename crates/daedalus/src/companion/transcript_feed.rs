//! The transcripts one phone connection has open, and the messages that keep them current.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, SystemTime};

use tokio::task::JoinHandle;
use tokio::time::{Interval, MissedTickBehavior};

use super::protocol::{Item, ServerMsg, Subagent, TranscriptKind};
use super::server::Snapshot;
use super::transcript::{Feed, Read};
use crate::transcripts;

const MAX_OPEN: usize = 4;
const MAX_LIMIT: usize = 200;
const MAX_MESSAGE: usize = 512 << 10;
const POLL: Duration = Duration::from_millis(500);

type Stamp = Option<(u64, Option<SystemTime>)>;
type Reads = Vec<(String, u64, Read)>;

struct Open {
    // Tells a read that started before a reopen apart from the current feed.
    epoch: u64,
    path: PathBuf,
    feed: Arc<Mutex<Feed>>,
    stamp: Stamp,
    limit: usize,
    // Agents run and write their own files while the parent is idle, so the stamp alone cannot trigger a read.
    agents: bool,
    // Fleet edits beyond the capped list must not resend an identical one.
    sent: Vec<Subagent>,
}

/// Up to four transcripts of one connection, polled for growth every half second.
pub(super) struct Transcripts {
    home: Option<PathBuf>,
    open: HashMap<String, Open>,
    epoch: u64,
    reading: Option<JoinHandle<Reads>>,
    ready: VecDeque<ServerMsg>,
    tick: Interval,
}

impl Transcripts {
    pub(super) fn new(home: Option<PathBuf>) -> Self {
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + POLL, POLL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            home,
            open: HashMap::new(),
            epoch: 0,
            reading: None,
            ready: VecDeque::new(),
            tick,
        }
    }

    /// Reads the session's transcript and answers with its newest page; opening it again starts over.
    pub(super) async fn open(
        &mut self,
        snapshot: &Snapshot,
        id: String,
        session: String,
        limit: u32,
    ) -> ServerMsg {
        let Some(known) = snapshot.sessions.iter().find(|s| s.id == session) else {
            return error(id, "session is not open");
        };
        if !self.open.contains_key(&session) && self.open.len() >= MAX_OPEN {
            return error(id, "too many transcripts open");
        }
        let Some(home) = self.home.clone() else {
            return error(id, "transcript not found");
        };
        let (cwd, root) = (PathBuf::from(&known.cwd), PathBuf::from(&known.main_root));
        let wanted = session.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            let path = transcripts::find(&home, &cwd, &wanted)
                .or_else(|| transcripts::find(&home, &root, &wanted))?;
            let stamp = stamp(&path);
            let mut feed = Feed::new(path.clone());
            feed.read_on();
            Some((path, stamp, feed))
        })
        .await
        .ok()
        .flatten();
        let Some((path, stamp, mut feed)) = loaded else {
            return error(id, "transcript not found");
        };
        let limit = clamp(limit);
        let page = page_message(&session, &feed, limit);
        let agents = feed.subagents();
        feed.take_subagents_changed();
        let running = feed.agents_running();
        self.forget(&session);
        self.epoch += 1;
        let epoch = self.epoch;
        let sent = agents.clone();
        if !agents.is_empty() {
            self.ready.push_back(ServerMsg::Subagents {
                session: session.clone(),
                agents,
            });
        }
        self.open.insert(
            session,
            Open {
                epoch,
                path,
                feed: Arc::new(Mutex::new(feed)),
                stamp,
                limit,
                agents: running,
                sent,
            },
        );
        page
    }

    /// The page of items older than `before`.
    pub(super) fn more(&self, id: String, session: String, before: u64, limit: u32) -> ServerMsg {
        let Some(open) = self.open.get(&session) else {
            return error(id, "transcript is not open");
        };
        let budget = budget(&session, TranscriptKind::Older);
        // A poll may be re-parsing the file under this lock, and the socket task must not wait on it.
        let feed = match open.feed.try_lock() {
            Ok(feed) => feed,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => return error(id, "busy"),
        };
        let (items, older) = feed.page(Some(before), clamp(limit), budget);
        ServerMsg::Transcript {
            session,
            kind: TranscriptKind::Older,
            items,
            before: older,
        }
    }

    pub(super) fn close(&mut self, session: &str) {
        self.forget(session);
    }

    /// The next `transcript` message for the phone; safe to cancel.
    pub(super) async fn next(&mut self) -> ServerMsg {
        loop {
            if let Some(message) = self.ready.pop_front() {
                return message;
            }
            if let Some(reading) = &mut self.reading {
                let done = reading.await;
                self.reading = None;
                for (session, epoch, read) in done.unwrap_or_default() {
                    self.deliver(&session, epoch, read);
                }
                continue;
            }
            if self.open.is_empty() {
                std::future::pending::<()>().await;
            }
            self.tick.tick().await;
            self.start_reading();
        }
    }

    fn forget(&mut self, session: &str) {
        self.open.remove(session);
        self.ready.retain(|m| {
            !matches!(m, ServerMsg::Transcript { session: s, .. } | ServerMsg::Subagents { session: s, .. } if s == session)
        });
    }

    fn start_reading(&mut self) {
        let changed: Vec<_> = self
            .open
            .iter_mut()
            .filter_map(|(session, open)| {
                let now = stamp(&open.path);
                (now != open.stamp || open.agents).then(|| {
                    open.stamp = now;
                    (session.clone(), open.epoch, open.feed.clone())
                })
            })
            .collect();
        if changed.is_empty() {
            return;
        }
        self.reading = Some(tokio::task::spawn_blocking(move || {
            changed
                .into_iter()
                .map(|(session, epoch, feed)| {
                    let read = lock(&feed).read_on();
                    (session, epoch, read)
                })
                .collect()
        }));
    }

    fn deliver(&mut self, session: &str, epoch: u64, read: Read) {
        let Some(open) = self.open.get_mut(session).filter(|o| o.epoch == epoch) else {
            return;
        };
        let mut feed = lock(&open.feed);
        let reset = matches!(read, Read::Reset);
        match read {
            Read::Reset => self
                .ready
                .push_back(page_message(session, &feed, open.limit)),
            Read::Changed(indices) => {
                let items = indices.iter().filter_map(|&i| feed.items().get(i));
                let budget = budget(session, TranscriptKind::Append);
                for chunk in chunks(items, budget) {
                    self.ready.push_back(ServerMsg::Transcript {
                        session: session.to_string(),
                        kind: TranscriptKind::Append,
                        items: chunk,
                        before: None,
                    });
                }
            }
        }
        let changed = feed.take_subagents_changed();
        open.agents = feed.agents_running();
        if reset {
            open.sent.clear();
        }
        let agents = feed.subagents();
        if (changed || reset) && agents != open.sent {
            open.sent.clone_from(&agents);
            self.ready
                .retain(|m| !matches!(m, ServerMsg::Subagents { session: s, .. } if s == session));
            self.ready.push_back(ServerMsg::Subagents {
                session: session.to_string(),
                agents,
            });
        }
    }
}

fn error(id: String, message: &str) -> ServerMsg {
    ServerMsg::Error {
        id: Some(id),
        message: message.into(),
    }
}

fn clamp(limit: u32) -> usize {
    usize::try_from(limit).map_or(MAX_LIMIT, |l| l.min(MAX_LIMIT))
}

fn lock(feed: &Mutex<Feed>) -> MutexGuard<'_, Feed> {
    feed.lock().unwrap_or_else(|e| e.into_inner())
}

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()))
}

fn page_message(session: &str, feed: &Feed, limit: usize) -> ServerMsg {
    let (items, before) = feed.page(None, limit, budget(session, TranscriptKind::Page));
    ServerMsg::Transcript {
        session: session.to_string(),
        kind: TranscriptKind::Page,
        items,
        before,
    }
}

/// What the items of one message may take up once the rest of the message is counted.
fn budget(session: &str, kind: TranscriptKind) -> usize {
    let envelope = ServerMsg::Transcript {
        session: session.to_string(),
        kind,
        items: Vec::new(),
        before: Some(u64::MAX),
    };
    let overhead = serde_json::to_vec(&envelope).map_or(0, |v| v.len());
    MAX_MESSAGE.saturating_sub(overhead)
}

fn item_size(item: &Item) -> usize {
    serde_json::to_vec(item).map_or(0, |v| v.len() + 1)
}

fn chunks<'a>(items: impl Iterator<Item = &'a Item>, budget: usize) -> Vec<Vec<Item>> {
    let mut out = Vec::new();
    let mut chunk = Vec::new();
    let mut size = 0;
    for item in items {
        let n = item_size(item);
        if !chunk.is_empty() && size + n > budget {
            out.push(std::mem::take(&mut chunk));
            size = 0;
        }
        chunk.push(item.clone());
        size += n;
    }
    if !chunk.is_empty() {
        out.push(chunk);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::protocol::{FileChange, ItemKind};

    fn item(n: u64, text: &str) -> Item {
        Item {
            id: format!("i{n}"),
            pos: n,
            at: 0,
            kind: ItemKind::Assistant,
            text: Some(text.into()),
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

    #[test]
    fn appended_items_are_split_to_fit_the_budget() {
        let items: Vec<Item> = (0..6).map(|n| item(n, &"x".repeat(100))).collect();
        let one = item_size(&items[0]);
        let parts = chunks(items.iter(), one * 2 + 1);
        assert_eq!(
            parts.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![2, 2, 2]
        );
        assert!(parts.iter().flatten().map(|i| i.pos).eq(0..6));
    }

    #[test]
    fn an_item_over_the_budget_still_goes_out_alone() {
        let items = [item(0, &"x".repeat(100)), item(1, "y")];
        let parts = chunks(items.iter(), 10);
        assert_eq!(parts.iter().map(Vec::len).collect::<Vec<_>>(), vec![1, 1]);
    }

    #[test]
    fn a_full_chunk_serializes_within_the_message_cap() {
        let session = "s".repeat(128);
        let items: Vec<Item> = (0..400)
            .map(|n| Item {
                kind: ItemKind::Tool,
                text: None,
                tool: Some("Bash".into()),
                tool_id: Some(format!("toolu_{n}")),
                summary: Some("x".repeat(200)),
                output: Some("x".repeat(1_024)),
                lines: Some(u32::MAX),
                files: (0..5)
                    .map(|_| FileChange {
                        path: "x".repeat(200),
                        added: u32::MAX,
                        removed: u32::MAX,
                        created: true,
                    })
                    .collect(),
                more_files: Some(u32::MAX),
                ..item(n, "")
            })
            .collect();
        let first = chunks(items.iter(), budget(&session, TranscriptKind::Append))
            .into_iter()
            .next()
            .unwrap();
        assert!(first.len() < items.len());
        let message = ServerMsg::Transcript {
            session,
            kind: TranscriptKind::Append,
            items: first,
            before: Some(u64::MAX),
        };
        let size = serde_json::to_vec(&message).unwrap().len();
        assert!(size <= MAX_MESSAGE, "{size} bytes");
        assert!(size > MAX_MESSAGE - 4_000, "{size} bytes");
    }

    fn queued(session: &str) -> ServerMsg {
        ServerMsg::Subagents {
            session: session.into(),
            agents: Vec::new(),
        }
    }

    #[tokio::test]
    async fn forgetting_a_session_drops_its_queued_subagents() {
        let mut feeds = Transcripts::new(None);
        feeds.ready.push_back(queued("s1"));
        feeds.ready.push_back(queued("s2"));
        feeds.forget("s1");
        assert!(matches!(
            feeds.ready.make_contiguous(),
            [ServerMsg::Subagents { session, .. }] if session == "s2"
        ));
    }

    #[tokio::test]
    async fn an_idle_transcript_is_read_again_only_while_its_agents_run() {
        let path = std::env::temp_dir().join(format!("daedalus-feed-{}.jsonl", std::process::id()));
        std::fs::write(&path, "").unwrap();
        for (agents, reads) in [(true, true), (false, false)] {
            let mut feeds = Transcripts::new(None);
            feeds.open.insert(
                "s1".into(),
                Open {
                    epoch: 1,
                    path: path.clone(),
                    feed: Arc::new(Mutex::new(Feed::new(path.clone()))),
                    stamp: stamp(&path),
                    limit: 10,
                    agents,
                    sent: Vec::new(),
                },
            );
            feeds.start_reading();
            assert_eq!(feeds.reading.is_some(), reads);
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn a_change_beyond_the_capped_list_is_not_sent() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("daedalus-feed-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let agents = dir.join("s1/subagents");
        std::fs::create_dir_all(&agents).unwrap();
        let path = dir.join("s1.jsonl");
        let mut parent = String::new();
        for n in 0..21 {
            parent += &serde_json::json!({
                "type": "assistant", "timestamp": format!("2026-01-01T00:00:{n:02}Z"),
                "message": {"content": [{"type": "tool_use", "id": format!("t{n}"), "name": "Agent",
                    "input": {"description": "d", "subagent_type": "builder"}}]},
            })
            .to_string();
            parent.push('\n');
            for (id, agent) in [(0, "a0"), (20, "a20")] {
                if n == id {
                    parent += &serde_json::json!({
                        "type": "user", "timestamp": format!("2026-01-01T00:00:{n:02}Z"),
                        "message": {"content": [{"type": "tool_result", "tool_use_id": format!("t{n}"), "content": "ok"}]},
                        "toolUseResult": {"status": "async_launched", "agentId": agent},
                    })
                    .to_string();
                    parent.push('\n');
                }
            }
        }
        std::fs::write(&path, parent).unwrap();
        let tool_use = format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant", "timestamp": "2026-01-01T01:00:00Z",
                "message": {"content": [{"type": "tool_use", "id": "x", "name": "Read", "input": {}}]},
            })
        );
        let mut feed = Feed::new(path.clone());
        feed.read_on();
        feed.take_subagents_changed();
        let mut feeds = Transcripts::new(None);
        let sent = feed.subagents();
        assert_eq!(sent.len(), 20);
        feeds.open.insert(
            "s1".into(),
            Open {
                epoch: 1,
                path: path.clone(),
                feed: Arc::new(Mutex::new(feed)),
                stamp: None,
                limit: 10,
                agents: true,
                sent,
            },
        );
        let grow = |agent: &str| {
            let file = agents.join(format!("agent-{agent}.jsonl"));
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(file)
                .unwrap();
            file.write_all(tool_use.as_bytes()).unwrap();
        };
        let read = |feeds: &Transcripts| lock(&feeds.open["s1"].feed).read_on();

        grow("a0");
        let read_on = read(&feeds);
        feeds.deliver("s1", 1, read_on);
        assert!(feeds.ready.is_empty(), "the 21st agent is not in the list");

        grow("a20");
        let read_on = read(&feeds);
        feeds.deliver("s1", 1, read_on);
        assert!(matches!(
            feeds.ready.make_contiguous(),
            [ServerMsg::Subagents { agents, .. }] if agents.len() == 20
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
