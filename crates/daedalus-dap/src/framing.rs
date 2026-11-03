use std::io::{BufRead, Write};
use std::sync::mpsc;

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

/// Larger messages are refused rather than buffered, as a corrupt length would ask for gigabytes.
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// Writes queued messages in order until the adapter stops reading or every sender is gone.
pub(crate) fn write_frames(mut out: impl Write, frames: mpsc::Receiver<Value>) {
    for message in frames {
        let Ok(body) = serde_json::to_vec(&message) else {
            continue;
        };
        let written = write!(out, "Content-Length: {}\r\n\r\n", body.len())
            .and_then(|()| out.write_all(&body))
            .and_then(|()| out.flush());
        if written.is_err() {
            return;
        }
    }
}

/// The next message, or `None` once the adapter has closed the stream.
pub(crate) fn read_message(input: &mut impl BufRead) -> Result<Option<Value>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.context("message without Content-Length")?;
    if length > MAX_MESSAGE {
        bail!("message of {length} bytes is too large");
    }
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::BufReader;

    #[test]
    fn frames_round_trip_with_their_byte_length_and_extra_headers() {
        let (tx, rx) = mpsc::channel();
        tx.send(json!({"seq": 1, "type": "request", "command": "threads"}))
            .unwrap();
        tx.send(json!({"seq": 2, "type": "event", "event": "output", "body": {"output": "é\n"}}))
            .unwrap();
        drop(tx);
        let mut out = Vec::new();
        write_frames(&mut out, rx);
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.starts_with("Content-Length: 46\r\n\r\n{"), "{text}");

        let mut with_type =
            b"Content-Type: application/json\r\ncontent-length: 2\r\n\r\n{}".to_vec();
        with_type.extend_from_slice(&out);
        let mut input = BufReader::new(&with_type[..]);
        assert_eq!(read_message(&mut input).unwrap(), Some(json!({})));
        assert_eq!(read_message(&mut input).unwrap().unwrap()["seq"], 1);
        let second = read_message(&mut input).unwrap().unwrap();
        assert_eq!(second["body"]["output"], "é\n");
        assert_eq!(read_message(&mut input).unwrap(), None);
    }

    #[test]
    fn a_frame_without_length_or_too_large_is_an_error() {
        let mut input = BufReader::new(&b"X-Other: 1\r\n\r\n{}"[..]);
        assert!(read_message(&mut input).is_err());
        let huge = format!("Content-Length: {}\r\n\r\n", MAX_MESSAGE + 1);
        let mut input = BufReader::new(huge.as_bytes());
        assert!(read_message(&mut input).is_err());
    }
}
