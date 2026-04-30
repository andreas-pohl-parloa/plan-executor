//! Supervisor: detects and recovers from LLM-protocol violations.
//!
//! Phase B1.1 ships the violation taxonomy and the pure detector function;
//! Phase B1.2 ships the corrective-prompt template catalog. The runtime
//! re-prompt loop that consumes both lives in B2.1 (`src/daemon.rs`).
//!
//! Most items here are scaffolding for the not-yet-active supervisor
//! runtime (the daemon's re-prompt loop wires in via a later phase). The
//! module-level `dead_code` allow keeps the build clean today; remove the
//! allow once the supervisor is wired in so genuine drift surfaces again.
#![allow(dead_code)]

pub mod detector;
pub mod prompts;
pub mod rollback;
pub mod violation;
pub mod wiring;
