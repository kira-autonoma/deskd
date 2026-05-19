//! Telegram + agent-command dispatch abstractions for the web adapter (#443/#445).
//!
//! Sending a magic-link message to Telegram from the web adapter must NOT
//! reach into the Telegram adapter directly — that would couple the two
//! adapters and bypass the bus. Instead we publish a bus message to
//! `telegram.out:<chat_id>`; the existing Telegram adapter is already
//! subscribed to `telegram.out:*` and forwards the text to Telegram (see
//! `app::adapters::telegram::bus_loop`).
//!
//! Structured agent commands (#445) follow the same shape — the web adapter
//! publishes a `{command: "restart" | "compact" | "stop"}` payload to
//! `agent:<name>`. The runtime decides what to do with it; the web adapter
//! is only responsible for the request half of the contract.
//!
//! `TelegramDispatcher` and `AgentCommandDispatcher` are thin traits so
//! integration tests can substitute recording doubles without spinning up
//! a unix bus.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[async_trait]
pub trait TelegramDispatcher: Send + Sync + 'static {
    /// Send `text` to Telegram chat `chat_id`. Returns Ok on best-effort
    /// dispatch (the Telegram adapter ack is fire-and-forget).
    async fn send(&self, chat_id: i64, text: &str) -> anyhow::Result<()>;
}

/// Bus dispatcher for arbitrary targets — used by the GitHub webhook adapter
/// (#457) to deliver event notifications to `agent:<name>` inboxes. Kept
/// separate from `TelegramDispatcher` so tests can stub it independently and
/// production wiring can fan out to any bus target without going through the
/// Telegram-out routing path.
#[async_trait]
pub trait BusSender: Send + Sync + 'static {
    /// Publish `text` to `target` on the bus along with a structured
    /// `metadata` JSON object the receiver can pivot on without re-fetching.
    async fn send_bus(
        &self,
        target: &str,
        text: &str,
        metadata: serde_json::Value,
    ) -> anyhow::Result<()>;
}

/// Production dispatcher: publishes to `telegram.out:<chat_id>` on the
/// agent bus. Wraps `app::bus::send_message` which expects a `task` payload
/// — the Telegram adapter's `bus_loop` reads `payload.result || payload.task
/// || payload.error`, so `send_message` works as-is.
pub struct BusDispatcher {
    pub bus_socket: String,
    pub source: String,
}

impl BusDispatcher {
    pub fn new(bus_socket: String, source: String) -> Self {
        Self { bus_socket, source }
    }
}

#[async_trait]
impl TelegramDispatcher for BusDispatcher {
    async fn send(&self, chat_id: i64, text: &str) -> anyhow::Result<()> {
        let target = format!("telegram.out:{}", chat_id);
        crate::app::bus::send_message(&self.bus_socket, &self.source, &target, text).await
    }
}

#[async_trait]
impl BusSender for BusDispatcher {
    async fn send_bus(
        &self,
        target: &str,
        text: &str,
        metadata: serde_json::Value,
    ) -> anyhow::Result<()> {
        use crate::domain::message::{Message, Metadata};
        use crate::ports::bus::MessageBus;
        let bus = crate::app::bus::connect_bus(&self.bus_socket).await?;
        bus.register(&self.source, &[]).await?;
        let msg = Message {
            id: uuid::Uuid::new_v4().to_string(),
            source: self.source.clone(),
            target: target.to_string(),
            payload: serde_json::json!({"task": text, "github": metadata}),
            reply_to: None,
            metadata: Metadata::default(),
        };
        bus.send(&msg).await?;
        Ok(())
    }
}

/// Structured per-agent command emitted by `/agent/<name>/<action>` (#445).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCommand {
    /// Stop the agent process and respawn it with the same config + session.
    Restart,
    /// Trigger configured context compaction strategy immediately (#403).
    Compact,
    /// Stop the agent; do not respawn.
    Stop,
}

