//! Bus application layer — re-exports bus server and provides bus construction.
//!
//! The bus server implementation lives in `infra::bus_server`. This module
//! re-exports it for convenience and adds app-level helpers (e.g. `connect_bus`).

pub use crate::infra::bus_server::serve;

use crate::infra::unix_bus::UnixBus;

/// Connect to a bus socket, returning a trait-erased `MessageBus`.
///
/// This is the composition-root factory for bus clients. Application code
/// should call this instead of importing `UnixBus` directly.
pub async fn connect_bus(socket_path: &str) -> anyhow::Result<impl crate::ports::bus::MessageBus> {
    UnixBus::connect(socket_path).await
}
