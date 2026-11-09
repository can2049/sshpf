//! The remote-port picker: state, the scan thread, and the keys' App methods.
//!
//! Typing a remote port is the one step in "make a tunnel" that asks the user for
//! something the machine next to them already knows. Pressing `p` on the 远端端口
//! field scans the **peer** for listening ports and offers them instead: pick one
//! to fill the field, or tick several and save to create one tunnel per port.
//!
//! # Why a thread and a channel
//!
//! The probe is a real SSH connection (see [`crate::ssh::probe`]), so it cannot
//! run on the UI thread — a 200 ms frame loop that blocks on a TCP handshake is a
//! frozen TUI. It therefore follows the same shape as a tunnel: one thread with a
//! `current_thread` runtime, reporting back over an `mpsc` channel that
//! [`App::drain_events`](crate::app::App::drain_events) polls once per frame.
//!
//! # Why a generation counter
//!
//! The user can change host or re-scan while a probe is in flight. Every scan
//! gets a `seq`, and a result whose `seq` is no longer current is dropped in
//! [`PortPick::install`] — a slow answer for the previous host must never
//! overwrite the list the user is looking at now.
//!
//! Belongs here: opening/closing the picker, starting and applying a scan, and
//! the cursor/tick operations the key handler calls.
//! Does not belong: turning ticked ports into tunnels ([`super::port_batch`]),
//! rendering ([`crate::ui::ports`]), key dispatch ([`crate::keys::ports`]).

use std::sync::mpsc;

use crate::app::{App, FormMode};
use crate::eventlog;
use crate::help;
use crate::model::{self, ForwardKind, PortPick, RemotePort, StatusMsg};
use crate::ssh::probe::{self, ScanOutcome};

/// One finished scan, tagged with the generation it belongs to.
#[derive(Debug)]
pub(crate) enum PortScanMsg {
    Done {
        seq: u64,
        /// `Ok((offerable ports, how many ssh-config-declared ports were
        /// hidden))`, or a bilingual reason the scan produced nothing.
        result: Result<(Vec<RemotePort>, usize), String>,
    },
}

impl App {
    // -- opening / closing -------------------------------------------------

    /// `p` on the 远端端口 field: scan the peer and show its listening ports.
    ///
    /// Refuses (with a status-line reason, never a silent no-op) when the current
    /// kind has no remote port to fill, when the SSH host is still empty, or when
    /// the scan cannot start at all.
    pub fn open_port_picker(&mut self) {
        let Some((host, multi)) = self.port_picker_preconditions() else {
            return;
        };
        let seq = self.next_port_seq();
        self.ports = Some(PortPick::scanning(host.clone(), multi, seq));
        // The two panes both hang off the form and would compete for the keyboard;
        // the one the user just asked for wins.
        self.close_import_pane();
        self.close_host_picker();
        eventlog::port_scan_started(&host, multi);
        self.spawn_port_scan(host, seq);
    }

    /// The host to scan and whether ticking rows may create tunnels, or `None`
    /// (after reporting why) when there is nothing to offer.
    fn port_picker_preconditions(&mut self) -> Option<(String, bool)> {
        let form = self.form.as_ref()?;
        if form.kind() == ForwardKind::Dynamic {
            self.set_status(StatusMsg::warn(help::bi("-D 动态转发没有远端端口", "A -D forward has no remote port")));
            return None;
        }
        let host = form.ssh_host.value().trim().to_string();
        if host.is_empty() {
            self.set_status(StatusMsg::warn(help::bi(
                "先填写 SSH 主机，才能列出对端端口",
                "Fill in the SSH host first, then the peer's ports can be listed",
            )));
            return None;
        }
        // A batch of `-L` forwards is the useful multi-select case: each ticked
        // service becomes its own tunnel. For `-R` the listed ports are the ones
        // already *occupied* on the peer, so the list is read-only: picking one
        // would collide at bind time. Editing an existing tunnel is one tunnel by
        // definition, so it fills the field and never batches.
        let multi = !form.is_modifying() && form.kind() == ForwardKind::Local;
        Some((host, multi))
    }

    /// Collapse the list, keeping nothing.
    pub fn close_port_picker(&mut self) {
        if self.ports.take().is_some() {
            self.port_rx = None;
            eventlog::port_scan_closed();
        }
    }

    /// Whether the list is expanded (and therefore claims its keys).
    pub fn port_picker_open(&self) -> bool {
        self.ports.as_ref().is_some_and(|p| p.open)
    }

    /// Whether the list has something worth drawing.
    pub fn port_picker_visible(&self) -> bool {
        self.ports.as_ref().is_some_and(|p| p.is_visible())
    }

