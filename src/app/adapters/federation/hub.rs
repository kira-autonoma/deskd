//! Federation hub — TCP listener that accepts peer connections (#462) +
//! forward middleware (#463).
//!
//! On startup [`run_hub`] runs a tailscale preflight, resolves the bind spec,
//! and binds a TCP listener. Each accepted connection is checked against the
//! current set of tailnet-allowed source IPs and, if approved, handed to a
//! per-connection task that runs the hello/welcome handshake, registers an
//! outbound channel in [`PeerConnections`], and pumps frames in both
//! directions:
//!
//! - **Outbound**: any frame published into the per-peer mpsc channel is
//!   written to the TCP stream. This is how the [`ForwardMiddleware`]'s
//!   fanout reaches connected peers.
//! - **Inbound**: subscribe/unsubscribe control frames update the routing
//!   table; `Forward { origin, message }` frames are injected into the local
//!   bus and re-fanned-out to other matching peers; legacy `BusMessage`
//!   frames are treated as `Forward { origin = peer_name }`.
//!
//! On disconnect the peer's outbound channel and routing-table entries are
//! dropped; the peer re-issues its subscriptions on reconnect.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::forward::{ForwardMiddleware, NullInjector, PEER_CHANNEL_CAPACITY, PeerConnections};
use super::frame::FederationFrame;
use super::preflight::{SharedIdentity, TailnetIdentity, preflight_or_error, resolve_bind};
use super::registry::PeerRegistry;
use super::routing::RoutingTable;

/// Keepalive cadence — ping every this often, drop after 2 missed pongs.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
/// How long to wait for the peer's `hello` after a connection is accepted.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum byte length accepted for a `Subscribe.pattern` or
/// `SubscribeInbox.name`. Frames exceeding this are dropped (and logged) so a
/// malicious or buggy peer cannot inflate routing-table keys without bound.
pub const MAX_SUBSCRIBE_PATTERN_BYTES: usize = 1024;

/// Configuration handed to [`run_hub`].
#[derive(Clone)]
pub struct HubConfig {
    /// Bind spec from `federation.hub.bind` (e.g. `tailscale0:7770`).
    pub bind: String,
    /// Name this hub identifies as in the `welcome` frame.
    pub hub_name: String,
    /// `tailscale` CLI binary path (used by `resolve_bind`).
    pub tailscale_binary: String,
    /// Idle peer timeout (from config). Unused for now — reserved for child 2.
    pub peer_timeout_secs: u64,
}

/// Run the hub listener until `cancel` is triggered. Blocks until the listener
/// loop exits (cancelled, or the accept call returns a fatal error).
///
/// The hub is built without a routing layer — callers that want federation
/// fanout should use [`run_hub_with_middleware`].
pub async fn run_hub(
    cfg: HubConfig,
    identity: SharedIdentity,
    registry: Arc<PeerRegistry>,
    cancel: CancellationToken,
) -> Result<()> {
    let middleware = Arc::new(ForwardMiddleware::new(
        Arc::new(RoutingTable::new()),
        Arc::new(PeerConnections::new()),
        Arc::new(NullInjector),
        cfg.hub_name.clone(),
    ));
    run_hub_with_middleware(cfg, identity, registry, middleware, cancel).await
}

/// Run the hub listener with an explicit forward middleware. Used by serve()
/// glue and integration tests that want to observe fanout.
pub async fn run_hub_with_middleware(
    cfg: HubConfig,
    identity: SharedIdentity,
    registry: Arc<PeerRegistry>,
    middleware: Arc<ForwardMiddleware>,
    cancel: CancellationToken,
) -> Result<()> {
    // Preflight: tailscale must be Running.
    let initial_status = preflight_or_error(identity.as_ref())
        .await
        .context("federation hub preflight")?;
    info!(
        target: "deskd::federation::hub",
        backend_state = %initial_status.backend_state,
        "federation hub preflight ok"
    );

    let bind_addr = resolve_bind(&cfg.bind, &cfg.tailscale_binary)
        .await
        .with_context(|| format!("resolve federation.hub.bind={}", cfg.bind))?;

    let listener = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("bind federation hub on {bind_addr}"))?;
    let local = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| bind_addr.clone());
    info!(
        target: "deskd::federation::hub",
        local_addr = %local,
        "federation hub listening"
    );

    // Cache of allowed tailnet IPs — refreshed on a slow timer.
    let allowed = Arc::new(Mutex::new(initial_status.allowed_ips()));
    spawn_identity_refresh(identity.clone(), allowed.clone(), cancel.clone());

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!(target: "deskd::federation::hub", "shutdown requested");
                return Ok(());
            }
            accept = listener.accept() => {
                match accept {
                    Ok((stream, peer_addr)) => {
                        let allowed = allowed.clone();
                        let registry = registry.clone();
                        let hub_name = cfg.hub_name.clone();
                        let cancel = cancel.clone();
                        let middleware = middleware.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(
                                stream,
                                peer_addr.ip(),
                                allowed,
                                registry,
                                hub_name,
                                middleware,
                                cancel,
                            ).await {
                                debug!(
                                    target: "deskd::federation::hub",
                                    error = %e,
                                    peer = %peer_addr,
                                    "connection closed"
                                );
                            }
                        });
                    }
                    Err(e) => {
                        warn!(target: "deskd::federation::hub", error = %e, "accept error");
                    }
                }
            }
        }
    }
}