impl AgentCommand {
    /// Stable serde tag (`"restart"` / `"compact"` / `"stop"`).
    pub fn as_str(self) -> &'static str {
        match self {
            AgentCommand::Restart => "restart",
            AgentCommand::Compact => "compact",
            AgentCommand::Stop => "stop",
        }
    }
}

/// Publish a structured command to `agent:<name>` on the bus. Implementations
/// must be cheap to clone (`Arc<dyn ...>` is the prevailing pattern).
#[async_trait]
pub trait AgentCommandDispatcher: Send + Sync + 'static {
    async fn send(&self, agent_name: &str, command: AgentCommand) -> anyhow::Result<()>;
}

/// Production command dispatcher: posts a `{command: "..."}` payload to
/// `agent:<name>` via the existing per-agent bus.
pub struct BusAgentCommandDispatcher {
    pub bus_socket: String,
    pub source: String,
}

impl BusAgentCommandDispatcher {
    pub fn new(bus_socket: String, source: String) -> Self {
        Self { bus_socket, source }
    }
}

#[async_trait]
impl AgentCommandDispatcher for BusAgentCommandDispatcher {
    async fn send(&self, agent_name: &str, command: AgentCommand) -> anyhow::Result<()> {
        // Send as the JSON payload `{"command": "<name>"}` — same shape the
        // Telegram adapter uses for `/restart` so the runtime can listen on
        // one envelope shape.
        let target = format!("agent:{}", agent_name);
        let payload_text = serde_json::json!({ "command": command.as_str() }).to_string();
        crate::app::bus::send_message(&self.bus_socket, &self.source, &target, &payload_text).await
    }
}

pub mod testing {
    //! Recording dispatchers for tests. Public so integration tests in the
    //! `tests/` tree can reuse them without re-declaring the trait impls.

    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default, Clone)]
    pub struct RecordingDispatcher {
        pub sent: Arc<Mutex<Vec<(i64, String)>>>,
    }

    impl RecordingDispatcher {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn calls(&self) -> Vec<(i64, String)> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TelegramDispatcher for RecordingDispatcher {
        async fn send(&self, chat_id: i64, text: &str) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push((chat_id, text.to_string()));
            Ok(())
        }
    }

    /// Recording bus sender. Captures `(target, text, metadata)` so tests can
    /// assert what the webhook adapter dispatched.
    #[derive(Default, Clone)]
    pub struct RecordingBusSender {
        pub sent: Arc<Mutex<Vec<(String, String, serde_json::Value)>>>,
    }

    impl RecordingBusSender {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn calls(&self) -> Vec<(String, String, serde_json::Value)> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl BusSender for RecordingBusSender {
        async fn send_bus(
            &self,
            target: &str,
            text: &str,
            metadata: serde_json::Value,
        ) -> anyhow::Result<()> {
            self.sent
                .lock()
                .unwrap()
                .push((target.to_string(), text.to_string(), metadata));
            Ok(())
        }
    }

    /// Recording dispatcher for `AgentCommandDispatcher`. Used by the #445
    /// integration tests to assert that POST handlers actually publish.
    #[derive(Default, Clone)]
    pub struct RecordingAgentCommandDispatcher {
        pub sent: Arc<Mutex<Vec<(String, AgentCommand)>>>,
    }

    impl RecordingAgentCommandDispatcher {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn calls(&self) -> Vec<(String, AgentCommand)> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AgentCommandDispatcher for RecordingAgentCommandDispatcher {
        async fn send(&self, agent_name: &str, command: AgentCommand) -> anyhow::Result<()> {
            self.sent
                .lock()
                .unwrap()
                .push((agent_name.to_string(), command));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_command_serializes_snake_case() {
        assert_eq!(AgentCommand::Restart.as_str(), "restart");
        assert_eq!(AgentCommand::Compact.as_str(), "compact");
        assert_eq!(AgentCommand::Stop.as_str(), "stop");
        let v = serde_json::to_value(AgentCommand::Restart).unwrap();
        assert_eq!(v, serde_json::json!("restart"));
    }
}
