//! Agent domain types — pure enums and structs, no I/O.

use serde::{Deserialize, Serialize};

/// Session mode for an agent: persistent (default) or ephemeral.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SessionMode {
    /// Continue existing session across tasks (uses --resume).
    #[default]
    Persistent,
    /// Start a fresh session for each task (no --resume).
    Ephemeral,
}

/// Agent runtime protocol: claude (default) or acp.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentRuntime {
    /// Claude stream-json protocol (default).
    #[default]
    Claude,
    /// Agent Client Protocol (ACP) — JSON-RPC 2.0 over stdin/stdout.
    Acp,
}
