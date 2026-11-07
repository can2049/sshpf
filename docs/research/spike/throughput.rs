//! Throughput sanity: is a pure-Rust tunnel usable for real work?
use std::sync::Arc;
use std::time::{Duration, Instant};
use anyhow::Result;
use russh::client;
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct Client;
impl client::Handler for Client {
    type Error = russh::Error;
    async fn check_server_key(&mut self, _k: &PublicKeyOrCertificate) -> Result<bool, Self::Error> { Ok(true) }
}

async fn conn() -> Result<client::Handle<Client>> {
    let cfg = Arc::new(client::Config { inactivity_timeout: Some(Duration::from_secs(60)), ..<_>::default() });
    let mut h = client::connect(cfg, ("127.0.0.1", 22200u16), Client).await?;
    let key = russh::keys::load_secret_key("/tmp/russh-spike/sshd/user_ed25519", None)?;
    let hash = h.best_supported_rsa_hash().await?.flatten();
    let r = h.authenticate_publickey("wangcan", PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await?;
    anyhow::ensure!(r.success(), "auth");
    Ok(h)
}

#[tokio::main]
async fn main() -> Result<()> {
    // echo server that we will reach *through* the tunnel
    let echo = TcpListener::bind(("127.0.0.1", 0)).await?;
    let echo_port = echo.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = echo.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 { break; }
                    if s.write_all(&buf[..n]).await.is_err() { break; }
                }
            });
        }
    });

    let h = Arc::new(conn().await?);

    // local forwarding listener
    let l = TcpListener::bind(("127.0.0.1", 0)).await?;
    let lport = l.local_addr()?.port();
    let h2 = h.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, oaddr) = match l.accept().await { Ok(x) => x, Err(_) => break };
            let h3 = h2.clone();
            tokio::spawn(async move {
                let ch = match h3.channel_open_direct_tcpip("127.0.0.1", echo_port as u32,
                        oaddr.ip().to_string(), oaddr.port() as u32).await { Ok(c) => c, Err(_) => return };
                let mut stream = ch.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
            });
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    // measure round-trip latency (small messages)
    let mut c = TcpStream::connect(("127.0.0.1", lport)).await?;
    let mut buf = [0u8; 64];
    let mut lat = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        c.write_all(b"ping").await?;
        let n = c.read(&mut buf).await?;
        anyhow::ensure!(n == 4, "echo short");
        lat.push(t.elapsed());
    }
    lat.sort();
    println!("== RTT through tunnel (20 x 4B) ==");
    println!("  min={:?} median={:?} max={:?}", lat[0], lat[lat.len()/2], lat[lat.len()-1]);

    // measure bulk throughput
    let total: usize = 32 * 1024 * 1024;
    let c = TcpStream::connect(("127.0.0.1", lport)).await?;
    let (mut r, mut w) = c.into_split();
    let t = Instant::now();
    let reader = tokio::spawn(async move {
        let mut got = 0usize;
        let mut b = vec![0u8; 262144];
        while got < total {
            match r.read(&mut b).await { Ok(0) => break, Ok(n) => got += n, Err(_) => break }
        }
        got
    });
    let payload = vec![0x5au8; 262144];
    let mut sent = 0usize;
    while sent < total {
        w.write_all(&payload).await?;
        sent += payload.len();
    }
    w.shutdown().await.ok();
    let got = reader.await?;
    let el = t.elapsed();
    println!("\n== bulk transfer ==");
    println!("  sent={:.1}MiB recv={:.1}MiB in {:?}", sent as f64/1048576.0, got as f64/1048576.0, el);
    println!("  throughput = {:.1} MiB/s", (got as f64/1048576.0)/el.as_secs_f64());
    anyhow::ensure!(got == sent, "data loss: sent {sent} got {got}");
    println!("\nRESULT throughput: PASS (no data loss)");
    Ok(())
}
