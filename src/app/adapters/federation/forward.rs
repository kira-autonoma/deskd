//! Forward middleware — wires the federation routing table into the hub and
//! peer (#463).
//!
//! Three responsibilities:
//!
//! 1. **Peer connection registry** — for each connected peer (hub side) keep
//!    a bounded `mpsc::Sender<FederationFrame>` ([`PEER_CHANNEL_CAPACITY`])
//!    that the writer task drains. Sending a `Forward { origin, message }`
//!    over this channel reaches the peer. Bounded so a slow peer cannot
//!    OOM the hub — on `Full` the frame is logged and dropped.
//! 2. **Local→peer fanout** — when a frame is published on the local bus,
//!    look up subscribed peers via the [`RoutingTable`] and forward the frame
//!    to each, **skipping the origin** to prevent loops.
//! 3. **Peer→local injection** — when a peer's reader receives a frame, inject
//!    it into the local bus via a [`LocalBusInjector`] (and re-fan-out to any
//!    *other* matching peers, so the hub also routes between peers).
//!
//! ## Loop prevention
//!
//! Every federated frame carries an `origin` peer-name field. The hub never
//! forwards a frame back to its origin. Frames originated locally are
//! attributed to the hub's own name (configurable; defaults to `local`) so
//! they fan out to every matching peer.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, warn};

use super::frame::FederationFrame;
use super::routing::RoutingTable;
use crate::ports::bus_wire::BusMessage;

/// Capacity of per-peer outbound channels. A slow peer's TCP write buffer
/// fills, then this queue fills — at which point new frames are dropped
/// rather than queued without bound. 1024 frames ≈ a few hundred KiB of
/// queued JSON in the worst case, which is fine; the cure for a peer that
/// stays full is reconnection, not unbounded RAM growth.
pub const PEER_CHANNEL_CAPACITY: usize = 1024;

/// Outbound channel handle per connected peer. Bounded — see
/// [`PEER_CHANNEL_CAPACITY`].
pub type PeerSender = mpsc::Sender<FederationFrame>;

/// Trait for injecting an inbound federated frame onto the local bus.
///
/// Production uses a Unix-socket bus client; tests use a channel-backed stub.
/// The implementation must not block — calls are made from inside per-peer
/// reader tasks.
pub trait LocalBusInjector: Send + Sync {
    /// Inject `msg` onto the local bus as if it had been published there.
    ///
    /// `origin` is the federation origin peer name and is kept for diagnostic
    /// logging only — local bus subscribers do not see it directly. Failures
    /// must be logged, not propagated.
    fn inject(&self, origin: &str, msg: &BusMessage);
}

/// No-op injector. Used in unit tests that only exercise fanout, not local
/// delivery, and as a default when the hub is configured without a local bus
/// connection.
pub struct NullInjector;
impl LocalBusInjector for NullInjector {
    fn inject(&self, _origin: &str, _msg: &BusMessage) {}
}

/// Connected-peer registry. Maps `peer_name → outbound sender`.
#[derive(Default)]
pub struct PeerConnections {
    inner: Mutex<HashMap<String, PeerSender>>,
}

impl PeerConnections {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a peer's outbound channel. Replaces any prior sender for the
    /// same name — last-write-wins, matching the registry's reconnect
    /// semantics.
    pub fn insert(&self, peer: &str, tx: PeerSender) {
        let mut map = self.inner.lock().expect("peer connections mutex poisoned");
        map.insert(peer.to_string(), tx);
    }

    /// Drop a peer's channel. Called on disconnect.
    pub fn remove(&self, peer: &str) {
        let mut map = self.inner.lock().expect("peer connections mutex poisoned");
        map.remove(peer);
    }

    /// Get the sender for `peer`, if connected.
    pub fn get(&self, peer: &str) -> Option<PeerSender> {
        let map = self.inner.lock().expect("peer connections mutex poisoned");
        map.get(peer).cloned()
    }

    /// Names of all currently connected peers.
    pub fn connected(&self) -> Vec<String> {
        let map = self.inner.lock().expect("peer connections mutex poisoned");
        map.keys().cloned().collect()
    }
}

/// Forward middleware — bundles the routing table, peer registry, and the
/// local injector. Shared by reference (cheap to clone the `Arc`s).
pub struct ForwardMiddleware {
    pub routing: Arc<RoutingTable>,
    pub peers: Arc<PeerConnections>,
    pub injector: Arc<dyn LocalBusInjector>,
    /// Name attributed to the hub itself in `origin` fields.
    pub local_origin: String,
}

impl ForwardMiddleware {
    /// Construct a middleware with the supplied dependencies.
    pub fn new(
        routing: Arc<RoutingTable>,
        peers: Arc<PeerConnections>,
        injector: Arc<dyn LocalBusInjector>,
        local_origin: impl Into<String>,
    ) -> Self {
        Self {
            routing,
            peers,
            injector,
            local_origin: local_origin.into(),
        }
    }

