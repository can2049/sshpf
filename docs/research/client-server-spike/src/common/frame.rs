//! The length-prefixed frame codec: `u32` big-endian length, then a UTF-8 TOML
//! document.
//!
//! What belongs here: writing and reading one frame, and the one hard limit that
//! protects the reader. What does not: what the payload means.
//!
//! The length limit is load-bearing. Without it, a single corrupted or hostile
//! length field makes the peer allocate that many bytes before it can discover
//! the frame is nonsense. `MAX_FRAME` turns that into a clean disconnect.

use std::io::{self, Read, Write};

/// Largest frame accepted, in bytes.
///
/// The real measurement (see `codec`) puts a 50-tunnel snapshot near 19 KB, so a
/// megabyte leaves generous headroom while keeping the allocation bounded.
pub const MAX_FRAME: usize = 1 << 20;

/// Write one frame: length then payload.
pub fn write_frame<W: Write>(w: &mut W, payload: &str) -> io::Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame length overflow"))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(payload.as_bytes())?;
    w.flush()
}

/// Read one frame. `Ok(None)` means a clean end of stream before any byte.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<String>> {
    let mut header = [0u8; 4];
    match read_exact_or_eof(r, &mut header)? {
        false => return Ok(None),
        true => {}
    }
    let len = u32::from_be_bytes(header) as usize;
    // Two distinct rejections: an empty frame is meaningless, and an oversized
    // one cannot be trusted, so neither is read into memory.
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad frame length {len}")));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf).map(Some).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame is not UTF-8"))
}

/// Fill `buf`, reporting `false` when the stream ended before the first byte.
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(false);
                }
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated frame header"));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_a_frame() {
        let mut buf = Vec::new();
        write_frame(&mut buf, "t = \"x\"\n").unwrap();
        let back = read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(back.as_deref(), Some("t = \"x\"\n"));
    }

    #[test]
    fn clean_eof_is_none_not_an_error() {
        let mut empty: &[u8] = &[];
        assert_eq!(read_frame(&mut empty).unwrap(), None);
    }

    #[test]
    fn a_zero_length_frame_is_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_be_bytes());
        assert!(read_frame(&mut buf.as_slice()).is_err());
    }

    #[test]
    fn an_oversized_length_is_rejected_before_allocating() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes());
        let err = read_frame(&mut buf.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
