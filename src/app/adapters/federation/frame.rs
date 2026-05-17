//! Federation wire frame — extends the bus envelope with control frames
//! (`hello`, `welcome`, `ping`, `pong`, `subscribe`, …) used between
//! federated peers.
//!
//! Serialized as line-delimited JSON with a discriminating `type` field, the
//! same shape used on the local UNIX socket bus. The `bus_message` variant
//! carries an unchanged `BusMessage` payload so the inner wire format remains
//! byte-identical between transports — this is the protocol-parity property
//! the federation epic (#461) calls out.
//!
//! # Frame variants
//!
//! - `hello { peer_name, deskd_version, capabilities }` — peer → hub on connect
//! - `welcome { hub_name, replay_supported }` — hub → peer in reply
//! - `ping` / `pong` — bidirectional keepalive (every 30s, two missed → drop)
//! - `subscribe { pattern }` — peer → hub, register a topic glob subscription
//! - `unsubscribe { pattern }` — peer → hub, drop a topic glob subscription
//! - `subscribe_inbox { name }` — peer → hub, register an inbox subscription
//! - `bus_message(BusMessage)` — raw bus frame (legacy / no origin tracking)
//! - `forward { origin, message }` — federated bus frame carrying its `origin`
//!   peer name, used by both directions for loop prevention.

use serde::{Deserialize, Serialize};

use crate::ports::bus_wire::BusMessage;

/// Federation wire frame. Discriminated on the JSON `type` field
/// (`hello`, `welcome`, `ping`, `pong`, `bus_message`).
///
/// Note: `PartialEq` is not derived because the wrapped `BusMessage` is not
/// `PartialEq`. Tests roundtrip through JSON instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FederationFrame {
    /// Peer → hub on connect, identifies the peer.
    Hello {
        peer_name: String,
        deskd_version: String,
        #[serde(default)]
        capabilities: Vec<String>,
    },
    /// Hub → peer in reply to hello, announces hub identity + capabilities.
    Welcome {
        hub_name: String,
        #[serde(default)]
        replay_supported: bool,
    },
    /// Keepalive request — expects `Pong` in reply within the keepalive window.
    Ping,
    /// Keepalive reply.
    Pong,
    /// Topic-glob subscription. Issued by a peer; the hub starts forwarding
    /// matching local frames over this peer link.
    Subscribe { pattern: String },
    /// Drop a previously registered glob subscription.
    Unsubscribe { pattern: String },
    /// Inbox subscription — preserves the existing `inbox/<name>` shape, with
    /// optional cross-device `inbox/<name>@<peer>` routing handled by the hub.
    SubscribeInbox { name: String },
    /// A regular bus message; transparently wrapped so the inner JSON is the
    /// same shape the UNIX socket bus speaks. No `origin` tracking — used for
    /// legacy / hello-time greetings only. Federated traffic uses `Forward`.
    BusMessage(BusMessage),
    /// A federated bus frame with origin tracking. Always sent in both
    /// directions for actual fanout — the hub uses `origin` to suppress
    /// forwarding back to the publishing peer, preventing loops.
    Forward { origin: String, message: BusMessage },
}

impl FederationFrame {
    /// Serialize to a single JSON line (terminating newline included). This is
    /// what should be written to the TCP stream.
    pub fn to_line(&self) -> serde_json::Result<String> {
        let mut s = serde_json::to_string(self)?;
        s.push('\n');
        Ok(s)
    }

