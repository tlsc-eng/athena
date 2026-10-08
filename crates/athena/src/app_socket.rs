use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use athena_proto::{AppMsg, AppReply, PaneId};

use crate::procinfo;

/// How long a client waits for the window; long enough for a person to answer a confirm.
const REPLY_TIMEOUT: Duration = Duration::from_secs(75);

/// One request from `athena` or the MCP bridge, answered on the window's thread.
pub struct Request {
    pub msg: AppMsg,
    /// The pane the client says it runs in, still to be checked against `lineage`.
    pub claimed: Option<PaneId>,
    /// The client process and its ancestors.
    pub lineage: Vec<i32>,
    pub reply: mpsc::SyncSender<AppReply>,
}

/// Listens on `app.sock`; same-user connections only.
pub fn listen() -> async_channel::Receiver<Request> {
    let (tx, rx) = async_channel::unbounded();
    let Ok(path) = athena_proto::app_socket_path() else {
        return rx;
    };
    // A live window answers a connect; only then is the socket someone else's.
    if UnixStream::connect(&path).is_ok() {
        return rx;
    }
    let _ = fs::remove_file(&path);
    let Ok(listener) = UnixListener::bind(&path) else {
        return rx;
    };
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    let _ = thread::Builder::new()
        .name("app-socket".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                if !same_user(&stream) {
                    continue;
                }
                let tx = tx.clone();
                let _ = thread::Builder::new()
                    .name("app-client".into())
                    .spawn(move || serve(stream, tx));
            }
        });
    rx
}

fn serve(mut stream: UnixStream, tx: async_channel::Sender<Request>) {
    let lineage = procinfo::peer_lineage(&stream);
    let mut claimed = None;
    while let Ok(Some(msg)) = athena_proto::read_frame::<_, AppMsg>(&mut stream) {
        if let AppMsg::Identify { session } = msg {
            claimed = Some(session);
            if athena_proto::write_frame(&mut stream, &AppReply::Ok).is_err() {
                return;
            }
            continue;
        }
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let request = Request {
            msg,
            claimed,
            lineage: lineage.clone(),
            reply: reply_tx,
        };
        if tx.send_blocking(request).is_err() {
            return;
        }
        let reply = reply_rx
            .recv_timeout(REPLY_TIMEOUT)
            .unwrap_or_else(|_| AppReply::Error("Athena did not answer in time".into()));
        if athena_proto::write_frame(&mut stream, &reply).is_err() {
            return;
        }
    }
}

fn same_user(stream: &UnixStream) -> bool {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: getpeereid writes two integers for a connected unix socket we own.
    let ok = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0;
    // SAFETY: geteuid has no preconditions.
    ok && uid == unsafe { libc::geteuid() }
}
