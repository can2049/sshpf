//! Reading one client frame at a time from a socket that has a read timeout.
//!
//! # Why this is not just `ipc::read_client_msg`
//!
//! `ipc::read_frame` reads a header and then a body with `read_exact`, and a read
//! timeout surfaces as `WouldBlock` **after** bytes have already been consumed from
//! the socket. Those bytes are gone but were never returned, so calling it again
//! would resume mid-frame and silently desynchronise the stream: a `Bye` split
//! across a poll would be lost and the client would look like it vanished.
//!
//! A session loop needs both properties at once — a timeout, so it can notice a
//! requested shutdown, and frame integrity, so it is correct. [`FrameReader`]
//! therefore accumulates bytes itself and only reports a message once a whole frame
//! is present, keeping the buffer across a `WouldBlock`. It is a resumable frame
//! decoder, and this module exists because of that specific need.
//!
//! Belongs here: buffering bytes, cutting frames, and the outcome vocabulary.
//! Does not belong: what a message means ([`super::session`]), or the frame format
//! itself ([`crate::ipc`] owns that).

use std::io::{BufReader, Read as _};
use std::net::TcpStream;

use crate::ipc::ClientMsg;

/// One read outcome, so the session loop reads as a table instead of nested
/// matches.
pub enum Incoming {
    Msg(ClientMsg),
    /// The peer closed cleanly at a frame boundary.
    Closed,
    /// Nothing complete arrived within the poll interval; try again.
    Timeout,
    /// The stream is unusable (malformed frame, truncation, I/O error).
    Failed(String),
}

/// A frame reader that survives a partial read.
pub struct FrameReader {
    stream: BufReader<TcpStream>,
    /// Bytes received but not yet consumed as a frame.
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new(stream: BufReader<TcpStream>) -> FrameReader {
        FrameReader { stream, buf: Vec::new() }
    }

    /// Read the next message, or report why there is none this time.
    pub fn read_incoming(&mut self) -> Incoming {
        // A whole frame may already be buffered from an earlier read.
        if let Some(frame) = self.take_frame() {
            return match frame {
                Ok(msg) => Incoming::Msg(msg),
                Err(e) => Incoming::Failed(e),
            };
        }
        match self.fill() {
            Ok(true) => match self.take_frame() {
                Some(Ok(msg)) => Incoming::Msg(msg),
                Some(Err(e)) => Incoming::Failed(e),
                // A whole frame did not arrive yet: keep the bytes and try again
                // next poll. This is the case a naive `read_frame` would corrupt.
                None => Incoming::Timeout,
            },
            Ok(false) => {
                if self.has_partial() {
                    Incoming::Failed("connection closed mid-frame".to_string())
                } else {
                    Incoming::Closed
                }
            }
            Err(e) if is_timeout(&e) => Incoming::Timeout,
            Err(e) => Incoming::Failed(e.to_string()),
        }
    }

    /// Read one more chunk from the socket into the buffer.
    ///
    /// `Ok(false)` is a clean end of stream. `Interrupted` is retried implicitly by
    /// reporting success with no new bytes, so a signal cannot look like a message.
    fn fill(&mut self) -> std::io::Result<bool> {
        let mut chunk = [0u8; 4096];
        match self.stream.read(&mut chunk) {
            Ok(0) => Ok(false),
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(true),
            Err(e) => Err(e),
        }
    }

    /// Try to cut one whole frame out of the buffer.
    ///
    /// `None` when the buffer does not hold a whole frame yet; the bytes stay for
    /// the next attempt.
    fn take_frame(&mut self) -> Option<Result<ClientMsg, String>> {
        if self.buf.len() < 4 {
            return None;
        }
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        // Reject an unusable length *before* waiting for a body: a corrupted header
        // must not make the session wait for bytes that will never come.
        if len == 0 || len > crate::ipc::MAX_FRAME {
            return Some(Err(format!("frame length {len} is not usable")));
        }
        if self.buf.len() < 4 + len {
            return None;
        }
        let body: Vec<u8> = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        Some(decode_client(&body))
    }

    /// Whether a partial frame is buffered, so an EOF here is a truncation rather
    /// than a clean close.
    fn has_partial(&self) -> bool {
        !self.buf.is_empty()
    }
}