    /// Drop a stale list after the host or the kind changed under it.
    ///
    /// Called once per form key press: cheap string compare, and it is what keeps
    /// "the list is from the host I am looking at" true without wiring a callback
    /// into every field-editing path.
    pub fn sync_port_picker(&mut self) {
        let Some(pick) = self.ports.as_ref() else {
            return;
        };
        let Some(form) = self.form.as_ref() else {
            self.close_port_picker();
            return;
        };
        let host = form.ssh_host.value().trim();
        let stale = pick.host != host || (pick.multi && (!self.can_batch_ports(form)));
        if stale {
            eventlog::port_picker_dropped(&pick.host.clone(), host);
            self.close_port_picker();
        }
    }

    /// Whether the open form may still create one tunnel per ticked port.
    fn can_batch_ports(&self, form: &crate::app::FormState) -> bool {
        matches!(form.mode, FormMode::Create) && form.kind() == ForwardKind::Local
    }

    // -- starting a scan ---------------------------------------------------

    fn next_port_seq(&mut self) -> u64 {
        self.port_seq = self.port_seq.saturating_add(1);
        self.port_seq
    }

    /// Re-scan the same host, keeping the user's ticks.
    pub fn rescan_ports(&mut self) {
        let (host, multi) = match self.ports.as_ref() {
            Some(pick) => (pick.host.clone(), pick.multi),
            None => return,
        };
        let seq = self.next_port_seq();
        if let Some(pick) = self.ports.as_mut() {
            pick.begin_rescan(seq);
        }
        eventlog::port_scan_started(&host, multi);
        self.spawn_port_scan(host, seq);
    }

    /// Start the probe thread for `host`/`seq`, replacing any channel still open.
    fn spawn_port_scan(&mut self, host: String, seq: u64) {
        let (tx, rx) = mpsc::channel::<PortScanMsg>();
        self.port_rx = Some(rx);
        let policy = self.host_key_policy;
        if let Err(e) = spawn_scan_thread(host, policy, seq, tx) {
            // The thread could not be created: report it the same way a failed
            // probe is reported, so the pane still explains itself.
            self.apply_port_scan(
                seq,
                Err(help::bi(
                    &format!("无法启动端口扫描线程: {e}"),
                    &format!("Cannot start the port scan thread: {e}"),
                )),
            );
        }
    }

    /// Apply one finished scan (dropped when a newer generation has taken over).
    pub(crate) fn apply_port_scan(&mut self, seq: u64, result: Result<(Vec<RemotePort>, usize), String>) {
        let Some(pick) = self.ports.as_mut() else {
            return;
        };
        match result {
            Ok((ports, hidden)) => {
                if !pick.install(seq, ports, hidden) {
                    eventlog::port_scan_stale(seq, pick.seq);
                    return;
                }
                eventlog::port_scan_finished(&pick.host, pick.len(), hidden);
                // A scan that found nothing is not a failure, but it does need a
                // sentence: otherwise the pane opens onto an empty box.
                if pick.is_empty() {
                    let host = pick.host.clone();
                    self.set_status(StatusMsg::info(help::bi(
                        &format!("{host} 上没有发现正在监听的端口"),
                        &format!("No listening port found on {host}"),
                    )));
                }
            }
            Err(reason) => {
                if !pick.fail(seq, reason.clone()) {
                    eventlog::port_scan_stale(seq, pick.seq);
                    return;
                }
                eventlog::port_scan_failed(&pick.host, &reason);
                self.set_status(StatusMsg::warn(reason));
            }
        }
    }

    /// Drain finished scans. Called once per frame from `drain_events`.
    pub fn drain_port_scan(&mut self) {
        let Some(rx) = self.port_rx.as_ref() else {
            return;
        };
        // Keep only the newest answer: they arrive one at a time in practice, and
        // an older one applied last would be a visible state regression.
        let mut latest: Option<PortScanMsg> = None;
        while let Ok(msg) = rx.try_recv() {
            latest = Some(msg);
        }
        match latest {
            Some(PortScanMsg::Done { seq, result }) => {
                self.port_rx = None;
                self.apply_port_scan(seq, result);
            }
            None => {}
        }
    }

    // -- cursor and ticks --------------------------------------------------

    pub fn port_move(&mut self, delta: i32) {
        if let Some(pick) = self.ports.as_mut() {
            pick.move_cursor(delta);
        }
    }

    pub fn port_home(&mut self) {
        if let Some(pick) = self.ports.as_mut() {
            pick.home();
        }
    }

    pub fn port_end(&mut self) {
        if let Some(pick) = self.ports.as_mut() {
            pick.end();
        }
    }

