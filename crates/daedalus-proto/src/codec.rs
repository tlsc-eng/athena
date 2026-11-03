use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Frames above this are rejected before allocating, so a bad peer cannot exhaust memory.
pub const MAX_FRAME: usize = 1024 * 1024;

pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = postcard::to_stdvec(msg).map_err(io::Error::other)?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&frame)?;
    w.flush()
}

/// Returns `Ok(None)` on a clean EOF between frames.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    postcard::from_bytes(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientMsg, ServerMsg};

    #[test]
    fn round_trip() {
        let mut buf = Vec::new();
        let msgs = [
            ClientMsg::Hello { proto: 1 },
            ClientMsg::Input {
                pane: 7,
                data: b"ls\r".to_vec(),
            },
            ClientMsg::Spawn {
                cwd: "/tmp".into(),
                rows: 24,
                cols: 80,
            },
        ];
        for m in &msgs {
            write_frame(&mut buf, m).unwrap();
        }
        let mut r = buf.as_slice();
        for m in &msgs {
            assert_eq!(
                read_frame::<_, ClientMsg>(&mut r).unwrap().as_ref(),
                Some(m)
            );
        }
        assert_eq!(read_frame::<_, ClientMsg>(&mut r).unwrap(), None);
    }

    #[test]
    fn rejects_oversized_length_before_reading_body() {
        let mut buf = ((MAX_FRAME + 1) as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(&[0; 8]);
        let err = read_frame::<_, ServerMsg>(&mut buf.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn truncated_frame_is_an_error() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &ClientMsg::ListPanes).unwrap();
        buf.extend_from_slice(&10u32.to_le_bytes());
        buf.extend_from_slice(&[1, 2]);
        let mut r = buf.as_slice();
        assert!(read_frame::<_, ClientMsg>(&mut r).unwrap().is_some());
        assert!(read_frame::<_, ClientMsg>(&mut r).is_err());
    }
}
