//! Which processes hold macOS power assertions, such as `caffeinate` keeping the Mac awake.

use std::os::raw::c_void;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSDictionary, NSNumber};

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMCopyAssertionsByProcess(assertions_by_pid: *mut *mut c_void) -> i32;
}

/// The pids holding at least one power assertion; empty when macOS will not say.
pub fn assertion_holders() -> Vec<i32> {
    let mut raw: *mut c_void = std::ptr::null_mut();
    // SAFETY: on success the call stores a CFDictionary we own (the Copy rule), keyed by CFNumber
    // pids; CFDictionary and CFNumber are toll-free bridged to NSDictionary and NSNumber.
    let dict: Option<Retained<NSDictionary<NSNumber, AnyObject>>> = unsafe {
        if IOPMCopyAssertionsByProcess(&mut raw) != 0 {
            return Vec::new();
        }
        Retained::from_raw(raw.cast())
    };
    let Some(dict) = dict else {
        return Vec::new();
    };
    let pids = dict.allKeys();
    (0..pids.count())
        .map(|i| pids.objectAtIndex(i).intValue())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_caffeinate_shows_up_as_a_holder() {
        // Hosted CI runners may not register or report power assertions.
        if std::env::var_os("CI").is_some() {
            return;
        }
        let mut child = std::process::Command::new("/usr/bin/caffeinate")
            .args(["-i", "-t", "10"])
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let held = (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            assertion_holders().contains(&pid)
        });
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(held);
    }
}
