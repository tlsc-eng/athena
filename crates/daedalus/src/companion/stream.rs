//! Bridges panes a phone attached to the session daemon, one daemon connection per pane.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self as std_mpsc, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use daedalus_proto as mux;
use tokio::sync::{Notify, mpsc};

use super::protocol::{self, DropReason, Kind, MAX_PAYLOAD, ServerMsg};

const MAX_TAIL: usize = 1 << 20;
const MAX_ATTACHED: usize = 8;
const MAX_OUTSTANDING: usize = 2 << 20;
const QUEUE: usize = 1024;
const INPUT_CHUNK: usize = 4096;
const INPUT_QUEUE: usize = 256;

/// What the connection loop writes to the phone next.
pub(super) enum Out {
    Msg(ServerMsg),
    /// A binary frame; the charge is released once the frame has been written.
    Frame(Vec<u8>, Charge),
}

/// Bytes queued for the phone and not yet written, released on drop.
pub(super) struct Charge {
    outstanding: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.outstanding.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}

enum Event {
    Msg(ServerMsg),
    Frame(Vec<u8>, Charge),
}

/// Shared between the async side and the threads of one attachment.
#[derive(Default)]
struct Control {
    stopped: AtomicBool,
    lagging: AtomicBool,
    stream: Mutex<Option<UnixStream>>,
    input: Mutex<Option<SyncSender<Vec<u8>>>>,
}

impl Control {
    /// Takes a handle to hang up from; false when the attachment was stopped before it connected.
    fn adopt(&self, stream: UnixStream) -> bool {
        let mut slot = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        if self.stopped.load(Ordering::SeqCst) {
            return false;
        }
        *slot = Some(stream);
        true
    }

    /// False when the input queue is full.
    fn queue_input(&self, chunk: Vec<u8>) -> bool {
        let input = self.input.lock().unwrap_or_else(|e| e.into_inner());
        !matches!(
            input.as_ref().map(|tx| tx.try_send(chunk)),
            Some(Err(TrySendError::Full(_)))
        )
    }

    // Shuts down a clone of the read half: `Connection::close` would wait on the writer lock
    // that a write to a stalled daemon holds.
    fn stop(&self) {
        let mut slot = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(stream) = slot.take() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        // Ends the writer thread, which holds the last handle that keeps the daemon's socket open.
        self.input.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
}

struct Attachment {
    control: Arc<Control>,
    events: mpsc::Receiver<Event>,
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.control.stop();
    }
}

/// The panes one WebSocket connection has attached; dropping it hangs up on the daemon.
pub(super) struct Attachments {
    mux_socket: PathBuf,
    live: HashMap<u64, Attachment>,
    cursor: usize,
    lagged: Arc<Notify>,
}

impl Attachments {
    pub(super) fn new(mux_socket: PathBuf) -> Self {
        Self {
            mux_socket,
            live: HashMap::new(),
            cursor: 0,
            lagged: Arc::new(Notify::new()),
        }
    }

