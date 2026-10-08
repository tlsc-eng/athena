use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::protocol::{self, Diagnostic, Location, Position};
use crate::{ServerKind, env};

const MAX_MESSAGE: usize = 64 * 1024 * 1024;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// What a server reports, in arrival order.
#[derive(Debug)]
pub enum Event {
    Ready,
    Diagnostics {
        path: PathBuf,
        list: Vec<Diagnostic>,
    },
    /// The server could not start, or stopped; the text says why.
    Stopped(String),
}

enum Outgoing {
    Notify(&'static str, Value),
    Request(i64, &'static str, Value),
    Shutdown,
}

type Pending = Arc<Mutex<HashMap<i64, async_channel::Sender<Value>>>>;
type Writer = Arc<Mutex<Option<BufWriter<ChildStdin>>>>;

/// One running language server. Calls never block: messages queue until `initialize` has finished.
pub struct Client {
    outgoing: async_channel::Sender<Outgoing>,
    pending: Pending,
    next_id: AtomicI64,
    child: Arc<Mutex<Option<Child>>>,
}

impl Client {
    pub fn start(kind: ServerKind, root: PathBuf) -> (Self, async_channel::Receiver<Event>) {
        let (out_tx, out_rx) = async_channel::unbounded();
        let (events_tx, events_rx) = async_channel::unbounded();
        let pending: Pending = Arc::default();
        let child: Arc<Mutex<Option<Child>>> = Arc::default();
        let session = Session {
            kind,
            root,
            outgoing: out_rx,
            events: events_tx.clone(),
            pending: pending.clone(),
            child: child.clone(),
        };
        let spawned = thread::Builder::new()
            .name(format!("lsp-{}", kind.program()))
            .spawn(move || {
                if let Err(e) = session.run() {
                    let _ = events_tx.send_blocking(Event::Stopped(format!("{e:#}")));
                }
            });
        if spawned.is_err() {
            let _ = events_rx.close();
        }
        let client = Self {
            outgoing: out_tx,
            pending,
            next_id: AtomicI64::new(1),
            child,
        };
        (client, events_rx)
    }

    fn notify(&self, method: &'static str, params: Value) {
        let _ = self.outgoing.try_send(Outgoing::Notify(method, params));
    }

    fn request(&self, method: &'static str, params: Value) -> async_channel::Receiver<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = async_channel::bounded(1);
        self.pending.lock().expect("pending lock").insert(id, tx);
        let _ = self
            .outgoing
            .try_send(Outgoing::Request(id, method, params));
        rx
    }

    pub fn did_open(&self, path: &Path, language_id: &str, version: i64, text: String) {
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {"uri": protocol::uri_from_path(path), "languageId": language_id,
                                    "version": version, "text": text}}),
        );
    }

    /// Sends the whole document; valid whichever sync kind the server asked for.
    pub fn did_change(&self, path: &Path, version: i64, text: String) {
        self.notify(
            "textDocument/didChange",
            json!({"textDocument": {"uri": protocol::uri_from_path(path), "version": version},
                   "contentChanges": [{"text": text}]}),
        );
    }

    pub fn did_save(&self, path: &Path) {
        self.notify(
            "textDocument/didSave",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}}),
        );
    }

    pub fn did_close(&self, path: &Path) {
        self.notify(
            "textDocument/didClose",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}}),
        );
    }

    pub async fn definition(&self, path: &Path, at: Position) -> Vec<Location> {
        let reply = self.request(
            "textDocument/definition",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at}),
        );
        reply
            .recv()
            .await
            .map(|v| protocol::parse_locations(&v))
            .unwrap_or_default()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.outgoing.try_send(Outgoing::Shutdown);
        let child = self.child.clone();
        // A server that ignores shutdown, or never finished starting, is killed.
        let _ = thread::Builder::new()
            .name("lsp-reaper".into())
            .spawn(move || {
                thread::sleep(SHUTDOWN_GRACE);
                if let Some(mut c) = child.lock().expect("child lock").take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            });
    }
}

struct Session {
    kind: ServerKind,
    root: PathBuf,
    outgoing: async_channel::Receiver<Outgoing>,
    events: async_channel::Sender<Event>,
    pending: Pending,
    child: Arc<Mutex<Option<Child>>>,
}

impl Session {
    fn run(self) -> Result<()> {
        let program = env::find_program(self.kind.program()).with_context(|| {
            format!(
                "{} is not installed. Install it with: {}",
                self.kind.program(),
                self.kind.install_hint()
            )
        })?;
        let mut child = Command::new(&program)
            .args(self.kind.args())
            .env_clear()
            .envs(env::server_env())
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("could not start {}", program.display()))?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        *self.child.lock().expect("child lock") = Some(child);

