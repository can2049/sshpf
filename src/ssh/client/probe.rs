//! Run one command on an SSH host and capture its standard output.
//!
//! This is the transport for the remote-port picker: `ssh::probe` decides *what*
//! to run and [`crate::model::parse_listen_ports`] decides what the answer means,
//! while this file only opens a session, executes the command and collects the
//! bytes.
//!
//! It deliberately reuses [`super::connect`] instead of opening its own TCP/SSH
//! path: a probe must honour exactly what a tunnel honours — `~/.ssh/config`
//! endpoints, `ProxyJump` chains, the same `IdentityFile` / ssh-agent
//! authentication and the same host-key policy. A second connection path is how
//! "the list works but the tunnel does not" happens.
//!
//! Nothing here is user-visible or stateful: the probe runs on its own thread
//! (see `crate::app::port_scan`) and reports a `Result`.

use std::sync::mpsc;

use anyhow::{Context, Result, bail};
use russh::ChannelMsg;

use crate::model::TunnelEvent;
use crate::ssh::config::Endpoint;

use super::connect::{self, Options};

/// How long the command may take before we give up on it.
///
/// A probe is a convenience, never a reason for the UI to feel stuck: the picker
/// shows "scanning" while this runs and stays usable. Ten seconds is generous for
/// `ss`/`netstat` even on a loaded host, and short enough that a wedged shell on
/// the far side does not hide the failure for long.
const EXEC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Most stdout we will accept from the probe, in bytes.
///
/// The answer is a few hundred bytes; a peer that streams megabytes (a shell
/// that starts an interactive program, a `netstat` on a pathological host) must
/// not be able to grow memory without bound. Truncation is reported, not
/// silently ignored — a partial list is still useful but the user is told.
const MAX_OUTPUT: usize = 256 * 1024;

/// Run `command` on `ssh_host` and return its stdout.
///
/// `command` is executed by the server's shell, so the caller must pass a string
/// that is safe by construction (see [`super::probe::list_command`], which only
/// ever emits a fixed chain of `ss` / `netstat` invocations).
pub(crate) async fn exec_capture(ssh_host: &str, opts: &Options, command: &str) -> Result<Vec<u8>> {
    let endpoint = Endpoint::resolve(ssh_host).with_context(|| format!("解析 SSH 主机 {ssh_host} 失败"))?;
    // `connect` reports progress through the tunnel event channel, which a probe
    // has no tunnel to attach to. A private channel that is dropped with this
    // function keeps that reporting harmless: the probe never shows up in a
    // tunnel's log or status.
    let (tx, _rx) = mpsc::channel::<TunnelEvent>();
    let handle = connect::connect(&endpoint, opts, 0, tx, None).await?;

    let mut channel = handle.channel_open_session().await.context("打开 SSH 会话通道失败")?;
    channel.exec(false, command).await.context("在远端执行命令失败")?;
    let output = tokio::time::timeout(EXEC_TIMEOUT, collect_stdout(&mut channel))
        .await
        .map_err(|_| anyhow::anyhow!("远端命令超时（{}s）", EXEC_TIMEOUT.as_secs()))??;

    // Closing is best-effort: the session is torn down with `handle` either way,
    // and a failed close must not throw away a good answer.
    let _ = handle.disconnect(russh::Disconnect::ByApplication, "", "en").await;
    Ok(output)
}

/// Read stdout until the channel ends, the cap is hit or the peer closes it.
///
/// Only `ChannelMsg::Data` (stdout) is collected. `ExtendedData` is stderr and is
/// deliberately dropped: it is where `netstat` writes "command not found", and
/// mixing it into the parsed list would invent port rows out of error text.
async fn collect_stdout(channel: &mut russh::Channel<russh::client::Msg>) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => {
                if out.len().saturating_add(data.len()) > MAX_OUTPUT {
                    bail!("远端返回内容过长（超过 {MAX_OUTPUT} 字节）");
                }
                out.extend_from_slice(&data);
            }
            ChannelMsg::Eof | ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use crate::ssh::probe::list_command;

    #[test]
    fn the_probe_command_never_contains_user_input() {
        // The command is a fixed chain: every word is a literal, so there is
        // nothing for a hostile host alias to inject into. What must never appear
        // is *interpolation* — a `$` (other than the printf format below, which is
        // a literal `%s`), a backtick, or a newline.
        let cmd = list_command();
        assert!(!cmd.contains('$'), "the probe command must not expand a variable: {cmd}");
        assert!(!cmd.contains('`'), "the probe command must not substitute a command: {cmd}");
        assert!(!cmd.contains('\n'), "the probe command must be a single line: {cmd}");
        // No argument is ever spliced in: the only `%` is printf's own literal.
        assert_eq!(cmd.matches('%').count(), 1, "only printf's own format specifier may use %: {cmd}");
    }
}
