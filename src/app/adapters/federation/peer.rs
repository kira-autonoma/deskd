//! Federation peer dialer (#462) — connects to a hub and stays connected,
//! plus forward middleware integration (#463).
//!
//! [`run_peer`] runs a connect/handshake/keepalive/reconnect loop with
//! exponential backoff from config. After welcome, the peer issues any
//! configured `Subscribe`/`SubscribeInbox` frames so the hub starts forwarding
//! matching traffic over the link. Inbound `Forward` frames are handed to the
//! configured [`LocalBusInjector`].
//!
//! Locally-originated publishes that should reach the hub are sent through
//! the per-connection `outbound` channel returned by [`PeerSessionHandle`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::forward::{LocalBusInjector, NullInjector, PEER_CHANNEL_CAPACITY};
use super::frame::FederationFrame;
use super::hub::KEEPALIVE_INTERVAL;

/// Configuration handed to [`run_peer`].
#[derive(Clone)]
pub struct PeerDialerConfig {
    /// `host:port` of the hub.
    pub hub_addr: String,
    /// Identifier this peer announces in `hello`.
    pub peer_name: String,
    /// Reconnect backoff sequence (seconds). Cycles through and stays at the
    /// final value once exhausted.
    pub reconnect_backoff_secs: Vec<u64>,
    /// `deskd_version` value to advertise. Typically `env!("CARGO_PKG_VERSION")`.
    pub deskd_version: String,
    /// Topic-glob subscriptions to issue immediately after welcome.
    #[doc(alias = "subscriptions")]
    pub subscribe_patterns: Vec<String>,
    /// Inbox subscriptions to issue immediately after welcome.
    pub subscribe_inboxes: Vec<String>,
}

impl Default for PeerDialerConfig {
    fn default() -> Self {
        Self {
            hub_addr: String::new(),
            peer_name: String::new(),
            reconnect_backoff_secs: vec![1, 2, 5, 15, 60],
            deskd_version: "0.0.0".into(),
            subscribe_patterns: Vec::new(),
            subscribe_inboxes: Vec::new(),
        }
    }
}

/// Handle for sending frames over the *current* peer session. The dialer
/// swaps the inner sender on every reconnect; frames enqueued while
/// disconnected are silently dropped (we don't queue across reconnects in v1
/// — that's the replay log work in #464).
///
/// The underlying channel is bounded ([`PEER_CHANNEL_CAPACITY`]); `send`
/// drops the frame on `Full` so a stalled hub can never pin unbounded memory
/// on the peer side either.
#[derive(Clone, Default)]
pub struct PeerSessionHandle {
    inner: Arc<Mutex<Option<mpsc::Sender<FederationFrame>>>>,
}

impl PeerSessionHandle {
    /// Construct an empty handle. Initially disconnected.
    pub fn new() -> Self {
        Self::default()
    }

    /// Try to send a frame over the current session. Returns `false` if the
    /// session is disconnected, or if the outbound channel is currently full
    /// (slow hub) — the frame is dropped in that case.
    pub async fn send(&self, frame: FederationFrame) -> bool {
        let guard = self.inner.lock().await;
        if let Some(tx) = guard.as_ref() {
            tx.try_send(frame).is_ok()
        } else {
            false
        }
    }

    async fn set(&self, tx: Option<mpsc::Sender<FederationFrame>>) {
        let mut guard = self.inner.lock().await;
        *guard = tx;
    }
}

/// Run the dialer until cancelled. The returned [`PeerSessionHandle`] is a
/// future-proof outbound API for child 2 wiring on the peer side.
pub async fn run_peer(cfg: PeerDialerConfig, cancel: CancellationToken) -> Result<()> {
    run_peer_with_injector(
        cfg,
        Arc::new(NullInjector) as Arc<dyn LocalBusInjector>,
        PeerSessionHandle::new(),
        cancel,
    )
    .await
}

/// Run the dialer with an injector + session handle supplied by serve() glue
/// or tests.
pub async fn run_peer_with_injector(
    cfg: PeerDialerConfig,
    injector: Arc<dyn LocalBusInjector>,
    session: PeerSessionHandle,
    cancel: CancellationToken,
) -> Result<()> {
    let mut attempt: usize = 0;
    let backoff = if cfg.reconnect_backoff_secs.is_empty() {
        vec![1u64, 2, 5, 15, 60]
    } else {
        cfg.reconnect_backoff_secs.clone()
    };

    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        match dial_once(&cfg, injector.as_ref(), &session, &cancel).await {
            Ok(SessionExit::Cancelled) => return Ok(()),
            Ok(SessionExit::Disconnected) => {
                info!(
                    target: "deskd::federation::peer",
                    hub = %cfg.hub_addr,
                    "disconnected from hub; will reconnect"
                );
            }
            Err(e) => {
                warn!(
                    target: "deskd::federation::peer",
                    hub = %cfg.hub_addr,
                    error = %e,
                    "hub dial failed"
                );
            }
        }
        // Clear the session handle while disconnected so locally-originated
        // publishes don't try to ride a dead connection.
        session.set(None).await;
        let delay_secs = *backoff
            .get(attempt.min(backoff.len().saturating_sub(1)))
            .unwrap_or(&60);
        attempt = attempt.saturating_add(1);
        debug!(
            target: "deskd::federation::peer",
            attempt,
            delay_secs,
            "backing off before reconnect"
        );
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_secs(delay_secs)) => {}
        }
    }
}