/// Decode one frame body into a client message.
///
/// Untrusted input: every failure is a message, never a panic.
fn decode_client(body: &[u8]) -> Result<ClientMsg, String> {
    let text = std::str::from_utf8(body).map_err(|_| "frame body is not UTF-8".to_string())?;
    toml::from_str::<ClientMsg>(text).map_err(|e| format!("{e}"))
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{ClientMsg, Op, write_message};

    /// Encode one message the way a client would.
    fn frame(msg: &ClientMsg) -> Vec<u8> {
        let mut buf = Vec::new();
        write_message(&mut buf, msg).expect("encoding into a Vec cannot fail");
        buf
    }

    impl FrameReader {
        /// Put bytes straight into the buffer, as `fill` would.
        fn feed(&mut self, bytes: &[u8]) {
            self.buf.extend_from_slice(bytes);
        }
    }

    /// A reader whose buffer is driven directly by `feed`.
    ///
    /// The buffering logic is what these tests are about, and it never touches the
    /// socket: `take_frame` only reads `buf`. The stream is a placeholder that is
    /// never read from.
    fn reader() -> FrameReader {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback bind");
        let addr = listener.local_addr().expect("addr");
        let stream = std::net::TcpStream::connect(addr).expect("connect");
        drop(listener);
        FrameReader { stream: std::io::BufReader::new(stream), buf: Vec::new() }
    }

    #[test]
    fn a_whole_frame_is_cut_out_and_decoded() {
        let mut reader = reader();
        reader.feed(&frame(&ClientMsg::Ping));
        match reader.take_frame().expect("a whole frame is present") {
            Ok(ClientMsg::Ping) => {}
            other => panic!("expected a ping, got {other:?}"),
        }
        assert!(!reader.has_partial(), "nothing must be left over");
    }

    #[test]
    fn a_frame_split_across_feeds_survives_in_the_buffer() {
        // The regression this type exists for: a naive `read_frame` would consume
        // the header, hit `WouldBlock` on the body, and lose both.
        let bytes = frame(&ClientMsg::Command { req_id: 3, expect_rev: None, op: Op::StopAll });
        let mut reader = reader();
        let split = bytes.len() / 2;
        reader.feed(&bytes[..split]);
        assert!(reader.take_frame().is_none(), "half a frame is not a frame");
        assert!(reader.has_partial(), "and the half must be kept");
        reader.feed(&bytes[split..]);
        match reader.take_frame().expect("now complete") {
            Ok(ClientMsg::Command { req_id, op, .. }) => {
                assert_eq!(req_id, 3);
                assert_eq!(op, Op::StopAll);
            }
            other => panic!("expected the whole command, got {other:?}"),
        }
    }

    #[test]
    fn two_frames_in_one_read_are_both_delivered() {
        let mut bytes = frame(&ClientMsg::Ping);
        bytes.extend(frame(&ClientMsg::Bye { last_exit: true }));
        let mut reader = reader();
        reader.feed(&bytes);
        assert!(matches!(reader.take_frame(), Some(Ok(ClientMsg::Ping))));
        assert!(matches!(reader.take_frame(), Some(Ok(ClientMsg::Bye { last_exit: true }))));
        assert!(reader.take_frame().is_none(), "and then nothing");
    }

    #[test]
    fn an_unusable_length_is_rejected_without_waiting_for_a_body() {
        let mut reader = reader();
        // A huge declared length must be refused immediately rather than buffering
        // forever waiting for bytes that will never arrive.
        reader.feed(&u32::MAX.to_be_bytes());
        match reader.take_frame() {
            Some(Err(e)) => assert!(e.contains("not usable"), "{e}"),
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn a_zero_length_frame_is_rejected() {
        let mut reader = reader();
        reader.feed(&0u32.to_be_bytes());
        assert!(matches!(reader.take_frame(), Some(Err(_))));
    }

    #[test]
    fn a_non_utf8_body_is_an_error_not_a_panic() {
        let mut reader = reader();
        reader.feed(&2u32.to_be_bytes());
        reader.feed(&[0xff, 0xfe]);
        assert!(matches!(reader.take_frame(), Some(Err(_))));
    }
}
