//! plan-executor library crate.
//!
//! Exposes the internal modules so integration tests under `tests/` can
//! exercise the Job-framework step API directly. The `plan-executor` binary
//! continues to be built from `src/main.rs`, which declares its own private
//! module tree; cargo compiles the library and binary as separate targets,
//! so this file is purely additive infrastructure for tests and does not
//! alter binary behavior.

pub mod cli;
pub mod compile;
pub mod config;
pub mod daemon;
pub mod executor;
pub mod finding;
pub mod handoff;
pub mod helper;
pub mod ipc;
pub mod job;
pub mod jobs;
pub mod plan;
pub mod proctree;
pub mod remote;
pub mod schema;
pub mod scheduler;
pub mod supervisor;
pub mod validate;