    /// Starts streaming `pane`, replacing an earlier attachment of it.
    pub(super) fn attach(&mut self, pane: u64, tail: u64) -> Result<(), &'static str> {
        if !self.live.contains_key(&pane) && self.live.len() >= MAX_ATTACHED {
            return Err("too many panes attached");
        }
        self.live.remove(&pane);
        let control = Arc::new(Control::default());
        let (input, queued) = std_mpsc::sync_channel(INPUT_QUEUE);
        *control.input.lock().unwrap_or_else(|e| e.into_inner()) = Some(input);
        let (events, queue) = mpsc::channel(QUEUE);
        let sink = Sink {
            pane,
            events,
            outstanding: Arc::default(),
            control: control.clone(),
            lagged: self.lagged.clone(),
        };
        let socket = self.mux_socket.clone();
        let tail = usize::try_from(tail).map_or(MAX_TAIL, |t| t.min(MAX_TAIL));
        std::thread::Builder::new()
            .name("companion-mux".into())
            .spawn(move || {
                stream_pane(&socket, tail, &sink, queued);
                sink.control.stop();
            })
            .map_err(|_| "could not attach")?;
        self.live.insert(
            pane,
            Attachment {
                control,
                events: queue,
            },
        );
        Ok(())
    }

    pub(super) fn detach(&mut self, pane: u64) {
        self.live.remove(&pane);
    }

    /// Queues keystrokes for `pane`; false when its input queue overflowed and it was dropped.
    pub(super) fn input(&mut self, pane: u64, data: &[u8]) -> bool {
        let Some(attachment) = self.live.get(&pane) else {
            return true;
        };
        let overflowed = !data
            .chunks(INPUT_CHUNK)
            .all(|chunk| attachment.control.queue_input(chunk.to_vec()));
        if overflowed {
            self.live.remove(&pane);
        }
        !overflowed
    }

    /// The next thing to tell the phone; safe to cancel.
    pub(super) async fn next(&mut self) -> Out {
        loop {
            let lagging = self
                .live
                .iter()
                .find(|(_, a)| a.control.lagging.load(Ordering::SeqCst))
                .map(|(&pane, _)| pane);
            if let Some(pane) = lagging {
                self.live.remove(&pane);
                return Out::Msg(ServerMsg::Dropped {
                    pane,
                    reason: DropReason::Lagging,
                });
            }
            let polled = std::future::poll_fn(|cx| {
                let mut panes: Vec<u64> = self.live.keys().copied().collect();
                panes.sort_unstable();
                if !panes.is_empty() {
                    let by = self.cursor % panes.len();
                    panes.rotate_left(by);
                }
                for pane in panes {
                    let Some(attachment) = self.live.get_mut(&pane) else {
                        continue;
                    };
                    if let Poll::Ready(Some(event)) = attachment.events.poll_recv(cx) {
                        self.cursor = self.cursor.wrapping_add(1);
                        return Poll::Ready((pane, event));
                    }
                }
                Poll::Pending
            });
            tokio::select! {
                (pane, event) = polled => return self.deliver(pane, event),
                () = self.lagged.notified() => {}
            }
        }
    }

    fn deliver(&mut self, pane: u64, event: Event) -> Out {
        match event {
            Event::Frame(bytes, charge) => Out::Frame(bytes, charge),
            Event::Msg(message) => {
                if matches!(
                    message,
                    ServerMsg::Exited { .. } | ServerMsg::Dropped { .. }
                ) {
                    self.live.remove(&pane);
                }
                Out::Msg(message)
            }
        }
    }
}

/// The thread side of one attachment; it never blocks on the async side.
struct Sink {
    pane: u64,
    events: mpsc::Sender<Event>,
    outstanding: Arc<AtomicUsize>,
    control: Arc<Control>,
    lagged: Arc<Notify>,
}

impl Sink {
    fn message(&self, message: ServerMsg) -> bool {
        self.push(Event::Msg(message))
    }

    /// Silent when the attachment was stopped on purpose.
    fn closed(&self) {
        if !self.control.stopped() {
            self.message(ServerMsg::Dropped {
                pane: self.pane,
                reason: DropReason::Closed,
            });
        }
    }

    fn output(&self, data: &[u8]) -> bool {
        data.chunks(MAX_PAYLOAD).all(|chunk| {
            let frame = protocol::frame(Kind::Output, self.pane, chunk);
            let bytes = frame.len();
            let held = self.outstanding.fetch_add(bytes, Ordering::SeqCst) + bytes;
            let charge = Charge {
                outstanding: self.outstanding.clone(),
                bytes,
            };
            if held > MAX_OUTSTANDING {
                self.lag();
                return false;
            }
            self.push(Event::Frame(frame, charge))
        })
    }

    fn push(&self, event: Event) -> bool {
        let sent = self.events.try_send(event).is_ok();
        if !sent {
            self.lag();
        }
        sent
    }

    // The queue may be full, so the phone learns of this through the flag and the notify.
    fn lag(&self) {
        self.control.lagging.store(true, Ordering::SeqCst);
        self.control.stop();
        self.lagged.notify_one();
    }
}

