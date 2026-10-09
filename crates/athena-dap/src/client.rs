use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::framing::{read_message, write_frames};
use crate::types::{
    self, BreakpointStatus, Evaluated, Scope, SourceBreakpoint, StackFrame, Stopped, Thread,
    Variable,
};

/// Most requests are answered at once; one that is not means the adapter is stuck.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Delve compiles the program before it answers `launch`, and macOS may first ask for a password.
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(300);
/// A login shell and the adapter both start before it dials back.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const WATCHDOG_TICK: Duration = Duration::from_millis(100);
/// macOS refuses Unix socket paths longer than this.
const MAX_SOCKET_PATH: usize = 100;

/// How Athena talks to an adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Stdio,
    /// The adapter dials a Unix socket Athena listens on, as `dlv dap --client-addr` does; `{socket}`
    /// in its arguments is replaced by that socket's path.
    DialIn,
}

/// A debug adapter to start.
#[derive(Clone, Debug)]
pub struct Adapter {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Started through the login shell, so toolchain managers such as goenv see their setup.
    pub login_shell: bool,
    pub transport: Transport,
}

/// What an adapter reports, in arrival order.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The adapter is ready for breakpoints, then `configurationDone`.
    Initialized,
    Stopped(Stopped),
    Continued {
        thread_id: Option<i64>,
        all_threads: bool,
    },
    Output {
        category: String,
        text: String,
    },
    Breakpoint {
        reason: String,
        breakpoint: BreakpointStatus,
    },
    Thread {
        reason: String,
        id: i64,
    },
    Terminated,
    Exited {
        code: i64,
    },
    /// The adapter stopped or never started; the text says why.
    Closed(String),
}

/// A reply's `body`, or why the request failed.
type Reply = Result<Value, String>;

struct Waiter {
    tx: async_channel::Sender<Reply>,
    deadline: Instant,
    command: String,
    timeout: Duration,
}

type Pending = Arc<Mutex<HashMap<i64, Waiter>>>;

/// State the client and its threads share.
struct Shared {
    pending: Pending,
    seq: AtomicI64,
    /// Set once the adapter's stream has ended, so later requests fail at once.
    closed: AtomicBool,
    frames: mpsc::Sender<Value>,
    events: async_channel::Sender<Event>,
}

impl Shared {
    fn next_seq(&self) -> i64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    fn close(&self, why: &str) {
        self.closed.store(true, Ordering::SeqCst);
        // Dropping the senders tells every waiter the adapter is gone.
        self.pending.lock().expect("pending lock").clear();
        let _ = self.events.try_send(Event::Closed(why.to_string()));
    }
}

/// One running debug adapter. Requests queue until the adapter has connected.
pub struct Client {
    shared: Arc<Shared>,
    child: Arc<Mutex<Option<Child>>>,
    scratch: PathBuf,
    capabilities: OnceLock<Value>,
}

impl Client {
    /// Starts `adapter`; everything it reports arrives on the returned channel.
    pub fn start(adapter: Adapter) -> Result<(Self, async_channel::Receiver<Event>)> {
        let scratch = scratch_dir()?;
        let (frames_tx, frames_rx) = mpsc::channel();
        let (events_tx, events_rx) = async_channel::unbounded();
        let shared = Arc::new(Shared {
            pending: Arc::default(),
            seq: AtomicI64::new(1),
            closed: AtomicBool::new(false),
            frames: frames_tx,
            events: events_tx,
        });
        let child: Arc<Mutex<Option<Child>>> = Arc::default();
        start_watchdog(Arc::downgrade(&shared.pending));
        let session = shared.clone();
        let slot = child.clone();
        let socket = scratch.join("s");
        thread::Builder::new()
            .name("dap-session".into())
            .spawn(move || {
                if let Err(e) = run(&adapter, &socket, &slot, &session, frames_rx) {
                    session.close(&format!("{e:#}"));
                }
            })?;
        let client = Self {
            shared,
            child,
            scratch,
            capabilities: OnceLock::new(),
        };
        Ok((client, events_rx))
    }

