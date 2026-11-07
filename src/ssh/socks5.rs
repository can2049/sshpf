//! Minimal SOCKS5 server for `-D` dynamic forwarding.
//!
//! `russh` has no SOCKS support, but a dynamic forward is just a SOCKS5
//! handshake followed by a `direct-tcpip` channel. Only the `CONNECT` command
//! with `NOAUTH` is implemented — that is what browsers and CLI tools actually
//! use against `ssh -D`.
//!
//! Protocol (RFC 1928):
//!
//! ```text
//! client -> VER=5 NMETHODS METHODS...
//! server <- VER=5 METHOD=0            (NOAUTH)
//! client -> VER=5 CMD=1 RSV=0 ATYP DST.ADDR DST.PORT
//! server <- VER=5 REP=0 RSV=0 ATYP BND.ADDR BND.PORT
//! <raw bytes>
//! ```
//!
//! Unsupported commands get `REP=0x07` (command not supported) and unsupported
//! address types `REP=0x08` (address type not supported), as the RFC requires.

use std::net::{Ipv4Addr, Ipv6Addr};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// SOCKS5 protocol version.
const VERSION: u8 = 0x05;
/// `NOAUTH` — the only method we accept.
const METHOD_NO_AUTH: u8 = 0x00;
/// `0xFF` — no acceptable methods.
const METHOD_NONE: u8 = 0xFF;
/// `CONNECT` command.
const CMD_CONNECT: u8 = 0x01;

/// Reply codes we emit.
const REP_SUCCESS: u8 = 0x00;
const REP_GENERAL_FAILURE: u8 = 0x01;
const REP_COMMAND_NOT_SUPPORTED: u8 = 0x07;
const REP_ATYP_NOT_SUPPORTED: u8 = 0x08;

/// Address type bytes.
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

/// A parsed SOCKS5 `CONNECT` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectRequest {
    /// Target host: either an IP literal or a domain name.
    pub host: String,
    /// Target port.
    pub port: u16,
}

/// Errors from the handshake. All are reported to the client as a reply code
/// before the connection is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocksError {
    /// Client spoke something other than SOCKS5.
    BadVersion,
    /// Not `CONNECT` (e.g. `BIND`, `UDP ASSOCIATE`).
    CommandNotSupported,
    /// Address type we do not handle.
    AddressTypeNotSupported,
    /// Malformed request or I/O error; reply with a general failure.
    Failure,
}

impl SocksError {
    /// The SOCKS5 reply code this error maps to.
    pub fn reply_code(self) -> u8 {
        match self {
            SocksError::BadVersion | SocksError::Failure => REP_GENERAL_FAILURE,
            SocksError::CommandNotSupported => REP_COMMAND_NOT_SUPPORTED,
            SocksError::AddressTypeNotSupported => REP_ATYP_NOT_SUPPORTED,
        }
    }
}

/// Perform the SOCKS5 greeting (method negotiation).
///
/// Returns `Ok(())` when the client must proceed with a request.
pub async fn negotiate<S>(stream: &mut S) -> Result<(), SocksError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).await.map_err(|_| SocksError::Failure)?;
    if header[0] != VERSION {
        return Err(SocksError::BadVersion);
    }
    let nmethods = header[1] as usize;
    let mut methods = vec![0u8; nmethods];
    stream.read_exact(&mut methods).await.map_err(|_| SocksError::Failure)?;
    if !methods.contains(&METHOD_NO_AUTH) {
        let _ = stream.write_all(&[VERSION, METHOD_NONE]).await;
        return Err(SocksError::BadVersion);
    }
    stream.write_all(&[VERSION, METHOD_NO_AUTH]).await.map_err(|_| SocksError::Failure)?;
    Ok(())
}

/// Read and validate one `CONNECT` request.
pub async fn read_request<S>(stream: &mut S) -> Result<ConnectRequest, SocksError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await.map_err(|_| SocksError::Failure)?;
    if head[0] != VERSION {
        return Err(SocksError::BadVersion);
    }
    if head[1] != CMD_CONNECT {
        // BIND / UDP ASSOCIATE: tell the client we do not do it.
        reply(stream, REP_COMMAND_NOT_SUPPORTED).await;
        return Err(SocksError::CommandNotSupported);
    }
    let host = match head[3] {
        ATYP_IPV4 => {
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).await.map_err(|_| SocksError::Failure)?;
            Ipv4Addr::from(buf).to_string()
        }
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await.map_err(|_| SocksError::Failure)?;
            if len[0] == 0 {
                reply(stream, REP_GENERAL_FAILURE).await;
                return Err(SocksError::Failure);
            }
            let mut buf = vec![0u8; len[0] as usize];
            stream.read_exact(&mut buf).await.map_err(|_| SocksError::Failure)?;
            String::from_utf8(buf).map_err(|_| SocksError::Failure)?
        }
        ATYP_IPV6 => {
            let mut buf = [0u8; 16];
            stream.read_exact(&mut buf).await.map_err(|_| SocksError::Failure)?;
            Ipv6Addr::from(buf).to_string()
        }
        _ => {
            reply(stream, REP_ATYP_NOT_SUPPORTED).await;
            return Err(SocksError::AddressTypeNotSupported);
        }
    };
    let mut port = [0u8; 2];
    stream.read_exact(&mut port).await.map_err(|_| SocksError::Failure)?;
    Ok(ConnectRequest { host, port: u16::from_be_bytes(port) })
}

/// Send a bare reply with no meaningful bound address.
async fn reply<S>(stream: &mut S, code: u8)
where
    S: AsyncWrite + Unpin,
{
    let _ = stream.write_all(&[VERSION, code, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0]).await;
}