fn stream_pane(socket: &Path, tail: usize, sink: &Sink, queued: std_mpsc::Receiver<Vec<u8>>) {
    let pane = sink.pane;
    let Ok((conn, mut stream)) = mux::connect(socket) else {
        sink.closed();
        return;
    };
    let Ok(hangup) = stream.try_clone() else {
        sink.closed();
        return;
    };
    if !sink.control.adopt(hangup) {
        return;
    }
    if conn.send(&mux::ClientMsg::Attach { pane }).is_err() {
        sink.closed();
        return;
    }
    let control = sink.control.clone();
    let writer = std::thread::Builder::new()
        .name("companion-mux-input".into())
        .spawn(move || {
            for data in queued {
                if control.stopped() || conn.send(&mux::ClientMsg::Input { pane, data }).is_err() {
                    break;
                }
            }
        });
    if writer.is_err() {
        sink.closed();
        return;
    }
    let mut replay = Some(Replay::new(tail));
    let mut attached = false;
    loop {
        let frame: std::io::Result<Option<mux::ServerMsg>> = mux::read_frame(&mut stream);
        let Ok(Some(message)) = frame else {
            sink.closed();
            return;
        };
        let delivered = match message {
            mux::ServerMsg::Attached { rows, cols, .. } => {
                attached = true;
                sink.message(ServerMsg::Attached { pane, rows, cols })
            }
            mux::ServerMsg::Output { data, .. } => match replay.as_mut() {
                Some(replay) => {
                    replay.push(&data);
                    true
                }
                None => sink.output(&data),
            },
            mux::ServerMsg::ReplayDone { .. } => match replay.take() {
                Some(replay) => {
                    sink.output(replay.finish()) && sink.message(ServerMsg::ReplayDone { pane })
                }
                None => true,
            },
            mux::ServerMsg::Exited { code, .. } => {
                sink.message(ServerMsg::Exited { pane, code });
                return;
            }
            // A refusal such as an unknown pane would otherwise leave the phone waiting.
            mux::ServerMsg::Error { .. } if !attached => {
                sink.closed();
                return;
            }
            _ => true,
        };
        if !delivered {
            return;
        }
    }
}

/// Collects a replay, keeping no more than `trim` needs.
struct Replay {
    tail: usize,
    buf: Vec<u8>,
}

impl Replay {
    fn new(tail: usize) -> Self {
        Self {
            tail,
            buf: Vec::new(),
        }
    }

    // One byte beyond the tail stays, so `finish` can tell the replay was cut.
    fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        let keep = self.tail + 1;
        if self.buf.len() >= 2 * keep {
            self.buf.drain(..self.buf.len() - keep);
        }
    }

    fn finish(&self) -> &[u8] {
        trim(&self.buf, self.tail)
    }
}

/// The newest `max` bytes at most, starting at a line when older bytes are cut off.
fn trim(all: &[u8], max: usize) -> &[u8] {
    if all.len() <= max {
        return all;
    }
    let cut = &all[all.len() - max..];
    match cut.iter().position(|&b| b == b'\n') {
        Some(newline) => &cut[newline + 1..],
        None => cut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replay_under_the_limit_is_kept_whole() {
        assert_eq!(trim(b"one\ntwo\n", 100), b"one\ntwo\n");
    }

    #[test]
    fn a_replay_exactly_at_the_limit_is_kept_whole() {
        assert_eq!(trim(b"one\ntwo\n", 8), b"one\ntwo\n");
    }

    #[test]
    fn a_cut_replay_starts_after_the_first_newline() {
        assert_eq!(trim(b"aaaa\nbbbb\ncccc\n", 10), b"cccc\n");
    }

    #[test]
    fn a_cut_replay_without_a_newline_is_kept_as_cut() {
        assert_eq!(trim(b"abcdefghij", 4), b"ghij");
    }

    #[test]
    fn collecting_a_replay_in_pieces_trims_like_the_whole() {
        let all: Vec<u8> = (0..5000u32)
            .flat_map(|i| format!("line {i}\n").into_bytes())
            .collect();
        for tail in [0, 1, 7, 100, 4096, all.len(), all.len() + 1] {
            let mut replay = Replay::new(tail);
            for piece in all.chunks(333) {
                replay.push(piece);
            }
            assert_eq!(replay.finish(), trim(&all, tail), "tail {tail}");
        }
    }
}
