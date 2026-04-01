//! Core message types for the deskd bus protocol.
//!
//! Pure data types — no I/O, no transport logic.
//! Serde lives on infra DTOs (infra::dto), not here.

#[derive(Debug, Clone)]
pub struct Register {
    pub name: String,
    pub subscriptions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub id: String,
    pub source: String,
    pub target: String,
    pub payload: serde_json::Value,
    pub reply_to: Option<String>,
    pub metadata: Metadata,
}

#[derive(Debug, Clone)]
pub struct Metadata {
    pub priority: u8,
    /// When true, the worker should start a fresh session for this task
    /// (no --resume), regardless of the agent's default session config.
    pub fresh: bool,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            priority: 5,
            fresh: false,
        }
    }
}

#[derive(Debug)]
pub enum Envelope {
    Register(Register),
    Message(Message),
    /// Query: list currently connected agents on this bus.
    List,
}
