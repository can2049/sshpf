//! smoke test: can russh do -L / -R / -D end-to-end against a real sshd?

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use russh::client::{self, Handle};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate, load_secret_key};
use russh::{Channel, ChannelMsg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const HOST: &str = "127.0.0.1";
const PORT: u16 = 22200;
const USER: &str = "wangcan";
const KEY: &str = "/tmp/russh-spike/sshd/user_ed25519";

struct Client;

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // spike: log the fingerprint, accept. Real impl validates known_hosts.
        match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                println!("  [handler] server key algo={:?} fp={}", key.algorithm(), key.fingerprint(HashAlg::Sha256));
            }
            PublicKeyOrCertificate::Certificate(c) => {
                println!("  [handler] got CERTIFICATE for {}", c.public_key().algorithm());
            }
        }
        Ok(true)
    }

    /// -R path: server accepted our tcpip-forward and now has an inbound conn
    /// for the port we asked it to listen on. `channel` is the data pipe.
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<client::Msg>,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        println!(
            "  [handler] forwarded-tcpip! connected={}:{} originator={}:{}",
            connected_address, connected_port, originator_address, originator_port
        );
        reply.accept().await;

        // For the spike: echo back whatever we receive, then close.
        tokio::spawn(async move {
            let mut ch = channel;
            loop {
                match ch.wait().await {
                    Some(ChannelMsg::Data { data }) => {
                        println!("  [handler] -R got {} bytes from remote", data.len());
                        let _ = ch.data_bytes(data).await;
                    }
                    Some(ChannelMsg::Eof) | None => {
                        println!("  [handler] -R channel eof/close");
                        let _ = ch.close().await;
                        break;
                    }
                    _ => {}
                }
            }
        });
        Ok(())
    }
}

async fn connect() -> Result<Handle<Client>> {
    let key = load_secret_key(KEY, None).context("load key")?;
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(60)),
        keepalive_interval: Some(Duration::from_secs(20)),
        ..<_>::default()
    });
    let t = Instant::now();
    let mut session = client::connect(config, (HOST, PORT), Client).await?;
    let hash = session.best_supported_rsa_hash().await?.flatten();
    let res = session
        .authenticate_publickey(
            USER,
            PrivateKeyWithHashAlg::new(Arc::new(key), hash),
        )
        .await?;
    println!("  auth success={} in {:?}", res.success(), t.elapsed());
    anyhow::ensure!(res.success(), "auth failed");
    Ok(session)
}

/// -L: listen locally, tunnel to something reachable from the sshd side.
async fn test_local_forward() -> Result<()> {
    println!("\n=== TEST -L (local forward) ===");
    let session = connect().await?;

    // Forward to the sshd's own port 22200: reading the target yields an SSH banner.
    let listener = TcpListener::bind((HOST, 0)).await?;
    let lport = listener.local_addr()?.port();
    println!("  listening on {HOST}:{lport} -> target {HOST}:{PORT}");
    let t = Instant::now();

    let handle = session;
    let task = tokio::spawn(async move {
        let (mut sock, oaddr) = listener.accept().await.unwrap();
        println!("  accepted local conn from {oaddr}");
        let mut ch = handle
            .channel_open_direct_tcpip(HOST, PORT as u32, oaddr.ip().to_string(), oaddr.port() as u32)
            .await
            .unwrap();
        println!("  direct-tcpip channel opened in {:?}", t.elapsed());

        let mut stream = ch.into_stream();
        let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
    });

    // act as a client on the forwarded port, read banner back
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut c = TcpStream::connect((HOST, lport)).await?;
    let mut buf = vec![0u8; 128];
    let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf)).await??;
    let banner = String::from_utf8_lossy(&buf[..n]).to_string();
    println!("  <<< GOT {n} bytes through tunnel: {:?}", banner.trim());
    anyhow::ensure!(banner.starts_with("SSH-2.0-"), "-L did not tunnel correctly");
    drop(c);
    let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    println!("  RESULT -L: PASS");
    Ok(())
}

/// -R: ask sshd to listen on a remote port, tunnel inbound back to us.
async fn test_remote_forward() -> Result<()> {
    println!("\n=== TEST -R (remote forward) ===");
    let session = connect().await?;

    // pick a free remote port
    let probe = TcpListener::bind((HOST, 0)).await?;
    let rport = probe.local_addr()?.port();
    drop(probe);

    let t = Instant::now();
    let granted = session.tcpip_forward(HOST.to_string(), rport as u32).await?;
    println!(
        "  tcpip_forward({HOST}, {rport}) -> granted={granted} in {:?}",
        t.elapsed()
    );
    let listen_port = if granted == 0 { rport } else { granted as u16 };

    // sshd now listens on HOST:listen_port. Connect to it -> handler fires.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut c = TcpStream::connect((HOST, listen_port)).await?;
    println!("  connected to remote-forwarded port {listen_port}");
    c.write_all(b"hello-through-remote-forward").await?;
    let mut buf = vec![0u8; 128];
    let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf)).await??;
    let echo = String::from_utf8_lossy(&buf[..n]).to_string();
    println!("  <<< GOT {n} bytes back: {:?}", echo);
    anyhow::ensure!(
        echo == "hello-through-remote-forward",
        "-R echo mismatch"
    );

    session.cancel_tcpip_forward(HOST.to_string(), listen_port as u32).await?;
    println!("  cancel_tcpip_forward ok");
    println!("  RESULT -R: PASS");
    Ok(())
}

