//! Port definitions — trait interfaces for infrastructure.
//!
//! Ports depend only on `domain` types. They define the contracts that
//! infrastructure modules implement.

pub mod bus;
pub mod bus_wire;
pub mod executor;
pub mod store;
