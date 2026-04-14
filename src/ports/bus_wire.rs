//! Bus wire-format types — shared protocol types for bus communication.
//!
//! These types define the serialization format for messages on the bus.
//! Used by both infrastructure (bus_server, unix_bus) and application
//! (mcp, worker) layers.

use serde::{Deserialize, Serialize};

use crate::domain::message::{Envelope, Message, Metadata, Register};

// ─── Wire types ─────────────────────────────────────────────────────────────

/// Wire format for messages on the Unix socket bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusMessage {
    pub id: String,
    pub source: String,
    pub target: String,
    pub payload: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub metadata: BusMetadata,
}

/// Wire format for message metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BusMetadata {
    #[serde(default = "default_priority")]
    pub priority: u8,
    #[serde(default)]
    pub fresh: bool,
}

fn default_priority() -> u8 {
    5
}

/// Wire format for the registration handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusRegister {
    pub name: String,
    #[serde(default)]
    pub subscriptions: Vec<String>,
}

/// Wire-level envelope: tagged union for all bus protocol messages.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BusEnvelope {
    Register(BusRegister),
    Message(BusMessage),
    List,
}

// ─── Domain ↔ Bus conversions ───────────────────────────────────────────────

impl From<BusMessage> for Message {
    fn from(dto: BusMessage) -> Self {
        Self {
            id: dto.id,
            source: dto.source,
            target: dto.target,
            payload: dto.payload,
            reply_to: dto.reply_to,
            metadata: Metadata {
                priority: dto.metadata.priority,
                fresh: dto.metadata.fresh,
            },
        }
    }
}

impl From<&Message> for BusMessage {
    fn from(msg: &Message) -> Self {
        Self {
            id: msg.id.clone(),
            source: msg.source.clone(),
            target: msg.target.clone(),
            payload: msg.payload.clone(),
            reply_to: msg.reply_to.clone(),
            metadata: BusMetadata {
                priority: msg.metadata.priority,
                fresh: msg.metadata.fresh,
            },
        }
    }
}

impl From<BusEnvelope> for Envelope {
    fn from(dto: BusEnvelope) -> Self {
        match dto {
            BusEnvelope::Register(r) => Envelope::Register(Register {
                name: r.name,
                subscriptions: r.subscriptions,
            }),
            BusEnvelope::Message(m) => Envelope::Message(m.into()),
            BusEnvelope::List => Envelope::List,
        }
    }
}
