//! State machine store application layer — re-exports store and DTO types.
//!
//! The state machine store implementation lives in `infra::sm_store`. This module
//! re-exports it for backward compatibility. The `StoredInstance` DTO is also
//! re-exported for serialization needs.

pub use crate::infra::dto::StoredInstance;
pub use crate::infra::sm_store::*;
