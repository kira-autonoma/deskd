//! Unix socket implementation of the MessageBus trait.
//!
//! Wraps a single UnixStream connection to the bus server.
//! Handles registration, sending envelopes, and receiving messages
//! as newline-delimited JSON.

use anyhow::{Context, Result};
use std::future::Future;
use std::pin::Pin;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Mutex;
use tracing::{info, warn};
use uuid::Uuid;

use crate::domain::message::Message;
use crate::ports::bus::MessageBus;

/// Production bus client backed by a Unix socket connection.
pub struct UnixBus {
    reader: Mutex<BufReader<OwnedReadHalf>>,
    writer: Mutex<OwnedWriteHalf>,
    socket_path: String,
}

impl UnixBus {
    /// Connect to the bus socket with retries (exponential backoff).
    pub async fn connect(socket_path: &str) -> Result<Self> {
        let max_retries = 10u32;
        let initial_delay = std::time::Duration::from_millis(100);

        let mut stream = None;
        let mut last_err = None;
        for attempt in 0..max_retries {
            match UnixStream::connect(socket_path).await {
                Ok(s) => {
                    if attempt > 0 {
                        info!(attempt = attempt + 1, "connected to bus after retry");
                    }
                    stream = Some(s);
                    break;
                }
                Err(e) => {
                    if attempt + 1 < max_retries {
                        let delay = initial_delay * 2u32.saturating_pow(attempt);
                        warn!(
                            attempt = attempt + 1,
                            delay_ms = delay.as_millis() as u64,
                            "bus not ready, retrying"
                        );
                        tokio::time::sleep(delay).await;
                    }
                    last_err = Some(e);
                }
            }
        }

        let stream = match stream {
            Some(s) => s,
            None => {
                return Err(last_err.unwrap()).with_context(|| {
                    format!(
                        "Failed to connect to bus at {} after {} attempts",
                        socket_path, max_retries
                    )
                });
            }
        };

        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: Mutex::new(BufReader::new(reader)),
            writer: Mutex::new(writer),
            socket_path: socket_path.to_string(),
        })
    }

    /// The socket path this bus is connected to.
    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }

    /// Build a message envelope JSON for the bus protocol.
    pub fn build_envelope(source: &str, target: &str, payload: serde_json::Value) -> String {
        let envelope = serde_json::json!({
            "type": "message",
            "id": Uuid::new_v4().to_string(),
            "source": source,
            "target": target,
            "payload": payload,
            "metadata": {"priority": 5u8},
        });
        serde_json::to_string(&envelope).unwrap_or_default()
    }
}

impl MessageBus for UnixBus {
    fn send<'a>(
        &'a self,
        msg: &'a Message,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let envelope = serde_json::json!({
                "type": "message",
                "id": &msg.id,
                "source": &msg.source,
                "target": &msg.target,
                "payload": &msg.payload,
                "reply_to": &msg.reply_to,
                "metadata": &msg.metadata,
            });
            let mut line = serde_json::to_string(&envelope)?;
            line.push('\n');
            let mut writer = self.writer.lock().await;
            writer.write_all(line.as_bytes()).await?;
            Ok(())
        })
    }

    fn recv(&self) -> Pin<Box<dyn Future<Output = Result<Message>> + Send + '_>> {
        Box::pin(async move {
            let mut reader = self.reader.lock().await;
            loop {
                let mut line = String::new();
                let n = reader.read_line(&mut line).await?;
                if n == 0 {
                    anyhow::bail!("bus connection closed");
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Message>(trimmed) {
                    Ok(msg) => return Ok(msg),
                    Err(e) => {
                        warn!(error = %e, "invalid message from bus, skipping");
                        continue;
                    }
                }
            }
        })
    }

    fn register(
        &self,
        name: &str,
        subscriptions: &[String],
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        let name = name.to_string();
        let subs = subscriptions.to_vec();
        Box::pin(async move {
            let envelope = serde_json::json!({
                "type": "register",
                "name": name,
                "subscriptions": subs,
            });
            let mut line = serde_json::to_string(&envelope)?;
            line.push('\n');
            let mut writer = self.writer.lock().await;
            writer.write_all(line.as_bytes()).await?;
            info!(agent = %name, "registered on bus");
            Ok(())
        })
    }
}
