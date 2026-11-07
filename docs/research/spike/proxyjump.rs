//! Can we do ProxyJump in pure Rust? Run SSH *inside* a direct-tcpip channel
//! by feeding `connect_stream` the channel's AsyncRead+AsyncWrite.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use russh::client::{self, Handle};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::*;

struct Client;
impl client::Handler for Client {
    type Error = russh::Error;
    async fn check_server_key(&mut self, _k: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

async fn auth(mut h: Handle<Client>) -> Result<Handle<Client>> {
    let key = russh::keys::load_secret_key("/tmp/russh-spike/sshd/user_ed25519", None)?;
    let hash = h.best_supported_rsa_hash().await?.flatten();
    let r = h.authenticate_publickey("wangcan", PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await?;
    anyhow::ensure!(r.success(), "auth failed");
    Ok(h)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Arc::new(client::Config { inactivity_timeout: Some(Duration::from_secs(30)), ..<_>::default() });
    let jump = client::connect(cfg.clone(), ("127.0.0.1", 22200u16), Client).await?;
    let jump = auth(jump).await?;
    println!("hop 1 (jump): authenticated");

    let ch = jump.channel_open_direct_tcpip("127.0.0.1", 22200u32, "127.0.0.1", 1234u32).await?;
    println!("hop 2: direct-tcpip channel through jump host opened");

    let stream = ch.into_stream();
    let target = client::connect_stream(
        Arc::new(client::Config { inactivity_timeout: Some(Duration::from_secs(30)), ..<_>::default() }),
        stream,
        Client,
    ).await?;
    let target = auth(target).await?;
    println!("hop 3: authenticated to target *through* the jump channel");

    let granted = target.tcpip_forward("127.0.0.1", 0u32).await?;
    println!("nested -R: server granted remote port {granted}");
    target.cancel_tcpip_forward("127.0.0.1", granted).await?;

    let mut ech = target.channel_open_session().await?;
    ech.exec(true, "echo NESTED_SSH_OK").await?;
    let mut out = Vec::new();
    while let Some(msg) = ech.wait().await {
        match msg {
            ChannelMsg::Data { data } => { println!("  [exec] Data {}B", data.len()); out.extend_from_slice(&data); }
            ChannelMsg::ExtendedData { data, .. } => { println!("  [exec] ExtData {}B", data.len()); out.extend_from_slice(&data); }
            ChannelMsg::Eof => { println!("  [exec] Eof"); break; }
            ChannelMsg::Close => { println!("  [exec] Close"); break; }
            ChannelMsg::ExitStatus { exit_status } => println!("  [exec] exit={exit_status}"),
            other => println!("  [exec] other: {other:?}"),
        }
    }
    let text = String::from_utf8_lossy(&out);
    println!("nested exec output: {:?}", text.trim());
    anyhow::ensure!(text.contains("NESTED_SSH_OK"), "nested session broken");

    println!("\nRESULT ProxyJump: PASS - SSH runs fine inside a direct-tcpip channel");
    Ok(())
}
