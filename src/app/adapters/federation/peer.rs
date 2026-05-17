//! Federation peer dialer (#462) — connects to a hub and stays connected.
//!
//! [`run_peer`] runs a connect/handshake/keepalive/reconnect loop with
//! exponential backoff from config. The dialer is foundation-only: it does
//! not currently forward bus traffic — child 2 wires that in.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

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
}

/// Run the dialer until cancelled.
pub async fn run_peer(cfg: PeerDialerConfig, cancel: CancellationToken) -> Result<()> {
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
        match dial_once(&cfg, &cancel).await {
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

async fn dial_once(cfg: &PeerDialerConfig, cancel: &CancellationToken) -> Result<SessionExit> {
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

    // Keepalive loop.
    let mut last_pong = Instant::now();
    let mut missed: u8 = 0;
    let mut ticker = tokio::time::interval(KEEPALIVE_INTERVAL);
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(SessionExit::Cancelled),
            _ = ticker.tick() => {
                if last_pong.elapsed() > KEEPALIVE_INTERVAL.saturating_mul(2) {
                    missed += 1;
                    if missed >= 2 {
                        info!(
                            target: "deskd::federation::peer",
                            "hub missed 2 pongs — dropping session"
                        );
                        return Ok(SessionExit::Disconnected);
                    }
                }
                let ping = FederationFrame::Ping.to_line()?;
                if writer.write_all(ping.as_bytes()).await.is_err() {
                    return Ok(SessionExit::Disconnected);
                }
                let _ = writer.flush().await;
            }
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(l)) => l,
                    Ok(None) | Err(_) => return Ok(SessionExit::Disconnected),
                };
                match FederationFrame::parse(&line) {
                    Ok(FederationFrame::Pong) => {
                        last_pong = Instant::now();
                        missed = 0;
                    }
                    Ok(FederationFrame::Ping) => {
                        let pong = FederationFrame::Pong.to_line()?;
                        if writer.write_all(pong.as_bytes()).await.is_err() {
                            return Ok(SessionExit::Disconnected);
                        }
                        let _ = writer.flush().await;
                    }
                    Ok(FederationFrame::BusMessage(_)) => {
                        // Foundation: log + discard.
                        debug!(target: "deskd::federation::peer", "discarding inbound bus frame");
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
                        return Ok(SessionExit::Disconnected);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::adapters::federation::hub::{HubConfig, run_hub};
    use crate::app::adapters::federation::preflight::testing::{StubIdentity, status_with};
    use crate::app::adapters::federation::preflight::{SharedIdentity, TailnetIdentity};
    use crate::app::adapters::federation::registry::PeerRegistry;
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
        let reg2 = registry.clone();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            let _ = run_hub(cfg, identity, reg2, cancel2).await;
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
        let reg2 = registry.clone();
        let hub_cancel2 = hub_cancel.clone();
        tokio::spawn(async move {
            let _ = run_hub(cfg, identity, reg2, hub_cancel2).await;
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
