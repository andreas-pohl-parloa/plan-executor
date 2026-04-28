//! Rollback layer: snapshot/restore of the execute-plan state file plus
//! resolution of `RecoveryPolicy::Rollback` targets.
//!
//! Phase B2.2 (rollback layer). The daemon-side wiring (snapshot before
//! every attempt, restore on exhaustion-with-rollback) lives in Phase D.
//! This module is intentionally pure: it owns no state and never spawns
//! processes.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::job::recovery::{CheckpointTarget, RecoveryPolicy};

/// Errors produced by the rollback layer.
#[derive(Debug, Error)]
pub enum RollbackError {
    /// Filesystem failure reading or writing a checkpoint or state file.
    #[error("io error at {path}: {source}")]
    Io {
        /// Path that caused the failure.
        path: PathBuf,
        /// Underlying io error.
        source: io::Error,
    },
    /// `restore_state` was called but no `checkpoint.json` exists for the
    /// supplied attempt directory.
    #[error("checkpoint.json missing at {path}")]
    MissingCheckpoint {
        /// The expected checkpoint path that was not found.
        path: PathBuf,
    },
    /// The configured rollback target is not applicable for the current
    /// attempt (e.g. `PreviousAttempt` from attempt 1).
    #[error("rollback target not applicable for current attempt")]
    NotApplicable,
}

type Result<T> = std::result::Result<T, RollbackError>;

/// A resolved rollback decision: which attempt's checkpoint to restore and
/// what policy to apply afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackTarget {
    /// 1-indexed attempt number whose checkpoint should be restored.
    pub attempt: u8,
    /// Recovery policy to apply after the rollback completes.
    pub then: RecoveryPolicy,
}

/// Copies `state_file` into `attempt_dir/checkpoint.json` atomically.
///
/// The daemon should call this BEFORE spawning a step that may need
/// rollback recovery. Writes to `<attempt_dir>/checkpoint.json.tmp` then
/// renames to the final location so a crash mid-write cannot corrupt the
/// checkpoint.
///
/// # Errors
///
/// Returns [`RollbackError::Io`] if the attempt directory cannot be
/// created, the source file cannot be read, or the destination write/rename
/// fails.
pub fn snapshot_state(state_file: &Path, attempt_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(attempt_dir).map_err(|source| RollbackError::Io {
        path: attempt_dir.to_path_buf(),
        source,
    })?;
    let bytes = fs::read(state_file).map_err(|source| RollbackError::Io {
        path: state_file.to_path_buf(),
        source,
    })?;
    let dest = attempt_dir.join("checkpoint.json");
    let tmp = attempt_dir.join("checkpoint.json.tmp");
    fs::write(&tmp, &bytes).map_err(|source| RollbackError::Io {
        path: tmp.clone(),
        source,
    })?;
    fs::rename(&tmp, &dest).map_err(|source| RollbackError::Io {
        path: dest.clone(),
        source,
    })?;
    Ok(dest)
}

