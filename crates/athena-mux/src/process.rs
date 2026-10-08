use std::ffi::CStr;
use std::os::raw::c_void;
use std::path::PathBuf;

use athena_proto::Process;

const PATH_MAX: usize = 4 * 1024;

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
