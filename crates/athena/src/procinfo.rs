//! Who is on the other end of `app.sock`, from the process tree rather than the client's word.

use std::os::fd::AsRawFd;
use std::os::raw::c_void;
use std::os::unix::net::UnixStream;

const MAX_DEPTH: usize = 64;

/// The pid of the process that opened this connection.
pub fn peer_pid(stream: &UnixStream) -> Option<i32> {
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: LOCAL_PEERPID writes one pid_t into the buffer we pass with its length.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            &mut pid as *mut _ as *mut c_void,
            &mut len,
        )
    };
    (rc == 0 && pid > 0).then_some(pid)
}

fn parent(pid: i32) -> Option<i32> {
    // SAFETY: zeroed is valid for this plain C struct; the kernel fills at most `size` bytes.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut c_void,
            size,
        )
    };
    (n == size).then_some(info.pbi_ppid as i32)
}

/// `pid` and every process above it, nearest first, stopping before launchd.
pub fn lineage(pid: i32, parent: impl Fn(i32) -> Option<i32>) -> Vec<i32> {
    let mut out = vec![pid];
    let mut at = pid;
    while out.len() < MAX_DEPTH {
        match parent(at) {
            Some(up) if up > 1 => {
                out.push(up);
                at = up;
            }
            _ => break,
        }
    }
    out
}

/// `pid` and its ancestors, nearest first.
pub fn ancestry(pid: i32) -> Vec<i32> {
    lineage(pid, parent)
}

/// The connecting process and its ancestors.
pub fn peer_lineage(stream: &UnixStream) -> Vec<i32> {
    peer_pid(stream)
        .map(|pid| lineage(pid, parent))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn walks_up_to_launchd() {
        // mcp(40) -> claude(30) -> zsh(20) -> athena-mux(10) -> launchd(1)
        let tree: HashMap<i32, i32> = [(40, 30), (30, 20), (20, 10), (10, 1)].into();
        assert_eq!(lineage(40, |p| tree.get(&p).copied()), vec![40, 30, 20, 10]);
    }

    #[test]
    fn stops_on_cycles_and_unknown_parents() {
        let tree: HashMap<i32, i32> = [(5, 6), (6, 5)].into();
        assert_eq!(lineage(5, |p| tree.get(&p).copied()).len(), MAX_DEPTH);
        assert_eq!(lineage(9, |_| None), vec![9]);
    }

    #[test]
    fn own_lineage_reaches_a_parent() {
        assert!(lineage(std::process::id() as i32, parent).len() >= 2);
    }
}