/// Restores the saved checkpoint at `attempt_dir/checkpoint.json` back to
/// `state_file` atomically.
///
/// Writes to `<state_file>.tmp.rollback` then renames to `state_file`.
///
/// # Errors
///
/// Returns [`RollbackError::MissingCheckpoint`] if no checkpoint exists in
/// the supplied attempt directory; [`RollbackError::Io`] if the read or
/// rename fails.
pub fn restore_state(attempt_dir: &Path, state_file: &Path) -> Result<()> {
    let src = attempt_dir.join("checkpoint.json");
    if !src.is_file() {
        return Err(RollbackError::MissingCheckpoint { path: src });
    }
    let bytes = fs::read(&src).map_err(|source| RollbackError::Io {
        path: src.clone(),
        source,
    })?;
    let tmp_path = state_file.with_extension("tmp.rollback");
    fs::write(&tmp_path, &bytes).map_err(|source| RollbackError::Io {
        path: tmp_path.clone(),
        source,
    })?;
    fs::rename(&tmp_path, state_file).map_err(|source| RollbackError::Io {
        path: state_file.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Resolves a [`RecoveryPolicy`] to a concrete [`RollbackTarget`] given
/// the current attempt number.
///
/// Currently supported targets:
///   - [`CheckpointTarget::PreviousAttempt`] — rolls back to attempt n-1
///     when n >= 2, else `None`.
///
/// `PreviousStep`, `PreviousPhase`, and `Named(_)` defer to step- or
/// phase-level rollback that Phase D will own; they currently return
/// `None`.
#[must_use]
pub fn resolve_rollback_target(
    policy: &RecoveryPolicy,
    current_attempt: u8,
) -> Option<RollbackTarget> {
    let RecoveryPolicy::Rollback { to, then } = policy else {
        return None;
    };
    match to {
        CheckpointTarget::PreviousAttempt => {
            if current_attempt < 2 {
                None
            } else {
                Some(RollbackTarget {
                    attempt: current_attempt - 1,
                    then: (**then).clone(),
                })
            }
        }
        CheckpointTarget::PreviousStep
        | CheckpointTarget::PreviousPhase
        | CheckpointTarget::Named(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::recovery::{Backoff, CorrectivePromptKey};
    use tempfile::TempDir;

    fn write(p: &Path, s: &str) {
        fs::write(p, s).expect("write");
    }

    #[test]
    fn snapshot_copies_state_into_attempt_dir() {
        let td = TempDir::new().unwrap();
        let state = td.path().join(".tmp-execute-plan-state.json");
        let attempt = td.path().join("attempts/1");
        write(&state, r#"{"handoffs":[]}"#);
        let dest = snapshot_state(&state, &attempt).expect("snapshot");
        assert_eq!(dest, attempt.join("checkpoint.json"));
        assert_eq!(fs::read_to_string(&dest).unwrap(), r#"{"handoffs":[]}"#);
    }

    #[test]
    fn restore_writes_checkpoint_back_byte_for_byte() {
        let td = TempDir::new().unwrap();
        let state = td.path().join(".tmp-execute-plan-state.json");
        let attempt = td.path().join("attempts/1");
        write(&state, "ORIGINAL");
        let _ = snapshot_state(&state, &attempt).unwrap();
        write(&state, "MUTATED");
        restore_state(&attempt, &state).expect("restore");
        assert_eq!(fs::read_to_string(&state).unwrap(), "ORIGINAL");
    }

    #[test]
    fn restore_returns_missing_checkpoint_when_absent() {
        let td = TempDir::new().unwrap();
        let state = td.path().join("state.json");
        let attempt = td.path().join("attempts/1");
        fs::create_dir_all(&attempt).unwrap();
        write(&state, "X");
        match restore_state(&attempt, &state) {
            Err(RollbackError::MissingCheckpoint { .. }) => {}
            other => panic!("expected MissingCheckpoint, got {other:?}"),
        }
    }

    #[test]
    fn resolve_returns_none_for_non_rollback_policy() {
        assert!(resolve_rollback_target(&RecoveryPolicy::None, 5).is_none());
        let rt = RecoveryPolicy::RetryTransient {
            max: 3,
            backoff: Backoff::Fixed { ms: 100 },
        };
        assert!(resolve_rollback_target(&rt, 5).is_none());
    }

    #[test]
    fn resolve_returns_previous_attempt_when_applicable() {
        let then = RecoveryPolicy::RetryProtocol {
            max: 1,
            corrective: CorrectivePromptKey("handoffs_missing".into()),
        };
        let policy = RecoveryPolicy::Rollback {
            to: CheckpointTarget::PreviousAttempt,
            then: Box::new(then.clone()),
        };
        let target = resolve_rollback_target(&policy, 3).expect("Some");
        assert_eq!(target.attempt, 2);
        assert_eq!(target.then, then);
    }

    #[test]
    fn resolve_returns_none_when_no_previous_attempt_exists() {
        let policy = RecoveryPolicy::Rollback {
            to: CheckpointTarget::PreviousAttempt,
            then: Box::new(RecoveryPolicy::None),
        };
        assert!(resolve_rollback_target(&policy, 1).is_none());
    }

    #[test]
    fn resolve_returns_none_for_unsupported_targets() {
        let policy = RecoveryPolicy::Rollback {
            to: CheckpointTarget::PreviousStep,
            then: Box::new(RecoveryPolicy::None),
        };
        assert!(resolve_rollback_target(&policy, 5).is_none());
        let policy = RecoveryPolicy::Rollback {
            to: CheckpointTarget::Named("foo".into()),
            then: Box::new(RecoveryPolicy::None),
        };
        assert!(resolve_rollback_target(&policy, 5).is_none());
    }
}
