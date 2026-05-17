//! Integration test for the federated bus forward middleware (#463).
//!
//! Runs hub + peer in the same process on `127.0.0.1`. The test exercises the
//! ticket's smoke-test goal:
//!
//! 1. Peer subscribes to `voice.*` after welcome.
//! 2. Hub fans out a local publish on `voice.recorded` → peer receives it on
//!    its local-bus injector.
//! 3. Peer publishes back through the session handle → hub's local injector
//!    sees it; the originating peer is NOT looped back to.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use deskd::app::adapters::federation::forward::{
    ForwardMiddleware, LocalBusInjector, PeerConnections,
};
use deskd::app::adapters::federation::frame::FederationFrame;
use deskd::app::adapters::federation::hub::{HubConfig, run_hub_with_middleware};
use deskd::app::adapters::federation::peer::{
    PeerDialerConfig, PeerSessionHandle, run_peer_with_injector,
};
use deskd::app::adapters::federation::preflight::testing::{StubIdentity, status_with};
use deskd::app::adapters::federation::preflight::{SharedIdentity, TailnetIdentity};
use deskd::app::adapters::federation::registry::PeerRegistry;
use deskd::app::adapters::federation::routing::RoutingTable;
use deskd::ports::bus_wire::{BusMessage, BusMetadata};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct CapturingInjector {
    seen: Mutex<Vec<(String, BusMessage)>>,
}
impl CapturingInjector {
    fn new() -> Self {
        Self::default()
    }
    fn taken(&self) -> Vec<(String, BusMessage)> {
        self.seen.lock().unwrap().clone()
    }
}
impl LocalBusInjector for CapturingInjector {
    fn inject(&self, origin: &str, msg: &BusMessage) {
        self.seen
            .lock()
            .unwrap()
            .push((origin.to_string(), msg.clone()));
    }
}

async fn pick_free_addr() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);
    addr
}

fn msg(id: &str, target: &str) -> BusMessage {
    BusMessage {
        id: id.into(),
        source: "agent.x".into(),
        target: target.into(),
        payload: serde_json::json!({"k": "v"}),
        reply_to: None,
        metadata: BusMetadata {
            priority: 5,
            fresh: false,
        },
    }
}

