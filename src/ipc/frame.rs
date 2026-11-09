//! Length-prefixed framing: a `u32` big-endian byte count, then that many bytes
//! of UTF-8 TOML.
//!
//! # Why this shape
//!
//! A stream socket has no message boundaries. Without an explicit length the
//! reader cannot tell where one message ends, so every message would need a
//! self-delimiting encoding (and TOML is not one). A `u32` prefix is two
//! instructions on both ends and makes a multi-frame batch trivial.
//!
//! Big-endian because it is the network byte order: a frame written on one
//! architecture reads the same on another, with no configuration.
//!
//! # The limit is load-bearing
//!
//! [`MAX_FRAME`] exists so a corrupted or hostile length field cannot make the
//! reader allocate an arbitrary amount of memory before it learns the frame is
//! nonsense. A declared length above the limit is rejected **without reading the
//! body**, because once the length cannot be trusted the byte stream has no
//! meaningful position to resynchronise from.
//!
//! Belongs here: writing/reading one frame, the size limit, and the errors those
//! two operations can produce.
//! Does not belong: what the payload means (`msg`), how a payload is encoded
//! (`codec`), or where the socket lives (`endpoint`).

use std::io::{self, Read, Write};

/// Largest frame accepted, in bytes.
///
/// A 50-tunnel snapshot measures ~94 KiB with its log tail, so 256 KiB leaves
/// room for growth while bounding a single allocation. Raising this is a
/// protocol change: an older peer would reject frames a newer one sends.
pub const MAX_FRAME: usize = 256 * 1024;

/// Write one frame: the length, then the payload.
///
/// The length is checked before anything is written, so a rejected frame leaves
/// the stream untouched rather than half-written.
pub fn write_frame<W: Write>(w: &mut W, payload: &str) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| invalid("payload length overflows u32"))?;
    if payload.len() > MAX_FRAME {
        return Err(invalid("payload exceeds MAX_FRAME"));
    }
    w.write_all(&len.to_be_bytes())?;
    w.write_all(payload.as_bytes())?;
    // Flush so a caller that writes one message and then waits is not stuck
    // behind a buffer that never reaches the peer.
    w.flush()
}

/// Read one frame.
///
/// `Ok(None)` means the peer closed the connection cleanly **before** any byte of
/// the next frame arrived — the normal end of a session, not an error. A partial
/// header or body is `UnexpectedEof`, because a truncated frame means the stream
/// can no longer be trusted.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<String>> {
    let mut header = [0u8; 4];
    if !read_exact_or_eof(r, &mut header)? {
        return Ok(None);
    }
    let len = u32::from_be_bytes(header) as usize;
    // Reject an unusable length before allocating anything for it.
    if len == 0 {
        return Err(invalid("zero-length frame"));
    }
    if len > MAX_FRAME {
        return Err(invalid("frame length exceeds MAX_FRAME"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    String::from_utf8(body).map(Some).map_err(|_| invalid("frame payload is not UTF-8"))
}

/// Fill `buf` completely, reporting `false` only when the stream ended before the
/// first byte (a clean close at a frame boundary).
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                return if filled == 0 {
                    Ok(false)
                } else {
                    Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated frame header"))
                };
            }
            Ok(n) => filled += n,
            // A signal interrupted the read; not a failure.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// A protocol violation, as opposed to an I/O failure.
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode into a byte buffer, the way a socket would carry it.
    fn encode(payload: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        write_frame(&mut buf, payload).expect("writing to a Vec cannot fail");
        buf
    }

    #[test]
    fn a_frame_round_trips() {
        let buf = encode("t = \"hello\"\n");
        let back = read_frame(&mut buf.as_slice()).expect("valid frame");
        assert_eq!(back.as_deref(), Some("t = \"hello\"\n"));
    }

    #[test]
    fn several_frames_read_back_in_order() {
        let mut buf = Vec::new();
        write_frame(&mut buf, "a = 1\n").expect("first frame");
        write_frame(&mut buf, "b = 2\n").expect("second frame");
        let mut cursor = buf.as_slice();
        assert_eq!(read_frame(&mut cursor).expect("frame 1").as_deref(), Some("a = 1\n"));
        assert_eq!(read_frame(&mut cursor).expect("frame 2").as_deref(), Some("b = 2\n"));
        // Then a clean end.
        assert_eq!(read_frame(&mut cursor).expect("end"), None);
    }

    #[test]
    fn a_clean_close_at_a_boundary_is_none_not_an_error() {
        let mut empty: &[u8] = &[];
        assert_eq!(read_frame(&mut empty).expect("clean close"), None);
    }

    #[test]
    fn a_zero_length_frame_is_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_be_bytes());
        let err = read_frame(&mut buf.as_slice()).expect_err("zero length must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn an_oversized_length_is_rejected_without_allocating() {
        let mut buf = Vec::new();
        let declared = u32::try_from(MAX_FRAME + 1).expect("fits u32");
        buf.extend_from_slice(&declared.to_be_bytes());
        let err = read_frame(&mut buf.as_slice()).expect_err("oversized must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_truncated_header_is_an_error_not_a_clean_close() {
        // One byte of a four-byte length: the stream died mid-header.
        let buf = [0u8, 0u8];
        let err = read_frame(&mut buf.as_slice()).expect_err("partial header must fail");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_truncated_body_is_an_error() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&10u32.to_be_bytes());
        buf.extend_from_slice(b"short");
        let err = read_frame(&mut buf.as_slice()).expect_err("partial body must fail");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_non_utf8_body_is_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&2u32.to_be_bytes());
        buf.extend_from_slice(&[0xff, 0xfe]);
        let err = read_frame(&mut buf.as_slice()).expect_err("invalid UTF-8 must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn an_oversized_payload_is_refused_before_writing_anything() {
        let payload = "x".repeat(MAX_FRAME + 1);
        let mut buf = Vec::new();
        let err = write_frame(&mut buf, &payload).expect_err("too large to send");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(buf.is_empty(), "a refused frame must not leave a partial write behind");
    }

    #[test]
    fn a_frame_at_exactly_the_limit_is_accepted() {
        // The boundary itself is legal: the limit is "greater than", not "equal to".
        let payload = "y".repeat(MAX_FRAME);
        let buf = encode(&payload);
        assert_eq!(buf.len(), MAX_FRAME + 4, "4-byte header plus the payload");
        let back = read_frame(&mut buf.as_slice()).expect("limit-sized frame is valid");
        assert_eq!(back.map(|s| s.len()), Some(MAX_FRAME));
    }
}
