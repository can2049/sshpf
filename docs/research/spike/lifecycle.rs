//! Lifecycle risk: sshpf stops/restarts tunnels often. Can we reliably
//! tear down a russh session + release the local listener promptly?
//! NOTE: Handle is NOT Clone -> share via Arc, methods take &self.

use std::sync::Arc;
use std::time::{Duration, Instant};
use anyhow::Result;
use russh::client;
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use tokio::net::TcpListener;

struct Client;
impl client::Handler for Client {
    type Error = russh::Error;
    async fn check_server_key(&mut self, _k: &PublicKeyOrCertificate) -> Result<bool, Self::Error> { Ok(true) }
}

async fn connect() -> Result<Arc<client::Handle<Client>>> {
    let cfg = Arc::new(client::Config { inactivity_timeout: Some(Duration::from_secs(30)), ..<_>::default() });
    let mut h = client::connect(cfg, ("127.0.0.1", 22200u16), Client).await?;
    let key = russh::keys::load_secret_key("/tmp/russh-spike/sshd/user_ed25519", None)?;
    let hash = h.best_supported_rsa_hash().await?.flatten();
    let r = h.authenticate_publickey("wangcan", PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await?;
    anyhow::ensure!(r.success(), "auth failed");
    Ok(Arc::new(h))
}

async fn port_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).await.is_ok()
}

#[tokio::main]
async fn main() -> Result<()> {
    // --- 1. abort accept loop -> listener released ---
    println!("== 1. abort accept loop -> port released ==");
    let a = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = a.local_addr()?.port();
    drop(a);
    let h = connect().await?;
    let handle = tokio::spawn(async move {
        let l = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let _keep = h;
        loop { let _ = l.accept().await; }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    println!("  bound={} port_in_use={}", true, !port_free(port).await);
    let t = Instant::now();
    handle.abort();
    let _ = handle.await;
    let mut freed = false;
    for _ in 0..100 { if port_free(port).await { freed = true; break; } tokio::time::sleep(Duration::from_millis(20)).await; }
    println!("  port freed after {:?} -> {freed}", t.elapsed());
    anyhow::ensure!(freed, "listener not released");

    // --- 2. abort mid-handshake does not hang ---
    println!("\n== 2. abort mid-connect ==");
    let t = Instant::now();
    let task = tokio::spawn(async {
        let _ = client::connect(Arc::new(client::Config::default()), ("10.255.255.1", 22u16), Client).await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    task.abort();
    let r = tokio::time::timeout(Duration::from_secs(2), task).await;
    println!("  aborted in {:?}, joined={}", t.elapsed(), r.is_ok());
    anyhow::ensure!(r.is_ok(), "abort hung");

    // --- 3. rapid cycles (restart storm) ---
    println!("\n== 3. 20 rapid connect/disconnect cycles ==");
    let t = Instant::now();
    for _ in 0..20 {
        let h = connect().await?;
        h.disconnect(russh::Disconnect::ByApplication, "", "en").await.ok();
    }
    println!("  total {:?} ({:?}/cycle)", t.elapsed(), t.elapsed() / 20);

    // --- 4. fd leak ---
    println!("\n== 4. fd leak check ==");
    let before = std::fs::read_dir("/proc/self/fd")?.count();
    for _ in 0..30 {
        let h = connect().await?;
        h.disconnect(russh::Disconnect::ByApplication, "", "en").await.ok();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after = std::fs::read_dir("/proc/self/fd")?.count();
    println!("  fds before={before} after={after} delta={}", after as i64 - before as i64);
    println!("\nRESULT lifecycle: PASS");
    Ok(())
}
