use std::ffi::CStr;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

// Absolute only: a PATH lookup would let any earlier directory impersonate the CLI.
const CLI_PATHS: [&str; 3] = [
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/usr/local/bin/tailscale",
    "/opt/homebrew/bin/tailscale",
];

const CLI_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_CLI_OUTPUT: u64 = 1 << 20;

/// A tailnet peer as reported by `tailscale whois`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub login: String,
    pub node: String,
    pub os: String,
}

/// Whether `ip` lies in 100.64.0.0/10 or fd7a:115c:a1e0::/48.
pub fn is_tailnet(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && (64..=127).contains(&b)
        }
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// Picks the Tailscale addresses from `(interface, address)` pairs, preferring one `utun*` interface that holds both families.
pub fn pick(addrs: &[(String, IpAddr)]) -> (Option<Ipv4Addr>, Option<Ipv6Addr>) {
    let mut v4s: Vec<(&str, Ipv4Addr)> = Vec::new();
    let mut v6s: Vec<(&str, Ipv6Addr)> = Vec::new();
    for (name, ip) in addrs {
        if !name.starts_with("utun") || !is_tailnet(*ip) {
            continue;
        }
        match ip {
            IpAddr::V4(v4) => v4s.push((name, *v4)),
            IpAddr::V6(v6) => v6s.push((name, *v6)),
        }
    }
    for (name, v4) in &v4s {
        if let Some((_, v6)) = v6s.iter().find(|(n, _)| n == name) {
            return (Some(*v4), Some(*v6));
        }
    }
    // fd7a:115c:a1e0::/48 is Tailscale's alone, so its interface decides which v4 is ours.
    if let Some((name, v6)) = v6s.first() {
        let v4 = v4s.iter().find(|(n, _)| n == name).map(|(_, a)| *a);
        return (v4, Some(*v6));
    }
    (v4s.first().map(|(_, a)| *a), None)
}

/// Every interface address on this machine as `(interface, address)`.
pub fn local_addrs() -> Vec<(String, IpAddr)> {
    let mut out = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list we only read until freeifaddrs; each
    // `ifa_addr` is null-checked and cast to the struct its `sa_family` names.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() {
                continue;
            }
            let ip = match i32::from((*ifa.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)))
                }
                libc::AF_INET6 => {
                    let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr))
                }
                _ => continue,
            };
            let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
            out.push((name, ip));
        }
        libc::freeifaddrs(head);
    }
    out
}