async fn wait_until<F: Fn() -> bool>(predicate: F, timeout: Duration, label: &str) {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return;
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for: {label}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn round_trip_hub_to_peer_and_back() {
    // ── Hub side ──
    let addr = pick_free_addr().await;
    let identity: SharedIdentity = Arc::new(StubIdentity::ok(status_with(
        "Running",
        &["100.64.0.1"],
        &[],
    ))) as Arc<dyn TailnetIdentity>;
    let hub_injector = Arc::new(CapturingInjector::new());
    let hub_routing = Arc::new(RoutingTable::new());
    let hub_peers = Arc::new(PeerConnections::new());
    let hub_middleware = Arc::new(ForwardMiddleware::new(
        hub_routing.clone(),
        hub_peers.clone(),
        hub_injector.clone() as Arc<dyn LocalBusInjector>,
        "vps".to_string(),
    ));

    let tmp = tempfile::tempdir().unwrap();
    let registry = Arc::new(PeerRegistry::open(&tmp.path().join("fed.sqlite")).unwrap());
    let hub_cancel = CancellationToken::new();
    let hub_cfg = HubConfig {
        bind: addr.clone(),
        hub_name: "vps".into(),
        tailscale_binary: "true".into(),
        peer_timeout_secs: 60,
    };
    let reg_for_hub = registry.clone();
    let hub_mw_for_run = hub_middleware.clone();
    let hub_cancel_clone = hub_cancel.clone();
    let _hub_task = tokio::spawn(async move {
        let _ = run_hub_with_middleware(
            hub_cfg,
            identity,
            reg_for_hub,
            hub_mw_for_run,
            hub_cancel_clone,
        )
        .await;
    });

    // ── Peer side ──
    let peer_injector = Arc::new(CapturingInjector::new());
    let session = PeerSessionHandle::new();
    let peer_cancel = CancellationToken::new();
    let pc = PeerDialerConfig {
        hub_addr: addr.clone(),
        peer_name: "mac".into(),
        reconnect_backoff_secs: vec![1],
        deskd_version: "test".into(),
        subscribe_patterns: vec!["voice.*".into()],
        subscribe_inboxes: vec![],
    };
    let pi = peer_injector.clone() as Arc<dyn LocalBusInjector>;
    let session_for_peer = session.clone();
    let peer_cancel_clone = peer_cancel.clone();
    let _peer_task = tokio::spawn(async move {
        let _ = run_peer_with_injector(pc, pi, session_for_peer, peer_cancel_clone).await;
    });

    // ── Wait for handshake + subscription ──
    let routing_for_check = hub_routing.clone();
    wait_until(
        move || {
            routing_for_check
                .match_topic("voice.recorded")
                .contains("mac")
        },
        Duration::from_secs(5),
        "peer 'mac' subscribes to voice.*",
    )
    .await;

    // ── Hub → peer fanout ──
    hub_middleware.fanout_local("voice.recorded", &msg("m1", "voice.recorded"));
    let injector_for_check = peer_injector.clone();
    wait_until(
        move || {
            injector_for_check
                .taken()
                .iter()
                .any(|(o, m)| o == "vps" && m.id == "m1" && m.target == "voice.recorded")
        },
        Duration::from_secs(3),
        "peer receives m1 forwarded from hub",
    )
    .await;

    // ── Non-matching topic must NOT reach the peer ──
    hub_middleware.fanout_local("other.topic", &msg("m-other", "other.topic"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        peer_injector.taken().iter().all(|(_, m)| m.id != "m-other"),
        "peer should not receive non-matching topic"
    );

    // ── Peer → hub publish via the session handle ──
    let sent = session
        .send(FederationFrame::Forward {
            origin: "mac".into(),
            message: msg("m2", "voice.recorded"),
        })
        .await;
    assert!(sent, "peer session handle must be connected");

    let hub_inj_for_check = hub_injector.clone();
    wait_until(
        move || {
            hub_inj_for_check
                .taken()
                .iter()
                .any(|(o, m)| o == "mac" && m.id == "m2")
        },
        Duration::from_secs(3),
        "hub injects m2 into local bus",
    )
    .await;

    // ── Loop prevention: peer should NOT receive its own publish back ──
    // Wait for an additional moment to give the hub a chance to (incorrectly)
    // fan back out, then assert nothing landed.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        peer_injector.taken().iter().all(|(_, m)| m.id != "m2"),
        "peer must not receive its own publish back (loop prevention)"
    );

    peer_cancel.cancel();
    hub_cancel.cancel();
}

#[tokio::test]
async fn unsubscribe_stops_fanout_to_peer() {
    let addr = pick_free_addr().await;
    let identity: SharedIdentity = Arc::new(StubIdentity::ok(status_with(
        "Running",
        &["100.64.0.1"],
        &[],
    ))) as Arc<dyn TailnetIdentity>;

    let hub_routing = Arc::new(RoutingTable::new());
    let hub_peers = Arc::new(PeerConnections::new());
    let hub_middleware = Arc::new(ForwardMiddleware::new(
        hub_routing.clone(),
        hub_peers.clone(),
        Arc::new(deskd::app::adapters::federation::forward::NullInjector),
        "vps".to_string(),
    ));
    let tmp = tempfile::tempdir().unwrap();
    let registry = Arc::new(PeerRegistry::open(&tmp.path().join("fed.sqlite")).unwrap());
    let hub_cancel = CancellationToken::new();
    let hub_cfg = HubConfig {
        bind: addr.clone(),
        hub_name: "vps".into(),
        tailscale_binary: "true".into(),
        peer_timeout_secs: 60,
    };
    let mw_for_run = hub_middleware.clone();
    let hub_cancel_clone = hub_cancel.clone();
    tokio::spawn(async move {
        let _ = run_hub_with_middleware(hub_cfg, identity, registry, mw_for_run, hub_cancel_clone)
            .await;
    });

    let peer_injector = Arc::new(CapturingInjector::new());
    let session = PeerSessionHandle::new();
    let peer_cancel = CancellationToken::new();
    let pc = PeerDialerConfig {
        hub_addr: addr.clone(),
        peer_name: "mac".into(),
        reconnect_backoff_secs: vec![1],
        deskd_version: "test".into(),
        subscribe_patterns: vec!["voice.*".into()],
        subscribe_inboxes: vec![],
    };
    let pi = peer_injector.clone() as Arc<dyn LocalBusInjector>;
    let session_for_peer = session.clone();
    let peer_cancel_clone = peer_cancel.clone();
    tokio::spawn(async move {
        let _ = run_peer_with_injector(pc, pi, session_for_peer, peer_cancel_clone).await;
    });

    // Wait for sub.
    let r = hub_routing.clone();
    wait_until(
        move || r.match_topic("voice.recorded").contains("mac"),
        Duration::from_secs(5),
        "mac subscribes voice.*",
    )
    .await;

    // Unsubscribe via the peer session.
    let sent = session
        .send(FederationFrame::Unsubscribe {
            pattern: "voice.*".into(),
        })
        .await;
    assert!(sent);

    // Wait for unsubscribe to take effect.
    let r = hub_routing.clone();
    wait_until(
        move || !r.match_topic("voice.recorded").contains("mac"),
        Duration::from_secs(3),
        "voice.* unsubscribed",
    )
    .await;

    // Now publish — peer should NOT receive.
    hub_middleware.fanout_local("voice.recorded", &msg("m-after", "voice.recorded"));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        peer_injector.taken().iter().all(|(_, m)| m.id != "m-after"),
        "unsubscribed peer should not receive fanout"
    );

    peer_cancel.cancel();
    hub_cancel.cancel();
}