    /// A private folder removed with the client, for files the session makes such as Delve's binary.
    pub fn scratch(&self) -> &Path {
        &self.scratch
    }

    /// The adapter's process id, once it is running.
    pub fn pid(&self) -> Option<u32> {
        self.child
            .lock()
            .expect("child lock")
            .as_ref()
            .map(Child::id)
    }

    /// Whether the adapter declared `capability` in its `initialize` reply.
    pub fn supports(&self, capability: &str) -> bool {
        self.capabilities
            .get()
            .and_then(|c| c.get(capability))
            .and_then(Value::as_bool)
            == Some(true)
    }

    /// Sends a request; its reply's `body` comes back, or why it failed or took over `timeout`.
    pub async fn call(&self, command: &str, arguments: Value, timeout: Duration) -> Reply {
        let reply = self.request(command, arguments, timeout);
        reply
            .recv()
            .await
            .unwrap_or_else(|_| Err("the debug adapter has exited".into()))
    }

    fn request(
        &self,
        command: &str,
        arguments: Value,
        timeout: Duration,
    ) -> async_channel::Receiver<Reply> {
        let seq = self.shared.next_seq();
        let (tx, rx) = async_channel::bounded(1);
        let waiter = Waiter {
            tx,
            deadline: Instant::now() + timeout,
            command: command.to_string(),
            timeout,
        };
        self.shared
            .pending
            .lock()
            .expect("pending lock")
            .insert(seq, waiter);
        tracing::debug!(seq, command, "dap request");
        let frame =
            json!({"seq": seq, "type": "request", "command": command, "arguments": arguments});
        if self.shared.closed.load(Ordering::SeqCst) || self.shared.frames.send(frame).is_err() {
            self.shared
                .pending
                .lock()
                .expect("pending lock")
                .remove(&seq);
        }
        rx
    }

    pub async fn initialize(&self, adapter_id: &str) -> Reply {
        let arguments = json!({
            "clientID": "athena",
            "clientName": "Athena",
            "adapterID": adapter_id,
            "locale": "en",
            "pathFormat": "path",
            "linesStartAt1": true,
            "columnsStartAt1": true,
            "supportsVariableType": true,
            "supportsRunInTerminalRequest": false,
        });
        let capabilities = self.call("initialize", arguments, REQUEST_TIMEOUT).await?;
        let _ = self.capabilities.set(capabilities.clone());
        Ok(capabilities)
    }

    pub async fn launch(&self, arguments: Value) -> Reply {
        self.call("launch", arguments, LAUNCH_TIMEOUT).await
    }

    /// Replaces every breakpoint in `path` with `breakpoints`.
    pub async fn set_breakpoints(
        &self,
        path: &Path,
        breakpoints: &[SourceBreakpoint],
    ) -> Result<Vec<BreakpointStatus>, String> {
        let list: Vec<Value> = breakpoints.iter().map(SourceBreakpoint::to_json).collect();
        let arguments = json!({
            "source": {"path": path, "name": path.file_name().map(|n| n.to_string_lossy())},
            "breakpoints": list,
        });
        let body = self
            .call("setBreakpoints", arguments, REQUEST_TIMEOUT)
            .await?;
        Ok(types::parse_breakpoints(&body))
    }

    pub async fn configuration_done(&self) -> Reply {
        self.call("configurationDone", json!({}), REQUEST_TIMEOUT)
            .await
    }

    pub async fn threads(&self) -> Result<Vec<Thread>, String> {
        let body = self.call("threads", json!({}), REQUEST_TIMEOUT).await?;
        Ok(types::parse_threads(&body))
    }

    pub async fn stack_trace(&self, thread: i64, levels: u32) -> Result<Vec<StackFrame>, String> {
        let arguments = json!({"threadId": thread, "startFrame": 0, "levels": levels});
        let body = self.call("stackTrace", arguments, REQUEST_TIMEOUT).await?;
        Ok(types::parse_frames(&body))
    }

