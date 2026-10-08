/// Finds OSC sequences (`ESC ] ... BEL` or `ESC ] ... ESC \`) in a byte stream split at arbitrary
/// points, keeping state between chunks.
#[derive(Default)]
pub struct OscScanner {
    state: State,
    buf: Vec<u8>,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum State {
    #[default]
    Ground,
    Escape,
    Body,
    BodyEscape,
}

/// Longest OSC payload kept; longer ones (inline images and the like) are skipped.
const MAX_PAYLOAD: usize = 4096;

impl OscScanner {
    pub fn feed(&mut self, bytes: &[u8], mut found: impl FnMut(&[u8])) {
        for &b in bytes {
            self.state = match (self.state, b) {
                (State::Ground, 0x1b) => State::Escape,
                (State::Ground, _) => State::Ground,
                (State::Escape, b']') => {
                    self.buf.clear();
                    State::Body
                }
                (State::Escape, 0x1b) => State::Escape,
                (State::Escape, _) => State::Ground,
                (State::Body, 0x07) => {
                    found(&self.buf);
                    State::Ground
                }
                (State::Body, 0x1b) => State::BodyEscape,
                (State::Body, _) => {
                    if self.buf.len() < MAX_PAYLOAD {
                        self.buf.push(b);
                    }
                    State::Body
                }
                (State::BodyEscape, b'\\') => {
                    found(&self.buf);
                    State::Ground
                }
                (State::BodyEscape, b']') => {
                    self.buf.clear();
                    State::Body
                }
                (State::BodyEscape, _) => State::Ground,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Vec<String> {
        let mut s = OscScanner::default();
        let mut out = Vec::new();
        for c in chunks {
            s.feed(c, |p| out.push(String::from_utf8_lossy(p).into_owned()));
        }
        out
    }

    #[test]
    fn finds_both_terminators() {
        assert_eq!(
            scan(&[b"a\x1b]133;A\x07b\x1b]9;hi\x1b\\c"]),
            vec!["133;A", "9;hi"]
        );
    }

    #[test]
    fn survives_chunk_boundaries() {
        assert_eq!(
            scan(&[b"x\x1b", b"]133;D;", b"0\x1b", b"\\"]),
            vec!["133;D;0"]
        );
    }

    #[test]
    fn ignores_other_escapes_and_caps_payloads() {
        assert_eq!(scan(&[b"\x1b[31mred\x1b[0m"]), Vec::<String>::new());
        let big = [b"\x1b]".as_slice(), &vec![b'x'; 10_000], b"\x07"].concat();
        assert_eq!(scan(&[&big])[0].len(), MAX_PAYLOAD);
    }
}