/// The first of the known absolute Tailscale CLI paths that is a file.
pub fn cli() -> Option<PathBuf> {
    CLI_PATHS
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

/// Asks the Tailscale CLI who owns `ip`; blocks up to 3 s.
pub fn whois(cli: &Path, ip: IpAddr) -> Option<Peer> {
    let ip = ip.to_string();
    parse_whois(&run(cli, &["whois", "--json", &ip], CLI_TIMEOUT)?)
}

/// This machine's MagicDNS name without the trailing dot; blocks up to 3 s.
pub fn self_dns(cli: &Path) -> Option<String> {
    parse_self_dns(&run(cli, &["status", "--json"], CLI_TIMEOUT)?)
}

fn parse_whois(json: &str) -> Option<Peer> {
    let v: Value = serde_json::from_str(json).ok()?;
    let text = |value: Option<&Value>| value?.as_str().map(str::to_owned);
    // Every tagged node shares this login, so matching it proves nothing about the owner.
    let login = text(v.pointer("/UserProfile/LoginName"))
        .filter(|s| !s.is_empty() && s != "tagged-devices")?;
    let node = text(v.pointer("/Node/ComputedName")).filter(|s| !s.is_empty())?;
    let os = text(v.pointer("/Node/Hostinfo/OS"))?;
    Some(Peer { login, node, os })
}

fn parse_self_dns(json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    let name = v.pointer("/Self/DNSName")?.as_str()?.trim_end_matches('.');
    (!name.is_empty()).then(|| name.to_owned())
}

/// Runs `program` without a shell and returns its stdout, killing its whole process group when `timeout` passes.
fn run(program: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    let pgid = child.id() as libc::pid_t;
    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    // Never joined: a grandchild holding the pipe must not be able to block us.
    std::thread::spawn(move || {
        let mut buf = String::new();
        let read = stdout.take(MAX_CLI_OUTPUT).read_to_string(&mut buf);
        let _ = tx.send(read.ok().map(|_| buf));
    });
    let deadline = Instant::now() + timeout;
    let mut exited = false;
    while !exited && Instant::now() < deadline {
        exited = has_exited(pgid);
        if !exited {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let out = if exited {
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .ok()
    } else {
        None
    };
    // The leader is still an unreaped zombie here, so its pid cannot name an unrelated group.
    // SAFETY: killpg takes plain integers.
    unsafe { libc::killpg(pgid, libc::SIGKILL) };
    let status = child.wait().ok()?;
    status.success().then_some(out).flatten().flatten()
}

/// Whether `pid` has exited, leaving it unreaped so its pid stays reserved.
fn has_exited(pid: libc::pid_t) -> bool {
    // SAFETY: an all-zero siginfo_t is a valid value and waitid only writes into it; WNOWAIT
    // keeps the child waitable, so `Child::wait` still reaps it.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let rc = libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        );
        rc != 0 || info.si_pid != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse().unwrap())
    }

    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse().unwrap())
    }

    fn entry(name: &str, ip: IpAddr) -> (String, IpAddr) {
        (name.to_owned(), ip)
    }

    #[test]
    fn the_ipv4_tailnet_range_ends_exactly_at_its_boundaries() {
        assert!(!is_tailnet(v4("100.63.255.255")));
        assert!(is_tailnet(v4("100.64.0.0")));
        assert!(is_tailnet(v4("100.127.255.255")));
        assert!(!is_tailnet(v4("100.128.0.0")));
        assert!(!is_tailnet(v4("192.168.1.2")));
        assert!(!is_tailnet(v4("127.0.0.1")));
    }

    #[test]
    fn the_ipv6_tailnet_range_is_exactly_the_slash_48() {
        assert!(is_tailnet(v6("fd7a:115c:a1e0::1")));
        assert!(is_tailnet(v6("fd7a:115c:a1e0:ffff:ffff:ffff:ffff:ffff")));
        assert!(!is_tailnet(v6("fd7a:115c:a1e1::1")));
        assert!(!is_tailnet(v6("fd7a:115c:a1df:ffff::1")));
        assert!(!is_tailnet(v6("fd7a:115d:a1e0::1")));
        assert!(!is_tailnet(v6("::1")));
    }

    #[test]
    fn an_ipv4_mapped_address_is_not_a_tailnet_address() {
        assert!(!is_tailnet(v6("::ffff:100.64.0.1")));
    }

    #[test]
    fn an_interface_with_both_families_wins() {
        let addrs = [
            entry("utun3", v4("100.100.1.1")),
            entry("utun5", v4("100.101.2.2")),
            entry("utun5", v6("fd7a:115c:a1e0::2")),
        ];
        assert_eq!(
            pick(&addrs),
            (
                Some("100.101.2.2".parse().unwrap()),
                Some("fd7a:115c:a1e0::2".parse().unwrap())
            )
        );
    }

    #[test]
    fn a_v4_only_interface_is_accepted_when_none_has_both() {
        let addrs = [
            entry("en0", v4("192.168.1.9")),
            entry("utun4", v4("100.90.0.7")),
        ];
        assert_eq!(pick(&addrs), (Some("100.90.0.7".parse().unwrap()), None));
    }

    #[test]
    fn another_cgnat_vpn_without_ipv6_loses_to_tailscale() {
        let addrs = [
            entry("utun2", v4("100.70.0.1")),
            entry("utun6", v4("100.80.0.1")),
            entry("utun6", v6("fd7a:115c:a1e0::9")),
        ];
        assert_eq!(
            pick(&addrs),
            (
                Some("100.80.0.1".parse().unwrap()),
                Some("fd7a:115c:a1e0::9".parse().unwrap())
            )
        );
    }

    #[test]
    fn nothing_is_picked_without_a_tailnet_address_on_a_utun() {
        let addrs = [
            entry("en0", v4("100.64.0.1")),
            entry("utun1", v4("10.0.0.2")),
            entry("utun1", v6("fe80::1")),
            entry("lo0", v4("127.0.0.1")),
        ];
        assert_eq!(pick(&addrs), (None, None));
        assert_eq!(pick(&[]), (None, None));
    }

    #[test]
    fn a_whois_reply_yields_login_node_and_os() {
        let json = r#"{
            "Node": {"ComputedName": "jasons-iphone", "Hostinfo": {"OS": "iOS"}},
            "UserProfile": {"LoginName": "jason@example.com", "DisplayName": "Jason"}
        }"#;
        assert_eq!(
            parse_whois(json),
            Some(Peer {
                login: "jason@example.com".into(),
                node: "jasons-iphone".into(),
                os: "iOS".into(),
            })
        );
    }

    #[test]
    fn a_whois_reply_missing_a_field_parses_to_none() {
        let no_login = r#"{"Node":{"ComputedName":"n","Hostinfo":{"OS":"iOS"}}}"#;
        let no_node = r#"{"UserProfile":{"LoginName":"a@b"},"Node":{"Hostinfo":{"OS":"iOS"}}}"#;
        let no_os = r#"{"UserProfile":{"LoginName":"a@b"},"Node":{"ComputedName":"n"}}"#;
        let empty_login = r#"{"UserProfile":{"LoginName":""},"Node":{"ComputedName":"n","Hostinfo":{"OS":"iOS"}}}"#;
        let wrong_type = r#"{"UserProfile":{"LoginName":7},"Node":{"ComputedName":"n","Hostinfo":{"OS":"iOS"}}}"#;
        for json in [no_login, no_node, no_os, empty_login, wrong_type] {
            assert_eq!(parse_whois(json), None, "{json}");
        }
    }

    #[test]
    fn malformed_json_parses_to_none_without_panicking() {
        for json in ["", "not json", "{", "[]", "null", "{\"Self\":"] {
            assert_eq!(parse_whois(json), None, "{json}");
            assert_eq!(parse_self_dns(json), None, "{json}");
        }
    }

    #[test]
    fn the_self_dns_name_loses_its_trailing_dot() {
        let json = r#"{"Self": {"DNSName": "jasons-mac.tail1234.ts.net."}}"#;
        assert_eq!(
            parse_self_dns(json).as_deref(),
            Some("jasons-mac.tail1234.ts.net")
        );
    }

    #[test]
    fn a_status_reply_without_a_usable_dns_name_parses_to_none() {
        let replies = [
            r#"{}"#,
            r#"{"Self":{}}"#,
            r#"{"Self":{"DNSName":"."}}"#,
            r#"{"Self":{"DNSName":3}}"#,
        ];
        for json in replies {
            assert_eq!(parse_self_dns(json), None, "{json}");
        }
    }

    fn is_running(pattern: &str) -> bool {
        std::process::Command::new("/usr/bin/pgrep")
            .args(["-f", pattern])
            .output()
            .is_ok_and(|out| out.status.success())
    }

    fn assert_dies_soon(pattern: &str) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while is_running(pattern) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!is_running(pattern), "{pattern} survived run");
    }

    #[test]
    fn a_grandchild_holding_stdout_cannot_outlast_the_timeout() {
        let started = Instant::now();
        let out = run(
            Path::new("/bin/sh"),
            &["-c", "sleep 37.5; true"],
            Duration::from_millis(200),
        );
        assert_eq!(out, None);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_dies_soon("sleep 37.5");
    }

    #[test]
    fn a_grandchild_left_behind_by_an_exited_child_is_killed() {
        let started = Instant::now();
        let out = run(
            Path::new("/bin/sh"),
            &["-c", "sleep 30.25 & echo hi"],
            Duration::from_millis(200),
        );
        assert_eq!(out, None);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_dies_soon("sleep 30.25");
    }

    #[test]
    fn a_tagged_node_has_no_usable_login() {
        let json = r#"{"UserProfile":{"LoginName":"tagged-devices"},"Node":{"ComputedName":"n","Hostinfo":{"OS":"linux"}}}"#;
        assert_eq!(parse_whois(json), None);
    }

    #[test]
    fn a_v4_from_another_interface_is_not_paired_with_the_tailscale_v6() {
        let addrs = [
            entry("utun2", v4("100.70.0.1")),
            entry("utun6", v6("fd7a:115c:a1e0::9")),
        ];
        assert_eq!(
            pick(&addrs),
            (None, Some("fd7a:115c:a1e0::9".parse().unwrap()))
        );
    }

    #[test]
    fn a_child_that_outlives_the_timeout_is_killed() {
        let started = Instant::now();
        let out = run(Path::new("/bin/sleep"), &["30"], Duration::from_millis(200));
        assert_eq!(out, None);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
