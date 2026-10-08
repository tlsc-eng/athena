use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::completion::{CompletionList, TextEdit, parse_completions, parse_text_edits};
use crate::markup::{Hover, parse_hover};
use crate::protocol::{self, Diagnostic, Location, Position};
use crate::signature::{SignatureHelp, parse_signature_help};
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

/// A reply's `result`, or the message of its `error`.
type Reply = Result<Value, String>;
type Pending = Arc<Mutex<HashMap<i64, async_channel::Sender<Reply>>>>;
/// Frames for the one thread that writes to the server, so no writer waits on another.
type Frames = mpsc::Sender<Value>;

/// One running language server. Calls never block: messages queue until `initialize` has finished.
pub struct Client {
    outgoing: async_channel::Sender<Outgoing>,
    pending: Pending,
    next_id: AtomicI64,
    child: Arc<Mutex<Option<Child>>>,
    /// Characters after which the server offers completions, known once it has started.
    triggers: Arc<OnceLock<Vec<String>>>,
    signature_triggers: Arc<OnceLock<Vec<String>>>,
}

impl Client {
    pub fn start(kind: ServerKind, root: PathBuf) -> (Self, async_channel::Receiver<Event>) {
        let (out_tx, out_rx) = async_channel::unbounded();
        let (events_tx, events_rx) = async_channel::unbounded();
        let pending: Pending = Arc::default();
        let child: Arc<Mutex<Option<Child>>> = Arc::default();
        let triggers: Arc<OnceLock<Vec<String>>> = Arc::default();
        let signature_triggers: Arc<OnceLock<Vec<String>>> = Arc::default();
        let session = Session {
            triggers: triggers.clone(),
            signature_triggers: signature_triggers.clone(),
            kind,
            root,
            outgoing: out_rx.clone(),
            events: events_tx.clone(),
            pending: pending.clone(),
            child: child.clone(),
        };
        let waiters = pending.clone();
        let spawned = thread::Builder::new()
            .name(format!("lsp-{}", kind.program()))
            .spawn(move || {
                if let Err(e) = session.run() {
                    let _ = events_tx.send_blocking(Event::Stopped(format!("{e:#}")));
                }
                // Requests sent while it failed to start are answered rather than left waiting.
                out_rx.close();
                waiters.lock().expect("pending lock").clear();
            });
        if spawned.is_err() {
            let _ = events_rx.close();
        }
        let client = Self {
            outgoing: out_tx,
            pending,
            next_id: AtomicI64::new(1),
            child,
            triggers,
            signature_triggers,
        };
        (client, events_rx)
    }

    /// The server's process id, once it is running.
    pub fn pid(&self) -> Option<u32> {
        self.child
            .lock()
            .expect("child lock")
            .as_ref()
            .map(Child::id)
    }

    /// The server's completion trigger characters; empty until it has started.
    pub fn completion_triggers(&self) -> &[String] {
        self.triggers.get().map_or(&[], Vec::as_slice)
    }

    /// Characters that open or move signature help, such as `(` and `,`; empty until started.
    pub fn signature_triggers(&self) -> &[String] {
        self.signature_triggers.get().map_or(&[], Vec::as_slice)
    }

    fn notify(&self, method: &'static str, params: Value) {
        let _ = self.outgoing.try_send(Outgoing::Notify(method, params));
    }