    /// Convenience constructor for testing — no-op injector, fresh routing
    /// table and peer registry.
    #[doc(hidden)]
    pub fn for_test(local_origin: &str) -> Self {
        Self::new(
            Arc::new(RoutingTable::new()),
            Arc::new(PeerConnections::new()),
            Arc::new(NullInjector),
            local_origin,
        )
    }

    /// Fanout a locally-published frame to all subscribed peers (skipping the
    /// `origin` peer, if any, to avoid forwarding straight back).
    ///
    /// `target_topic` is the bus target the frame was published under — used
    /// to drive glob matching. Inbox-shaped targets (`inbox/<name>`) fall
    /// through to the inbox table.
    pub fn fanout(&self, origin: &str, target_topic: &str, msg: &BusMessage) {
        let mut peers = self.routing.match_topic(target_topic);
        for p in self.routing.match_inbox(target_topic) {
            peers.insert(p);
        }
        for peer in peers {
            if peer == origin {
                continue;
            }
            let Some(tx) = self.peers.get(&peer) else {
                debug!(
                    target: "deskd::federation::forward",
                    peer = %peer,
                    "skipping fanout — peer not currently connected"
                );
                continue;
            };
            let frame = FederationFrame::Forward {
                origin: origin.to_string(),
                message: msg.clone(),
            };
            match tx.try_send(frame) {
                Ok(()) => {
                    debug!(
                        target: "deskd::federation::forward",
                        peer = %peer,
                        origin = %origin,
                        target_topic = %target_topic,
                        "forwarded frame to peer"
                    );
                }
                Err(TrySendError::Full(_)) => {
                    warn!(
                        target: "deskd::federation::forward",
                        peer = %peer,
                        target_topic = %target_topic,
                        "peer outbound channel full — dropping frame (slow peer)"
                    );
                }
                Err(TrySendError::Closed(_)) => {
                    warn!(
                        target: "deskd::federation::forward",
                        peer = %peer,
                        "peer outbound channel closed — dropping frame"
                    );
                }
            }
        }
    }

    /// Convenience: fanout a locally-originated publish under
    /// `self.local_origin`.
    pub fn fanout_local(&self, target_topic: &str, msg: &BusMessage) {
        let origin = self.local_origin.clone();
        self.fanout(&origin, target_topic, msg);
    }

