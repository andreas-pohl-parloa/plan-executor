//! Supervisor: detects and recovers from LLM-protocol violations.
//!
//! Phase B1.1 ships the violation taxonomy and the pure detector function;
//! Phase B1.2 ships the corrective-prompt template catalog. The runtime
//! re-prompt loop that consumes both lives in B2.1 (`src/daemon.rs`).

pub mod detector;
pub mod prompts;
pub mod violation;
pub mod wiring;
