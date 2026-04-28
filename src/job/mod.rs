//! Job framework: core types for jobs, steps, and recovery semantics.
//!
//! This module hosts the type definitions used by the Job/Step framework. The
//! `types` submodule contains pure data structures (no behavior). Subsequent
//! waves will introduce execution, recovery, and storage submodules.

pub mod recovery;
pub mod registry;
pub mod step;
pub mod steps;
pub mod storage;
pub mod types;
