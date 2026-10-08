use std::ffi::CStr;
use std::os::raw::c_void;
use std::path::PathBuf;

use athena_proto::Process;

const PATH_MAX: usize = 4 * 1024;
/// `proc_listpids` filter for processes whose controlling terminal is a given device.
const PROC_TTY_ONLY: u32 = 3;

/// Name, executable and working directory of `pid`, or `None` if it has already exited.
pub fn describe(pid: i32) -> Option<Process> {
    let path = pidpath(pid)?;
    let name = name(pid).unwrap_or_else(|| {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    Some(Process {
        pid,
        name,
        path,
        cwd: cwd(pid),
    })
}

/// Processes whose controlling terminal is the device `tty`: a pane's shell, its jobs and their children.
pub fn on_tty(tty: u32) -> Vec<i32> {
    // SAFETY: a null buffer only asks for the size needed.
    let bytes = unsafe { libc::proc_listpids(PROC_TTY_ONLY, tty, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Vec::new();
    }
    // Room for processes started between the two calls.
    let mut pids = vec![0i32; bytes as usize / size_of::<i32>() + 32];
    // SAFETY: the buffer's length in bytes is passed alongside it.
    let bytes = unsafe {
        libc::proc_listpids(
            PROC_TTY_ONLY,
            tty,
            pids.as_mut_ptr() as *mut c_void,
            (pids.len() * size_of::<i32>()) as i32,
        )
    };
    pids.truncate(bytes.max(0) as usize / size_of::<i32>());
    pids.retain(|&pid| pid > 0);
    pids
}

/// Sends `signal` to every process on `tty`; returns how many there were.
pub fn signal_tty(tty: u32, signal: i32) -> usize {
    let pids = on_tty(tty);
    for &pid in &pids {
        // SAFETY: plain kill(2); the terminal is still held open, so these pids belong to it.
        unsafe { libc::kill(pid, signal) };
    }
    pids.len()
}

fn pidpath(pid: i32) -> Option<PathBuf> {
    let mut buf = vec![0u8; PATH_MAX];
    // SAFETY: the buffer is PATH_MAX bytes and its length is passed alongside it.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    (n > 0).then(|| PathBuf::from(String::from_utf8_lossy(&buf[..n as usize]).into_owned()))
}

fn name(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; 256];
    // SAFETY: as above.
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

fn cwd(pid: i32) -> Option<PathBuf> {
    // SAFETY: zeroed is a valid bit pattern for this plain C struct.
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
    // SAFETY: the kernel writes at most `size` bytes into `info`.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut c_void,
            size,
        )
    };
    if n != size {
        return None;
    }
    // SAFETY: vip_path is a NUL-terminated C string filled by the kernel.
    let path = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr() as *const _) };
    Some(PathBuf::from(path.to_string_lossy().into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_self() {
        let me = describe(std::process::id() as i32).expect("own process");
        assert!(me.path.is_absolute());
        assert_eq!(me.cwd, std::env::current_dir().ok());
    }
}
