//! The tailnet WebSocket server that phones pair with and connect to.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener as StdListener};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, oneshot, watch};
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::{WebSocketStream, accept_hdr_async_with_config};

use super::devices::{Device, Devices, PairingCode};
use super::protocol::{
    self, ClientMsg, DropReason, Kind, NoticeKind, PROTO, PairReply, PairRequest, Project,
    ServerMsg, Session, Terminal,
};
use super::stream::{Attachments, Out};
use super::tailnet;
use crate::ide::lock::{token, write_private};

const MAX_HANDSHAKES: usize = 16;
const WHOIS_TTL: Duration = Duration::from_secs(10 * 60);
const ID_FILE: &str = "server-id";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PER_DEVICE: usize = 4;
const MAX_INPUT: usize = 64 * 1024;
const MAX_MESSAGE: usize = 1 << 20;
const PING_EVERY: Duration = Duration::from_secs(20);
const PONG_TIMEOUT: Duration = Duration::from_secs(45);
const SNAPSHOT_GAP: Duration = Duration::from_millis(250);
const PAIR_WAIT: Duration = Duration::from_secs(60);
const SPAWN_WAIT: Duration = Duration::from_secs(10);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const PAIR_HEADER: &str = "x-daedalus-pair";
const VERSION_CLOSE: u16 = 4001;
const MAX_DEVICE_NAME: usize = 64;

pub struct Config {
    pub addrs: Vec<SocketAddr>,
    pub devices: Arc<Mutex<Devices>>,
    /// Holds the server id, which must survive restarts because phones key their pairing by it.
    pub data_dir: PathBuf,
    pub cli: Option<PathBuf>,
    /// Lets loopback peers in; nothing else is exempt from the tailnet check.
    pub allow_loopback_for_tests: bool,
    /// The Mac's name, shown on the phone.
    pub name: String,
    /// The session daemon's Unix socket, where attached panes are streamed from.
    pub mux_socket: PathBuf,
}

/// The answer to a pairing prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny,
}

/// Who is asking to pair; `tailnet` is empty when the Tailscale CLI is not available.
#[derive(Clone, Debug)]
pub struct PairPeer {
    pub ip: IpAddr,
    pub tailnet: Option<tailnet::Peer>,
}

/// A request the server hands to the app, which answers through `reply`.
pub enum Command {
    Pair {
        device: String,
        peer: PairPeer,
        reply: oneshot::Sender<Verdict>,
    },
    SpawnClaude {
        root: PathBuf,
        reply: oneshot::Sender<Result<u64, String>>,
    },
    /// The number of live connections changed to this total.
    ClientsChanged(usize),
}

/// What phones see; the server numbers each one per connection.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub projects: Vec<Project>,
    pub sessions: Vec<Session>,
    pub terminals: Vec<Terminal>,
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub pane: u64,
    pub kind: NoticeKind,
    pub message: String,
    pub at: u64,
}

struct Shared {
    id: String,
    name: String,
    devices: Arc<Mutex<Devices>>,
    pairing: Mutex<Option<PairingCode>>,
    cli: Option<PathBuf>,
    mux_socket: PathBuf,
    allow_loopback: bool,
    commands: async_channel::Sender<Command>,
    snapshot: watch::Sender<Arc<Snapshot>>,
    notices: broadcast::Sender<Notice>,
    live: Mutex<HashMap<String, usize>>,
    welcomed: Mutex<usize>,
    revoked: broadcast::Sender<String>,
    handshakes: Arc<Semaphore>,
    whois: WhoisCache,
}

/// A running companion server. Dropping it starts the stop without waiting; `shutdown` says when it is done.
pub struct Server {
    shared: Arc<Shared>,
    addrs: Vec<SocketAddr>,
    stop: Option<oneshot::Sender<()>>,
    closed: Option<futures::channel::oneshot::Receiver<()>>,
}