/// -D: no built-in SOCKS5 in russh; measure what we'd need to write.
async fn test_dynamic_forward() -> Result<()> {
    println!("\n=== TEST -D (dynamic / SOCKS5) ===");
    println!("  russh has no built-in SOCKS5 server. Checking primitives exist...");
    let session = connect().await?;

    // minimal SOCKS5 no-auth handshake done by hand + direct-tcpip
    let listener = TcpListener::bind((HOST, 0)).await?;
    let lport = listener.local_addr()?.port();
    println!("  socks5 listener on {HOST}:{lport}");

    let handle: Handle<Client> = session;
    tokio::spawn(async move {
        let (mut sock, oaddr) = listener.accept().await.unwrap();
        // greeting: VER=5 NMETHODS METHODS...
        let mut hdr = [0u8; 2];
        sock.read_exact(&mut hdr).await.unwrap();
        let nmethods = hdr[1] as usize;
        let mut methods = vec![0u8; nmethods];
        sock.read_exact(&mut methods).await.unwrap();
        // pick no-auth (0x00)
        sock.write_all(&[0x05, 0x00]).await.unwrap();
        // request: VER CMD RSV ATYP DST.ADDR DST.PORT
        let mut req = [0u8; 4];
        sock.read_exact(&mut req).await.unwrap();
        let (cmd, atyp) = (req[1], req[3]);
        anyhow::ensure!(cmd == 0x01, "only CONNECT supported");
        let dst_host = match atyp {
            0x01 => {
                let mut a = [0u8; 4];
                sock.read_exact(&mut a).await.unwrap();
                std::net::Ipv4Addr::from(a).to_string()
            }
            0x03 => {
                let mut l = [0u8; 1];
                sock.read_exact(&mut l).await.unwrap();
                let mut d = vec![0u8; l[0] as usize];
                sock.read_exact(&mut d).await.unwrap();
                String::from_utf8(d).unwrap()
            }
            _ => anyhow::bail!("atyp {atyp} unsupported"),
        };
        let mut pb = [0u8; 2];
        sock.read_exact(&mut pb).await.unwrap();
        let dst_port = u16::from_be_bytes(pb);
        println!("  socks5 CONNECT {dst_host}:{dst_port}");

        let mut ch = handle
            .channel_open_direct_tcpip(
                dst_host,
                dst_port as u32,
                oaddr.ip().to_string(),
                oaddr.port() as u32,
            )
            .await
            .unwrap();
        // success reply
        sock.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        let mut stream = ch.into_stream();
        let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
        Ok::<(), anyhow::Error>(())
    });

    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut c = TcpStream::connect((HOST, lport)).await?;
    c.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut r = [0u8; 2];
    c.read_exact(&mut r).await?;
    anyhow::ensure!(r == [0x05, 0x00], "socks5 greeting failed");
    // CONNECT 127.0.0.1:22200 -> expect SSH banner through socks
    let mut req = vec![0x05, 0x01, 0x00, 0x01];
    req.extend_from_slice(&[127, 0, 0, 1]);
    req.extend_from_slice(&PORT.to_be_bytes());
    c.write_all(&req).await?;
    let mut rep = [0u8; 10];
    c.read_exact(&mut rep).await?;
    println!("  socks5 reply: {:02x?}", rep);
    anyhow::ensure!(rep[1] == 0x00, "socks5 connect denied");
    let mut buf = vec![0u8; 128];
    let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf)).await??;
    let banner = String::from_utf8_lossy(&buf[..n]).to_string();
    println!("  <<< through SOCKS5 got: {:?}", banner.trim());
    anyhow::ensure!(banner.starts_with("SSH-2.0-"), "socks5 tunnel broken");
    println!("  RESULT -D: PASS (with ~60 lines of hand-written SOCKS5)");
    Ok(())
}

/// known_hosts check against the real sshd host key.
fn test_known_hosts() -> Result<()> {
    println!("\n=== TEST known_hosts ===");
    let path = "/tmp/russh-spike/sshd/known_hosts";
    let _ = std::fs::remove_file(path);
    let pubkey = russh::keys::load_public_key("/tmp/russh-spike/sshd/host_ed25519.pub")?;
    let r = russh::keys::known_hosts::check_known_hosts_path(HOST, PORT, &pubkey, path);
    println!("  check on empty file -> {:?}", r.map_err(|e| e.to_string()));
    russh::keys::known_hosts::learn_known_hosts_path(HOST, PORT, &pubkey, path)?;
    let r2 = russh::keys::known_hosts::check_known_hosts_path(HOST, PORT, &pubkey, path);
    println!("  check after learn  -> {:?}", r2);
    println!("  file contents: {:?}", std::fs::read_to_string(path)?);
    anyhow::ensure!(r2? == true, "known_hosts learn/check roundtrip failed");
    println!("  RESULT known_hosts: PASS");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut results: HashMap<&str, String> = HashMap::new();
    macro_rules! run {
        ($name:expr, $f:expr) => {
            match $f.await {
                Ok(()) => { results.insert($name, "PASS".into()); }
                Err(e) => {
                    println!("  RESULT {}: FAIL - {:#}", $name, e);
                    results.insert($name, format!("FAIL: {e}"));
                }
            }
        };
    }
    run!("-L", test_local_forward());
    run!("-R", test_remote_forward());
    run!("-D", test_dynamic_forward());
    test_known_hosts()?;
    println!("\n=== SUMMARY ===");
    for k in ["-L", "-R", "-D"] {
        println!("  {k}: {}", results[k]);
    }
    Ok(())
}