        let writer: Writer = Arc::new(Mutex::new(Some(BufWriter::new(stdin))));
        let reader = Reader {
            writer: writer.clone(),
            pending: self.pending.clone(),
            events: self.events.clone(),
        };
        thread::Builder::new()
            .name("lsp-reader".into())
            .spawn(move || reader.run(stdout))?;

        let init = {
            let (tx, rx) = async_channel::bounded(1);
            self.pending.lock().expect("pending lock").insert(0, tx);
            rx
        };
        send(&writer, &request(0, "initialize", self.initialize_params()))?;
        let result = init
            .recv_blocking()
            .context("the server stopped while starting")?;
        if result.is_null() {
            bail!("{} refused to start", self.kind.program());
        }
        send(&writer, &notification("initialized", json!({})))?;
        let _ = self.events.send_blocking(Event::Ready);

        while let Ok(message) = self.outgoing.recv_blocking() {
            let frame = match message {
                Outgoing::Notify(method, params) => notification(method, params),
                Outgoing::Request(id, method, params) => request(id, method, params),
                Outgoing::Shutdown => break,
            };
            send(&writer, &frame)?;
        }
        let _ = send(&writer, &request(i64::MAX, "shutdown", Value::Null));
        let _ = send(&writer, &notification("exit", Value::Null));
        Ok(())
    }

    fn initialize_params(&self) -> Value {
        let uri = protocol::uri_from_path(&self.root);
        let name = self
            .root
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "athena", "version": env!("CARGO_PKG_VERSION")},
            "rootUri": uri,
            "workspaceFolders": [{"uri": uri, "name": name}],
            "capabilities": {
                "general": {"positionEncodings": ["utf-16"]},
                "workspace": {"configuration": true, "workspaceFolders": true},
                "textDocument": {
                    "synchronization": {"didSave": true},
                    "publishDiagnostics": {},
                    "definition": {"linkSupport": true}
                }
            }
        })
    }
}

struct Reader {
    writer: Writer,
    pending: Pending,
    events: async_channel::Sender<Event>,
}

impl Reader {
    fn run(self, stdout: impl Read) {
        let mut input = BufReader::new(stdout);
        while let Ok(Some(message)) = read_message(&mut input) {
            self.handle(message);
        }
        // Waiters see a closed channel instead of hanging.
        self.pending.lock().expect("pending lock").clear();
        *self.writer.lock().expect("writer lock") = None;
        let _ = self
            .events
            .send_blocking(Event::Stopped("the language server exited".into()));
    }

    fn handle(&self, message: Value) {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();
        match (method, id) {
            (None, Some(id)) => {
                let waiter = id
                    .as_i64()
                    .and_then(|id| self.pending.lock().expect("pending lock").remove(&id));
                if let Some(waiter) = waiter {
                    let result = message.get("result").cloned().unwrap_or(Value::Null);
                    let _ = waiter.try_send(result);
                }
            }
            // Servers wait for these answers; gopls stalls without them.
            (Some(method), Some(id)) => {
                let result = match method {
                    "workspace/configuration" => {
                        let items = message
                            .pointer("/params/items")
                            .and_then(Value::as_array)
                            .map_or(0, Vec::len);
                        Value::Array(vec![Value::Null; items])
                    }
                    "workspace/workspaceFolders" => Value::Array(Vec::new()),
                    _ => Value::Null,
                };
                let _ = send(
                    &self.writer,
                    &json!({"jsonrpc": "2.0", "id": id, "result": result}),
                );
            }
            (Some("textDocument/publishDiagnostics"), None) => {
                if let Some((path, list)) =
                    message.get("params").and_then(protocol::parse_diagnostics)
                {
                    let _ = self.events.send_blocking(Event::Diagnostics { path, list });
                }
            }
            _ => {}
        }
    }
}

fn request(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

fn send(writer: &Writer, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    let mut guard = writer.lock().expect("writer lock");
    let out = guard.as_mut().context("the language server has exited")?;
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()?;
    Ok(())
}

fn read_message(input: &mut impl BufRead) -> Result<Option<Value>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.context("message without Content-Length")?;
    if length > MAX_MESSAGE {
        bail!("message of {length} bytes is too large");
    }
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_read_with_their_length() {
        let body = br#"{"jsonrpc":"2.0","method":"x"}"#;
        let mut raw =
            format!("Content-Length: {}\r\nContent-Type: x\r\n\r\n", body.len()).into_bytes();
        raw.extend_from_slice(body);
        raw.extend_from_slice(b"Content-Length: 2\r\n\r\n{}");
        let mut input = BufReader::new(&raw[..]);
        assert_eq!(read_message(&mut input).unwrap().unwrap()["method"], "x");
        assert_eq!(read_message(&mut input).unwrap().unwrap(), json!({}));
        assert!(read_message(&mut input).unwrap().is_none());
    }
}
