//! Length-prefixed JSON frames.
//!
//! Wire format (little-endian):
//!
//! ```text
//! [u32 LE payload_len][payload_len bytes of UTF-8 JSON]
//! ```
//!
//! JSON is compact (no pretty-print). Maximum payload is [`MAX_FRAME_BYTES`]
//! so a hostile or buggy peer cannot grow RSS without bound.

use std::io::{Read, Write};

use thiserror::Error;

/// 256 KiB is far above any status/pace payload we emit.
pub const MAX_FRAME_BYTES: usize = 256 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("payload exceeds {MAX_FRAME_BYTES} bytes")]
    TooLarge,
    #[error("unexpected end of stream")]
    UnexpectedEof,
    #[error("io error: {0}")]
    Io(String),
}

impl From<std::io::Error> for FrameError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            Self::UnexpectedEof
        } else {
            Self::Io(e.to_string())
        }
    }
}

pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge);
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

pub fn decode_len(header: [u8; 4]) -> Result<usize, FrameError> {
    let n = u32::from_le_bytes(header) as usize;
    if n > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge);
    }
    Ok(n)
}

pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> Result<(), FrameError> {
    let frame = encode_frame(payload)?;
    w.write_all(&frame)?;
    w.flush()?;
    Ok(())
}

pub fn read_frame<R: Read>(r: &mut R) -> Result<Vec<u8>, FrameError> {
    let mut header = [0u8; 4];
    r.read_exact(&mut header)?;
    let n = decode_len(header)?;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip() {
        let payload = br#"{"id":1,"method":"ping"}"#;
        let mut cur = Cursor::new(encode_frame(payload).unwrap());
        let got = read_frame(&mut cur).unwrap();
        assert_eq!(got, payload);
    }

    #[test]
    fn rejects_too_large_len() {
        let mut header = (MAX_FRAME_BYTES as u32 + 1).to_le_bytes().to_vec();
        header.extend_from_slice(&[0u8; 8]);
        let mut cur = Cursor::new(header);
        assert_eq!(read_frame(&mut cur).unwrap_err(), FrameError::TooLarge);
    }

    #[test]
    fn unexpected_eof() {
        let mut cur = Cursor::new([1u8, 0, 0]);
        assert_eq!(read_frame(&mut cur).unwrap_err(), FrameError::UnexpectedEof);
    }

    #[test]
    fn encode_rejects_giant_payload() {
        let giant = vec![0u8; MAX_FRAME_BYTES + 1];
        assert_eq!(encode_frame(&giant).unwrap_err(), FrameError::TooLarge);
    }

    #[test]
    fn exact_max_frame_roundtrips() {
        let payload = vec![b'x'; MAX_FRAME_BYTES];
        let mut cur = Cursor::new(encode_frame(&payload).unwrap());
        assert_eq!(read_frame(&mut cur).unwrap(), payload);
    }

    #[test]
    fn decode_len_rejects_oversize_prefix() {
        assert_eq!(
            decode_len(((MAX_FRAME_BYTES as u32) + 1).to_le_bytes()),
            Err(FrameError::TooLarge)
        );
        assert_eq!(decode_len(0u32.to_le_bytes()).unwrap(), 0);
        assert_eq!(
            decode_len((MAX_FRAME_BYTES as u32).to_le_bytes()).unwrap(),
            MAX_FRAME_BYTES
        );
    }

    #[test]
    fn zero_length_payload() {
        let mut cur = Cursor::new(encode_frame(b"").unwrap());
        assert_eq!(read_frame(&mut cur).unwrap(), b"");
    }

    #[test]
    fn truncated_length_prefix() {
        for n in 0..4 {
            let header = [1u8, 0, 0, 0];
            let mut cur = Cursor::new(&header[..n]);
            assert_eq!(
                read_frame(&mut cur).unwrap_err(),
                FrameError::UnexpectedEof,
                "header of {n} bytes"
            );
        }
    }

    #[test]
    fn truncated_body_after_valid_prefix() {
        let mut buf = 8u32.to_le_bytes().to_vec();
        buf.extend_from_slice(b"abcd"); // 4 of 8 claimed bytes
        let mut cur = Cursor::new(buf);
        assert_eq!(read_frame(&mut cur).unwrap_err(), FrameError::UnexpectedEof);
    }
}