    pub async fn scopes(&self, frame: i64) -> Result<Vec<Scope>, String> {
        let body = self
            .call("scopes", json!({"frameId": frame}), REQUEST_TIMEOUT)
            .await?;
        Ok(types::parse_scopes(&body))
    }

    pub async fn variables(&self, reference: i64) -> Result<Vec<Variable>, String> {
        let arguments = json!({"variablesReference": reference});
        let body = self.call("variables", arguments, REQUEST_TIMEOUT).await?;
        Ok(types::parse_variables(&body))
    }

    /// Evaluates `expression` in `frame`; `context` is `hover`, `watch` or `repl`.
    pub async fn evaluate(
        &self,
        expression: &str,
        frame: Option<i64>,
        context: &str,
    ) -> Result<Evaluated, String> {
        let mut arguments = json!({"expression": expression, "context": context});
        if let Some(frame) = frame {
            arguments["frameId"] = json!(frame);
        }
        let body = self.call("evaluate", arguments, REQUEST_TIMEOUT).await?;
        Ok(types::parse_evaluated(&body))
    }

    pub async fn continue_(&self, thread: i64) -> Reply {
        self.call("continue", json!({"threadId": thread}), REQUEST_TIMEOUT)
            .await
    }

    pub async fn next(&self, thread: i64) -> Reply {
        self.call("next", json!({"threadId": thread}), REQUEST_TIMEOUT)
            .await
    }

    pub async fn step_in(&self, thread: i64) -> Reply {
        self.call("stepIn", json!({"threadId": thread}), REQUEST_TIMEOUT)
            .await
    }

    pub async fn step_out(&self, thread: i64) -> Reply {
        self.call("stepOut", json!({"threadId": thread}), REQUEST_TIMEOUT)
            .await
    }

    pub async fn pause(&self, thread: i64) -> Reply {
        self.call("pause", json!({"threadId": thread}), REQUEST_TIMEOUT)
            .await
    }

    /// Ends the session, stopping the program it launched.
    pub async fn disconnect(&self) -> Reply {
        let arguments = json!({"restart": false, "terminateDebuggee": true});
        self.call("disconnect", arguments, REQUEST_TIMEOUT).await
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if !self.shared.closed.load(Ordering::SeqCst) {
            let seq = self.shared.next_seq();
            let _ = self
                .shared
                .frames
                .send(json!({"seq": seq, "type": "request",
                "command": "disconnect", "arguments": {"terminateDebuggee": true}}));
        }
        let child = self.child.clone();
        let scratch = self.scratch.clone();
        // The adapter, and the program it runs in its process group, go even if it ignores us.
        let _ = thread::Builder::new()
            .name("dap-reaper".into())
            .spawn(move || {
                thread::sleep(SHUTDOWN_GRACE);
                if let Some(mut c) = child.lock().expect("child lock").take() {
                    // SAFETY: the unreaped child keeps its group id from being reused.
                    unsafe { libc::killpg(c.id() as libc::pid_t, libc::SIGKILL) };
                    let _ = c.wait();
                }
                let _ = std::fs::remove_dir_all(&scratch);
            });
    }
}

/// A new private folder under the temporary folder, short enough to hold a Unix socket.
fn scratch_dir() -> Result<PathBuf> {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        "athena-dap-{}-{}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    );
    let mut dir = std::env::temp_dir().join(&name);
    if dir.as_os_str().len() + 2 > MAX_SOCKET_PATH {
        dir = PathBuf::from("/tmp").join(&name);
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("could not create {}", dir.display()))?;
    Ok(dir)
}

fn start_watchdog(pending: Weak<Mutex<HashMap<i64, Waiter>>>) {
    let _ = thread::Builder::new()
        .name("dap-watchdog".into())
        .spawn(move || {
            loop {
                thread::sleep(WATCHDOG_TICK);
                let Some(pending) = pending.upgrade() else {
                    return;
                };
                let now = Instant::now();
                let mut pending = pending.lock().expect("pending lock");
                let late: Vec<i64> = pending
                    .iter()
                    .filter(|(_, w)| w.deadline <= now)
                    .map(|(seq, _)| *seq)
                    .collect();
                for seq in late {
                    if let Some(w) = pending.remove(&seq) {
                        let why = format!("{} got no answer within {}", w.command, span(w.timeout));
                        let _ = w.tx.try_send(Err(why));
                    }
                }
            }
        });
}

