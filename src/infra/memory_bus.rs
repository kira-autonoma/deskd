//! In-memory implementation of MessageBus for testing.
//!
//! Uses tokio mpsc channels — no Unix sockets, no filesystem.

use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use crate::domain::message::Message;
use crate::ports::bus::MessageBus;
// Note: InMemoryBus uses domain::Message directly since it's in-memory
// (no serialization boundary). DTOs are for wire/storage formats only.

/// Test-only bus backed by in-memory channels.
pub struct InMemoryBus {
    tx: mpsc::UnboundedSender<Message>,
    rx: Mutex<mpsc::UnboundedReceiver<Message>>,
    registered_name: Mutex<Option<String>>,
}

impl InMemoryBus {
    /// Create a linked pair of buses for testing (client ↔ server).
    ///
    /// Messages sent on `a` arrive at `b.recv()` and vice versa.
    pub fn pair() -> (Self, Self) {
        let (tx_a, rx_b) = mpsc::unbounded_channel();
        let (tx_b, rx_a) = mpsc::unbounded_channel();
        (
            Self {
                tx: tx_a,
                rx: Mutex::new(rx_a),
                registered_name: Mutex::new(None),
            },
            Self {
                tx: tx_b,
                rx: Mutex::new(rx_b),
                registered_name: Mutex::new(None),
            },
        )
    }

    /// Create a single bus where sent messages loop back to recv.
    pub fn loopback() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: Mutex::new(rx),
            registered_name: Mutex::new(None),
        }
    }

    /// Get the registered name (set by register()).
    pub async fn registered_name(&self) -> Option<String> {
        self.registered_name.lock().await.clone()
    }
}

impl MessageBus for InMemoryBus {
    fn send<'a>(
        &'a self,
        msg: &'a Message,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            self.tx
                .send(msg.clone())
                .map_err(|_| anyhow::anyhow!("bus channel closed"))?;
            Ok(())
        })
    }

    fn recv(&self) -> Pin<Box<dyn Future<Output = Result<Message>> + Send + '_>> {
        Box::pin(async move {
            let mut rx = self.rx.lock().await;
            rx.recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("bus channel closed"))
        })
    }

    fn register(
        &self,
        name: &str,
        _subscriptions: &[String],
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            *self.registered_name.lock().await = Some(name);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::{Message, Metadata};

    fn test_message(source: &str, target: &str, task: &str) -> Message {
        Message {
            id: uuid::Uuid::new_v4().to_string(),
            source: source.to_string(),
            target: target.to_string(),
            payload: serde_json::json!({"task": task}),
            reply_to: None,
            metadata: Metadata::default(),
        }
    }

    #[tokio::test]
    async fn test_pair_send_recv() {
        let (a, b) = InMemoryBus::pair();
        let msg = test_message("alice", "agent:bob", "hello");
        a.send(&msg).await.unwrap();
        let received = b.recv().await.unwrap();
        assert_eq!(received.source, "alice");
        assert_eq!(received.target, "agent:bob");
    }

    #[tokio::test]
    async fn test_register() {
        let bus = InMemoryBus::loopback();
        assert!(bus.registered_name().await.is_none());
        bus.register("test-agent", &["agent:test-agent".to_string()])
            .await
            .unwrap();
        assert_eq!(bus.registered_name().await.unwrap(), "test-agent");
    }

    #[tokio::test]
    async fn test_loopback() {
        let bus = InMemoryBus::loopback();
        let msg = test_message("self", "agent:self", "echo");
        bus.send(&msg).await.unwrap();
        let received = bus.recv().await.unwrap();
        assert_eq!(received.id, msg.id);
    }
}
