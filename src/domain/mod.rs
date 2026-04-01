//! Domain layer — pure types and business rules, no I/O.
//!
//! This module depends only on `std` and `serde_json::Value`. It must never import from
//! `infra`, `app`, `adapters`, or any other outer layer.

pub mod agent;
pub mod context;
pub mod message;
pub mod statemachine;
pub mod task;
