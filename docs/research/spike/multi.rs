//! N concurrent tunnels on separate runtime threads (mirrors sshpf's model:
//! one thread per tunnel, sharing nothing but process resources).
use std::sync::mpsc;
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

/// One tunnel = one thread + one current_thread runtime + one -L listener.
fn spawn_tunnel(idx: usize, echo_port: u16, tx: mpsc::Sender<(usize, u16, Result<()>)>) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let res = rt.block_on(async move {
            let cfg = Arc::new(client::Config { inactivity_timeout: Some(Duration::from_secs(30)), ..<_>::default() });
            let mut h = client::connect(cfg, ("127.0.0.1", 22200u16), Client).await?;
            let key = russh::keys::load_secret_key("/tmp/russh-spike/sshd/user_ed25519", None)?;
            let hash = h.best_supported_rsa_hash().await?.flatten();
            let r = h.authenticate_publickey("wangcan", PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await?;
            anyhow::ensure!(r.success(), "auth");
            let h = Arc::new(h);
            let l = TcpListener::bind(("127.0.0.1", 0)).await?;
            let port = l.local_addr()?.port();
            tx.send((idx, port, Ok(()))).ok();
            loop {
                let (mut sock, oaddr) = l.accept().await?;
                let h2 = h.clone();
                tokio::spawn(async move {
                    if let Ok(ch) = h2.channel_open_direct_tcpip("127.0.0.1", echo_port as u32,
                            oaddr.ip().to_string(), oaddr.port() as u32).await {
                        let mut s = ch.into_stream();
                        let _ = tokio::io::copy_bidirectional(&mut sock, &mut s).await;
                    }
                });
            }
            #[allow(unreachable_code)] Ok::<(), anyhow::Error>(())
        });
        let _ = res;
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    // echo target
    let echo = TcpListener::bind(("127.0.0.1", 0)).await?;
    let echo_port = echo.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = echo.accept().await.unwrap();
            tokio::spawn(async move {
                let mut b = vec![0u8; 65536];
                while let Ok(n) = s.read(&mut b).await { if n == 0 { break; } if s.write_all(&b[..n]).await.is_err() { break; } }
            });
        }
    });

    const N: usize = 10;
    let (tx, rx) = mpsc::channel();
    let t = Instant::now();
    for i in 0..N { spawn_tunnel(i, echo_port, tx.clone()); }
    drop(tx);

    let mut ports = Vec::new();
    while ports.len() < N {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok((_i, p, Ok(()))) => ports.push(p),
            Ok((i, _, Err(e))) => { println!("tunnel {i} failed: {e}"); }
            Err(e) => anyhow::bail!("timeout, only {} up: {e}", ports.len()),
        }
    }
    println!("{N} tunnels up in {:?}", t.elapsed());
    anyhow::ensure!(ports.len() == N, "not all up");

    // each tunnel must carry data independently and concurrently
    let t2 = Instant::now();
    let mut tasks = Vec::new();
    for (i, p) in ports.iter().enumerate() {
        let p = *p;
        tasks.push(tokio::spawn(async move {
            let mut c = TcpStream::connect(("127.0.0.1", p)).await.unwrap();
            let msg = format!("tunnel-{i}");
            c.write_all(msg.as_bytes()).await.unwrap();
            let mut b = vec![0u8; 64];
            let n = c.read(&mut b).await.unwrap();
            String::from_utf8_lossy(&b[..n]).to_string()
        }));
    }
    let mut ok = 0;
    for (i, h) in tasks.into_iter().enumerate() {
        let got = h.await?;
        if got == format!("tunnel-{i}") { ok += 1; } else { println!("  tunnel {i} got {got:?}"); }
    }
    println!("  {ok}/{N} tunnels carried correct data concurrently in {:?}", t2.elapsed());
    anyhow::ensure!(ok == N, "data mismatch");
    println!("\nRESULT multi-tunnel: PASS");
    Ok(())
}
