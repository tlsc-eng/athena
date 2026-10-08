use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use athena_proto::{
    ClientMsg, ErrorKind, MAX_OUTPUT_CHUNK, Notice, NoticeKind, PROTO_VERSION, PaneId, PaneInfo,
    ServerMsg, read_frame, write_frame,
};

use crate::notices::{PaneWatcher, clean};
use crate::pane::{Pane, PaneOutput, READ_CHUNK, SCROLLBACK_BYTES};
use crate::process;

const CLIENT_QUEUE: usize = 256;
/// How long a full client queue may stay full before the daemon hangs up on that client.
const LAG_GRACE: Duration = Duration::from_secs(2);
const LAG_POLL: Duration = Duration::from_millis(5);
const IDLE_EXIT: Duration = Duration::from_secs(60);
const REAPER_TICK: Duration = Duration::from_secs(5);
const FOREGROUND_TICK: Duration = Duration::from_millis(500);
const MAX_DIM: u16 = 1000;
const BACKLOG: usize = 50;

// A replay is queued whole under the lock, so it must fit an empty queue with room to spare.
const _: () = assert!(SCROLLBACK_BYTES / MAX_OUTPUT_CHUNK + 8 <= CLIENT_QUEUE);

type ClientId = u64;

struct Client {
    tx: SyncSender<ServerMsg>,
    /// A handle on the connection for hanging up on a client that stopped reading.
    stream: UnixStream,
}

type Targets = Vec<(ClientId, SyncSender<ServerMsg>)>;

pub struct Server {
    socket: PathBuf,
    zdotdir: Option<PathBuf>,
    notify_after: Duration,
    state: Mutex<State>,
}

struct State {
    panes: HashMap<PaneId, Pane>,
    clients: HashMap<ClientId, Client>,
    subscribers: Vec<ClientId>,
    /// Notices raised while no window was listening, delivered on the next `Subscribe`.
    backlog: VecDeque<Notice>,
    next_pane: PaneId,
    next_client: ClientId,
    idle_since: Option<Instant>,
}

