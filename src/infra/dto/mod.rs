//! Infrastructure DTOs — wire and storage formats.
//!
//! Split by aggregate boundary:
//! - `bus` — bus wire format, domain↔bus conversions, event serialization
//! - `task` — task storage format
//! - `instance` — state machine instance storage format
//! - `config` — YAML config parsing types (agent, model, transition)
//! - `context` — context config and persistence types

pub mod bus;
pub mod config;
pub mod context;
pub mod instance;
pub mod task;

// Re-export all types at the `dto` level for backward compatibility.
pub use bus::*;
pub use config::*;
pub use context::*;
pub use instance::*;
pub use task::*;
