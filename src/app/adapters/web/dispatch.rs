//! Telegram dispatch abstraction for the web adapter (#443).
//!
//! Sending a magic-link message to Telegram from the web adapter must NOT
//! reach into the Telegram adapter directly — that would couple the two
//! adapters and bypass the bus. Instead we publish a bus message to
//! `telegram.out:<chat_id>`; the existing Telegram adapter is already
//! subscribed to `telegram.out:*` and forwards the text to Telegram (see
//! `app::adapters::telegram::bus_loop`).
//!
//! `TelegramDispatcher` is a thin trait so integration tests can substitute
//! a recording double without spinning up a unix bus.

use async_trait::async_trait;

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

pub mod testing {
    //! Recording dispatcher for tests. Public so integration tests in the
    //! `tests/` tree can reuse it without re-declaring the trait impl.

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
}
