//! Failure classification: with a process we parse stderr; with russh we get
//! typed errors. Are they distinguishable enough to keep requirement #6
//! ("failures must show the reason")?

use std::sync::Arc;
use std::time::Duration;
use anyhow::Result;
use russh::client;
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use tokio::net::TcpListener;

struct Client;
impl client::Handler for Client {
    type Error = russh::Error;
    async fn check_server_key(&mut self, _k: &PublicKeyOrCertificate) -> Result<bool, Self::Error> { Ok(true) }
}

fn cfg() -> Arc<client::Config> {
    Arc::new(client::Config { inactivity_timeout: Some(Duration::from_secs(8)), ..<_>::default() })
}

#[tokio::main]
async fn main() {
    // 1. connection refused (nothing listening)
    println!("== 1. connection refused ==");
    match tokio::time::timeout(Duration::from_secs(6), client::connect(cfg(), ("127.0.0.1", 1u16), Client)).await {
        Ok(Err(e)) => println!("  russh::Error = {e:?}\n  Display = {e}"),
        Ok(Ok(_)) => println!("  unexpected connect"),
        Err(_) => println!("  TIMEOUT"),
    }

    // 2. DNS failure
    println!("\n== 2. DNS failure ==");
    match tokio::time::timeout(Duration::from_secs(6), client::connect(cfg(), ("no-such-host.invalid", 22u16), Client)).await {
        Ok(Err(e)) => println!("  russh::Error = {e:?}\n  Display = {e}"),
        Ok(Ok(_)) => println!("  unexpected"),
        Err(_) => println!("  TIMEOUT"),
    }

    // 3. local bind conflict (-L on a taken port)
    println!("\n== 3. local bind conflict ==");
    let a = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let p = a.local_addr().unwrap().port();
    match TcpListener::bind(("127.0.0.1", p)).await {
        Ok(_) => println!("  unexpected: bound twice"),
        Err(e) => println!("  io::Error = {e:?}\n  Display = {e}\n  kind = {:?}", e.kind()),
    }

    // 4. auth failure (wrong key)
    println!("\n== 4. auth failure ==");
    let mut h = client::connect(cfg(), ("127.0.0.1", 22200u16), Client).await.unwrap();
    let badkey = russh::keys::load_secret_key("/tmp/russh-spike/sshd/host_ed25519", None).unwrap();
    let r = h.authenticate_publickey("wangcan", PrivateKeyWithHashAlg::new(Arc::new(badkey), None)).await;
    println!("  auth result = {r:?}");

    // 5. remote forward to a privileged / in-use port
    println!("\n== 5. remote forward failure ==");
    let mut h = client::connect(cfg(), ("127.0.0.1", 22200u16), Client).await.unwrap();
    let key = russh::keys::load_secret_key("/tmp/russh-spike/sshd/user_ed25519", None).unwrap();
    let hash = h.best_supported_rsa_hash().await.unwrap().flatten();
    let ok = h.authenticate_publickey("wangcan", PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await.unwrap();
    println!("  authed={}", ok.success());
    // ask for the port sshd itself is listening on -> must be denied
    match h.tcpip_forward("127.0.0.1".to_string(), 22200u32).await {
        Ok(p) => println!("  tcpip_forward IN USE port -> Ok({p})  <-- granted, unexpected"),
        Err(e) => println!("  tcpip_forward IN USE port -> Err = {e:?}\n    Display = {e}"),
    }
    // ask a second time for the same fresh port -> second must be denied
    let probe = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let fp = probe.local_addr().unwrap().port();
    drop(probe);
    println!("  first tcpip_forward({fp}) = {:?}", h.tcpip_forward("127.0.0.1".to_string(), fp as u32).await);
    println!("  second tcpip_forward({fp}) = {:?}", h.tcpip_forward("127.0.0.1".to_string(), fp as u32).await);

    // 6. target unreachable via direct-tcpip (-L where target port nothing listens)
    println!("\n== 6. direct-tcpip to dead target ==");
    let probe = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let dead = probe.local_addr().unwrap().port();
    drop(probe);
    match h.channel_open_direct_tcpip("127.0.0.1", dead as u32, "127.0.0.1", 1234u32).await {
        Ok(_) => println!("  opened (unexpected)"),
        Err(e) => println!("  Err = {e:?}\n    Display = {e}"),
    }

    // 7. host key mismatch -> our own check_server_key decides
    println!("\n== 7. host key check (ours) ==");
    println!("  check_server_key sees PublicKeyOrCertificate; we compare vs known_hosts and return false to reject");
    println!("  => full control over strict/accept-new/ask semantics (better than parsing stderr)");
}
