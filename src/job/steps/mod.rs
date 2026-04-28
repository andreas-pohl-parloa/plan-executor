//! Concrete `Step` implementations grouped by `JobKind`.
//!
//! Phase A only contains stub shells under `plan`. Real bodies arrive in
//! Phase A2.2 (delegation) and Phase D (preflight). Phase C1 adds the
//! production `pr_finalize` step bodies.

pub mod plan;
pub mod pr_finalize;
