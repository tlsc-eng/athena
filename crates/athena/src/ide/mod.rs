//! The Claude Code IDE server: MCP over a loopback WebSocket that Claude Code finds through a
//! lock file. It runs on its own thread; the window gets [`Event`]s and answers through [`Server`].

mod lock;
mod session;
mod transport;

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener as StdListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use rmcp::model::{CustomNotification, ServerNotification};
use rmcp::{Peer, RoleServer, ServiceExt};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

pub use lock::default_dir as default_lock_dir;

/// How long quitting waits for rejected diffs to reach Claude Code.
pub const QUIT_WAIT: Duration = Duration::from_millis(500);

/// One proposal Claude Code is waiting on: the connection and the tab name it chose.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DiffKey {
    pub client: u64,
    pub tab: String,
}

impl DiffKey {
    /// A stable string for the tab, unique across connections.
    pub fn id(&self) -> String {
        format!("{}:{}", self.client, self.tab)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The user accepted; Claude Code writes these contents itself.
    Accepted(String),
    Rejected,
    /// Athena cannot show it; Claude Code then asks in the terminal.
    Unavailable,
}

/// What Claude Code asked for, for the window to act on.
#[derive(Debug)]
pub enum Event {
    OpenDiff {
        key: DiffKey,
        path: PathBuf,
        contents: String,
    },
    /// Claude Code closed the tab or went away; the proposal is no longer pending.
    CloseDiff {
        key: DiffKey,
    },
    /// `path` is `None` for every open file. Lines and characters are 0-based.
    Diagnostics {
        path: Option<PathBuf>,
        reply: oneshot::Sender<Vec<FileDiagnostics>>,
    },
    /// A connection, and later the pid of the Claude Code process behind it.
    Client {
        client: u64,
        pid: Option<i32>,
    },
    Disconnected {
        client: u64,
    },
}

#[derive(Debug)]
pub struct FileDiagnostics {
    pub path: PathBuf,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub message: String,
    /// `Error`, `Warning`, `Info` or `Hint`, as Claude Code spells them.
    pub severity: &'static str,
    pub source: Option<String>,
    pub code: Option<serde_json::Value>,
    pub range: Range,
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

pub struct Config {
    /// Where the lock file goes; Claude Code scans `~/.claude/ide`.
    pub lock_dir: PathBuf,
    /// Where athena-mux reads the port for new shells; it also names the port to listen on again.
    pub env_file: Option<PathBuf>,
    pub folders: Vec<PathBuf>,
}

struct Client {
    peer: Peer<RoleServer>,
    pid: Option<i32>,
}

#[derive(Default)]
struct State {
    pending: HashMap<DiffKey, oneshot::Sender<Verdict>>,
    /// openDiff calls that have not returned yet.
    answering: usize,
    clients: HashMap<u64, Client>,
    next_client: u64,
    stopping: bool,
}

struct Shared {
    token: String,
    events: async_channel::Sender<Event>,
    state: Mutex<State>,
    settled: Condvar,
    lock_path: PathBuf,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn send(&self, event: Event) {
        let _ = self.events.try_send(event);
    }

    /// Rejects the pending proposals `matches` picks and tells the window to close their tabs.
    fn drop_diffs(&self, matches: impl Fn(&DiffKey) -> bool) -> usize {
        let dropped: Vec<(DiffKey, oneshot::Sender<Verdict>)> = {
            let mut state = self.lock();
            let keys: Vec<DiffKey> = state
                .pending
                .keys()
                .filter(|k| matches(k))
                .cloned()
                .collect();
            keys.into_iter()
                .filter_map(|k| state.pending.remove_entry(&k))
                .collect()
        };
        let count = dropped.len();
        for (key, tx) in dropped {
            let _ = tx.send(Verdict::Rejected);
            self.send(Event::CloseDiff { key });
        }
        count
    }