/// Tell the client the tunnel is established.
pub async fn reply_success<S>(stream: &mut S) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    // BND.ADDR/BND.PORT are informational; zeroes are fine and widely accepted.
    stream.write_all(&[VERSION, REP_SUCCESS, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0]).await
}

/// Tell the client the upstream connection failed.
pub async fn reply_failure<S>(stream: &mut S)
where
    S: AsyncWrite + Unpin,
{
    reply(stream, REP_GENERAL_FAILURE).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    /// Drive a full handshake against `negotiate` + `read_request` over a duplex
    /// pipe, returning what the server produced.
    async fn run(client_bytes: &[u8]) -> (Vec<u8>, Result<ConnectRequest, SocksError>) {
        let (client, server) = duplex(1024);
        let (mut cr, mut cw) = tokio::io::split(client);
        let mut server = server;
        let payload = client_bytes.to_vec();
        // Write the request asynchronously; keep the write half open afterwards.
        let writer = tokio::spawn(async move {
            let _ = cw.write_all(&payload).await;
            let _ = cw.flush().await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });
        let neg = negotiate(&mut server).await;
        let req = match neg {
            Ok(()) => read_request(&mut server).await,
            Err(e) => Err(e),
        };
        let reader = tokio::spawn(async move {
            let mut replies = Vec::new();
            let _ = cr.read_to_end(&mut replies).await;
            replies
        });
        drop(server);
        let _ = writer.await;
        let replies = reader.await.unwrap_or_default();
        (replies, req)
    }

    #[tokio::test]
    async fn ipv4_connect() {
        let mut bytes = vec![0x05, 0x01, 0x00]; // greeting: no-auth
        bytes.extend_from_slice(&[0x05, 0x01, 0x00, 0x01, 127, 0, 0, 1]); // CONNECT 127.0.0.1
        bytes.extend_from_slice(&8080u16.to_be_bytes());
        let (replies, req) = run(&bytes).await;
        assert_eq!(req, Ok(ConnectRequest { host: "127.0.0.1".into(), port: 8080 }));
        // Greeting reply is 05 00.
        assert!(replies.starts_with(&[0x05, 0x00]));
    }

    #[tokio::test]
    async fn domain_connect() {
        let host = b"example.com";
        let mut bytes = vec![0x05, 0x01, 0x00];
        bytes.extend_from_slice(&[0x05, 0x01, 0x00, 0x03, host.len() as u8]);
        bytes.extend_from_slice(host);
        bytes.extend_from_slice(&443u16.to_be_bytes());
        let (_, req) = run(&bytes).await;
        assert_eq!(req, Ok(ConnectRequest { host: "example.com".into(), port: 443 }));
    }

    #[tokio::test]
    async fn ipv6_connect() {
        let mut bytes = vec![0x05, 0x01, 0x00];
        bytes.extend_from_slice(&[0x05, 0x01, 0x00, 0x04]);
        bytes.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
        bytes.extend_from_slice(&22u16.to_be_bytes());
        let (_, req) = run(&bytes).await;
        assert_eq!(req, Ok(ConnectRequest { host: "::1".into(), port: 22 }));
    }

    #[tokio::test]
    async fn udp_associate_is_rejected() {
        // CMD 0x03 = UDP ASSOCIATE.
        let mut bytes = vec![0x05, 0x01, 0x00];
        bytes.extend_from_slice(&[0x05, 0x03, 0x00, 0x01, 127, 0, 0, 1]);
        bytes.extend_from_slice(&1080u16.to_be_bytes());
        let (replies, req) = run(&bytes).await;
        assert_eq!(req, Err(SocksError::CommandNotSupported));
        // Greeting + a 0x07 reply.
        assert!(replies.windows(2).any(|w| w == [0x05, REP_COMMAND_NOT_SUPPORTED]), "replies={replies:02x?}");
    }

    #[tokio::test]
    async fn unknown_address_type_is_rejected() {
        let mut bytes = vec![0x05, 0x01, 0x00];
        bytes.extend_from_slice(&[0x05, 0x01, 0x00, 0x09]); // ATYP 9 = bogus
        let (replies, req) = run(&bytes).await;
        assert_eq!(req, Err(SocksError::AddressTypeNotSupported));
        assert!(replies.windows(2).any(|w| w == [0x05, REP_ATYP_NOT_SUPPORTED]), "replies={replies:02x?}");
    }

    #[tokio::test]
    async fn non_socks_greeting_is_rejected() {
        let bytes = b"GET / HTTP/1.1\r\n\r\n";
        let (_, req) = run(bytes).await;
        assert_eq!(req, Err(SocksError::BadVersion));
    }

    #[tokio::test]
    async fn success_reply_has_zero_rep() {
        let (mut client, mut server) = duplex(64);
        reply_success(&mut server).await.unwrap();
        drop(server);
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, vec![0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn error_reply_codes_follow_rfc1928() {
        assert_eq!(SocksError::CommandNotSupported.reply_code(), REP_COMMAND_NOT_SUPPORTED);
        assert_eq!(SocksError::AddressTypeNotSupported.reply_code(), REP_ATYP_NOT_SUPPORTED);
        assert_eq!(SocksError::BadVersion.reply_code(), REP_GENERAL_FAILURE);
        assert_eq!(SocksError::Failure.reply_code(), REP_GENERAL_FAILURE);
    }

    #[tokio::test]
    async fn failure_reply_is_general_failure() {
        let (mut client, mut server) = duplex(64);
        reply_failure(&mut server).await;
        drop(server);
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf[1], REP_GENERAL_FAILURE);
    }
}
