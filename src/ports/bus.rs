//! MessageBus port — abstraction over message transport.
//!
//! The bus handles routing messages between agents, adapters, and external
//! targets. Implementations may use Unix sockets, in-memory channels, or
//! network transports.

use anyhow::Result;
use std::future::Future;
use std::pin::Pin;

use crate::domain::message::Message;

/// Abstraction over message transport (object-safe).
///
/// Workers and adapters interact with the bus through this trait.
/// Implementations: `UnixBus` (production), `InMemoryBus` (testing).
pub trait MessageBus: Send + Sync {
    /// Send a message to the bus for routing.
    fn send<'a>(
        &'a self,
        msg: &'a Message,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

    /// Receive the next message addressed to this client.
    fn recv(&self) -> Pin<Box<dyn Future<Output = Result<Message>> + Send + '_>>;

    /// Register this client on the bus with the given subscriptions.
    fn register(
        &self,
        name: &str,
        subscriptions: &[String],
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
}