fn span(d: Duration) -> String {
    match d.as_secs() {
        0 => format!("{} ms", d.as_millis()),
        s => format!("{s} s"),
    }
}

fn run(
    adapter: &Adapter,
    socket: &Path,
    slot: &Mutex<Option<Child>>,
    shared: &Arc<Shared>,
    frames: mpsc::Receiver<Value>,
) -> Result<()> {
    let args: Vec<String> = adapter
        .args
        .iter()
        .map(|a| a.replace("{socket}", &socket.to_string_lossy()))
        .collect();
    let listener = match adapter.transport {
        Transport::DialIn => Some(UnixListener::bind(socket).context("could not listen")?),
        Transport::Stdio => None,
    };
    let mut cmd = match adapter.login_shell {
        true => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
            let mut c = Command::new(shell);
            c.args(["-lc", "exec \"$0\" \"$@\""]).arg(&adapter.program);
            c
        }
        false => Command::new(&adapter.program),
    };
    cmd.args(&args)
        .current_dir(&adapter.cwd)
        .process_group(0)
        .stdin(match adapter.transport {
            Transport::Stdio => Stdio::piped(),
            Transport::DialIn => Stdio::null(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("could not start {}", adapter.program.display()))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().context("no stdout")?;
    forward_lines(child.stderr.take(), "stderr", shared);
    *slot.lock().expect("child lock") = Some(child);
    match listener {
        Some(listener) => {
            forward_lines(Some(stdout), "stdout", shared);
            let stream = accept(&listener, slot)?;
            serve(stream.try_clone()?, stream, shared, frames);
        }
        None => serve(stdout, stdin.context("no stdin")?, shared, frames),
    }
    Ok(())
}

/// Waits for the adapter to dial in, giving up if it exits or takes too long.
fn accept(
    listener: &UnixListener,
    slot: &Mutex<Option<Child>>,
) -> Result<std::os::unix::net::UnixStream> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                return Ok(stream);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
        if let Some(child) = slot.lock().expect("child lock").as_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            bail!("the debug adapter exited before connecting ({status})");
        }
        if Instant::now() >= deadline {
            bail!(
                "the debug adapter did not connect within {} s",
                CONNECT_TIMEOUT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Sends what the adapter prints outside the protocol to the console, line by line.
fn forward_lines(pipe: Option<impl Read + Send + 'static>, category: &str, shared: &Arc<Shared>) {
    let Some(pipe) = pipe else {
        return;
    };
    let events = shared.events.clone();
    let category = category.to_string();
    let _ = thread::Builder::new()
        .name("dap-output".into())
        .spawn(move || {
            for line in BufReader::new(pipe).lines() {
                let Ok(line) = line else {
                    return;
                };
                let text = format!("{line}\n");
                let category = category.clone();
                if events
                    .send_blocking(Event::Output { category, text })
                    .is_err()
                {
                    return;
                }
            }
        });
}

/// Writes queued frames on one thread and reads the adapter's on this one until it closes.
fn serve(
    input: impl Read,
    output: impl Write + Send + 'static,
    shared: &Arc<Shared>,
    frames: mpsc::Receiver<Value>,
) {
    let _ = thread::Builder::new()
        .name("dap-writer".into())
        .spawn(move || write_frames(BufWriter::new(output), frames));
    let mut input = BufReader::new(input);
    let why = loop {
        match read_message(&mut input) {
            Ok(Some(message)) => handle(&message, shared),
            Ok(None) => break "the debug adapter exited".to_string(),
            Err(e) => break format!("the debug adapter sent an unreadable message: {e:#}"),
        }
    };
    shared.close(&why);
}

fn handle(message: &Value, shared: &Shared) {
    let str_of = |key: &str| message.get(key).and_then(Value::as_str);
    match str_of("type") {
        Some("response") => {
            let Some(seq) = message.get("request_seq").and_then(Value::as_i64) else {
                return;
            };
            let Some(waiter) = shared.pending.lock().expect("pending lock").remove(&seq) else {
                return;
            };
            let success = message.get("success").and_then(Value::as_bool) == Some(true);
            tracing::debug!(seq, success, "dap reply");
            let reply = match success {
                true => Ok(message.get("body").cloned().unwrap_or(Value::Null)),
                false => Err(failure(message)),
            };
            let _ = waiter.tx.try_send(reply);
        }
        Some("event") => {
            let body = message.get("body").cloned().unwrap_or(Value::Null);
            if let Some(event) = event(str_of("event").unwrap_or_default(), &body) {
                let _ = shared.events.send_blocking(event);
            }
        }
        // Reverse requests such as runInTerminal are refused; the adapter then runs the program.
        Some("request") => {
            let command = str_of("command").unwrap_or_default();
            let seq = shared.next_seq();
            let _ = shared.frames.send(json!({
                "seq": seq,
                "type": "response",
                "request_seq": message.get("seq").cloned().unwrap_or(Value::Null),
                "command": command,
                "success": false,
                "message": format!("Athena does not support {command}"),
            }));
        }
        _ => {}
    }
}

/// The most specific reason a failed reply gives.
fn failure(message: &Value) -> String {
    message
        .pointer("/body/error/format")
        .and_then(Value::as_str)
        .or_else(|| message.get("message").and_then(Value::as_str))
        .unwrap_or("the request failed")
        .to_string()
}

fn event(name: &str, body: &Value) -> Option<Event> {
    let text = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Some(match name {
        "initialized" => Event::Initialized,
        "stopped" => Event::Stopped(types::parse_stopped(body)),
        "continued" => Event::Continued {
            thread_id: body.get("threadId").and_then(Value::as_i64),
            all_threads: body.get("allThreadsContinued").and_then(Value::as_bool) != Some(false),
        },
        "output" => Event::Output {
            category: body
                .get("category")
                .and_then(Value::as_str)
                .unwrap_or("console")
                .to_string(),
            text: text("output"),
        },
        "breakpoint" => Event::Breakpoint {
            reason: text("reason"),
            breakpoint: types::parse_breakpoint(body.get("breakpoint")?),
        },
        "thread" => Event::Thread {
            reason: text("reason"),
            id: body.get("threadId")?.as_i64()?,
        },
        "terminated" => Event::Terminated,
        "exited" => Event::Exited {
            code: body.get("exitCode").and_then(Value::as_i64).unwrap_or(0),
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};
        struct Unpark(thread::Thread);
        impl Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(Unpark(thread::current())));
        let mut cx = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                return out;
            }
            thread::park_timeout(Duration::from_millis(50));
        }
    }

    fn write_message(out: &mut impl Write, message: &Value) {
        let body = serde_json::to_vec(message).unwrap();
        write!(out, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        out.write_all(&body).unwrap();
        out.flush().unwrap();
    }

    /// A client wired to an in-process adapter over a socket pair, without a process.
    fn connected() -> (Client, async_channel::Receiver<Event>, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (frames_tx, frames_rx) = mpsc::channel();
        let (events_tx, events_rx) = async_channel::unbounded();
        let shared = Arc::new(Shared {
            pending: Arc::default(),
            seq: AtomicI64::new(1),
            closed: AtomicBool::new(false),
            frames: frames_tx,
            events: events_tx,
        });
        start_watchdog(Arc::downgrade(&shared.pending));
        let session = shared.clone();
        let output = ours.try_clone().unwrap();
        thread::spawn(move || serve(ours, output, &session, frames_rx));
        let client = Client {
            shared,
            child: Arc::default(),
            scratch: scratch_dir().unwrap(),
            capabilities: OnceLock::new(),
        };
        (client, events_rx, theirs)
    }

    #[test]
    fn replies_match_their_request_seq_whatever_order_they_come_in() {
        let (client, events, adapter) = connected();
        let mut reader = BufReader::new(adapter.try_clone().unwrap());
        let mut writer = adapter;
        let fake = thread::spawn(move || {
            let first = read_message(&mut reader).unwrap().unwrap();
            let second = read_message(&mut reader).unwrap().unwrap();
            assert_eq!(first["command"], "threads");
            assert_eq!(second["command"], "evaluate");
            assert_eq!(second["arguments"]["frameId"], 1000);
            assert!(second["seq"].as_i64() > first["seq"].as_i64());
            // An event, a reverse request, then the replies in the opposite order.
            write_message(
                &mut writer,
                &json!({"seq": 1, "type": "event", "event": "output",
                "body": {"category": "stdout", "output": "hello\n"}}),
            );
            write_message(
                &mut writer,
                &json!({"seq": 2, "type": "request",
                "command": "runInTerminal", "arguments": {}}),
            );
            write_message(
                &mut writer,
                &json!({"seq": 3, "type": "response",
                "request_seq": second["seq"], "success": false, "command": "evaluate",
                "message": "Unable to evaluate",
                "body": {"error": {"id": 2009, "format": "could not find symbol value for x"}}}),
            );
            write_message(
                &mut writer,
                &json!({"seq": 4, "type": "response",
                "request_seq": first["seq"], "success": true, "command": "threads",
                "body": {"threads": [{"id": 1, "name": "* [Go 1] main.main"}]}}),
            );
            let refusal = read_message(&mut reader).unwrap().unwrap();
            assert_eq!(refusal["type"], "response");
            assert_eq!(refusal["request_seq"], 2);
            assert_eq!(refusal["success"], false);
            assert!(refusal["seq"].as_i64() > second["seq"].as_i64());
        });
        let threads = client.request("threads", json!({}), REQUEST_TIMEOUT);
        let evaluated = block_on(client.evaluate("x", Some(1000), "hover"));
        assert_eq!(evaluated.unwrap_err(), "could not find symbol value for x");
        let threads = block_on(threads.recv()).unwrap().unwrap();
        assert_eq!(types::parse_threads(&threads)[0].name, "* [Go 1] main.main");
        assert_eq!(
            block_on(events.recv()).unwrap(),
            Event::Output {
                category: "stdout".into(),
                text: "hello\n".into()
            }
        );
        fake.join().unwrap();
    }

    #[test]
    fn a_request_the_adapter_ignores_times_out_and_later_ones_still_work() {
        let (client, _events, adapter) = connected();
        let mut reader = BufReader::new(adapter.try_clone().unwrap());
        let mut writer = adapter;
        let fake = thread::spawn(move || {
            let _ignored = read_message(&mut reader).unwrap().unwrap();
            let answered = read_message(&mut reader).unwrap().unwrap();
            write_message(
                &mut writer,
                &json!({"seq": 1, "type": "response",
                "request_seq": answered["seq"], "success": true, "command": "threads"}),
            );
        });
        let started = Instant::now();
        let late = block_on(client.call("pause", json!({}), Duration::from_millis(300)));
        assert_eq!(late.unwrap_err(), "pause got no answer within 300 ms");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(block_on(client.threads()).unwrap(), Vec::new());
        fake.join().unwrap();
    }

    #[test]
    fn when_the_adapter_closes_waiters_and_later_requests_fail_at_once() {
        let (client, events, adapter) = connected();
        let mut reader = BufReader::new(adapter.try_clone().unwrap());
        let fake = thread::spawn(move || {
            let _ = read_message(&mut reader).unwrap().unwrap();
            adapter.shutdown(std::net::Shutdown::Both).unwrap();
        });
        let started = Instant::now();
        assert_eq!(
            block_on(client.threads()).unwrap_err(),
            "the debug adapter has exited"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "not left to time out"
        );
        assert_eq!(
            block_on(events.recv()).unwrap(),
            Event::Closed("the debug adapter exited".into())
        );
        assert!(block_on(client.next(1)).is_err());
        fake.join().unwrap();
    }

    #[test]
    fn events_are_read_into_their_kinds() {
        assert_eq!(event("initialized", &Value::Null), Some(Event::Initialized));
        assert_eq!(
            event("exited", &json!({"exitCode": 3})),
            Some(Event::Exited { code: 3 })
        );
        assert_eq!(
            event("continued", &json!({"threadId": 1})),
            Some(Event::Continued {
                thread_id: Some(1),
                all_threads: true
            })
        );
        let Some(Event::Stopped(stopped)) =
            event("stopped", &json!({"reason": "step", "threadId": 4}))
        else {
            panic!("not a stop");
        };
        assert_eq!(
            (stopped.reason.as_str(), stopped.thread_id),
            ("step", Some(4))
        );
        assert_eq!(event("module", &json!({})), None);
        assert_eq!(event("thread", &json!({"reason": "started"})), None);
    }

    fn script(name: &str, body: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("athena-dap-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("adapter");
        std::fs::write(&program, body.replace("{dir}", &dir.to_string_lossy())).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, program)
    }

    #[test]
    fn a_silent_adapter_script_times_out_and_dropping_it_kills_what_it_started() {
        let (dir, program) = script(
            "silent",
            "#!/bin/sh\nsleep 30 &\necho $! > '{dir}/helper.pid'\necho started >&2\nexec sleep 30\n",
        );
        let adapter = Adapter {
            program,
            args: Vec::new(),
            cwd: dir.clone(),
            login_shell: false,
            transport: Transport::Stdio,
        };
        let (client, events) = Client::start(adapter).unwrap();
        let scratch = client.scratch().to_path_buf();
        assert!(scratch.is_dir());
        let reply = block_on(client.call("initialize", json!({}), Duration::from_millis(400)));
        assert_eq!(reply.unwrap_err(), "initialize got no answer within 400 ms");
        assert_eq!(
            block_on(events.recv()).unwrap(),
            Event::Output {
                category: "stderr".into(),
                text: "started\n".into()
            }
        );
        let helper: i32 = std::fs::read_to_string(dir.join("helper.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let alive = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;
        assert!(alive(helper));
        drop(client);
        let dropped = Instant::now();
        while (alive(helper) || scratch.exists())
            && dropped.elapsed() < SHUTDOWN_GRACE + Duration::from_secs(3)
        {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(helper), "the adapter's own child outlived it");
        assert!(!scratch.exists(), "the scratch folder stayed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dial_in_adapter_connects_back_on_the_socket_it_is_given() {
        // nc speaks for a fake adapter: it connects, then echoes a canned initialize reply.
        let (dir, program) = script(
            "dial",
            "#!/bin/sh\nbody='{\"seq\":1,\"type\":\"response\",\"request_seq\":1,\"success\":true,\
             \"command\":\"initialize\",\"body\":{\"supportsConfigurationDoneRequest\":true}}'\n\
             (sleep 0.3; printf 'Content-Length: %s\\r\\n\\r\\n%s' \"${#body}\" \"$body\") | \
             nc -U \"$1\" > '{dir}/heard'\n",
        );
        let adapter = Adapter {
            program,
            args: vec!["{socket}".into()],
            cwd: dir.clone(),
            login_shell: false,
            transport: Transport::DialIn,
        };
        let (client, _events) = Client::start(adapter).unwrap();
        let capabilities = block_on(client.initialize("go")).unwrap();
        assert_eq!(capabilities["supportsConfigurationDoneRequest"], true);
        assert!(client.supports("supportsConfigurationDoneRequest"));
        assert!(!client.supports("supportsRestartRequest"));
        drop(client);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_adapter_that_exits_without_dialing_in_is_reported() {
        let (dir, program) = script("gone", "#!/bin/sh\nexit 3\n");
        let adapter = Adapter {
            program,
            args: Vec::new(),
            cwd: dir.clone(),
            login_shell: false,
            transport: Transport::DialIn,
        };
        let (client, events) = Client::start(adapter).unwrap();
        let closed = block_on(events.recv()).unwrap();
        let Event::Closed(why) = closed else {
            panic!("{closed:?}");
        };
        assert!(why.contains("exited before connecting"), "{why}");
        assert!(block_on(client.threads()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