    /// Parse a single line (without the trailing newline) into a frame.
    pub fn parse(line: &str) -> serde_json::Result<Self> {
        serde_json::from_str(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::bus_wire::BusMetadata;

    #[test]
    fn hello_roundtrip() {
        let frame = FederationFrame::Hello {
            peer_name: "mac".into(),
            deskd_version: "0.1.0".into(),
            capabilities: vec!["basic".into()],
        };
        let line = frame.to_line().unwrap();
        // Newline appended for wire framing.
        assert!(line.ends_with('\n'));
        let trimmed = line.trim_end();
        let back = FederationFrame::parse(trimmed).unwrap();
        match back {
            FederationFrame::Hello {
                peer_name,
                deskd_version,
                capabilities,
            } => {
                assert_eq!(peer_name, "mac");
                assert_eq!(deskd_version, "0.1.0");
                assert_eq!(capabilities, vec!["basic".to_string()]);
            }
            _ => panic!("expected Hello"),
        }
    }

    #[test]
    fn welcome_roundtrip() {
        let frame = FederationFrame::Welcome {
            hub_name: "vps".into(),
            replay_supported: false,
        };
        let line = frame.to_line().unwrap();
        let back = FederationFrame::parse(line.trim_end()).unwrap();
        match back {
            FederationFrame::Welcome {
                hub_name,
                replay_supported,
            } => {
                assert_eq!(hub_name, "vps");
                assert!(!replay_supported);
            }
            _ => panic!("expected Welcome"),
        }
    }

    #[test]
    fn ping_pong_roundtrip() {
        let ping_line = FederationFrame::Ping.to_line().unwrap();
        let pong_line = FederationFrame::Pong.to_line().unwrap();
        assert!(matches!(
            FederationFrame::parse(ping_line.trim_end()).unwrap(),
            FederationFrame::Ping
        ));
        assert!(matches!(
            FederationFrame::parse(pong_line.trim_end()).unwrap(),
            FederationFrame::Pong
        ));
    }

    #[test]
    fn bus_message_roundtrip_preserves_wire_shape() {
        // Wrapping a BusMessage must preserve the inner field order/names so
        // that any peer-side decoder sees the exact same JSON as on UNIX.
        let inner = BusMessage {
            id: "msg-1".into(),
            source: "agent-a".into(),
            target: "agent-b".into(),
            payload: serde_json::json!({"text": "hi"}),
            reply_to: Some("agent-a".into()),
            metadata: BusMetadata {
                priority: 5,
                fresh: false,
            },
        };
        let frame = FederationFrame::BusMessage(inner.clone());
        let line = frame.to_line().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["type"], "bus_message");
        assert_eq!(parsed["id"], "msg-1");
        assert_eq!(parsed["source"], "agent-a");
        assert_eq!(parsed["target"], "agent-b");

        let back = FederationFrame::parse(line.trim_end()).unwrap();
        match back {
            FederationFrame::BusMessage(m) => {
                assert_eq!(m.id, "msg-1");
                assert_eq!(m.source, "agent-a");
            }
            _ => panic!("expected BusMessage"),
        }
    }

    #[test]
    fn subscribe_roundtrip() {
        let frame = FederationFrame::Subscribe {
            pattern: "voice.*".into(),
        };
        let line = frame.to_line().unwrap();
        assert!(line.contains("\"type\":\"subscribe\""));
        let back = FederationFrame::parse(line.trim_end()).unwrap();
        match back {
            FederationFrame::Subscribe { pattern } => assert_eq!(pattern, "voice.*"),
            _ => panic!("expected Subscribe"),
        }
    }

    #[test]
    fn unsubscribe_roundtrip() {
        let frame = FederationFrame::Unsubscribe {
            pattern: "voice.*".into(),
        };
        let line = frame.to_line().unwrap();
        assert!(line.contains("\"type\":\"unsubscribe\""));
        let back = FederationFrame::parse(line.trim_end()).unwrap();
        assert!(matches!(back, FederationFrame::Unsubscribe { .. }));
    }

    #[test]
    fn subscribe_inbox_roundtrip() {
        let frame = FederationFrame::SubscribeInbox {
            name: "personal".into(),
        };
        let line = frame.to_line().unwrap();
        assert!(line.contains("\"type\":\"subscribe_inbox\""));
        let back = FederationFrame::parse(line.trim_end()).unwrap();
        match back {
            FederationFrame::SubscribeInbox { name } => assert_eq!(name, "personal"),
            _ => panic!("expected SubscribeInbox"),
        }
    }

    #[test]
    fn forward_roundtrip_preserves_origin_and_inner() {
        let inner = BusMessage {
            id: "msg-7".into(),
            source: "agent.x".into(),
            target: "voice.recorded".into(),
            payload: serde_json::json!({"transcript": "hi"}),
            reply_to: None,
            metadata: BusMetadata {
                priority: 5,
                fresh: false,
            },
        };
        let frame = FederationFrame::Forward {
            origin: "phone".into(),
            message: inner,
        };
        let line = frame.to_line().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["type"], "forward");
        assert_eq!(parsed["origin"], "phone");
        assert_eq!(parsed["message"]["id"], "msg-7");
        // Roundtrip.
        let back = FederationFrame::parse(line.trim_end()).unwrap();
        match back {
            FederationFrame::Forward { origin, message } => {
                assert_eq!(origin, "phone");
                assert_eq!(message.id, "msg-7");
                assert_eq!(message.target, "voice.recorded");
            }
            _ => panic!("expected Forward"),
        }
    }

    #[test]
    fn unknown_type_rejected() {
        let line = r#"{"type":"goodbye"}"#;
        assert!(FederationFrame::parse(line).is_err());
    }

    #[test]
    fn malformed_json_rejected() {
        assert!(FederationFrame::parse("not json").is_err());
    }
}
