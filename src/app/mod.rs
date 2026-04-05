//! Application layer — wiring, commands, and runtime modules.
//!
//! Groups internal modules that aren't part of the public library API
//! (domain types and port traits). Reduces lib.rs fan-out.

pub mod acp;
pub mod adapters;
pub mod agent;
pub mod bus;
pub mod cli;
pub mod commands;
pub mod context;
pub mod graph;
pub mod mcp;
pub mod mcp_service;
pub mod message;
pub mod schedule;
pub mod serve;
pub mod statemachine;
pub mod task;
pub mod tasklog;
pub mod timeout_sweep;
pub mod unified_inbox;
pub mod worker;
pub mod workflow;