/// Outcome of a single connect attempt.
enum SessionExit {
    /// Cancel token fired — exit cleanly.
    Cancelled,
    /// Session ended by remote close, missed pongs, or read error.
    Disconnected,
}

async fn dial_once(
    cfg: &PeerDialerConfig,
    injector: &dyn LocalBusInjector,
    session: &PeerSessionHandle,
    cancel: &CancellationToken,
) -> Result<SessionExit> {
    info!(
        target: "deskd::federation::peer",
        hub = %cfg.hub_addr,
        peer_name = %cfg.peer_name,
        "dialing hub"
    );
    let stream = TcpStream::connect(&cfg.hub_addr)
        .await
        .with_context(|| format!("connect to {}", cfg.hub_addr))?;

    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let hello = FederationFrame::Hello {
        peer_name: cfg.peer_name.clone(),
        deskd_version: cfg.deskd_version.clone(),
        capabilities: vec![],
    };
    writer
        .write_all(hello.to_line()?.as_bytes())
        .await
        .context("write hello")?;
    writer.flush().await.ok();

    // Await welcome.
    let line = match tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await {
        Ok(Ok(Some(l))) => l,
        Ok(Ok(None)) => {
            return Ok(SessionExit::Disconnected);
        }
        Ok(Err(e)) => return Err(anyhow::Error::from(e).context("read welcome")),
        Err(_) => {
            warn!(
                target: "deskd::federation::peer",
                hub = %cfg.hub_addr,
                "welcome timed out"
            );
            return Ok(SessionExit::Disconnected);
        }
    };
    match FederationFrame::parse(&line) {
        Ok(FederationFrame::Welcome { hub_name, .. }) => {
            info!(
                target: "deskd::federation::peer",
                hub = %cfg.hub_addr,
                hub_name = %hub_name,
                "handshake complete"
            );
        }
        Ok(other) => {
            warn!(
                target: "deskd::federation::peer",
                received = ?other,
                "first frame from hub was not welcome — dropping"
            );
            return Ok(SessionExit::Disconnected);
        }
        Err(e) => {
            warn!(
                target: "deskd::federation::peer",
                error = %e,
                "malformed welcome — dropping"
            );
            return Ok(SessionExit::Disconnected);
        }
    }

    // ── Outbound channel + writer task ──────────────────────────────────
    let (out_tx, mut out_rx) = mpsc::channel::<FederationFrame>(PEER_CHANNEL_CAPACITY);
    session.set(Some(out_tx.clone())).await;

    // Issue config-driven subscriptions immediately after welcome.
    for pattern in &cfg.subscribe_patterns {
        let _ = out_tx.try_send(FederationFrame::Subscribe {
            pattern: pattern.clone(),
        });
    }
    for inbox in &cfg.subscribe_inboxes {
        let _ = out_tx.try_send(FederationFrame::SubscribeInbox {
            name: inbox.clone(),
        });
    }

    let writer_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let line = match frame.to_line() {
                Ok(l) => l,
                Err(_) => continue,
            };
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
    });

    // ── Keepalive + read loop ───────────────────────────────────────────
    let mut last_pong = Instant::now();
    let mut missed: u8 = 0;
    let mut ticker = tokio::time::interval(KEEPALIVE_INTERVAL);
    ticker.tick().await;

    let result = loop {
        tokio::select! {
            _ = cancel.cancelled() => break Ok(SessionExit::Cancelled),
            _ = ticker.tick() => {
                if last_pong.elapsed() > KEEPALIVE_INTERVAL.saturating_mul(2) {
                    missed += 1;
                    if missed >= 2 {
                        info!(
                            target: "deskd::federation::peer",
                            "hub missed 2 pongs — dropping session"
                        );
                        break Ok(SessionExit::Disconnected);
                    }
                }
                if out_tx.try_send(FederationFrame::Ping).is_err() {
                    break Ok(SessionExit::Disconnected);
                }
            }
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(l)) => l,
                    Ok(None) | Err(_) => break Ok(SessionExit::Disconnected),
                };
                match FederationFrame::parse(&line) {
                    Ok(FederationFrame::Pong) => {
                        last_pong = Instant::now();
                        missed = 0;
                    }
                    Ok(FederationFrame::Ping) => {
                        let _ = out_tx.try_send(FederationFrame::Pong);
                    }
                    Ok(FederationFrame::Forward { origin, message }) => {
                        debug!(
                            target: "deskd::federation::peer",
                            origin = %origin,
                            target = %message.target,
                            "injecting forwarded frame into local bus"
                        );
                        injector.inject(&origin, &message);
                    }
                    Ok(FederationFrame::BusMessage(message)) => {
                        // Legacy / origin-less; inject as if it came from the
                        // hub.
                        injector.inject("hub", &message);
                    }
                    Ok(FederationFrame::Subscribe { .. })
                    | Ok(FederationFrame::Unsubscribe { .. })
                    | Ok(FederationFrame::SubscribeInbox { .. }) => {
                        debug!(
                            target: "deskd::federation::peer",
                            "ignoring subscribe-class frame from hub"
                        );
                    }
                    Ok(FederationFrame::Hello { .. }) | Ok(FederationFrame::Welcome { .. }) => {
                        debug!(target: "deskd::federation::peer", "ignoring unexpected handshake frame mid-session");
                    }
                    Err(e) => {
                        warn!(
                            target: "deskd::federation::peer",
                            error = %e,
                            "malformed frame from hub — dropping"
                        );
                        break Ok(SessionExit::Disconnected);
                    }
                }
            }
        }
    };

    drop(out_tx);
    writer_task.abort();
    session.set(None).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::adapters::federation::forward::{
        ForwardMiddleware, NullInjector, PeerConnections,
    };
    use crate::app::adapters::federation::hub::{HubConfig, run_hub_with_middleware};
    use crate::app::adapters::federation::preflight::testing::{StubIdentity, status_with};
    use crate::app::adapters::federation::preflight::{SharedIdentity, TailnetIdentity};
    use crate::app::adapters::federation::registry::PeerRegistry;
    use crate::app::adapters::federation::routing::RoutingTable;
    use std::sync::Arc;

    async fn spawn_test_hub() -> (String, CancellationToken, Arc<PeerRegistry>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        let identity: SharedIdentity = Arc::new(StubIdentity::ok(status_with(
            "Running",
            &["100.64.0.1"],
            &[],
        ))) as Arc<dyn TailnetIdentity>;
        let registry = Arc::new(PeerRegistry::in_memory().unwrap());
        let cancel = CancellationToken::new();
        let cfg = HubConfig {
            bind: addr.clone(),
            hub_name: "test-hub".into(),
            tailscale_binary: "true".into(),
            peer_timeout_secs: 60,
        };
        let middleware = Arc::new(ForwardMiddleware::new(
            Arc::new(RoutingTable::new()),
            Arc::new(PeerConnections::new()),
            Arc::new(NullInjector),
            "test-hub".to_string(),
        ));
        let reg2 = registry.clone();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            let _ = run_hub_with_middleware(cfg, identity, reg2, middleware, cancel2).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        (addr, cancel, registry)
    }

    #[tokio::test]
    async fn peer_dialer_completes_handshake_and_appears_in_registry() {
        let (addr, hub_cancel, registry) = spawn_test_hub().await;

        let peer_cancel = CancellationToken::new();
        let pc = PeerDialerConfig {
            hub_addr: addr.clone(),
            peer_name: "mac".into(),
            reconnect_backoff_secs: vec![1],
            deskd_version: "test".into(),
            ..Default::default()
        };
        let pc_cancel = peer_cancel.clone();
        let handle = tokio::spawn(async move {
            let _ = run_peer(pc, pc_cancel).await;
        });

        // Poll for the peer record to land in the registry.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if registry.lookup("mac").unwrap().is_some() {
                break;
            }
            if Instant::now() > deadline {
                panic!("peer never registered with hub");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        peer_cancel.cancel();
        hub_cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn peer_dialer_reconnects_when_hub_starts_late() {
        // Pick a free port; nothing listens yet.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);

        let peer_cancel = CancellationToken::new();
        let pc = PeerDialerConfig {
            hub_addr: addr.clone(),
            peer_name: "mac".into(),
            reconnect_backoff_secs: vec![1],
            deskd_version: "test".into(),
            ..Default::default()
        };
        let pc_cancel = peer_cancel.clone();
        let handle = tokio::spawn(async move {
            let _ = run_peer(pc, pc_cancel).await;
        });

        // Give the dialer one or two failures, then bring the hub up.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let identity: SharedIdentity = Arc::new(StubIdentity::ok(status_with(
            "Running",
            &["100.64.0.1"],
            &[],
        ))) as Arc<dyn TailnetIdentity>;
        let registry = Arc::new(PeerRegistry::in_memory().unwrap());
        let hub_cancel = CancellationToken::new();
        let cfg = HubConfig {
            bind: addr.clone(),
            hub_name: "test-hub".into(),
            tailscale_binary: "true".into(),
            peer_timeout_secs: 60,
        };
        let middleware = Arc::new(ForwardMiddleware::new(
            Arc::new(RoutingTable::new()),
            Arc::new(PeerConnections::new()),
            Arc::new(NullInjector),
            "test-hub".to_string(),
        ));
        let reg2 = registry.clone();
        let hub_cancel2 = hub_cancel.clone();
        tokio::spawn(async move {
            let _ = run_hub_with_middleware(cfg, identity, reg2, middleware, hub_cancel2).await;
        });

        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if registry.lookup("mac").unwrap().is_some() {
                break;
            }
            if Instant::now() > deadline {
                panic!("peer never registered with hub after reconnect");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        peer_cancel.cancel();
        hub_cancel.cancel();
        let _ = handle.await;
    }
}