    /// Handle an inbound frame from a peer connection. Injects into the local
    /// bus, then re-fans-out to any *other* subscribed peers (so the hub
    /// routes between peers transparently).
    pub fn handle_inbound(&self, from_peer: &str, origin: &str, msg: &BusMessage) {
        // The injector decides whether to surface this on the local bus.
        self.injector.inject(origin, msg);
        // Re-fanout to other peers. Skip the sender (the connection we read
        // from) AND the origin (which may be further upstream) to avoid loops.
        let target_topic = &msg.target;
        let mut peers = self.routing.match_topic(target_topic);
        for p in self.routing.match_inbox(target_topic) {
            peers.insert(p);
        }
        for peer in peers {
            if peer == from_peer || peer == origin {
                continue;
            }
            let Some(tx) = self.peers.get(&peer) else {
                continue;
            };
            let frame = FederationFrame::Forward {
                origin: origin.to_string(),
                message: msg.clone(),
            };
            match tx.try_send(frame) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    warn!(
                        target: "deskd::federation::forward",
                        peer = %peer,
                        "peer outbound channel full — dropping re-forward (slow peer)"
                    );
                }
                Err(TrySendError::Closed(_)) => {
                    warn!(
                        target: "deskd::federation::forward",
                        peer = %peer,
                        "peer outbound channel closed — dropping re-forward"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::bus_wire::BusMetadata;
    use std::sync::Mutex as StdMutex;

    fn msg(target: &str) -> BusMessage {
        BusMessage {
            id: "m-1".into(),
            source: "agent.x".into(),
            target: target.into(),
            payload: serde_json::json!({}),
            reply_to: None,
            metadata: BusMetadata {
                priority: 5,
                fresh: false,
            },
        }
    }

    struct CapturingInjector {
        seen: StdMutex<Vec<(String, String)>>,
    }
    impl CapturingInjector {
        fn new() -> Self {
            Self {
                seen: StdMutex::new(Vec::new()),
            }
        }
        fn taken(&self) -> Vec<(String, String)> {
            self.seen.lock().unwrap().clone()
        }
    }
    impl LocalBusInjector for CapturingInjector {
        fn inject(&self, origin: &str, msg: &BusMessage) {
            self.seen
                .lock()
                .unwrap()
                .push((origin.to_string(), msg.target.clone()));
        }
    }

    fn channel() -> (PeerSender, mpsc::Receiver<FederationFrame>) {
        mpsc::channel(PEER_CHANNEL_CAPACITY)
    }

    #[test]
    fn fanout_skips_origin() {
        let mw = ForwardMiddleware::for_test("local");
        let (tx_mac, mut rx_mac) = channel();
        let (tx_phone, mut rx_phone) = channel();
        mw.peers.insert("mac", tx_mac);
        mw.peers.insert("phone", tx_phone);
        mw.routing.subscribe("mac", "voice.*");
        mw.routing.subscribe("phone", "voice.*");

        // phone originated the frame → should NOT receive it back.
        mw.fanout("phone", "voice.recorded", &msg("voice.recorded"));
        assert!(rx_mac.try_recv().is_ok());
        assert!(rx_phone.try_recv().is_err());
    }

    #[test]
    fn fanout_local_uses_local_origin() {
        let mw = ForwardMiddleware::for_test("local");
        let (tx_mac, mut rx_mac) = channel();
        mw.peers.insert("mac", tx_mac);
        mw.routing.subscribe("mac", "voice.*");

        mw.fanout_local("voice.recorded", &msg("voice.recorded"));
        let frame = rx_mac.try_recv().unwrap();
        match frame {
            FederationFrame::Forward { origin, message } => {
                assert_eq!(origin, "local");
                assert_eq!(message.target, "voice.recorded");
            }
            _ => panic!("expected Forward"),
        }
    }

    #[test]
    fn fanout_excludes_non_matching_peers() {
        let mw = ForwardMiddleware::for_test("local");
        let (tx_mac, mut rx_mac) = channel();
        let (tx_phone, mut rx_phone) = channel();
        mw.peers.insert("mac", tx_mac);
        mw.peers.insert("phone", tx_phone);
        mw.routing.subscribe("mac", "voice.*");
        // phone is not subscribed.

        mw.fanout_local("voice.recorded", &msg("voice.recorded"));
        assert!(rx_mac.try_recv().is_ok());
        assert!(rx_phone.try_recv().is_err());
    }

    #[test]
    fn handle_inbound_injects_and_does_not_loop_back_to_sender() {
        let routing = Arc::new(RoutingTable::new());
        let peers = Arc::new(PeerConnections::new());
        let injector = Arc::new(CapturingInjector::new());
        let mw = ForwardMiddleware::new(
            routing,
            peers.clone(),
            injector.clone(),
            "local".to_string(),
        );

        // mac is subscribed to voice.* and connected.
        let (tx_mac, mut rx_mac) = channel();
        mw.peers.insert("mac", tx_mac);
        mw.routing.subscribe("mac", "voice.*");

        // Inbound from mac with origin=mac — local bus should see it, but
        // we must NOT echo back to mac.
        mw.handle_inbound("mac", "mac", &msg("voice.recorded"));
        assert!(rx_mac.try_recv().is_err());
        let seen = injector.taken();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "mac");
        assert_eq!(seen[0].1, "voice.recorded");
    }

    #[test]
    fn handle_inbound_forwards_to_other_peers() {
        let mw = ForwardMiddleware::for_test("local");
        let (tx_mac, mut rx_mac) = channel();
        let (tx_phone, mut rx_phone) = channel();
        mw.peers.insert("mac", tx_mac);
        mw.peers.insert("phone", tx_phone);
        mw.routing.subscribe("mac", "voice.*");
        mw.routing.subscribe("phone", "voice.*");

        // Inbound from mac → re-fanout to phone, skip mac.
        mw.handle_inbound("mac", "mac", &msg("voice.recorded"));
        assert!(rx_mac.try_recv().is_err());
        assert!(rx_phone.try_recv().is_ok());
    }

    #[test]
    fn inbox_targets_route_via_inbox_table() {
        let mw = ForwardMiddleware::for_test("local");
        let (tx_mac, mut rx_mac) = channel();
        mw.peers.insert("mac", tx_mac);
        mw.routing.subscribe_inbox("mac", "personal");

        mw.fanout_local("inbox/personal", &msg("inbox/personal"));
        assert!(rx_mac.try_recv().is_ok());

        // Cross-device shape: inbox/personal@mac.
        mw.fanout_local("inbox/personal@mac", &msg("inbox/personal@mac"));
        assert!(rx_mac.try_recv().is_ok());
    }

    #[test]
    fn slow_peer_full_channel_drops_frame_without_blocking() {
        // Tiny channel so we can force "Full" deterministically.
        let mw = ForwardMiddleware::for_test("local");
        let (tx, mut rx) = mpsc::channel::<FederationFrame>(1);
        mw.peers.insert("slow", tx);
        mw.routing.subscribe("slow", "voice.*");

        // First publish fills the channel.
        mw.fanout_local("voice.recorded", &msg("voice.recorded"));
        // Second publish should be dropped on Full (channel still has the
        // first frame queued — receiver hasn't drained yet).
        mw.fanout_local("voice.recorded", &msg("voice.recorded"));

        // Exactly one frame survived.
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn drop_peer_via_routing_table_clears_subscriptions() {
        let mw = ForwardMiddleware::for_test("local");
        let (tx_mac, mut rx_mac) = channel();
        mw.peers.insert("mac", tx_mac);
        mw.routing.subscribe("mac", "voice.*");
        mw.routing.drop_peer("mac");
        mw.peers.remove("mac");

        mw.fanout_local("voice.recorded", &msg("voice.recorded"));
        assert!(rx_mac.try_recv().is_err());
    }
}
