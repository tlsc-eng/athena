use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;

use athena_proto::AppMsg;

/// Listens on `app.sock` for folders sent by `athena <folder>`; same-user connections only.
pub fn listen() -> async_channel::Receiver<PathBuf> {
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
            for mut stream in listener.incoming().flatten() {
                if !same_user(&stream) {
                    continue;
                }
                if let Ok(Some(AppMsg::OpenProject { path })) =
                    athena_proto::read_frame(&mut stream)
                    && path.is_dir()
                    && tx.send_blocking(path).is_err()
                {
                    return;
                }
            }
        });
    rx
}

fn same_user(stream: &UnixStream) -> bool {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: getpeereid writes two integers for a connected unix socket we own.
    let ok = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0;
    // SAFETY: geteuid has no preconditions.
    ok && uid == unsafe { libc::geteuid() }
}
