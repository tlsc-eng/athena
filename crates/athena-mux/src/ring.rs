use std::collections::VecDeque;

/// Fixed-capacity byte history; the oldest bytes fall off the front.
pub struct Ring {
    buf: VecDeque<u8>,
    cap: usize,
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            cap,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        let bytes = &bytes[bytes.len().saturating_sub(self.cap)..];
        let overflow = (self.buf.len() + bytes.len()).saturating_sub(self.cap);
        self.buf.drain(..overflow);
        self.buf.extend(bytes);
    }

    /// The newest `max` bytes at most, starting at a line when older bytes are cut off.
    pub fn tail(&mut self, max: usize) -> &[u8] {
        let all = self.buf.make_contiguous();
        if all.len() <= max {
            return all;
        }
        let cut = &all[all.len() - max..];
        match cut.iter().position(|&b| b == b'\n') {
            Some(newline) => &cut[newline + 1..],
            None => cut,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contents(r: &mut Ring) -> Vec<u8> {
        r.tail(usize::MAX).to_vec()
    }

    #[test]
    fn keeps_newest_bytes() {
        let mut r = Ring::new(5);
        r.push(b"abc");
        r.push(b"defg");
        assert_eq!(contents(&mut r), b"cdefg");
        r.push(b"0123456789");
        assert_eq!(contents(&mut r), b"56789");
    }

    #[test]
    fn a_tail_starts_after_the_first_cut_line() {
        let mut r = Ring::new(64);
        r.push(b"one\ntwo\nthree\n");
        assert_eq!(r.tail(9), b"three\n");
        assert_eq!(r.tail(4), b"");
        assert_eq!(r.tail(100), b"one\ntwo\nthree\n");
    }
}