impl Server {
    pub fn start(config: Config) -> io::Result<(Self, async_channel::Receiver<Command>)> {
        let listeners = config
            .addrs
            .iter()
            .map(|addr| {
                let listener = StdListener::bind(addr)?;
                listener.set_nonblocking(true)?;
                Ok(listener)
            })
            .collect::<io::Result<Vec<_>>>()?;
        let addrs = listeners
            .iter()
            .map(StdListener::local_addr)
            .collect::<io::Result<Vec<_>>>()?;
        let (commands, rx) = async_channel::unbounded();
        let shared = Arc::new(Shared {
            id: load_or_create_id(&config.data_dir)?,
            name: config.name,
            devices: config.devices,
            pairing: Mutex::new(None),
            cli: config.cli,
            mux_socket: config.mux_socket,
            allow_loopback: config.allow_loopback_for_tests,
            commands,
            snapshot: watch::channel(Arc::default()).0,
            notices: broadcast::channel(64).0,
            live: Mutex::default(),
            welcomed: Mutex::default(),
            revoked: broadcast::channel(16).0,
            handshakes: Arc::new(Semaphore::new(MAX_HANDSHAKES)),
            whois: WhoisCache::default(),
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()?;
        let (stop, stopped) = oneshot::channel();
        let serving = shared.clone();
        let (done, closed) = futures::channel::oneshot::channel();
        std::thread::Builder::new()
            .name("companion-server".into())
            .spawn(move || {
                runtime.block_on(async move {
                    for listener in listeners {
                        match TcpListener::from_std(listener) {
                            Ok(listener) => {
                                tokio::spawn(accept(listener, serving.clone()));
                            }
                            Err(e) => tracing::error!("companion: could not listen: {e}"),
                        }
                    }
                    let _ = stopped.await;
                });
                // Dropping the runtime would wait for an in-flight whois subprocess.
                runtime.shutdown_background();
                let _ = done.send(());
            })?;
        tracing::info!(?addrs, "companion server listening");
        let server = Self {
            shared,
            addrs,
            stop: Some(stop),
            closed: Some(closed),
        };
        Ok((server, rx))
    }

    /// The addresses actually bound, which differ from the config when it asked for port 0.
    pub fn local_addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// Stops serving; the receiver resolves once every listener is closed and the port is free.
    pub fn shutdown(mut self) -> futures::channel::oneshot::Receiver<()> {
        drop(self.stop.take());
        self.closed.take().expect("closed is taken only here")
    }

    /// The id phones see in `welcome` and `paired`.
    pub fn id(&self) -> &str {
        &self.shared.id
    }

    /// Makes `code` the one a phone may pair with, replacing any earlier one.
    pub fn set_pairing_code(&self, code: Option<PairingCode>) {
        *lock(&self.shared.pairing) = code;
    }

    /// Shows connected phones a new snapshot, unless it equals the current one.
    pub fn set_snapshot(&self, snapshot: Arc<Snapshot>) {
        self.shared.snapshot.send_if_modified(|current| {
            if **current == *snapshot {
                return false;
            }
            *current = snapshot;
            true
        });
    }

    /// Removes a paired device and closes its live connections at once.
    pub fn revoke(&self, id: &str) -> io::Result<bool> {
        let removed = lock(&self.shared.devices).revoke(id);
        // The device is gone from memory even when saving failed, so its sockets close either way.
        let _ = self.shared.revoked.send(id.to_string());
        removed
    }

    /// Tells every connected phone about a session that needs attention.
    pub fn push(&self, notice: Notice) {
        let _ = self.shared.notices.send(notice);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        drop(self.stop.take());
    }
}

type Ws = WebSocketStream<TcpStream>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// `is_tailnet` rejects IPv4-mapped addresses, so a dual-stack listener needs them unmapped first.
fn peer_allowed(ip: IpAddr, allow_loopback: bool) -> bool {
    let ip = ip.to_canonical();
    tailnet::is_tailnet(ip) || (allow_loopback && ip.is_loopback())
}

async fn accept(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::warn!("companion: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let ip = peer.ip().to_canonical();
        if !peer_allowed(ip, shared.allow_loopback) {
            tracing::info!(%ip, "companion: refused a peer outside the tailnet");
            continue;
        }
        let Ok(permit) = shared.handshakes.clone().try_acquire_owned() else {
            tracing::debug!("companion: too many connections waiting to authenticate");
            continue;
        };
        tokio::spawn(serve(stream, ip, shared.clone(), permit));
    }
}

async fn serve(stream: TcpStream, ip: IpAddr, shared: Arc<Shared>, permit: OwnedSemaphorePermit) {
    let opened = timeout(HANDSHAKE_TIMEOUT, open(stream, ip, &shared))
        .await
        .ok()
        .flatten();
    // The pairing prompt can wait a minute, which must not hold one of the handshake slots.
    drop(permit);
    match opened {
        Some(Opened::Connect(ws, slot)) => connect(ws, slot, &shared).await,
        Some(Opened::Pair(ws, device, whois)) => pair(ws, device, ip, whois, &shared).await,
        None => {}
    }
}

async fn lookup_peer(shared: &Shared, cli: &Path, ip: IpAddr) -> Option<tailnet::Peer> {
    if let Some(peer) = shared.whois.get(ip, std::time::Instant::now()) {
        return Some(peer);
    }
    let cli = cli.to_path_buf();
    // whois runs a subprocess for up to 3 s, which would stall an async worker.
    let peer = tokio::task::spawn_blocking(move || tailnet::whois(&cli, ip))
        .await
        .ok()
        .flatten()?;
    shared
        .whois
        .put(ip, peer.clone(), std::time::Instant::now());
    Some(peer)
}

/// Successful lookups only, so a failed CLI call does not lock a phone out for the whole TTL.
#[derive(Default)]
struct WhoisCache(Mutex<HashMap<IpAddr, (tailnet::Peer, std::time::Instant)>>);

impl WhoisCache {
    fn get(&self, ip: IpAddr, now: std::time::Instant) -> Option<tailnet::Peer> {
        let entries = lock(&self.0);
        let (peer, at) = entries.get(&ip)?;
        (now.saturating_duration_since(*at) < WHOIS_TTL).then(|| peer.clone())
    }

    fn put(&self, ip: IpAddr, peer: tailnet::Peer, now: std::time::Instant) {
        let mut entries = lock(&self.0);
        entries.retain(|_, (_, at)| now.saturating_duration_since(*at) < WHOIS_TTL);
        entries.insert(ip, (peer, now));
    }
}

/// Phones key their Keychain entry by this id, so a malformed stored one is replaced rather than trusted.
fn load_or_create_id(dir: &Path) -> io::Result<String> {
    let path = dir.join(ID_FILE);
    if let Ok(text) = fs::read_to_string(&path) {
        let stored = text.trim();
        if is_server_id(stored) {
            return Ok(stored.to_string());
        }
    }
    let id = token()?[..16].to_string();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    write_private(&path, id.as_bytes())?;
    Ok(id)
}

fn is_server_id(text: &str) -> bool {
    text.len() == 16 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

enum Opened {
    Connect(Ws, Slot),
    Pair(Ws, String, Option<tailnet::Peer>),
}

enum Route {
    Pair,
    Connect(Slot),
}

/// Runs from accept through `welcome` (or the `pair` message), under the handshake timeout.
// The error type is tungstenite's handshake callback contract.
#[allow(clippy::result_large_err)]
async fn open(stream: TcpStream, ip: IpAddr, shared: &Arc<Shared>) -> Option<Opened> {
    let whois = match &shared.cli {
        Some(cli) if !ip.is_loopback() => {
            let found = lookup_peer(shared, cli, ip).await;
            if found.is_none() {
                tracing::info!(%ip, "companion: refused a peer Tailscale cannot attribute to a user");
            }
            Some(found?)
        }
        _ => None,
    };
    let mut route = None;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let mut ws = accept_hdr_async_with_config(
        stream,
        |request: &Request, response: Response| {
            let (response, admitted) = admit(request, response, shared, whois.as_ref())?;
            route = Some(admitted);
            Ok(response)
        },
        Some(config),
    )
    .await
    .inspect_err(|e| tracing::info!(%ip, "companion: refused a connection: {e}"))
    .ok()?;
    match route? {
        Route::Pair => {
            let PairRequest::Pair { device, proto } = read_json(&mut ws).await?;
            if proto != PROTO {
                close(
                    &mut ws,
                    CloseCode::Library(VERSION_CLOSE),
                    "version mismatch",
                )
                .await;
                return None;
            }
            Some(Opened::Pair(ws, device, whois))
        }
        Route::Connect(mut slot) => {
            let ClientMsg::Hello { proto, .. } = read_json(&mut ws).await? else {
                return None;
            };
            if proto != PROTO {
                close(
                    &mut ws,
                    CloseCode::Library(VERSION_CLOSE),
                    "version mismatch",
                )
                .await;
                return None;
            }
            if !is_paired(shared, &slot.device) {
                close(&mut ws, CloseCode::Policy, "device revoked").await;
                return None;
            }
            let welcome = ServerMsg::Welcome {
                proto: PROTO,
                server: shared.id.clone(),
                name: shared.name.clone(),
                app: env!("CARGO_PKG_VERSION").into(),
            };
            if !send(&mut ws, &welcome).await {
                return None;
            }
            slot.welcome();
            Some(Opened::Connect(ws, slot))
        }
    }
}

/// Decides from the upgrade request alone, so a refusal is an HTTP status before any WebSocket exists.
// The error type is tungstenite's handshake callback contract.
#[allow(clippy::result_large_err)]
fn admit(
    request: &Request,
    response: Response,
    shared: &Arc<Shared>,
    whois: Option<&tailnet::Peer>,
) -> Result<(Response, Route), ErrorResponse> {
    let headers = request.headers();
    if headers.contains_key("origin") {
        return Err(refuse(StatusCode::FORBIDDEN));
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    match request.uri().path() {
        "/v1/pair" => {
            let given = header(PAIR_HEADER).unwrap_or_default();
            let accepted = lock(&shared.pairing)
                .as_mut()
                .is_some_and(|code| code.take(given, std::time::Instant::now()));
            if !accepted {
                return Err(refuse(StatusCode::UNAUTHORIZED));
            }
            Ok((response, Route::Pair))
        }
        "/v1/connect" => {
            // Subscribed before the token check so a revoke after it cannot be missed.
            let revoked = shared.revoked.subscribe();
            let token = header("authorization")
                .and_then(|v| v.strip_prefix("Bearer "))
                .unwrap_or_default();
            let device = lock(&shared.devices).verify(token).cloned();
            let Some(device) = device else {
                return Err(refuse(StatusCode::UNAUTHORIZED));
            };
            if !same_owner(&device, whois) {
                return Err(refuse(StatusCode::UNAUTHORIZED));
            }
            let Some(slot) = Slot::reserve(shared, &device.id, revoked) else {
                return Err(refuse(StatusCode::TOO_MANY_REQUESTS));
            };
            Ok((response, Route::Connect(slot)))
        }
        _ => Err(refuse(StatusCode::NOT_FOUND)),
    }
}

/// A device paired without the CLI has no login to compare, so it is held to its token alone.
fn same_owner(device: &Device, whois: Option<&tailnet::Peer>) -> bool {
    match whois {
        Some(peer) if !device.login.is_empty() => peer.login == device.login,
        _ => true,
    }
}

fn refuse(status: StatusCode) -> ErrorResponse {
    let mut response = ErrorResponse::new(None);
    *response.status_mut() = status;
    response
}

/// One live connection of a device, released on drop.
struct Slot {
    shared: Arc<Shared>,
    device: String,
    revoked: broadcast::Receiver<String>,
    welcomed: bool,
}

impl Slot {
    fn reserve(
        shared: &Arc<Shared>,
        device: &str,
        revoked: broadcast::Receiver<String>,
    ) -> Option<Self> {
        let mut live = lock(&shared.live);
        let count = live.entry(device.to_string()).or_default();
        if *count >= MAX_PER_DEVICE {
            return None;
        }
        *count += 1;
        Some(Self {
            shared: shared.clone(),
            device: device.to_string(),
            revoked,
            welcomed: false,
        })
    }

    /// Counts the connection for the app, which a handshake that fails before `welcome` must not.
    fn welcome(&mut self) {
        self.welcomed = true;
        let mut total = lock(&self.shared.welcomed);
        *total += 1;
        notify_clients(&self.shared, *total);
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut live = lock(&self.shared.live);
        if let Some(count) = live.get_mut(&self.device) {
            *count -= 1;
            if *count == 0 {
                live.remove(&self.device);
            }
        }
        drop(live);
        if self.welcomed {
            let mut total = lock(&self.shared.welcomed);
            *total -= 1;
            notify_clients(&self.shared, *total);
        }
    }
}

/// Sends under the `welcomed` lock so totals reach the app in the order they were counted.
fn notify_clients(shared: &Shared, total: usize) {
    let _ = shared.commands.try_send(Command::ClientsChanged(total));
}

async fn pair(
    mut ws: Ws,
    device: String,
    ip: IpAddr,
    whois: Option<tailnet::Peer>,
    shared: &Shared,
) {
    let name = clean_name(&device);
    let (reply, answer) = oneshot::channel();
    let command = Command::Pair {
        device: name.clone(),
        peer: PairPeer {
            ip,
            tailnet: whois.clone(),
        },
        reply,
    };
    let verdict = if shared.commands.send(command).await.is_ok() {
        timeout(PAIR_WAIT, answer).await.ok().and_then(Result::ok)
    } else {
        None
    };
    let message = match verdict {
        Some(Verdict::Allow) => {
            let (login, node) = whois.map(|p| (p.login, p.node)).unwrap_or_default();
            let issued = lock(&shared.devices).issue(&name, &login, &node);
            match issued {
                Ok((_, token)) => PairReply::Paired {
                    token,
                    server: shared.id.clone(),
                    name: shared.name.clone(),
                },
                Err(e) => {
                    tracing::warn!("companion: could not store a paired device: {e}");
                    PairReply::Error {
                        message: "could not save the pairing".into(),
                    }
                }
            }
        }
        _ => PairReply::Error {
            message: "pairing was not approved".into(),
        },
    };
    if send(&mut ws, &message).await {
        close(&mut ws, CloseCode::Normal, "").await;
    }
}

/// The name goes into a confirm prompt, so control characters and length are stripped.
fn clean_name(raw: &str) -> String {
    let name: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DEVICE_NAME)
        .collect();
    let name = name.trim();
    if name.is_empty() {
        "Phone".into()
    } else {
        name.into()
    }
}

async fn connect(mut ws: Ws, mut slot: Slot, shared: &Shared) {
    let mut snapshot = shared.snapshot.subscribe();
    let mut notices = shared.notices.subscribe();
    let mut ping = tokio::time::interval_at(Instant::now() + PING_EVERY, PING_EVERY);
    let mut last_pong = Instant::now();
    let mut dirty = true;
    let mut next_snapshot = Instant::now();
    let mut seq = 0u64;
    let mut attachments = Attachments::new(shared.mux_socket.clone());
    loop {
        tokio::select! {
            // Biased so a queued revoke wins over a request already readable on the socket.
            biased;
            revoked = slot.revoked.recv() => {
                let gone = match revoked {
                    Ok(id) => id == slot.device,
                    Err(broadcast::error::RecvError::Lagged(_)) => !is_paired(shared, &slot.device),
                    Err(broadcast::error::RecvError::Closed) => true,
                };
                if gone {
                    close(&mut ws, CloseCode::Policy, "device revoked").await;
                    break;
                }
            }
            incoming = ws.next() => {
                let Some(Ok(message)) = incoming else { break };
                match message {
                    Message::Pong(_) => last_pong = Instant::now(),
                    Message::Text(text) => {
                        if text.len() > MAX_INPUT {
                            close(&mut ws, CloseCode::Size, "frame too large").await;
                            break;
                        }
                        let Ok(request) = serde_json::from_str::<ClientMsg>(text.as_str()) else {
                            close(&mut ws, CloseCode::Invalid, "invalid message").await;
                            break;
                        };
                        let reply = match request {
                            ClientMsg::Hello { .. } => {
                                close(&mut ws, CloseCode::Protocol, "unexpected hello").await;
                                break;
                            }
                            ClientMsg::Refresh => {
                                dirty = true;
                                None
                            }
                            ClientMsg::Detach { pane } => {
                                attachments.detach(pane);
                                None
                            }
                            ClientMsg::Attach { pane, tail } => {
                                // Only panes the phone was shown may be attached, so it cannot probe for others.
                                let open = snapshot.borrow().terminals.iter().any(|t| t.pane == pane);
                                let refused = if open {
                                    attachments.attach(pane, tail).err()
                                } else {
                                    Some("pane is not open")
                                };
                                refused.map(|message| ServerMsg::Error {
                                    id: None,
                                    message: message.into(),
                                })
                            }
                            ClientMsg::SpawnClaude { id, project } => {
                                let current = Arc::clone(&snapshot.borrow());
                                Some(spawn_claude(shared, &current, id, project).await)
                            }
                        };
                        if let Some(reply) = reply && !send(&mut ws, &reply).await {
                            break;
                        }
                    }
                    Message::Binary(bytes) if protocol::parse(&bytes).is_none() => {
                        let code = if bytes.len() > MAX_INPUT {
                            CloseCode::Size
                        } else {
                            CloseCode::Protocol
                        };
                        close(&mut ws, code, "invalid frame").await;
                        break;
                    }
                    Message::Binary(bytes) => {
                        if let Some((Kind::Input, pane, data)) = protocol::parse(&bytes)
                            && !attachments.input(pane, data)
                        {
                            let dropped = ServerMsg::Dropped { pane, reason: DropReason::Lagging };
                            if !send(&mut ws, &dropped).await {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
            changed = snapshot.changed() => {
                if changed.is_err() {
                    break;
                }
                dirty = true;
            }
            () = tokio::time::sleep_until(next_snapshot), if dirty => {
                dirty = false;
                next_snapshot = Instant::now() + SNAPSHOT_GAP;
                seq += 1;
                let current = Arc::clone(&snapshot.borrow_and_update());
                let message = ServerMsg::Snapshot {
                    seq,
                    projects: current.projects.clone(),
                    sessions: current.sessions.clone(),
                    terminals: current.terminals.clone(),
                };
                if !send(&mut ws, &message).await {
                    break;
                }
            }
            notice = notices.recv() => match notice {
                Ok(n) => {
                    let message = ServerMsg::Notice {
                        pane: n.pane,
                        kind: n.kind,
                        message: n.message,
                        at: n.at,
                    };
                    if !send(&mut ws, &message).await {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = ping.tick() => {
                if !is_paired(shared, &slot.device) {
                    close(&mut ws, CloseCode::Policy, "device revoked").await;
                    break;
                }
                if timeout(SEND_TIMEOUT, ws.send(Message::Ping(Vec::new().into()))).await.is_err() {
                    break;
                }
            }
            () = tokio::time::sleep_until(last_pong + PONG_TIMEOUT) => break,
            // Last, so steady output cannot starve the ping and the pong deadline.
            out = attachments.next() => {
                let sent = match out {
                    Out::Msg(message) => send(&mut ws, &message).await,
                    Out::Frame(bytes, _charge) => send_frame(&mut ws, bytes).await,
                };
                if !sent {
                    break;
                }
            }
        }
    }
}

fn is_paired(shared: &Shared, device: &str) -> bool {
    lock(&shared.devices).list().iter().any(|d| d.id == device)
}

async fn spawn_claude(
    shared: &Shared,
    snapshot: &Snapshot,
    id: String,
    project: String,
) -> ServerMsg {
    let failed = |id: String, message: &str| ServerMsg::Error {
        id: Some(id),
        message: message.into(),
    };
    if !snapshot.projects.iter().any(|p| p.root == project) {
        return failed(id, "project is not open in a window");
    }
    let (reply, answer) = oneshot::channel();
    let command = Command::SpawnClaude {
        root: PathBuf::from(project),
        reply,
    };
    if shared.commands.send(command).await.is_err() {
        return failed(id, "could not start claude");
    }
    match timeout(SPAWN_WAIT, answer).await {
        Ok(Ok(Ok(pane))) => ServerMsg::Spawned { id, pane },
        Ok(Ok(Err(message))) => ServerMsg::Error {
            id: Some(id),
            message,
        },
        _ => failed(id, "could not start claude"),
    }
}

/// Reads the next text frame as JSON; anything else, including an oversized frame, gives `None`.
async fn read_json<T: DeserializeOwned>(ws: &mut Ws) -> Option<T> {
    loop {
        match ws.next().await?.ok()? {
            Message::Text(text) if text.len() <= MAX_INPUT => {
                return serde_json::from_str(text.as_str()).ok();
            }
            Message::Ping(_) | Message::Pong(_) => {}
            _ => return None,
        }
    }
}

async fn send<T: Serialize>(ws: &mut Ws, message: &T) -> bool {
    let Ok(text) = serde_json::to_string(message) else {
        return false;
    };
    matches!(
        timeout(SEND_TIMEOUT, ws.send(Message::text(text))).await,
        Ok(Ok(()))
    )
}

async fn send_frame(ws: &mut Ws, bytes: Vec<u8>) -> bool {
    matches!(
        timeout(SEND_TIMEOUT, ws.send(Message::binary(bytes))).await,
        Ok(Ok(()))
    )
}

async fn close(ws: &mut Ws, code: CloseCode, reason: &'static str) {
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    let _ = timeout(SEND_TIMEOUT, ws.close(Some(frame))).await;
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant as StdInstant;

    use daedalus_proto as mux;

    use tokio_tungstenite::client_async;
    use tokio_tungstenite::tungstenite::Error as WsError;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    use super::*;

    struct Fixture {
        dir: PathBuf,
        mux: PathBuf,
        devices: Arc<Mutex<Devices>>,
        server: Server,
        commands: async_channel::Receiver<Command>,
    }

    impl Fixture {
        fn new() -> Self {
            Self::listening_on(1)
        }

        fn listening_on(listeners: usize) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "daedalus-companion-server-{}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            // sun_path is 104 bytes on macOS, which `temp_dir` can exceed.
            let mux = PathBuf::from(format!("/tmp/dmx-{}-{n}.sock", std::process::id()));
            let devices = Arc::new(Mutex::new(Devices::load(&dir)));
            let (server, commands) = Server::start(Config {
                addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0)); listeners],
                devices: devices.clone(),
                data_dir: dir.clone(),
                cli: None,
                allow_loopback_for_tests: true,
                name: "Test Mac".into(),
                mux_socket: mux.clone(),
            })
            .unwrap();
            Self {
                dir,
                mux,
                devices,
                server,
                commands,
            }
        }

        async fn next_command(&self) -> Result<Command, async_channel::RecvError> {
            loop {
                match self.commands.recv().await {
                    Ok(Command::ClientsChanged(_)) => {}
                    other => return other,
                }
            }
        }

        /// True when nothing but client counts is queued.
        fn no_command(&self) -> bool {
            while let Ok(command) = self.commands.try_recv() {
                if !matches!(command, Command::ClientsChanged(_)) {
                    return false;
                }
            }
            true
        }

        fn pair_device(&self) -> (String, String) {
            let (device, token) = lock(&self.devices).issue("Phone", "a@b.c", "node").unwrap();
            (device.id, token)
        }

        async fn dial(
            &self,
            path: &str,
            headers: &[(&'static str, String)],
        ) -> Result<Ws, WsError> {
            let addr = self.server.local_addrs()[0];
            let mut request = format!("ws://{addr}{path}").into_client_request().unwrap();
            for (name, value) in headers {
                request
                    .headers_mut()
                    .insert(*name, HeaderValue::from_str(value).unwrap());
            }
            let stream = TcpStream::connect(addr).await.unwrap();
            client_async(request, stream).await.map(|(ws, _)| ws)
        }

        async fn connect(&self, token: &str) -> Result<Ws, WsError> {
            self.dial(
                "/v1/connect",
                &[("authorization", format!("Bearer {token}"))],
            )
            .await
        }

        /// Connects and completes `hello`, returning the socket after its `welcome`.
        async fn welcomed(&self, token: &str) -> Ws {
            let mut ws = self.connect(token).await.unwrap();
            send_json(&mut ws, &hello(PROTO)).await;
            assert!(matches!(next_msg(&mut ws).await, ServerMsg::Welcome { .. }));
            ws
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
            let _ = std::fs::remove_file(&self.mux);
        }
    }

    fn hello(proto: u32) -> ClientMsg {
        ClientMsg::Hello {
            proto,
            app: "0.1.0".into(),
            device: "Phone".into(),
        }
    }

    async fn send_json<T: Serialize>(ws: &mut Ws, message: &T) {
        ws.send(Message::text(serde_json::to_string(message).unwrap()))
            .await
            .unwrap();
    }

    async fn next_msg(ws: &mut Ws) -> ServerMsg {
        loop {
            let message = timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("timed out")
                .expect("closed")
                .expect("socket error");
            if let Message::Text(text) = message {
                return serde_json::from_str(text.as_str()).unwrap();
            }
        }
    }

    fn status(result: Result<Ws, WsError>) -> StatusCode {
        match result {
            Err(WsError::Http(response)) => response.status(),
            Err(other) => panic!("expected an HTTP refusal, got {other}"),
            Ok(_) => panic!("expected an HTTP refusal, got a connection"),
        }
    }

    /// The code of the close frame the server sends, or `None` if the stream ends without one.
    async fn close_code(ws: &mut Ws) -> Option<CloseCode> {
        loop {
            match timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("timed out")
            {
                Some(Ok(Message::Close(frame))) => return frame.map(|f| f.code),
                Some(Ok(_)) => {}
                _ => return None,
            }
        }
    }

    fn terminal(pane: u64) -> protocol::Terminal {
        protocol::Terminal {
            pane,
            project: "/p".into(),
            title: "zsh".into(),
            program: None,
            cwd: None,
            claude: None,
            rows: 24,
            cols: 80,
        }
    }

    #[tokio::test]
    async fn unknown_token_gets_401() {
        let fx = Fixture::new();
        assert_eq!(status(fx.connect("nope").await), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn revoked_token_gets_401() {
        let fx = Fixture::new();
        let (id, token) = fx.pair_device();
        fx.welcomed(&token).await;
        assert!(lock(&fx.devices).revoke(&id).unwrap());
        assert_eq!(status(fx.connect(&token).await), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn revoking_closes_a_live_socket_at_once() {
        let fx = Fixture::new();
        let (id, token) = fx.pair_device();
        let mut ws = fx.welcomed(&token).await;
        assert!(fx.server.revoke(&id).unwrap());
        let code = timeout(Duration::from_secs(1), close_code(&mut ws))
            .await
            .expect("no close frame within a second of the revoke");
        assert_eq!(code, Some(CloseCode::Policy));
    }

    fn open_project(fx: &Fixture) {
        fx.server.set_snapshot(Arc::new(Snapshot {
            projects: vec![protocol::Project {
                root: "/p".into(),
                name: "p".into(),
                window: 1,
            }],
            ..Snapshot::default()
        }));
    }

    fn spawn_request() -> ClientMsg {
        ClientMsg::SpawnClaude {
            id: "r1".into(),
            project: "/p".into(),
        }
    }

    /// Drains the socket until it ends, failing on a reply to the request; a reset may swallow the close frame.
    async fn assert_unanswered(ws: &mut Ws) {
        loop {
            match timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("timed out")
            {
                Some(Ok(Message::Text(text))) => {
                    let message: ServerMsg = serde_json::from_str(text.as_str()).unwrap();
                    assert!(
                        !matches!(
                            message,
                            ServerMsg::Welcome { .. }
                                | ServerMsg::Spawned { .. }
                                | ServerMsg::Error { .. }
                        ),
                        "a revoked device was answered: {text}"
                    );
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            }
        }
    }

    async fn send_spawn(ws: &mut Ws) {
        let text = serde_json::to_string(&spawn_request()).unwrap();
        let _ = ws.send(Message::text(text)).await;
    }

    #[tokio::test]
    async fn a_device_revoked_after_the_upgrade_gets_no_welcome_and_no_command() {
        let fx = Fixture::new();
        let (id, token) = fx.pair_device();
        open_project(&fx);
        let mut ws = fx.connect(&token).await.unwrap();
        assert!(fx.server.revoke(&id).unwrap());
        send_json(&mut ws, &hello(PROTO)).await;
        send_spawn(&mut ws).await;
        assert_unanswered(&mut ws).await;
        assert!(fx.no_command());
    }

    #[tokio::test]
    async fn a_request_sent_right_after_a_revoke_is_not_served() {
        let fx = Fixture::new();
        let (id, token) = fx.pair_device();
        open_project(&fx);
        let mut ws = fx.welcomed(&token).await;
        assert!(fx.server.revoke(&id).unwrap());
        send_spawn(&mut ws).await;
        assert_unanswered(&mut ws).await;
        assert!(fx.no_command());
    }

    /// A revoke that lands while `spawn_claude` is waiting must beat the next queued request.
    #[tokio::test]
    async fn a_revoke_during_a_spawn_beats_the_next_queued_request() {
        let fx = Fixture::new();
        open_project(&fx);
        for _ in 0..30 {
            let (id, token) = fx.pair_device();
            let mut ws = fx.welcomed(&token).await;
            send_spawn(&mut ws).await;
            let Ok(Command::SpawnClaude { reply, .. }) =
                timeout(Duration::from_secs(5), fx.next_command())
                    .await
                    .expect("the first spawn never reached the app")
            else {
                panic!("expected a spawn command");
            };
            send_spawn(&mut ws).await;
            assert!(fx.server.revoke(&id).unwrap());
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = reply.send(Ok(1));
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(fx.no_command());
        }
    }

    #[tokio::test]
    async fn handshake_slots_are_shared_between_listeners() {
        let fx = Fixture::listening_on(2);
        let addrs = fx.server.local_addrs().to_vec();
        let mut idle = Vec::new();
        for _ in 0..MAX_HANDSHAKES {
            idle.push(TcpStream::connect(addrs[0]).await.unwrap());
        }
        let taken = async {
            while fx.server.shared.handshakes.available_permits() > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        timeout(Duration::from_secs(2), taken)
            .await
            .expect("idle connections never held the handshake slots");
        let (_, token) = fx.pair_device();
        let mut request = format!("ws://{}/v1/connect", addrs[1])
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        let stream = TcpStream::connect(addrs[1]).await.unwrap();
        assert!(client_async(request, stream).await.is_err());
    }

    #[test]
    fn the_whois_cache_expires_and_holds_only_what_it_was_given() {
        let peer = || tailnet::Peer {
            login: "a@b.c".into(),
            node: "node".into(),
            os: "iOS".into(),
        };
        let ip = IpAddr::from([100, 64, 0, 1]);
        let cache = WhoisCache::default();
        let start = StdInstant::now();
        assert!(cache.get(ip, start).is_none());
        cache.put(ip, peer(), start);
        assert_eq!(cache.get(ip, start + WHOIS_TTL / 2), Some(peer()));
        assert!(cache.get(ip, start + WHOIS_TTL).is_none());
        assert!(cache.get(IpAddr::from([100, 64, 0, 2]), start).is_none());
    }

    #[test]
    fn the_server_id_persists_and_a_malformed_one_is_replaced() {
        let fx = Fixture::new();
        let start_again = |dir: &Path| {
            let (server, _) = Server::start(Config {
                addrs: Vec::new(),
                devices: fx.devices.clone(),
                data_dir: dir.to_path_buf(),
                cli: None,
                allow_loopback_for_tests: true,
                name: "Test Mac".into(),
                mux_socket: fx.mux.clone(),
            })
            .unwrap();
            server.id().to_string()
        };
        let first = fx.server.id().to_string();
        assert!(is_server_id(&first));
        assert_eq!(start_again(&fx.dir), first);
        let path = fx.dir.join(ID_FILE);
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(
                &std::fs::metadata(&path).unwrap().permissions()
            ) & 0o777,
            0o600
        );
        std::fs::write(&path, "NOT-A-SERVER-ID!").unwrap();
        let replaced = start_again(&fx.dir);
        assert!(is_server_id(&replaced));
        assert_eq!(start_again(&fx.dir), replaced);
    }

    #[tokio::test]
    async fn shutdown_resolves_once_the_port_is_free() {
        let fx = Fixture::new();
        let (server, _) = Server::start(Config {
            addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
            devices: fx.devices.clone(),
            data_dir: fx.dir.clone(),
            cli: None,
            allow_loopback_for_tests: true,
            name: "Test Mac".into(),
            mux_socket: fx.mux.clone(),
        })
        .unwrap();
        let addr = server.local_addrs()[0];
        let _ = timeout(Duration::from_secs(5), server.shutdown())
            .await
            .expect("the server never finished stopping");
        StdListener::bind(addr).expect("the port was still held");
    }

    #[tokio::test]
    async fn origin_gets_403() {
        let fx = Fixture::new();
        let (_, token) = fx.pair_device();
        let headers = [
            ("authorization", format!("Bearer {token}")),
            ("origin", "https://example.com".to_string()),
        ];
        assert_eq!(
            status(fx.dial("/v1/connect", &headers).await),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn other_paths_get_404() {
        let fx = Fixture::new();
        let (_, token) = fx.pair_device();
        let headers = [("authorization", format!("Bearer {token}"))];
        assert_eq!(
            status(fx.dial("/v2/connect", &headers).await),
            StatusCode::NOT_FOUND
        );
    }

    async fn pair_with(fx: &Fixture, code: &str) -> Result<Ws, WsError> {
        fx.dial("/v1/pair", &[(PAIR_HEADER, code.to_string())])
            .await
    }

    #[tokio::test]
    async fn pairing_issues_a_token_that_connects() {
        let fx = Fixture::new();
        let code = PairingCode::new(StdInstant::now()).unwrap();
        let given = code.code().to_string();
        fx.server.set_pairing_code(Some(code));

        let mut ws = pair_with(&fx, &given).await.unwrap();
        send_json(
            &mut ws,
            &PairRequest::Pair {
                device: "Jason's iPhone".into(),
                proto: PROTO,
            },
        )
        .await;
        let Ok(Command::Pair {
            device,
            peer,
            reply,
        }) = timeout(Duration::from_secs(5), fx.next_command())
            .await
            .unwrap()
        else {
            panic!("expected a pair command");
        };
        assert_eq!(device, "Jason's iPhone");
        assert!(peer.ip.is_loopback());
        reply.send(Verdict::Allow).unwrap();

        let reply: PairReply = serde_json::from_str(
            match timeout(Duration::from_secs(5), ws.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => text.to_string(),
                other => panic!("expected a text frame, got {other:?}"),
            }
            .as_str(),
        )
        .unwrap();
        let PairReply::Paired { token, server, .. } = reply else {
            panic!("expected paired");
        };
        assert_eq!(token.len(), 64);
        assert_eq!(server, fx.server.id());
        assert!(
            server.len() == 16
                && server
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        );

        let mut phone = fx.connect(&token).await.unwrap();
        send_json(&mut phone, &hello(PROTO)).await;
        let ServerMsg::Welcome {
            server: welcomed, ..
        } = next_msg(&mut phone).await
        else {
            panic!("expected welcome");
        };
        assert_eq!(welcomed, server);
    }

    #[tokio::test]
    async fn a_pair_code_works_once() {
        let fx = Fixture::new();
        let code = PairingCode::new(StdInstant::now()).unwrap();
        let given = code.code().to_string();
        fx.server.set_pairing_code(Some(code));

        assert!(pair_with(&fx, &given).await.is_ok());
        assert_eq!(
            status(pair_with(&fx, &given).await),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_denied_pairing_issues_nothing() {
        let fx = Fixture::new();
        let code = PairingCode::new(StdInstant::now()).unwrap();
        let given = code.code().to_string();
        fx.server.set_pairing_code(Some(code));

        let mut ws = pair_with(&fx, &given).await.unwrap();
        send_json(
            &mut ws,
            &PairRequest::Pair {
                device: "Phone".into(),
                proto: PROTO,
            },
        )
        .await;
        let Ok(Command::Pair { reply, .. }) = fx.next_command().await else {
            panic!("expected a pair command");
        };
        reply.send(Verdict::Deny).unwrap();
        let Some(Ok(Message::Text(text))) = ws.next().await else {
            panic!("expected a reply");
        };
        assert!(matches!(
            serde_json::from_str(text.as_str()).unwrap(),
            PairReply::Error { .. }
        ));
        assert!(lock(&fx.devices).list().is_empty());
    }

    #[tokio::test]
    async fn a_version_mismatch_closes_with_4001() {
        let fx = Fixture::new();
        let (_, token) = fx.pair_device();
        let mut ws = fx.connect(&token).await.unwrap();
        send_json(&mut ws, &hello(PROTO + 1)).await;
        assert_eq!(close_code(&mut ws).await, Some(CloseCode::Library(4001)));
    }

    #[tokio::test]
    async fn the_app_hears_the_total_of_live_connections() {
        let fx = Fixture::new();
        let (_, token) = fx.pair_device();
        let ws = fx.welcomed(&token).await;
        let heard = timeout(Duration::from_secs(5), fx.commands.recv())
            .await
            .unwrap();
        assert!(matches!(heard, Ok(Command::ClientsChanged(1))));
        drop(ws);
        let heard = timeout(Duration::from_secs(5), fx.commands.recv())
            .await
            .unwrap();
        assert!(matches!(heard, Ok(Command::ClientsChanged(0))));
    }

    #[tokio::test]
    async fn a_handshake_that_fails_before_welcome_is_not_counted() {
        let fx = Fixture::new();
        let (_, token) = fx.pair_device();
        let mut ws = fx.connect(&token).await.unwrap();
        send_json(&mut ws, &hello(PROTO + 1)).await;
        assert_eq!(
            close_code(&mut ws).await,
            Some(CloseCode::Library(VERSION_CLOSE))
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(fx.commands.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_fifth_connection_for_one_device_is_refused() {
        let fx = Fixture::new();
        let (_, token) = fx.pair_device();
        let mut live = Vec::new();
        for _ in 0..MAX_PER_DEVICE {
            live.push(fx.welcomed(&token).await);
        }
        assert_eq!(
            status(fx.connect(&token).await),
            StatusCode::TOO_MANY_REQUESTS
        );
        drop(live.pop());
        // The slot frees when the server notices the close, which takes a moment.
        let mut accepted = false;
        for _ in 0..50 {
            if fx.connect(&token).await.is_ok() {
                accepted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(accepted);
    }

    #[tokio::test]
    async fn an_oversized_frame_closes_the_connection() {
        let fx = Fixture::new();
        let (_, token) = fx.pair_device();
        let mut ws = fx.welcomed(&token).await;
        ws.send(Message::binary(vec![0u8; MAX_INPUT + 1024]))
            .await
            .unwrap();
        assert_eq!(close_code(&mut ws).await, Some(CloseCode::Size));
    }

    #[tokio::test]
    async fn a_snapshot_is_pushed_on_change() {
        let fx = Fixture::new();
        let (_, token) = fx.pair_device();
        let mut ws = fx.welcomed(&token).await;
        let ServerMsg::Snapshot {
            seq: first,
            terminals,
            ..
        } = next_msg(&mut ws).await
        else {
            panic!("expected the first snapshot");
        };
        assert!(terminals.is_empty());

        fx.server.set_snapshot(Arc::new(Snapshot {
            terminals: vec![terminal(7)],
            ..Snapshot::default()
        }));
        let ServerMsg::Snapshot { seq, terminals, .. } = next_msg(&mut ws).await else {
            panic!("expected a snapshot after the change");
        };
        assert!(seq > first);
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].pane, 7);
    }

    #[tokio::test]
    async fn attach_needs_a_pane_in_the_snapshot() {
        let fx = Fixture::new();
        let (_, token) = fx.pair_device();
        let mut ws = fx.welcomed(&token).await;
        next_msg(&mut ws).await;
        send_json(&mut ws, &ClientMsg::Attach { pane: 99, tail: 0 }).await;
        let ServerMsg::Error { message, .. } = next_msg(&mut ws).await else {
            panic!("expected an error");
        };
        assert_eq!(message, "pane is not open");
    }

    enum Seen {
        Input(u64, Vec<u8>),
        Eof,
    }

    /// Stands in for the session daemon; `script` runs on each `Attach`.
    struct FakeMux {
        seen: std::sync::mpsc::Receiver<Seen>,
        path: PathBuf,
        stop: Arc<std::sync::atomic::AtomicBool>,
    }

    impl FakeMux {
        fn start(
            path: &Path,
            script: impl Fn(&mut UnixStream, u64) + Send + Sync + 'static,
        ) -> Self {
            let listener = UnixListener::bind(path).unwrap();
            let (seen_tx, seen) = std::sync::mpsc::channel();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let script = Arc::new(script);
            let stopping = stop.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { break };
                    if stopping.load(Ordering::SeqCst) {
                        break;
                    }
                    let (seen_tx, script) = (seen_tx.clone(), script.clone());
                    std::thread::spawn(move || serve_mux(stream, &*script, &seen_tx));
                }
            });
            Self {
                seen,
                path: path.to_path_buf(),
                stop,
            }
        }

        fn next(&self) -> Seen {
            self.seen
                .recv_timeout(Duration::from_secs(10))
                .expect("the fake daemon saw nothing")
        }

        fn expect_eof(&self) {
            assert!(matches!(self.next(), Seen::Eof), "expected a hang-up");
        }
    }

    impl Drop for FakeMux {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = UnixStream::connect(&self.path);
        }
    }

    fn serve_mux(
        mut stream: UnixStream,
        script: &(dyn Fn(&mut UnixStream, u64) + Send + Sync),
        seen: &std::sync::mpsc::Sender<Seen>,
    ) {
        let hello: Option<mux::ClientMsg> = mux::read_frame(&mut stream).unwrap();
        assert!(matches!(hello, Some(mux::ClientMsg::Hello { .. })));
        let reply = mux::ServerMsg::Hello {
            proto: mux::PROTO_VERSION,
            pid: 0,
            panes: 0,
        };
        mux::write_frame(&mut stream, &reply).unwrap();
        loop {
            let message: std::io::Result<Option<mux::ClientMsg>> = mux::read_frame(&mut stream);
            match message {
                Ok(Some(mux::ClientMsg::Attach { pane })) => script(&mut stream, pane),
                Ok(Some(mux::ClientMsg::Input { pane, data })) => {
                    let _ = seen.send(Seen::Input(pane, data));
                }
                Ok(Some(_)) => {}
                _ => {
                    let _ = seen.send(Seen::Eof);
                    return;
                }
            }
        }
    }

    fn mux_send(stream: &mut UnixStream, message: mux::ServerMsg) -> bool {
        mux::write_frame(stream, &message).is_ok()
    }

    fn mux_attached(stream: &mut UnixStream, pane: u64) {
        mux_send(
            stream,
            mux::ServerMsg::Attached {
                pane,
                rows: 30,
                cols: 100,
            },
        );
    }

    /// Replays `aaaa\nbbbb\ncccc\n` in three pieces, then sends `live`.
    fn mux_replay_then_live(stream: &mut UnixStream, pane: u64) {
        mux_attached(stream, pane);
        for piece in [&b"aaaa\nbb"[..], b"bb\ncccc", b"\n"] {
            let data = piece.to_vec();
            mux_send(stream, mux::ServerMsg::Output { pane, data });
        }
        mux_send(stream, mux::ServerMsg::ReplayDone { pane });
        let data = b"live".to_vec();
        mux_send(stream, mux::ServerMsg::Output { pane, data });
    }

    fn mux_attached_only(stream: &mut UnixStream, pane: u64) {
        mux_attached(stream, pane);
        mux_send(stream, mux::ServerMsg::ReplayDone { pane });
    }

    #[derive(Debug)]
    enum Item {
        Msg(ServerMsg),
        Bytes(u64, Vec<u8>),
    }

    /// The next message or output frame, skipping snapshots.
    async fn next_item(ws: &mut Ws) -> Item {
        loop {
            let message = timeout(Duration::from_secs(10), ws.next())
                .await
                .expect("timed out")
                .expect("closed")
                .expect("socket error");
            match message {
                Message::Text(text) => match serde_json::from_str(text.as_str()).unwrap() {
                    ServerMsg::Snapshot { .. } => {}
                    other => return Item::Msg(other),
                },
                Message::Binary(bytes) => {
                    let (kind, pane, payload) = protocol::parse(&bytes).unwrap();
                    assert_eq!(kind, Kind::Output);
                    assert!(payload.len() <= protocol::MAX_PAYLOAD);
                    return Item::Bytes(pane, payload.to_vec());
                }
                _ => {}
            }
        }
    }

    fn show_panes(fx: &Fixture, panes: &[u64]) {
        fx.server.set_snapshot(Arc::new(Snapshot {
            terminals: panes.iter().map(|&p| terminal(p)).collect(),
            ..Snapshot::default()
        }));
    }

    /// Welcomes a new phone and attaches `pane`, returning after the replay finished.
    async fn attached(fx: &Fixture, pane: u64, tail: u64) -> Ws {
        show_panes(fx, &[pane]);
        let (_, token) = fx.pair_device();
        let mut ws = fx.welcomed(&token).await;
        send_json(&mut ws, &ClientMsg::Attach { pane, tail }).await;
        loop {
            if let Item::Msg(ServerMsg::ReplayDone { .. }) = next_item(&mut ws).await {
                return ws;
            }
        }
    }

    async fn send_input(ws: &mut Ws, pane: u64, data: &[u8]) {
        let frame = protocol::frame(Kind::Input, pane, data);
        ws.send(Message::binary(frame)).await.unwrap();
    }

    #[tokio::test]
    async fn an_attached_pane_gets_its_trimmed_tail_then_live_output() {
        let fx = Fixture::new();
        let _mux = FakeMux::start(&fx.mux, mux_replay_then_live);
        show_panes(&fx, &[7]);
        let (_, token) = fx.pair_device();
        let mut ws = fx.welcomed(&token).await;
        send_json(&mut ws, &ClientMsg::Attach { pane: 7, tail: 10 }).await;
        assert!(matches!(
            next_item(&mut ws).await,
            Item::Msg(ServerMsg::Attached {
                pane: 7,
                rows: 30,
                cols: 100
            })
        ));
        let mut tail = Vec::new();
        loop {
            match next_item(&mut ws).await {
                Item::Bytes(7, bytes) => tail.extend(bytes),
                Item::Msg(ServerMsg::ReplayDone { pane: 7 }) => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(tail, b"cccc\n");
        assert!(matches!(next_item(&mut ws).await, Item::Bytes(7, b) if b == b"live"));
    }

    #[tokio::test]
    async fn input_reaches_the_daemon_for_attached_panes_only() {
        let fx = Fixture::new();
        let mux = FakeMux::start(&fx.mux, mux_attached_only);
        let mut ws = attached(&fx, 7, 0).await;
        send_input(&mut ws, 8, b"ignored").await;
        send_input(&mut ws, 7, b"ls\n").await;
        let Seen::Input(pane, data) = mux.next() else {
            panic!("expected input");
        };
        assert_eq!((pane, data), (7, b"ls\n".to_vec()));
    }

    #[tokio::test]
    async fn large_input_arrives_in_order_in_small_chunks() {
        let fx = Fixture::new();
        let mux = FakeMux::start(&fx.mux, mux_attached_only);
        let mut ws = attached(&fx, 7, 0).await;
        let sent: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        send_input(&mut ws, 7, &sent).await;
        let mut sizes = Vec::new();
        let mut got = Vec::new();
        while got.len() < sent.len() {
            let Seen::Input(7, data) = mux.next() else {
                panic!("expected input");
            };
            sizes.push(data.len());
            got.extend(data);
        }
        assert_eq!(sizes, [4096, 4096, 1808]);
        assert_eq!(got, sent);
    }

    #[tokio::test]
    async fn detaching_hangs_up_on_the_daemon() {
        let fx = Fixture::new();
        let mux = FakeMux::start(&fx.mux, mux_attached_only);
        let mut ws = attached(&fx, 7, 0).await;
        send_json(&mut ws, &ClientMsg::Detach { pane: 7 }).await;
        mux.expect_eof();
    }

    #[tokio::test]
    async fn closing_the_socket_hangs_up_on_the_daemon() {
        let fx = Fixture::new();
        let mux = FakeMux::start(&fx.mux, mux_attached_only);
        let ws = attached(&fx, 7, 0).await;
        drop(ws);
        mux.expect_eof();
    }

    #[tokio::test]
    async fn revoking_a_device_hangs_up_on_the_daemon() {
        let fx = Fixture::new();
        let mux = FakeMux::start(&fx.mux, mux_attached_only);
        show_panes(&fx, &[7]);
        let (device, token) = fx.pair_device();
        let mut ws = fx.welcomed(&token).await;
        send_json(&mut ws, &ClientMsg::Attach { pane: 7, tail: 0 }).await;
        assert!(matches!(
            next_item(&mut ws).await,
            Item::Msg(ServerMsg::Attached { .. })
        ));
        fx.server.revoke(&device).unwrap();
        mux.expect_eof();
    }

    #[tokio::test]
    async fn an_exited_pane_is_reported_with_its_code() {
        let fx = Fixture::new();
        let _mux = FakeMux::start(&fx.mux, |stream, pane| {
            mux_attached_only(stream, pane);
            mux_send(
                stream,
                mux::ServerMsg::Exited {
                    pane,
                    code: Some(3),
                },
            );
        });
        let mut ws = attached(&fx, 7, 0).await;
        assert!(matches!(
            next_item(&mut ws).await,
            Item::Msg(ServerMsg::Exited {
                pane: 7,
                code: Some(3)
            })
        ));
    }

    #[tokio::test]
    async fn an_unreachable_daemon_drops_the_pane_as_closed() {
        let fx = Fixture::new();
        show_panes(&fx, &[7]);
        let (_, token) = fx.pair_device();
        let mut ws = fx.welcomed(&token).await;
        send_json(&mut ws, &ClientMsg::Attach { pane: 7, tail: 0 }).await;
        assert!(matches!(
            next_item(&mut ws).await,
            Item::Msg(ServerMsg::Dropped {
                pane: 7,
                reason: DropReason::Closed
            })
        ));
    }

    #[tokio::test]
    async fn a_daemon_hang_up_without_an_exit_drops_the_pane_as_closed() {
        let fx = Fixture::new();
        let _mux = FakeMux::start(&fx.mux, |stream, pane| {
            mux_attached_only(stream, pane);
            let _ = stream.shutdown(std::net::Shutdown::Both);
        });
        let mut ws = attached(&fx, 7, 0).await;
        assert!(matches!(
            next_item(&mut ws).await,
            Item::Msg(ServerMsg::Dropped {
                pane: 7,
                reason: DropReason::Closed
            })
        ));
    }

    #[tokio::test]
    async fn eight_panes_can_be_attached_and_a_reattach_replaces() {
        let fx = Fixture::new();
        let mux = FakeMux::start(&fx.mux, mux_attached_only);
        show_panes(&fx, &(1..=9).collect::<Vec<_>>());
        let (_, token) = fx.pair_device();
        let mut ws = fx.welcomed(&token).await;
        for pane in 1..=8 {
            send_json(&mut ws, &ClientMsg::Attach { pane, tail: 0 }).await;
        }
        let mut done = 0;
        while done < 8 {
            if let Item::Msg(ServerMsg::ReplayDone { .. }) = next_item(&mut ws).await {
                done += 1;
            }
        }
        send_json(&mut ws, &ClientMsg::Attach { pane: 9, tail: 0 }).await;
        assert!(matches!(
            next_item(&mut ws).await,
            Item::Msg(ServerMsg::Error { id: None, .. })
        ));
        send_json(&mut ws, &ClientMsg::Attach { pane: 1, tail: 0 }).await;
        assert!(matches!(
            next_item(&mut ws).await,
            Item::Msg(ServerMsg::Attached { pane: 1, .. })
        ));
        mux.expect_eof();
    }

    #[tokio::test]
    async fn a_phone_that_stops_reading_is_dropped_as_lagging() {
        let fx = Fixture::new();
        let mux = FakeMux::start(&fx.mux, |stream, pane| {
            mux_attached_only(stream, pane);
            for _ in 0..256 {
                let data = vec![b'x'; 256 * 1024];
                if !mux_send(stream, mux::ServerMsg::Output { pane, data }) {
                    break;
                }
            }
        });
        show_panes(&fx, &[7]);
        let (_, token) = fx.pair_device();
        let mut ws = fx.welcomed(&token).await;
        send_json(&mut ws, &ClientMsg::Attach { pane: 7, tail: 0 }).await;
        // The phone reads nothing until the server has hung up on the daemon.
        mux.expect_eof();
        loop {
            if let Item::Msg(ServerMsg::Dropped { pane, reason }) = next_item(&mut ws).await {
                assert_eq!((pane, reason), (7, DropReason::Lagging));
                break;
            }
        }
    }

    #[tokio::test]
    async fn a_daemon_refusal_drops_the_pane_as_closed() {
        let fx = Fixture::new();
        let _mux = FakeMux::start(&fx.mux, |stream, pane| {
            let kind = mux::ErrorKind::NoSuchPane(pane);
            mux_send(stream, mux::ServerMsg::Error { kind });
        });
        show_panes(&fx, &[7]);
        let (_, token) = fx.pair_device();
        let mut ws = fx.welcomed(&token).await;
        send_json(&mut ws, &ClientMsg::Attach { pane: 7, tail: 0 }).await;
        assert!(matches!(
            next_item(&mut ws).await,
            Item::Msg(ServerMsg::Dropped {
                pane: 7,
                reason: DropReason::Closed
            })
        ));
    }

    #[tokio::test]
    async fn a_daemon_that_stops_reading_input_drops_the_pane_as_lagging() {
        let fx = Fixture::new();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let released = Mutex::new(released);
        let mux = FakeMux::start(&fx.mux, move |stream, pane| {
            mux_attached_only(stream, pane);
            // Not reading input until released is what makes the daemon look stalled.
            let _ = released
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10));
        });
        let mut ws = attached(&fx, 7, 0).await;
        let chunk = vec![b'k'; protocol::MAX_PAYLOAD];
        for _ in 0..24 {
            send_input(&mut ws, 7, &chunk).await;
        }
        loop {
            if let Item::Msg(ServerMsg::Dropped { pane, reason }) = next_item(&mut ws).await {
                assert_eq!((pane, reason), (7, DropReason::Lagging));
                break;
            }
        }
        release.send(()).unwrap();
        while !matches!(mux.next(), Seen::Eof) {}
    }

    #[tokio::test]
    async fn a_reattach_discards_what_the_old_attachment_still_had_queued() {
        let fx = Fixture::new();
        let attaches = AtomicUsize::new(0);
        let _mux = FakeMux::start(&fx.mux, move |stream, pane| {
            mux_attached_only(stream, pane);
            let (data, burst) = if attaches.fetch_add(1, Ordering::SeqCst) == 0 {
                (b"old".to_vec(), 200)
            } else {
                (b"new".to_vec(), 1)
            };
            for _ in 0..burst {
                let data = data.clone();
                mux_send(stream, mux::ServerMsg::Output { pane, data });
            }
        });
        let mut ws = attached(&fx, 7, 0).await;
        assert!(matches!(next_item(&mut ws).await, Item::Bytes(7, b) if b == b"old"));
        send_json(&mut ws, &ClientMsg::Attach { pane: 7, tail: 0 }).await;
        loop {
            if let Item::Msg(ServerMsg::Attached { pane: 7, .. }) = next_item(&mut ws).await {
                break;
            }
        }
        assert!(matches!(
            next_item(&mut ws).await,
            Item::Msg(ServerMsg::ReplayDone { pane: 7 })
        ));
        assert!(matches!(next_item(&mut ws).await, Item::Bytes(7, b) if b == b"new"));
    }

    #[test]
    fn only_tailnet_peers_are_allowed() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(peer_allowed(ip("100.100.1.1"), false));
        assert!(peer_allowed(ip("fd7a:115c:a1e0::1"), false));
        assert!(peer_allowed(ip("::ffff:100.100.1.1"), false));
        assert!(!peer_allowed(ip("192.168.1.5"), false));
        assert!(!peer_allowed(ip("8.8.8.8"), false));
        assert!(!peer_allowed(ip("127.0.0.1"), false));
        assert!(!peer_allowed(ip("::1"), false));
        assert!(peer_allowed(ip("127.0.0.1"), true));
        assert!(peer_allowed(ip("::ffff:127.0.0.1"), true));
        assert!(!peer_allowed(ip("192.168.1.5"), true));
        assert!(!peer_allowed(ip("::ffff:192.168.1.5"), true));
    }

    #[test]
    fn device_names_lose_control_characters() {
        assert_eq!(clean_name("Jason\n's\u{7} iPhone"), "Jason's iPhone");
        assert_eq!(clean_name(" \n "), "Phone");
        assert_eq!(clean_name(&"x".repeat(200)).len(), MAX_DEVICE_NAME);
    }

    #[test]
    fn a_login_mismatch_is_refused() {
        let device = Device {
            id: "d".into(),
            name: "n".into(),
            token_sha256: String::new(),
            login: "a@b.c".into(),
            node: "node".into(),
            created_ms: 0,
            last_seen_ms: 0,
        };
        let peer = |login: &str| tailnet::Peer {
            login: login.into(),
            node: "node".into(),
            os: "iOS".into(),
        };
        assert!(same_owner(&device, Some(&peer("a@b.c"))));
        assert!(!same_owner(&device, Some(&peer("x@y.z"))));
        assert!(same_owner(&device, None));
    }
}