    pub fn port_toggle_cursor(&mut self) {
        if let Some(pick) = self.ports.as_mut() {
            pick.toggle_cursor();
        }
    }

    pub fn port_check_all(&mut self) {
        if let Some(pick) = self.ports.as_mut() {
            pick.check_all();
        }
    }

    pub fn port_uncheck_all(&mut self) {
        if let Some(pick) = self.ports.as_mut() {
            pick.uncheck_all();
        }
    }

    pub fn port_invert(&mut self) {
        if let Some(pick) = self.ports.as_mut() {
            pick.invert();
        }
    }

    /// `enter` on a row: put that port in the field and collapse the list.
    ///
    /// This is the single-selection path — the user reviews and saves one tunnel.
    /// Ticking rows and saving is the multi path ([`super::port_batch`]); the two
    /// are deliberately different gestures so neither has to guess what a tick
    /// means.
    pub fn port_accept_cursor(&mut self) {
        let Some(row) = self.ports.as_ref().and_then(|p| p.cursor_row()).cloned() else {
            return;
        };
        let port = row.port.port;
        let suggested_host = row.port.suggest_target_host().map(str::to_string);
        if let Some(form) = self.form.as_mut() {
            form.remote_port.set(port.to_string());
            // Only replace the target address when the field still holds the
            // default: a concrete bind address is a better guess than
            // `127.0.0.1`, but never better than something the user typed.
            if let Some(host) = suggested_host
                && is_default_remote_host(form.remote_host.value())
            {
                form.remote_host.set(host);
            }
            form.revalidate();
        }
        self.refresh_local_port_suggestion();
        self.close_port_picker();
        self.set_status(StatusMsg::ok(help::bi(
            &format!("已填入远端端口 {port}（可继续编辑）"),
            &format!("Remote port {port} filled in (still editable)"),
        )));
    }
}

/// Whether a target-address buffer still holds the untouched default.
///
/// `127.0.0.1` is what a new form starts with, so treating it as "nothing typed"
/// lets the discovered bind address — which is where the service really is —
/// replace it. Anything else is a deliberate value and is left alone.
fn is_default_remote_host(value: &str) -> bool {
    matches!(value.trim(), "" | "127.0.0.1" | "localhost")
}

/// Spawn the probe thread; returns the spawn error so the caller can report it.
fn spawn_scan_thread(
    host: String,
    policy: crate::ssh::known_hosts::HostKeyPolicy,
    seq: u64,
    tx: mpsc::Sender<PortScanMsg>,
) -> std::io::Result<()> {
    std::thread::Builder::new().name("sshpf-portscan".to_string()).spawn(move || {
        let result = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt.block_on(run_scan(&host, policy)),
            Err(e) => {
                Err(help::bi(&format!("无法启动扫描运行时: {e}"), &format!("Cannot start the scan runtime: {e}")))
            }
        };
        // The receiver is gone when the user closed the pane; nothing to do then.
        let _ = tx.send(PortScanMsg::Done { seq, result });
    })?;
    Ok(())
}

/// One probe, filtered against what the host's ssh config already declares.
///
/// Runs on the scan thread (never the UI thread): it opens a real SSH session and
/// reads the local ssh config.
async fn run_scan(
    host: &str,
    policy: crate::ssh::known_hosts::HostKeyPolicy,
) -> Result<(Vec<RemotePort>, usize), String> {
    let outcome = probe::scan_listen_ports(host, policy).await.map_err(|e| {
        help::bi(&format!("扫描 {host} 的端口失败: {e}"), &format!("Scanning the ports of {host} failed: {e}"))
    })?;
    let ports = match outcome {
        ScanOutcome::Found(ports) => ports,
        ScanOutcome::NoTool => {
            return Err(help::bi(
                &format!("{host} 上既没有 ss 也没有 netstat，无法列出端口"),
                &format!("{host} has neither ss nor netstat, so its ports cannot be listed"),
            ));
        }
        ScanOutcome::Unparsed => {
            return Err(help::bi(
                &format!("{host} 的端口列表格式无法解析（只支持 ss / netstat -l）"),
                &format!("The port list from {host} is in an unsupported format (ss / netstat -l only)"),
            ));
        }
    };
    // The requirement: do not re-offer what `~/.ssh/config` already enumerates —
    // those rows belong to the import pane, and two ways to create the same
    // tunnel is how a user ends up with two.
    let declared = crate::ssh::forwards::forwards_for_host(host)
        .map(|forwards| model::declared_ports(&forwards))
        .unwrap_or_default();
    Ok(model::filter_declared(ports, &declared))
}
