//! Federation transport (#462) + routing layer (#463) — TCP hub, peer dialer,
//! and the forward middleware that turns the deskd bus into a mesh-wide bus.
//!
//! Module layout:
//!
//! - [`frame`] — line-delimited JSON wire frame (hello/welcome/ping/pong/bus,
//!   plus `subscribe`/`unsubscribe`/`subscribe_inbox`/`forward` added in #463).
//! - [`preflight`] — `tailscale status --json` health check + identity gate.
//! - [`registry`] — SQLite-backed peer registry.
//! - [`glob`] — topic glob matching (`*` one segment, `>` everything below).
//! - [`routing`] — in-memory routing table (`pattern → peers`).
//! - [`forward`] — forward middleware (fanout + loop prevention).
//! - [`hub`] — TCP listener that accepts tailnet peers, runs the handshake,
//!   and applies the forward middleware.
//! - [`peer`] — dialer that connects to a hub, handles keepalive + reconnect,
//!   and registers config-driven subscriptions.
//!
//! Both `hub` and `peer` are gated by `federation.{hub,peer}.enabled` in
//! `workspace.yaml` — block absent or disabled → zero behavior.

pub mod forward;
pub mod frame;
pub mod glob;
pub mod hub;
pub mod peer;
pub mod preflight;
pub mod registry;
pub mod routing;

pub use forward::{ForwardMiddleware, LocalBusInjector, NullInjector, PeerConnections};
pub use frame::FederationFrame;
pub use hub::run_hub;
pub use peer::run_peer;
pub use preflight::{CliTailnetIdentity, SharedIdentity, TailnetIdentity, preflight_or_error};
pub use registry::{PeerRecord, PeerRegistry};
pub use routing::RoutingTable;