    /// Rejects every pending proposal and waits, at most `wait`, for the answers to go out.
    fn settle(&self, wait: Duration, state: MutexGuard<'_, State>) {
        let mut state = state;
        state.stopping = true;
        let had = !state.pending.is_empty();
        for (_, tx) in state.pending.drain() {
            let _ = tx.send(Verdict::Rejected);
        }
        let deadline = Instant::now() + wait;
        while state.answering > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = match self.settled.wait_timeout(state, left) {
                Ok((s, _)) => s,
                Err(e) => e.into_inner().0,
            };
        }
        drop(state);
        if had {
            // The reply is written just after the call returns, on the server's thread.
            std::thread::sleep(Duration::from_millis(50).min(wait));
        }
    }
}

/// The server most recently started, for the panic hook.
static RUNNING: Mutex<Option<Weak<Shared>>> = Mutex::new(None);

/// Called from the panic hook: rejects pending proposals and removes the lock file.
pub fn on_panic() {
    let Ok(running) = RUNNING.try_lock() else {
        return;
    };
    let Some(shared) = running.as_ref().and_then(Weak::upgrade) else {
        return;
    };
    if let Ok(state) = shared.state.try_lock() {
        shared.settle(QUIT_WAIT, state);
    }
    let _ = std::fs::remove_file(&shared.lock_path);
}

/// A running IDE server. Dropping it rejects pending proposals and removes the lock file.
pub struct Server {
    shared: Arc<Shared>,
    lock: lock::Lock,
    env_file: Option<PathBuf>,
    port: u16,
    folders: Vec<PathBuf>,
    runtime: tokio::runtime::Handle,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    pub fn start(config: Config) -> io::Result<(Self, async_channel::Receiver<Event>)> {
        let listener = bind(config.env_file.as_deref().and_then(lock::previous_port))?;
        let port = listener.local_addr()?.port();
        let token = lock::token()?;
        let lock = lock::Lock::create(&config.lock_dir, port, &token, &config.folders)?;
        if let Some(env) = &config.env_file
            && let Err(e) = lock::write_env(env, port)
        {
            lock.remove();
            return Err(e);
        }
        let (events, rx) = async_channel::unbounded();
        let shared = Arc::new(Shared {
            token,
            events,
            state: Mutex::default(),
            settled: Condvar::new(),
            lock_path: lock.path().to_path_buf(),
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()?;
        let handle = runtime.handle().clone();
        let (stop, stopped) = oneshot::channel();
        let accepting = shared.clone();
        let thread = std::thread::Builder::new()
            .name("ide-server".into())
            .spawn(move || {
                runtime.block_on(async move {
                    match TcpListener::from_std(listener) {
                        Ok(listener) => {
                            tokio::select! {
                                () = accept(listener, accepting) => {}
                                _ = stopped => {}
                            }
                        }
                        Err(e) => tracing::error!("ide: could not listen: {e}"),
                    }
                });
            })?;
        *RUNNING.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(&shared));
        tracing::info!(port, "Claude Code IDE server listening");
        let server = Self {
            shared,
            lock,
            env_file: config.env_file,
            port,
            folders: config.folders,
            runtime: handle,
            stop: Some(stop),
            thread: Some(thread),
        };
        Ok((server, rx))
    }

    #[cfg(test)]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Rewrites the lock file when the open projects change.
    pub fn set_folders(&mut self, folders: Vec<PathBuf>) {
        if folders != self.folders {
            if let Err(e) = self.lock.update(&folders) {
                tracing::warn!("ide: could not update {}: {e}", self.lock.path().display());
            }
            self.folders = folders;
        }
    }

    /// Answers a pending proposal; false if Claude Code stopped waiting for it.
    pub fn resolve(&self, key: &DiffKey, verdict: Verdict) -> bool {
        let tx = self.shared.lock().pending.remove(key);
        tx.is_some_and(|tx| tx.send(verdict).is_ok())
    }

    #[cfg(test)]
    pub fn is_pending(&self, key: &DiffKey) -> bool {
        self.shared.lock().pending.contains_key(key)
    }

    #[cfg(test)]
    /// Connected clients and the pid each reported.
    pub fn clients(&self) -> Vec<(u64, Option<i32>)> {
        let state = self.shared.lock();
        let mut list: Vec<_> = state.clients.iter().map(|(id, c)| (*id, c.pid)).collect();
        list.sort_unstable();
        list
    }

    /// Sends a notification such as `selection_changed` to some of the connected clients.
    pub fn notify(&self, clients: &[u64], method: &'static str, params: serde_json::Value) {
        let peers: Vec<Peer<RoleServer>> = {
            let state = self.shared.lock();
            clients
                .iter()
                .filter_map(|id| state.clients.get(id).map(|c| c.peer.clone()))
                .collect()
        };
        if peers.is_empty() {
            return;
        }
        self.runtime.spawn(async move {
            for peer in peers {
                let note = CustomNotification::new(method, Some(params.clone()));
                if let Err(e) = peer
                    .send_notification(ServerNotification::CustomNotification(note))
                    .await
                {
                    tracing::debug!("ide: {method} not sent: {e}");
                }
            }
        });
    }

    /// Stops for good: also withdraws the port from new shells.
    pub fn turn_off(self) {
        if let Some(env) = &self.env_file {
            forget_env(env);
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let state = self.shared.lock();
        self.shared.settle(QUIT_WAIT, state);
        self.lock.remove();
        tracing::info!(port = self.port, "Claude Code IDE server stopped");
        let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        if running
            .as_ref()
            .is_some_and(|w| w.ptr_eq(&Arc::downgrade(&self.shared)))
        {
            *running = None;
        }
        drop(running);
        // Waits for the listener to close, so the next start can bind the same port.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Listens on loopback only, on the port an earlier run used if it is still free.
fn bind(previous: Option<u16>) -> io::Result<StdListener> {
    let at = |port| SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = previous
        .and_then(|port| StdListener::bind(at(port)).ok())
        .map_or_else(|| StdListener::bind(at(0)), Ok)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

async fn accept(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::warn!("ide: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if !peer.ip().is_loopback() {
            continue;
        }
        tokio::spawn(serve(stream, shared.clone()));
    }
}

#[allow(clippy::result_large_err)]
async fn serve(stream: tokio::net::TcpStream, shared: Arc<Shared>) {
    let token = shared.token.clone();
    let ws = match tokio_tungstenite::accept_hdr_async(stream, |req: &_, res| {
        transport::admit(req, res, &token)
    })
    .await
    {
        Ok(ws) => ws,
        Err(e) => {
            tracing::info!("ide: refused a connection: {e}");
            return;
        }
    };
    let client = {
        let mut state = shared.lock();
        state.next_client += 1;
        state.next_client
    };
    let session = session::Session {
        client,
        shared: shared.clone(),
    };
    let ending = shared.clone();
    let on_end = move || {
        ending.drop_diffs(|k| k.client == client);
    };
    let running = match session.serve(transport::json_rpc(ws, on_end)).await {
        Ok(running) => running,
        Err(e) => {
            tracing::info!("ide: MCP handshake failed: {e}");
            return;
        }
    };
    shared.lock().clients.insert(
        client,
        Client {
            peer: running.peer().clone(),
            pid: None,
        },
    );
    shared.send(Event::Client { client, pid: None });
    let _ = running.waiting().await;
    shared.lock().clients.remove(&client);
    shared.drop_diffs(|k| k.client == client);
    shared.send(Event::Disconnected { client });
}

/// The env file athena-mux reads, or `None` when the data directory cannot be made.
pub fn env_file() -> Option<PathBuf> {
    athena_proto::ide_env_path().ok()
}

/// Withdraws the port from new shells while the integration is off.
pub fn forget_env(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests;