    fn request(&self, method: &'static str, params: Value) -> async_channel::Receiver<Reply> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(id, method, "lsp request");
        let (tx, rx) = async_channel::bounded(1);
        self.pending.lock().expect("pending lock").insert(id, tx);
        // Once the server has gone the queue is closed, and the waiter is told so at once.
        if self
            .outgoing
            .try_send(Outgoing::Request(id, method, params))
            .is_err()
        {
            self.pending.lock().expect("pending lock").remove(&id);
        }
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

    /// Where the symbol at `at` is defined; `Err` carries the server's own reason.
    pub async fn definition(&self, path: &Path, at: Position) -> Result<Vec<Location>, String> {
        let reply = self.request(
            "textDocument/definition",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at}),
        );
        locations(reply).await
    }

    /// Every use of the symbol at `at`, its declaration included.
    pub async fn references(&self, path: &Path, at: Position) -> Result<Vec<Location>, String> {
        let reply = self.request(
            "textDocument/references",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at,
                   "context": {"includeDeclaration": true}}),
        );
        locations(reply).await
    }

    /// Documentation for the symbol at `at`; `None` when the server has nothing to say.
    pub async fn hover(&self, path: &Path, at: Position) -> Result<Option<Hover>, String> {
        let reply = self.request(
            "textDocument/hover",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at}),
        );
        Ok(parse_hover(&answer(reply).await?))
    }

    /// The signature of the call around `at`; `None` outside any call.
    pub async fn signature_help(
        &self,
        path: &Path,
        at: Position,
    ) -> Result<Option<SignatureHelp>, String> {
        let reply = self.request(
            "textDocument/signatureHelp",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at}),
        );
        Ok(parse_signature_help(&answer(reply).await?))
    }

    /// The edits that format the whole document, indenting with tabs or `tab_size` spaces.
    pub async fn formatting(
        &self,
        path: &Path,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<Vec<TextEdit>, String> {
        let reply = self.request(
            "textDocument/formatting",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)},
                   "options": {"tabSize": tab_size, "insertSpaces": insert_spaces}}),
        );
        Ok(parse_text_edits(&answer(reply).await?))
    }

    /// Suggestions at `at`; `trigger` is the character typed that asked for them, if any.
    pub async fn completion(
        &self,
        path: &Path,
        at: Position,
        trigger: Option<&str>,
    ) -> Result<CompletionList, String> {
        let context = match trigger {
            Some(c) => json!({"triggerKind": 2, "triggerCharacter": c}),
            None => json!({"triggerKind": 1}),
        };
        let reply = self.request(
            "textDocument/completion",
            json!({"textDocument": {"uri": protocol::uri_from_path(path)}, "position": at,
                   "context": context}),
        );
        Ok(parse_completions(&answer(reply).await?))
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
    triggers: Arc<OnceLock<Vec<String>>>,
    signature_triggers: Arc<OnceLock<Vec<String>>>,
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
        self.serve(stdin, stdout)
    }

    fn serve(
        self,
        stdin: impl Write + Send + 'static,
        stdout: impl Read + Send + 'static,
    ) -> Result<()> {
        let (writer, frames) = mpsc::channel();
        thread::Builder::new()
            .name("lsp-writer".into())
            .spawn(move || write_frames(BufWriter::new(stdin), frames))?;
        let reader = Reader {
            writer: writer.clone(),
            outgoing: self.outgoing.clone(),
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
        send(&writer, request(0, "initialize", self.initialize_params()))?;
        let result = init
            .recv_blocking()
            .context("the server stopped while starting")?
            .map_err(anyhow::Error::msg)?;
        if result.is_null() {
            bail!("{} refused to start", self.kind.program());
        }
        let strings = |pointer: &str| -> Vec<String> {
            result
                .pointer(pointer)
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        let _ = self.triggers.set(strings(
            "/capabilities/completionProvider/triggerCharacters",
        ));
        let mut signature = strings("/capabilities/signatureHelpProvider/triggerCharacters");
        signature.extend(strings(
            "/capabilities/signatureHelpProvider/retriggerCharacters",
        ));
        let _ = self.signature_triggers.set(signature);
        send(&writer, notification("initialized", json!({})))?;
        let _ = self.events.send_blocking(Event::Ready);

        while let Ok(message) = self.outgoing.recv_blocking() {
            let frame = match message {
                Outgoing::Notify(method, params) => notification(method, params),
                Outgoing::Request(id, method, params) => request(id, method, params),
                Outgoing::Shutdown => break,
            };
            send(&writer, frame)?;
        }
        let _ = send(&writer, request(i64::MAX, "shutdown", Value::Null));
        let _ = send(&writer, notification("exit", Value::Null));
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
                    "definition": {"linkSupport": true},
                    "references": {},
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "formatting": {},
                    "signatureHelp": {
                        "signatureInformation": {
                            "documentationFormat": ["markdown", "plaintext"],
                            "parameterInformation": {"labelOffsetSupport": true}
                        }
                    },
                    "completion": {
                        "completionItem": {"snippetSupport": true},
                        "contextSupport": true
                    }
                }
            }
        })
    }
}

struct Reader {
    writer: Frames,
    outgoing: async_channel::Receiver<Outgoing>,
    pending: Pending,
    events: async_channel::Sender<Event>,
}

