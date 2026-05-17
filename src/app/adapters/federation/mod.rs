//! Federation transport (#462) — TCP hub + peer dialer foundation.
//!
//! This module is the *foundation* of the federated bus epic (#461). It does
//! not yet forward bus messages between peers — children 2 and 3 add the
//! routing and replay layers. What lands here:
//!
//! - [`frame`] — line-delimited JSON wire frame (hello/welcome/ping/pong/bus).
//! - [`preflight`] — `tailscale status --json` health check + identity gate.
//! - [`registry`] — SQLite-backed peer registry.
//! - [`hub`] — TCP listener that accepts tailnet peers and runs the handshake.
//! - [`peer`] — dialer that connects to a hub, handles keepalive + reconnect.
//!
//! Both `hub` and `peer` are gated by `federation.{hub,peer}.enabled` in
//! `workspace.yaml` — block absent or disabled → zero behavior.

pub mod frame;
pub mod hub;
pub mod peer;
pub mod preflight;
pub mod registry;

pub use frame::FederationFrame;
pub use hub::run_hub;
pub use peer::run_peer;
pub use preflight::{CliTailnetIdentity, SharedIdentity, TailnetIdentity, preflight_or_error};
pub use registry::{PeerRecord, PeerRegistry};