impl Server {
    pub fn new(socket: PathBuf, zdotdir: Option<PathBuf>, notify_after: Duration) -> Self {
        // Ids start from the clock so a pane id saved by the GUI never matches a later daemon's pane.
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            socket,
            zdotdir,
            notify_after,
            state: Mutex::new(State {
                panes: HashMap::new(),
                clients: HashMap::new(),
                subscribers: Vec::new(),
                backlog: VecDeque::new(),
                next_pane: epoch * 1_000_000,
                next_client: 0,
                idle_since: None,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn start_idle_reaper(self: Arc<Self>) {
        thread::Builder::new()
            .name("mux-reaper".into())
            .spawn(move || {
                loop {
                    thread::sleep(REAPER_TICK);
                    let mut st = self.lock();
                    if !st.panes.is_empty() || !st.clients.is_empty() {
                        st.idle_since = None;
                        continue;
                    }
                    let since = *st.idle_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= IDLE_EXIT {
                        tracing::info!("idle, exiting");
                        self.exit(st);
                    }
                }
            })
            .expect("spawn reaper thread");
    }

    /// Tells attached clients when a pane's foreground program changes (shell, vim, claude, ...).
    pub fn start_foreground_poller(self: Arc<Self>) {
        thread::Builder::new()
            .name("mux-foreground".into())
            .spawn(move || {
                loop {
                    thread::sleep(FOREGROUND_TICK);
                    // Only the cheap ioctl runs under the lock; describing a process is three
                    // syscalls per pane, and nobody needs it for a pane no client watches.
                    let leaders: Vec<_> = self
                        .lock()
                        .panes
                        .iter()
                        .filter(|(_, p)| p.exit.is_none() && !p.attached.is_empty())
                        .map(|(id, p)| (*id, p.leader()))
                        .collect();
                    let described: Vec<_> = leaders
                        .into_iter()
                        .map(|(id, leader)| (id, leader.and_then(process::describe)))
                        .collect();
                    let updates: Vec<_> = {
                        let mut st = self.lock();
                        let State { panes, clients, .. } = &mut *st;
                        described
                            .into_iter()
                            .filter_map(|(id, now)| {
                                let p = panes.get_mut(&id)?;
                                if now == p.foreground {
                                    return None;
                                }
                                p.foreground = now.clone();
                                Some((
                                    targets(&p.attached, clients),
                                    ServerMsg::Foreground {
                                        pane: id,
                                        process: now,
                                    },
                                ))
                            })
                            .collect()
                    };
                    for (targets, msg) in updates {
                        self.deliver(targets, &msg);
                    }
                }
            })
            .expect("spawn foreground thread");
    }

    fn exit(&self, mut st: MutexGuard<'_, State>) -> ! {
        st.panes.clear();
        let _ = std::fs::remove_file(&self.socket);
        std::process::exit(0);
    }

    pub fn serve(self: Arc<Self>, mut stream: UnixStream) {
        if !same_user(&stream) {
            tracing::warn!("rejected connection from another user");
            return;
        }
        match read_frame::<_, ClientMsg>(&mut stream) {
            Ok(Some(ClientMsg::Hello { proto })) => {
                let panes = self.lock().panes.len() as u32;
                let hello = ServerMsg::Hello {
                    proto: PROTO_VERSION,
                    pid: std::process::id(),
                    panes,
                };
                if write_frame(&mut stream, &hello).is_err() || proto != PROTO_VERSION {
                    return;
                }
            }
            _ => return,
        }

        let (Ok(mut writer), Ok(handle)) = (stream.try_clone(), stream.try_clone()) else {
            return;
        };
        let (tx, rx) = sync_channel::<ServerMsg>(CLIENT_QUEUE);
        let id = {
            let mut st = self.lock();
            st.next_client += 1;
            let id = st.next_client;
            st.clients.insert(
                id,
                Client {
                    tx: tx.clone(),
                    stream: handle,
                },
            );
            id
        };
        let writer_thread = thread::Builder::new()
            .name("mux-client-write".into())
            .spawn(move || {
                for msg in rx {
                    if write_frame(&mut writer, &msg).is_err() {
                        break;
                    }
                }
                let _ = writer.shutdown(std::net::Shutdown::Both);
            });
        if writer_thread.is_err() {
            self.forget(id);
            return;
        }

        while let Ok(Some(msg)) = read_frame::<_, ClientMsg>(&mut stream) {
            if !self.handle(id, &tx, msg) {
                break;
            }
        }

        self.forget(id);
    }

    /// Detaches a client from everything; returns it so the caller may also hang up.
    fn forget(&self, id: ClientId) -> Option<Client> {
        let mut st = self.lock();
        st.subscribers.retain(|c| *c != id);
        for pane in st.panes.values_mut() {
            pane.attached.retain(|c| *c != id);
        }
        st.clients.remove(&id)
    }

    /// Disconnects a client that stopped reading, so it cannot hold up output for anyone else.
    /// It reattaches and replays the scrollback, as after any lost connection.
    fn hang_up(&self, id: ClientId) {
        if let Some(client) = self.forget(id) {
            tracing::warn!("client {id} stopped reading; disconnecting it");
            let _ = client.stream.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Queues `msg` for each target, waiting up to `LAG_GRACE` for a full queue to drain.
    fn deliver(&self, targets: Targets, msg: &ServerMsg) {
        for (id, tx) in targets {
            let deadline = Instant::now() + LAG_GRACE;
            let mut msg = msg.clone();
            loop {
                match tx.try_send(msg) {
                    Ok(()) | Err(TrySendError::Disconnected(_)) => break,
                    Err(TrySendError::Full(back)) if Instant::now() < deadline => {
                        msg = back;
                        thread::sleep(LAG_POLL);
                    }
                    Err(TrySendError::Full(_)) => {
                        self.hang_up(id);
                        break;
                    }
                }
            }
        }
    }

    /// Returns false to drop the connection.
    fn handle(
        self: &Arc<Self>,
        client: ClientId,
        tx: &SyncSender<ServerMsg>,
        msg: ClientMsg,
    ) -> bool {
        match msg {
            ClientMsg::Hello { .. } => {
                let _ = tx.send(ServerMsg::Error {
                    kind: ErrorKind::Protocol("duplicate hello".into()),
                });
                return false;
            }
            ClientMsg::ListPanes => {
                let st = self.lock();
                let panes = st
                    .panes
                    .iter()
                    .map(|(id, p)| PaneInfo {
                        id: *id,
                        cwd: p.cwd.clone(),
                        rows: p.rows,
                        cols: p.cols,
                        alive: p.exit.is_none(),
                    })
                    .collect();
                let _ = tx.send(ServerMsg::Panes { panes });
            }
            ClientMsg::Spawn { cwd, rows, cols } => {
                let reply = match self.spawn(&cwd, clamp(rows), clamp(cols)) {
                    Ok(pane) => ServerMsg::Spawned { pane },
                    Err(err) => ServerMsg::Error {
                        kind: ErrorKind::SpawnFailed(format!("{err:#}")),
                    },
                };
                let _ = tx.send(reply);
            }
            ClientMsg::Attach { pane } => {
                let mut st = self.lock();
                let Some(p) = st.panes.get_mut(&pane) else {
                    let _ = tx.send(ServerMsg::Error {
                        kind: ErrorKind::NoSuchPane(pane),
                    });
                    return true;
                };
                if !p.attached.contains(&client) {
                    p.attached.push(client);
                }
                // Queued under the lock so no live output slips in ahead of the history, but
                // without blocking: a client with no room for it is not reading.
                let head = ServerMsg::Attached {
                    pane,
                    rows: p.rows,
                    cols: p.cols,
                };
                let history = p
                    .ring
                    .chunks(MAX_OUTPUT_CHUNK)
                    .map(|chunk| ServerMsg::Output {
                        pane,
                        data: chunk.to_vec(),
                    });
                let tail = [
                    Some(ServerMsg::ReplayDone { pane }),
                    Some(ServerMsg::Foreground {
                        pane,
                        process: p.foreground.clone(),
                    }),
                    p.exit.map(|code| ServerMsg::Exited { pane, code }),
                ];
                let mut replay = std::iter::once(head)
                    .chain(history)
                    .chain(tail.into_iter().flatten());
                let full = replay.any(|msg| tx.try_send(msg).is_err());
                drop(replay);
                drop(st);
                if full {
                    self.hang_up(client);
                }
            }
            ClientMsg::Input { pane, data } => {
                if let Some(p) = self.lock().panes.get(&pane) {
                    p.write(data);
                }
            }
            ClientMsg::Resize { pane, rows, cols } => {
                if let Some(p) = self.lock().panes.get_mut(&pane) {
                    p.resize(clamp(rows), clamp(cols));
                }
            }
            ClientMsg::Kill { pane } => {
                self.lock().panes.remove(&pane);
            }
            ClientMsg::Subscribe => {
                let mut st = self.lock();
                if !st.subscribers.contains(&client) {
                    st.subscribers.push(client);
                }
                let backlog: Vec<_> = st.backlog.drain(..).collect();
                let full = backlog
                    .into_iter()
                    .any(|notice| tx.try_send(ServerMsg::Notice(notice)).is_err());
                drop(st);
                if full {
                    self.hang_up(client);
                }
            }
            ClientMsg::Notify { pane, kind } => self.notify(pane, sanitize(kind)),
            ClientMsg::Shutdown => {
                tracing::info!("shutdown requested");
                self.exit(self.lock());
            }
        }
        true
    }

    fn spawn(
        self: &Arc<Self>,
        cwd: &std::path::Path,
        rows: u16,
        cols: u16,
    ) -> anyhow::Result<PaneId> {
        let id = {
            let mut st = self.lock();
            st.next_pane += 1;
            st.next_pane
        };
        let (pane, output) = Pane::spawn(id, cwd, rows, cols, self.zdotdir.as_deref())?;
        self.lock().panes.insert(id, pane);
        let server = self.clone();
        thread::Builder::new()
            .name("pane-read".into())
            .spawn(move || server.pump(id, output))?;
        Ok(id)
    }

    /// Copies a pane's output into its history and to every attached client until the shell exits.
    fn pump(self: Arc<Self>, id: PaneId, mut output: PaneOutput) {
        let mut buf = vec![0u8; READ_CHUNK];
        let mut watcher = PaneWatcher::new(self.notify_after);
        loop {
            let n = match output.reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            for kind in watcher.feed(&buf[..n]) {
                self.notify(Some(id), kind);
            }
            let Some(targets) = self.record(id, |p| p.ring.push(&buf[..n])) else {
                let _ = output.child.wait();
                return;
            };
            let msg = ServerMsg::Output {
                pane: id,
                data: buf[..n].to_vec(),
            };
            self.deliver(targets, &msg);
        }
        let code = output.child.wait().ok().map(|s| s.exit_code() as i32);
        let Some(targets) = self.record(id, |p| p.exit = Some(code)) else {
            return;
        };
        self.deliver(targets, &ServerMsg::Exited { pane: id, code });
    }

    /// Sends a notice to every subscribed window, or keeps it until one subscribes.
    fn notify(&self, pane: Option<PaneId>, kind: NoticeKind) {
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let notice = Notice { pane, kind, at };
        let targets: Targets = {
            let mut st = self.lock();
            if st.subscribers.is_empty() {
                st.backlog.push_back(notice);
                if st.backlog.len() > BACKLOG {
                    st.backlog.pop_front();
                }
                return;
            }
            targets(&st.subscribers, &st.clients)
        };
        self.deliver(targets, &ServerMsg::Notice(notice));
    }

    /// Applies `f` to the pane and returns its attached clients' queues; `None` once the pane is gone.
    fn record(&self, id: PaneId, f: impl FnOnce(&mut Pane)) -> Option<Targets> {
        let mut st = self.lock();
        let State { panes, clients, .. } = &mut *st;
        let pane = panes.get_mut(&id)?;
        f(pane);
        Some(targets(&pane.attached, clients))
    }
}

fn targets(ids: &[ClientId], clients: &HashMap<ClientId, Client>) -> Targets {
    ids.iter()
        .filter_map(|id| clients.get(id).map(|c| (*id, c.tx.clone())))
        .collect()
}

fn clamp(n: u16) -> u16 {
    n.clamp(1, MAX_DIM)
}

/// Only the user who started the daemon may talk to it, even if socket permissions are loosened.
fn same_user(stream: &UnixStream) -> bool {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: getpeereid writes two integers for a connected unix socket we own.
    let ok = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0;
    // SAFETY: geteuid has no preconditions.
    ok && uid == unsafe { libc::geteuid() }
}

/// Notices from `athena notify` come from scripts; keep their text printable and short too.
fn sanitize(kind: NoticeKind) -> NoticeKind {
    match kind {
        NoticeKind::ClaudeNeedsInput { message } => NoticeKind::ClaudeNeedsInput {
            message: clean(&message),
        },
        NoticeKind::Message { title, body } => NoticeKind::Message {
            title: clean(&title),
            body: clean(&body),
        },
        NoticeKind::CommandFinished {
            exit_code,
            elapsed_ms,
            command,
        } => NoticeKind::CommandFinished {
            exit_code,
            elapsed_ms,
            command: command.map(|c: String| clean(&c)),
        },
        other => other,
    }
}