/// Spawn a background task that refreshes the allowed-IP set every 60s.
fn spawn_identity_refresh(
    identity: SharedIdentity,
    allowed: Arc<Mutex<std::collections::HashSet<String>>>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        // First tick fires immediately; skip it (we already preflighted).
        interval.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = interval.tick() => {
                    match identity.status().await {
                        Ok(status) => {
                            let new_set = status.allowed_ips();
                            *allowed.lock().await = new_set;
                        }
                        Err(e) => {
                            warn!(
                                target: "deskd::federation::hub",
                                error = %e,
                                "tailscale status refresh failed; keeping previous allowed set"
                            );
                        }
                    }
                }
            }
        }
    });
}

/// Handle a single accepted connection: auth-gate by source IP, run the
/// hello/welcome handshake, register the peer's outbound channel, then pump
/// frames + keepalive until disconnect.
async fn handle_connection(
    stream: TcpStream,
    peer_ip: IpAddr,
    allowed: Arc<Mutex<std::collections::HashSet<String>>>,
    registry: Arc<PeerRegistry>,
    hub_name: String,
    middleware: Arc<ForwardMiddleware>,
    cancel: CancellationToken,
) -> Result<()> {
    // ── Auth gate ───────────────────────────────────────────────────────
    let peer_ip_s = peer_ip.to_string();
    let pass = {
        let set = allowed.lock().await;
        // Loopback is permitted to support local integration tests + the
        // hub-and-peer-on-same-host case noted in the ticket.
        peer_ip.is_loopback() || set.contains(&peer_ip_s)
    };
    if !pass {
        warn!(
            target: "deskd::federation::hub",
            source_ip = %peer_ip_s,
            "rejecting connection: source ip not in tailnet allowlist"
        );
        drop(stream);
        return Ok(());
    }

    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // ── Wait for hello ──────────────────────────────────────────────────
    let line = match timeout(HELLO_TIMEOUT, lines.next_line()).await {
        Ok(Ok(Some(l))) => l,
        Ok(Ok(None)) => {
            debug!(
                target: "deskd::federation::hub",
                source_ip = %peer_ip_s,
                "peer closed before hello"
            );
            return Ok(());
        }
        Ok(Err(e)) => {
            return Err(anyhow::Error::from(e).context("read hello"));
        }
        Err(_) => {
            warn!(
                target: "deskd::federation::hub",
                source_ip = %peer_ip_s,
                "hello timed out"
            );
            return Ok(());
        }
    };

    let frame = FederationFrame::parse(&line).context("parse hello")?;
    let peer_name = match frame {
        FederationFrame::Hello { peer_name, .. } => peer_name,
        other => {
            warn!(
                target: "deskd::federation::hub",
                source_ip = %peer_ip_s,
                received = ?other,
                "first frame was not hello — dropping connection"
            );
            return Ok(());
        }
    };

    let now = now_secs();
    registry
        .upsert_on_hello(&peer_name, Some(&peer_ip_s), now)
        .context("registry upsert")?;
    info!(
        target: "deskd::federation::hub",
        peer_name = %peer_name,
        source_ip = %peer_ip_s,
        "peer connected"
    );

    // ── Send welcome ────────────────────────────────────────────────────
    let welcome = FederationFrame::Welcome {
        hub_name,
        replay_supported: false,
    };
    writer
        .write_all(welcome.to_line()?.as_bytes())
        .await
        .context("write welcome")?;
    writer.flush().await.ok();

    // ── Wire up outbound channel ────────────────────────────────────────
    // The writer task drains this channel onto the TCP socket. Both the
    // keepalive timer and the forward middleware enqueue frames here.
    // Bounded so a slow peer cannot pin unbounded memory on the hub.
    let (out_tx, mut out_rx) = mpsc::channel::<FederationFrame>(PEER_CHANNEL_CAPACITY);
    middleware.peers.insert(&peer_name, out_tx.clone());

    let peer_name_for_writer = peer_name.clone();
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let line = match frame.to_line() {
                Ok(l) => l,
                Err(e) => {
                    warn!(
                        target: "deskd::federation::hub",
                        peer_name = %peer_name_for_writer,
                        error = %e,
                        "frame serialize failed; dropping"
                    );
                    continue;
                }
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
    let mut missed = 0u8;
    let mut ticker = tokio::time::interval(KEEPALIVE_INTERVAL);
    ticker.tick().await; // immediate first tick — skip

    let result = loop {
        tokio::select! {
            _ = cancel.cancelled() => break Ok(()),
            _ = ticker.tick() => {
                if last_pong.elapsed() > KEEPALIVE_INTERVAL.saturating_mul(2) {
                    missed += 1;
                    if missed >= 2 {
                        info!(
                            target: "deskd::federation::hub",
                            peer_name = %peer_name,
                            "dropping peer: 2 missed pongs"
                        );
                        break Ok(());
                    }
                }
                if out_tx.try_send(FederationFrame::Ping).is_err() {
                    break Ok(());
                }
            }
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(l)) => l,
                    Ok(None) => break Ok(()),
                    Err(_) => break Ok(()),
                };
                match FederationFrame::parse(&line) {
                    Ok(FederationFrame::Pong) => {
                        last_pong = Instant::now();
                        missed = 0;
                    }
                    Ok(FederationFrame::Ping) => {
                        let _ = out_tx.try_send(FederationFrame::Pong);
                    }
                    Ok(FederationFrame::Subscribe { pattern }) => {
                        if pattern.len() > MAX_SUBSCRIBE_PATTERN_BYTES {
                            warn!(
                                target: "deskd::federation::hub",
                                peer_name = %peer_name,
                                pattern_bytes = pattern.len(),
                                limit = MAX_SUBSCRIBE_PATTERN_BYTES,
                                "dropping Subscribe with oversize pattern"
                            );
                        } else {
                            debug!(
                                target: "deskd::federation::hub",
                                peer_name = %peer_name,
                                pattern = %pattern,
                                "peer subscribed"
                            );
                            middleware.routing.subscribe(&peer_name, &pattern);
                        }
                    }
                    Ok(FederationFrame::Unsubscribe { pattern }) => {
                        if pattern.len() > MAX_SUBSCRIBE_PATTERN_BYTES {
                            warn!(
                                target: "deskd::federation::hub",
                                peer_name = %peer_name,
                                pattern_bytes = pattern.len(),
                                limit = MAX_SUBSCRIBE_PATTERN_BYTES,
                                "dropping Unsubscribe with oversize pattern"
                            );
                        } else {
                            debug!(
                                target: "deskd::federation::hub",
                                peer_name = %peer_name,
                                pattern = %pattern,
                                "peer unsubscribed"
                            );
                            middleware.routing.unsubscribe(&peer_name, &pattern);
                        }
                    }
                    Ok(FederationFrame::SubscribeInbox { name }) => {
                        if name.len() > MAX_SUBSCRIBE_PATTERN_BYTES {
                            warn!(
                                target: "deskd::federation::hub",
                                peer_name = %peer_name,
                                name_bytes = name.len(),
                                limit = MAX_SUBSCRIBE_PATTERN_BYTES,
                                "dropping SubscribeInbox with oversize name"
                            );
                        } else {
                            debug!(
                                target: "deskd::federation::hub",
                                peer_name = %peer_name,
                                inbox = %name,
                                "peer subscribed inbox"
                            );
                            middleware.routing.subscribe_inbox(&peer_name, &name);
                        }
                    }
                    Ok(FederationFrame::Forward { origin, message }) => {
                        middleware.handle_inbound(&peer_name, &origin, &message);
                    }
                    Ok(FederationFrame::BusMessage(message)) => {
                        // Legacy frame (no origin tracked) — treat the peer
                        // as origin so the message can't loop back.
                        middleware.handle_inbound(&peer_name, &peer_name, &message);
                    }
                    Ok(FederationFrame::Hello { .. }) | Ok(FederationFrame::Welcome { .. }) => {
                        debug!(
                            target: "deskd::federation::hub",
                            peer_name = %peer_name,
                            "ignoring unexpected handshake frame mid-session"
                        );
                    }
                    Err(e) => {
                        warn!(
                            target: "deskd::federation::hub",
                            peer_name = %peer_name,
                            error = %e,
                            "malformed frame — dropping connection"
                        );
                        break Ok(());
                    }
                }
                // Touch registry last_seen on every observed line.
                let _ = registry.upsert_on_hello(&peer_name, Some(&peer_ip_s), now_secs());
            }
        }
    };

    // ── Cleanup ─────────────────────────────────────────────────────────
    middleware.peers.remove(&peer_name);
    middleware.routing.drop_peer(&peer_name);
    drop(out_tx);
    writer_task.abort();
    info!(
        target: "deskd::federation::hub",
        peer_name = %peer_name,
        "peer disconnected — subscriptions dropped"
    );
    result
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Convenience constructor used by `serve` glue and tests.
pub fn shared_identity_from_cli() -> SharedIdentity {
    Arc::new(super::preflight::CliTailnetIdentity::new()) as Arc<dyn TailnetIdentity>
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::adapters::federation::preflight::testing::{StubIdentity, status_with};
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    async fn start_hub_on(
        addr: &str,
        identity: SharedIdentity,
    ) -> (
        Arc<PeerRegistry>,
        Arc<ForwardMiddleware>,
        String,
        CancellationToken,
    ) {
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let local = listener.local_addr().unwrap().to_string();
        drop(listener);
        let registry = Arc::new(PeerRegistry::in_memory().unwrap());
        let cancel = CancellationToken::new();
        let cfg = HubConfig {
            bind: local.clone(),
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
        let mw2 = middleware.clone();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            let _ = run_hub_with_middleware(cfg, identity, reg2, mw2, cancel2).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        (registry, middleware, local, cancel)
    }

    #[tokio::test]
    async fn hello_welcome_roundtrip_and_registry_persist() {
        let identity = Arc::new(StubIdentity::ok(status_with(
            "Running",
            &["100.64.0.1"],
            &[],
        ))) as SharedIdentity;
        let (registry, _mw, addr, cancel) = start_hub_on("127.0.0.1:0", identity).await;

        let mut stream = TcpStream::connect(&addr).await.unwrap();
        let hello = FederationFrame::Hello {
            peer_name: "mac".into(),
            deskd_version: "0.1.0".into(),
            capabilities: vec![],
        };
        stream
            .write_all(hello.to_line().unwrap().as_bytes())
            .await
            .unwrap();
        stream.flush().await.unwrap();

        let (r, _w) = stream.split();
        let mut lines = BufReader::new(r).lines();
        let line = lines.next_line().await.unwrap().expect("welcome");
        let frame = FederationFrame::parse(&line).unwrap();
        assert!(matches!(frame, FederationFrame::Welcome { .. }));

        // Registry should have the peer.
        let rec = registry.lookup("mac").unwrap().expect("peer recorded");
        assert_eq!(rec.name, "mac");

        cancel.cancel();
    }

    #[tokio::test]
    async fn first_frame_not_hello_drops_connection() {
        let identity = Arc::new(StubIdentity::ok(status_with(
            "Running",
            &["100.64.0.1"],
            &[],
        ))) as SharedIdentity;
        let (_registry, _mw, addr, cancel) = start_hub_on("127.0.0.1:0", identity).await;

        let mut stream = TcpStream::connect(&addr).await.unwrap();
        let ping = FederationFrame::Ping;
        stream
            .write_all(ping.to_line().unwrap().as_bytes())
            .await
            .unwrap();
        stream.flush().await.unwrap();

        let (r, _w) = stream.split();
        let mut lines = BufReader::new(r).lines();
        let res = tokio::time::timeout(Duration::from_secs(2), lines.next_line()).await;
        assert!(matches!(res, Ok(Ok(None))) || matches!(res, Ok(Err(_))));
        cancel.cancel();
    }

    #[tokio::test]
    async fn subscribe_frame_registers_in_routing_table() {
        let identity = Arc::new(StubIdentity::ok(status_with(
            "Running",
            &["100.64.0.1"],
            &[],
        ))) as SharedIdentity;
        let (_registry, middleware, addr, cancel) = start_hub_on("127.0.0.1:0", identity).await;

        let mut stream = TcpStream::connect(&addr).await.unwrap();
        // hello
        stream
            .write_all(
                FederationFrame::Hello {
                    peer_name: "mac".into(),
                    deskd_version: "test".into(),
                    capabilities: vec![],
                }
                .to_line()
                .unwrap()
                .as_bytes(),
            )
            .await
            .unwrap();
        // subscribe
        stream
            .write_all(
                FederationFrame::Subscribe {
                    pattern: "voice.*".into(),
                }
                .to_line()
                .unwrap()
                .as_bytes(),
            )
            .await
            .unwrap();
        stream.flush().await.unwrap();

        // Wait for routing table to reflect the subscription.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let peers = middleware.routing.match_topic("voice.recorded");
            if peers.contains("mac") {
                break;
            }
            if Instant::now() > deadline {
                panic!("subscribe never registered in routing table");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        cancel.cancel();
    }

    #[tokio::test]
    async fn disconnect_drops_subscriptions() {
        let identity = Arc::new(StubIdentity::ok(status_with(
            "Running",
            &["100.64.0.1"],
            &[],
        ))) as SharedIdentity;
        let (_registry, middleware, addr, cancel) = start_hub_on("127.0.0.1:0", identity).await;

        let mut stream = TcpStream::connect(&addr).await.unwrap();
        stream
            .write_all(
                FederationFrame::Hello {
                    peer_name: "mac".into(),
                    deskd_version: "test".into(),
                    capabilities: vec![],
                }
                .to_line()
                .unwrap()
                .as_bytes(),
            )
            .await
            .unwrap();
        stream
            .write_all(
                FederationFrame::Subscribe {
                    pattern: "voice.*".into(),
                }
                .to_line()
                .unwrap()
                .as_bytes(),
            )
            .await
            .unwrap();
        stream.flush().await.unwrap();

        // wait for sub
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if middleware
                .routing
                .match_topic("voice.recorded")
                .contains("mac")
            {
                break;
            }
            if Instant::now() > deadline {
                panic!("subscribe never registered");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Close connection. Hub should drop subscriptions.
        drop(stream);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if !middleware
                .routing
                .match_topic("voice.recorded")
                .contains("mac")
            {
                break;
            }
            if Instant::now() > deadline {
                panic!("subscriptions never dropped after disconnect");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        cancel.cancel();
    }

    #[tokio::test]
    async fn oversize_subscribe_pattern_is_dropped() {
        let identity = Arc::new(StubIdentity::ok(status_with(
            "Running",
            &["100.64.0.1"],
            &[],
        ))) as SharedIdentity;
        let (_registry, middleware, addr, cancel) = start_hub_on("127.0.0.1:0", identity).await;

        let mut stream = TcpStream::connect(&addr).await.unwrap();
        stream
            .write_all(
                FederationFrame::Hello {
                    peer_name: "mac".into(),
                    deskd_version: "test".into(),
                    capabilities: vec![],
                }
                .to_line()
                .unwrap()
                .as_bytes(),
            )
            .await
            .unwrap();

        // One byte over the cap.
        let huge = "a".repeat(MAX_SUBSCRIBE_PATTERN_BYTES + 1);
        stream
            .write_all(
                FederationFrame::Subscribe {
                    pattern: huge.clone(),
                }
                .to_line()
                .unwrap()
                .as_bytes(),
            )
            .await
            .unwrap();
        // Follow with a valid Subscribe to confirm the connection is still
        // alive (oversize frame should be dropped, not close the socket).
        stream
            .write_all(
                FederationFrame::Subscribe {
                    pattern: "voice.*".into(),
                }
                .to_line()
                .unwrap()
                .as_bytes(),
            )
            .await
            .unwrap();
        stream.flush().await.unwrap();

        // The small subscribe should land in the routing table…
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if middleware
                .routing
                .match_topic("voice.recorded")
                .contains("mac")
            {
                break;
            }
            if Instant::now() > deadline {
                panic!("valid subscribe never registered after oversize frame");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // …while the oversize pattern must NOT have been recorded.
        assert!(
            !middleware.routing.match_topic(&huge).contains("mac"),
            "oversize Subscribe pattern leaked into routing table"
        );

        cancel.cancel();
    }
}
