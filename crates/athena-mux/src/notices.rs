use std::collections::VecDeque;
use std::time::{Duration, Instant};

use athena_proto::NoticeKind;

use crate::scanner::OscScanner;

const MAX_TEXT: usize = 200;
/// Program-raised messages (OSC 9 / 777) allowed per pane per second.
const MESSAGE_BURST: usize = 10;

/// Turns one pane's output into notices: finished long commands and program messages.
pub struct PaneWatcher {
    scanner: OscScanner,
    running: Option<(Instant, Option<String>)>,
    recent_messages: VecDeque<Instant>,
    threshold: Duration,
}

impl PaneWatcher {
    pub fn new(threshold: Duration) -> Self {
        Self {
            scanner: OscScanner::default(),
            running: None,
            recent_messages: VecDeque::new(),
            threshold,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<NoticeKind> {
        let mut payloads = Vec::new();
        self.scanner.feed(bytes, |p| {
            payloads.push(String::from_utf8_lossy(p).into_owned())
        });
        payloads.into_iter().filter_map(|p| self.osc(&p)).collect()
    }

    fn osc(&mut self, payload: &str) -> Option<NoticeKind> {
        let mut parts = payload.splitn(3, ';');
        match (parts.next()?, parts.next(), parts.next()) {
            ("133", Some("C"), command) => {
                let command = command
                    .and_then(decode_base64)
                    .map(|c| clean(&c))
                    .filter(|c| !c.is_empty());
                self.running = Some((Instant::now(), command));
                None
            }
            ("133", Some("D"), code) => {
                let (started, command) = self.running.take()?;
                let elapsed = started.elapsed();
                (elapsed >= self.threshold).then(|| NoticeKind::CommandFinished {
                    exit_code: code.and_then(|c| c.parse().ok()).unwrap_or(0),
                    elapsed_ms: elapsed.as_millis() as u64,
                    command,
                })
            }
            // `9;4;...` is a progress report some tools send, not a message.
            ("9", Some(text), rest) if text != "4" => {
                let body = rest.map_or(text.to_string(), |r| format!("{text};{r}"));
                self.message("Terminal".into(), body)
            }
            ("777", Some("notify"), Some(rest)) => {
                let (title, body) = rest.split_once(';').unwrap_or((rest, ""));
                self.message(title.to_string(), body.to_string())
            }
            _ => None,
        }
    }

    fn message(&mut self, title: String, body: String) -> Option<NoticeKind> {
        let now = Instant::now();
        while self
            .recent_messages
            .front()
            .is_some_and(|t| now - *t > Duration::from_secs(1))
        {
            self.recent_messages.pop_front();
        }
        if self.recent_messages.len() >= MESSAGE_BURST {
            return None;
        }
        self.recent_messages.push_back(now);
        Some(NoticeKind::Message {
            title: clean(&title),
            body: clean(&body),
        })
    }
}

/// Printable text only, so a program cannot smuggle escapes into a system notification.
pub fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT)
        .collect()
}

fn decode_base64(s: &str) -> Option<String> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = Vec::new();
    for b in s.bytes().filter(|b| !b.is_ascii_whitespace() && *b != b'=') {
        bits = (bits << 6) | ALPHABET.iter().position(|a| *a == b)? as u32;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_commands_finish_with_their_text() {
        let mut w = PaneWatcher::new(Duration::ZERO);
        assert!(w.feed(b"\x1b]133;C;bnBtIHRlc3Q=\x07").is_empty());
        let notices = w.feed(b"output\x1b]133;D;1\x07\x1b]133;A\x07");
        assert!(matches!(
            &notices[..],
            [NoticeKind::CommandFinished { exit_code: 1, command: Some(c), .. }] if c == "npm test"
        ));
    }

    #[test]
    fn short_commands_stay_quiet() {
        let mut w = PaneWatcher::new(Duration::from_secs(10));
        w.feed(b"\x1b]133;C\x07");
        assert!(w.feed(b"\x1b]133;D;0\x07").is_empty());
        assert!(
            w.feed(b"\x1b]133;D;0\x07").is_empty(),
            "no finish without a start"
        );
    }

    #[test]
    fn program_messages_are_cleaned_and_rate_limited() {
        let mut w = PaneWatcher::new(Duration::ZERO);
        let n = w.feed(b"\x1b]777;notify;Build;done\x08\x7f!\x07");
        assert_eq!(
            n,
            vec![NoticeKind::Message {
                title: "Build".into(),
                body: "done!".into()
            }]
        );
        assert!(
            w.feed(b"\x1b]9;4;1;50\x07").is_empty(),
            "progress reports are not messages"
        );
        let burst: Vec<u8> = b"\x1b]9;hi\x07".repeat(20);
        assert_eq!(w.feed(&burst).len(), MESSAGE_BURST - 1);
    }

    #[test]
    fn decodes_base64() {
        assert_eq!(
            decode_base64("aGVsbG8gd29ybGQ=").as_deref(),
            Some("hello world")
        );
        assert_eq!(decode_base64("!!"), None);
    }
}
