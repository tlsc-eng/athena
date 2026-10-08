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

    pub fn chunks(&mut self, size: usize) -> impl Iterator<Item = &[u8]> {
        self.buf.make_contiguous().chunks(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contents(r: &mut Ring) -> Vec<u8> {
        r.chunks(3).flatten().copied().collect()
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
}
