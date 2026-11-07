//! Architectural question: russh needs tokio, but sshpf's TUI is a
//! synchronous single-threaded crossterm loop with no tokio.
//! Can we run tokio on a background thread and bridge via std::sync::mpsc?

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use russh::client;
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use tokio::net::TcpListener;

#[derive(Debug)]
enum Ev {
    Ready,
    Line(String),
    Exited(String),
}

struct Client { tx: Sender<Ev> }

impl client::Handler for Client {
    type Error = russh::Error;
    async fn check_server_key(&mut self, _k: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

fn main() -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<Ev>();

    // --- tokio runtime on a dedicated background thread ---
    let tx2 = tx.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let _ = run(tx2).await;
        });
    });

    // --- this thread is the "UI thread": synchronous, polls with timeout ---
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got_ready = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ev::Ready) => { println!("UI: got Ready (tunnel is up)"); got_ready = true; }
            Ok(Ev::Line(l)) => println!("UI: log line: {l}"),
            Ok(Ev::Exited(why)) => { println!("UI: exited: {why}"); break; }
            Err(RecvTimeoutError::Timeout) => { /* UI would redraw here */ }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if got_ready { break; }
    }
    anyhow::ensure!(got_ready, "never became ready");
    println!("\nRESULT tokio-on-thread + std::sync::mpsc: PASS - no tokio needed on the UI thread");
    Ok(())
}

async fn run(tx: Sender<Ev>) -> anyhow::Result<()> {
    let cfg = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(30)),
        keepalive_interval: Some(Duration::from_secs(10)),
        keepalive_max: 3,
        ..<_>::default()
    });
    let mut h = client::connect(cfg, ("127.0.0.1", 22200u16), Client { tx: tx.clone() }).await?;
    let key = russh::keys::load_secret_key("/tmp/russh-spike/sshd/user_ed25519", None)?;
    let hash = h.best_supported_rsa_hash().await?.flatten();
    let r = h.authenticate_publickey("wangcan", PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await?;
    anyhow::ensure!(r.success(), "auth failed");
    let _ = tx.send(Ev::Line("authenticated".into()));

    // -L: bind and start accepting; emit Ready as soon as the bind succeeds
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let _ = tx.send(Ev::Line(format!("listening on {port}")));
    let _ = tx.send(Ev::Ready);

    // background accept loop
    let _ = port;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let _ = tx.send(Ev::Exited("test finished".into()));
    Ok(())
}