impl Reader {
    fn run(self, stdout: impl Read) {
        let mut input = BufReader::new(stdout);
        while let Ok(Some(message)) = read_message(&mut input) {
            self.handle(message);
        }
        // Waiters see a closed channel instead of hanging; closing the queue first means a
        // request that misses this clear is refused by the queue instead.
        self.outgoing.close();
        self.pending.lock().expect("pending lock").clear();
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
                    tracing::debug!(%id, failed = message.get("error").is_some(), "lsp reply");
                    let reply = match message.get("error") {
                        Some(error) => Err(error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("the request failed")
                            .to_string()),
                        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = waiter.try_send(reply);
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
                    json!({"jsonrpc": "2.0", "id": id, "result": result}),
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

async fn answer(reply: async_channel::Receiver<Reply>) -> Result<Value, String> {
    reply
        .recv()
        .await
        .unwrap_or_else(|_| Err("the language server exited".into()))
}

async fn locations(reply: async_channel::Receiver<Reply>) -> Result<Vec<Location>, String> {
    answer(reply).await.map(|v| protocol::parse_locations(&v))
}

fn request(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

fn send(writer: &Frames, message: Value) -> Result<()> {
    writer
        .send(message)
        .map_err(|_| anyhow!("the language server has exited"))
}

/// Writes queued frames in order until the server stops reading or every sender is gone.
fn write_frames(mut out: impl Write, frames: mpsc::Receiver<Value>) {
    for message in frames {
        let Ok(body) = serde_json::to_vec(&message) else {
            continue;
        };
        let written = write!(out, "Content-Length: {}\r\n\r\n", body.len())
            .and_then(|()| out.write_all(&body))
            .and_then(|()| out.flush());
        if written.is_err() {
            return;
        }
    }
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
    use std::os::unix::net::UnixStream;
    use std::time::Instant;

    fn write_message(out: &mut impl Write, message: &Value) {
        let body = serde_json::to_vec(message).unwrap();
        write!(out, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        out.write_all(&body).unwrap();
    }

    /// A server that floods configuration requests while the client sends a huge didChange.
    #[test]
    fn answering_server_requests_never_waits_behind_a_large_write() {
        const REQUESTS: i64 = 2000;
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let (out_tx, out_rx) = async_channel::unbounded();
        let (events_tx, events) = async_channel::unbounded();
        let session = Session {
            kind: ServerKind::Go,
            root: PathBuf::from("/tmp"),
            outgoing: out_rx,
            events: events_tx,
            pending: Arc::default(),
            child: Arc::default(),
            triggers: Arc::default(),
            signature_triggers: Arc::default(),
        };
        let stdin = client_end.try_clone().unwrap();
        thread::spawn(move || session.serve(stdin, client_end));

        let (done_tx, done) = mpsc::channel();
        thread::spawn(move || {
            let mut input = BufReader::new(server_end.try_clone().unwrap());
            let mut output = server_end;
            let init = read_message(&mut input).unwrap().unwrap();
            write_message(
                &mut output,
                &json!({"jsonrpc": "2.0", "id": init["id"], "result": {"capabilities": {}}}),
            );
            let initialized = read_message(&mut input).unwrap().unwrap();
            assert_eq!(initialized["method"], "initialized");
            for id in 0..REQUESTS {
                write_message(
                    &mut output,
                    &json!({"jsonrpc": "2.0", "id": 1000 + id, "method": "workspace/configuration",
                            "params": {"items": [{"section": "gopls"}]}}),
                );
            }
            let (mut replies, mut changed) = (0, false);
            while replies < REQUESTS || !changed {
                let message = read_message(&mut input).unwrap().unwrap();
                match message["method"].as_str() {
                    Some("textDocument/didChange") => changed = true,
                    Some(_) => {}
                    None => replies += 1,
                }
            }
            let _ = done_tx.send(());
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(events.try_recv(), Ok(Event::Ready)) {
            assert!(Instant::now() < deadline, "never became ready");
            thread::sleep(Duration::from_millis(5));
        }
        let text = "x".repeat(4 * 1024 * 1024);
        out_tx
            .try_send(Outgoing::Notify(
                "textDocument/didChange",
                json!({"contentChanges": [{"text": text}]}),
            ))
            .unwrap();
        assert!(
            done.recv_timeout(Duration::from_secs(20)).is_ok(),
            "client and server deadlocked"
        );
    }

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
