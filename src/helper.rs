//! Helper-skill subprocess invocation for the Job framework.
//!
//! [`invoke_helper`] spawns a `claude -p "/plan-executor:<skill>"` child
//! process for one of the four narrow helper skills, passes a JSON `input`
//! envelope through a sidecar file under [`StepContext::workdir`], drains
//! stdout/stderr concurrently, validates the child's stdout JSON envelope
//! against the matching helper output schema, and returns a structured
//! [`HelperOutput`] or an [`HelperError`] mapped to the failure-mode
//! taxonomy.
//!
//! # Failure-mode taxonomy → [`crate::job::recovery::RecoveryPolicy`]
//!
//! | [`HelperError`] variant         | Recommended `RecoveryPolicy`                        |
//! |---------------------------------|-----------------------------------------------------|
//! | [`HelperError::HardInfra`]      | `OperatorDecision { decision_key }` — the fault is on the host (claude not on PATH, auth missing). Retrying without operator intervention will not change the outcome. |
//! | [`HelperError::TransientInfra`] | `RetryTransient { max, backoff: Exponential { .. } }` — covers timeouts, 5xx, network blips. |
//! | [`HelperError::ProtocolViolation { .. }`] | `RetryProtocol { max, corrective }` — the helper produced output that violates the schema; the corrective prompt steers it back to a valid envelope. |
//! | [`HelperError::SemanticFailure { .. }`]   | Caller-specific. The helper finished cleanly but reported `fix_required` / `blocked` / `abort`; the caller pattern-matches on `status` to decide between dispatching a fix wave, escalating, or terminating. |
//!
//! Per-helper wrapper functions (e.g. `run_reviewer_team`) live in a
//! follow-up task (D2.2). Tests live in D2.3.
//!
//! # Subprocess hardening
//!
//! Mirrors [`crate::compile::ClaudeInvoker`]:
//! - `--allowed-tools "Read,Write,Edit"` whitelists tool surface entirely
//!   so a prompt-injected child cannot reach Bash / Grep / WebFetch.
//! - `--dangerously-skip-permissions` removes interactive permission
//!   prompts so the subprocess runs unattended; orthogonal to allowed-tools.
//! - Sensitive credential env vars are scrubbed via
//!   [`crate::compile::scrubbed_env_command`].
//! - Timeout defaults to 600s, override via
//!   `PLAN_EXECUTOR_HELPER_TIMEOUT_SECS` env var or
//!   [`HelperInvocation::timeout`].
//! - stdout / stderr drained concurrently in reader threads to avoid
//!   pipe-buffer deadlocks.

use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use wait_timeout::ChildExt;

use crate::compile::{join_drainer, scrubbed_env_command, spawn_drainer, truncate_for_error};
use crate::job::step::StepContext;

/// Maximum bytes captured from each subprocess stream after a timeout / exit.
const SUBPROCESS_STREAM_CAP_BYTES: u64 = 16 * 1024 * 1024;

/// Maximum bytes of any single subprocess stream embedded in error messages.
const ERROR_TRUNCATE_BYTES: usize = 2048;

/// Default subprocess timeout in seconds when neither
/// [`HelperInvocation::timeout`] nor `PLAN_EXECUTOR_HELPER_TIMEOUT_SECS` is set.
const DEFAULT_HELPER_TIMEOUT_SECS: u64 = 600;

/// Env var consulted to override [`DEFAULT_HELPER_TIMEOUT_SECS`].
const HELPER_TIMEOUT_ENV: &str = "PLAN_EXECUTOR_HELPER_TIMEOUT_SECS";

/// Output schema for the `run-reviewer-team-non-interactive` helper.
const RUN_REVIEWER_TEAM_OUTPUT_SCHEMA: &str =
    include_str!("schemas/helpers/run_reviewer_team/output.schema.json");

/// Output schema for the `review-execution-output-non-interactive` helper.
const REVIEW_EXECUTION_OUTPUT_OUTPUT_SCHEMA: &str =
    include_str!("schemas/helpers/review_execution_output/output.schema.json");

/// Output schema for the `validate-execution-plan-non-interactive` helper.
const VALIDATE_EXECUTION_PLAN_OUTPUT_SCHEMA: &str =
    include_str!("schemas/helpers/validate_execution_plan/output.schema.json");

/// Output schema for the `pr-finalize` helper.
const PR_FINALIZE_OUTPUT_SCHEMA: &str =
    include_str!("schemas/helpers/pr_finalize/output.schema.json");

/// Identifies the narrow helper skill to invoke.
///
/// The slash-command identifier returned by [`HelperSkill::skill_id`] is the
/// non-interactive variant (`*-non-interactive`) for skills that have one;
/// `pr-finalize` has only one form so its id is unsuffixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HelperSkill {
    /// `plan-executor:run-reviewer-team-non-interactive` — frozen reviewer set.
    RunReviewerTeam,
    /// `plan-executor:review-execution-output-non-interactive` — Phase-5 review loop.
    ReviewExecutionOutput,
    /// `plan-executor:validate-execution-plan-non-interactive` — plan-vs-output validation.
    ValidateExecutionPlan,
    /// `plan-executor:pr-finalize` — PR finalize / Bugbot triage.
    PrFinalize,
}

impl HelperSkill {
    /// Slash-command identifier passed to `claude -p "/plan-executor:<id>"`.
    #[must_use]
    pub fn skill_id(self) -> &'static str {
        match self {
            Self::RunReviewerTeam => "run-reviewer-team-non-interactive",
            Self::ReviewExecutionOutput => "review-execution-output-non-interactive",
            Self::ValidateExecutionPlan => "validate-execution-plan-non-interactive",
            Self::PrFinalize => "pr-finalize",
        }
    }

    /// Embedded JSON-Schema text used to validate the helper's stdout envelope.
    ///
    /// The schema is mirrored from the plan-executor-plugin tree at build
    /// time via `include_str!`; it is the wire-format authority for what
    /// helper outputs are accepted.
    #[must_use]
    pub fn output_schema(self) -> &'static str {
        match self {
            Self::RunReviewerTeam => RUN_REVIEWER_TEAM_OUTPUT_SCHEMA,
            Self::ReviewExecutionOutput => REVIEW_EXECUTION_OUTPUT_OUTPUT_SCHEMA,
            Self::ValidateExecutionPlan => VALIDATE_EXECUTION_PLAN_OUTPUT_SCHEMA,
            Self::PrFinalize => PR_FINALIZE_OUTPUT_SCHEMA,
        }
    }

    /// Stable lower_snake_case label used for sidecar file names.
    fn input_file_stem(self) -> &'static str {
        match self {
            Self::RunReviewerTeam => "run_reviewer_team",
            Self::ReviewExecutionOutput => "review_execution_output",
            Self::ValidateExecutionPlan => "validate_execution_plan",
            Self::PrFinalize => "pr_finalize",
        }
    }
}

/// Status reported by the helper in the output envelope.
///
/// Mirrors the `status` enum across all four helper output schemas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HelperStatus {
    /// Helper completed and the caller may proceed.
    Success,
    /// Helper finished but found work the caller must dispatch as a fix wave.
    FixRequired,
    /// Helper is blocked on a missing input or external precondition.
    Blocked,
    /// Helper terminated; the caller must escalate or stop.
    Abort,
}

/// Successful helper output envelope.
///
/// Mirrors the contract from D1.1 — the four required fields shared by every
/// helper output schema. The strongly-typed [`HelperStatus`] is paired with
/// free-form `next_step` and `notes` strings (callers interpret these per
/// helper) and a generic `state_updates` object passed through unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HelperOutput {
    /// Top-level status reported by the helper.
    pub status: HelperStatus,
    /// Next-step hint (helper-specific enum, kept as a string here).
    pub next_step: String,
    /// Free-form caller-facing notes.
    pub notes: String,
    /// Helper-specific state updates; opaque to this module.
    pub state_updates: serde_json::Value,
}

/// Errors surfaced by [`invoke_helper`].
///
/// Each variant maps to a specific [`crate::job::recovery::RecoveryPolicy`]
/// at the call site:
///
/// - [`HelperError::HardInfra`] → `OperatorDecision` (host-level fault, no retry).
/// - [`HelperError::TransientInfra`] → `RetryTransient` with exponential backoff.
/// - [`HelperError::ProtocolViolation`] → `RetryProtocol` with a corrective prompt.
/// - [`HelperError::SemanticFailure`] → caller-specific (often a fix-wave dispatch).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HelperError {
    /// Host-level fault (claude binary missing from `PATH`, auth missing,
    /// permission denied). Operator must intervene before the next attempt.
    #[error("hard infra failure: {0}")]
    HardInfra(String),

    /// Transient host fault (timeout, network blip, 5xx). Safe to retry
    /// with a backoff under [`crate::job::recovery::RecoveryPolicy::RetryTransient`].
    #[error("transient infra failure: {0}")]
    TransientInfra(String),

    /// Helper produced output that violates the wire-format contract
    /// (non-JSON, missing required field, schema violation, status enum
    /// drift). Callers should re-dispatch under
    /// [`crate::job::recovery::RecoveryPolicy::RetryProtocol`] with a
    /// corrective prompt keyed on `category`.
    #[error("protocol violation ({category}): {detail}")]
    ProtocolViolation {
        /// Short tag identifying which schema rule failed (used to select the
        /// corrective-prompt template).
        category: String,
        /// Human-readable detail for logs and operator review.
        detail: String,
    },

    /// Helper completed cleanly but reported a non-success status. The
    /// caller pattern-matches on `status` to choose between dispatching a
    /// fix wave, escalating, or terminating.
    #[error("semantic failure (status={status:?}): {notes}")]
    SemanticFailure {
        /// Status reported by the helper (`fix_required` / `blocked` / `abort`).
        status: HelperStatus,
        /// Helper-supplied notes; passed through verbatim for caller display.
        notes: String,
    },
}

/// Optional knobs passed to [`invoke_helper`].
///
/// Used in lieu of bool / numeric positional params; extends naturally as
/// later phases need additional knobs.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct HelperInvocation {
    /// Override the per-call timeout. `None` falls back to the
    /// `PLAN_EXECUTOR_HELPER_TIMEOUT_SECS` env var, then 600 s.
    pub timeout: Option<Duration>,
}

/// Spawns the helper skill in a child `claude` process and returns its
/// validated output envelope.
///
/// `input` is serialized to a JSON sidecar file under
/// `ctx.workdir/.plan-executor/helpers/<step_seq>-<attempt>-<skill>.json`
/// and its absolute path is appended to the slash-command. The child reads
/// the file via the `Read` tool; this matches the existing convention used
/// by the compile-plan skill (path-arg sidecars, no stdin).
///
/// # Errors
///
/// - [`HelperError::HardInfra`] — `claude` not on `PATH`, permission
///   denied while writing the sidecar file, or workdir not writable.
/// - [`HelperError::TransientInfra`] — child timed out or exited non-zero
///   for a reason that looks recoverable (no `claude not found` signal).
/// - [`HelperError::ProtocolViolation`] — child stdout could not be parsed
///   as JSON, or parsed JSON failed schema validation, or the `status`
///   enum was unrecognized.
/// - [`HelperError::SemanticFailure`] — child reported `fix_required`,
///   `blocked`, or `abort` in its `status` field.
pub fn invoke_helper(
    skill: HelperSkill,
    input: serde_json::Value,
    ctx: &StepContext,
) -> Result<HelperOutput, HelperError> {
    invoke_helper_with(skill, input, ctx, &HelperInvocation::default())
}

/// Variant of [`invoke_helper`] that accepts an explicit
/// [`HelperInvocation`] for timeout overrides.
///
/// # Errors
///
/// Same as [`invoke_helper`].
pub fn invoke_helper_with(
    skill: HelperSkill,
    input: serde_json::Value,
    ctx: &StepContext,
    options: &HelperInvocation,
) -> Result<HelperOutput, HelperError> {
    let sidecar_path = write_input_sidecar(skill, &input, ctx)?;
    let stdout = run_claude_subprocess(skill, &sidecar_path, options)?;
    parse_and_validate_output(skill, &stdout)
}

/// Writes the `input` JSON to a sidecar file the child will read via the
/// `Read` tool, returning its absolute path.
fn write_input_sidecar(
    skill: HelperSkill,
    input: &serde_json::Value,
    ctx: &StepContext,
) -> Result<PathBuf, HelperError> {
    let dir = ctx.workdir.join(".plan-executor").join("helpers");
    fs::create_dir_all(&dir).map_err(|e| {
        HelperError::HardInfra(format!(
            "create helper sidecar dir {} failed: {e}",
            dir.display()
        ))
    })?;
    let file_name = format!(
        "{step:03}-{attempt:03}-{skill}.input.json",
        step = ctx.step_seq,
        attempt = ctx.attempt_n,
        skill = skill.input_file_stem()
    );
    let path = dir.join(file_name);
    let bytes = serde_json::to_vec_pretty(input).map_err(|e| HelperError::ProtocolViolation {
        category: "input_serialize".to_string(),
        detail: format!("serialize helper input failed: {e}"),
    })?;
    fs::write(&path, bytes).map_err(|e| {
        HelperError::HardInfra(format!(
            "write helper sidecar {} failed: {e}",
            path.display()
        ))
    })?;
    Ok(path)
}

/// Spawns the child `claude` process and returns its captured stdout.
///
/// The process pattern (allowed-tools, scrubbed env, drainer threads,
/// timeout, kill-on-timeout) mirrors [`crate::compile::ClaudeInvoker`].
fn run_claude_subprocess(
    skill: HelperSkill,
    sidecar_path: &std::path::Path,
    options: &HelperInvocation,
) -> Result<String, HelperError> {
    let sidecar_str = sidecar_path.to_str().ok_or_else(|| {
        HelperError::HardInfra(format!(
            "sidecar path is not valid UTF-8: {}",
            sidecar_path.display()
        ))
    })?;
    let prompt = format!(
        "/plan-executor:{skill_id} {sidecar}",
        skill_id = skill.skill_id(),
        sidecar = sidecar_str,
    );

    let timeout = resolve_timeout(options);

    let mut child = match scrubbed_env_command()
        .arg("-p")
        .arg(&prompt)
        .arg("--allowed-tools")
        .arg("Read,Write,Edit")
        .arg("--dangerously-skip-permissions")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Err(HelperError::HardInfra(format!(
                "claude binary not found on PATH: {err}"
            )));
        }
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            return Err(HelperError::HardInfra(format!(
                "claude binary not executable: {err}"
            )));
        }
        Err(err) => {
            return Err(HelperError::TransientInfra(format!(
                "spawn claude failed: {err}"
            )));
        }
    };

    let stdout_handle = child
        .stdout
        .take()
        .map(|s| spawn_drainer(s, SUBPROCESS_STREAM_CAP_BYTES));
    let stderr_handle = child
        .stderr
        .take()
        .map(|s| spawn_drainer(s, SUBPROCESS_STREAM_CAP_BYTES));

    let wait_result = child.wait_timeout(timeout);

    let needs_kill = matches!(wait_result, Ok(None) | Err(_));
    if needs_kill {
        let _ = child.kill();
        let _ = child.wait();
    }

    let stdout = stdout_handle.map(join_drainer).unwrap_or_default();
    let stderr = stderr_handle.map(join_drainer).unwrap_or_default();

    let status = match wait_result {
        Ok(Some(status)) => status,
        Ok(None) => {
            return Err(HelperError::TransientInfra(format!(
                "claude timed out after {}s; stdout={}; stderr={}",
                timeout.as_secs(),
                truncate_for_error(stdout.trim(), ERROR_TRUNCATE_BYTES),
                truncate_for_error(stderr.trim(), ERROR_TRUNCATE_BYTES)
            )));
        }
        Err(err) => {
            return Err(HelperError::TransientInfra(format!(
                "wait on claude child failed: {err}"
            )));
        }
    };

    if !status.success() {
        return Err(HelperError::TransientInfra(format!(
            "claude exited {:?}; stderr={}",
            status.code(),
            truncate_for_error(&stderr, ERROR_TRUNCATE_BYTES)
        )));
    }

    Ok(stdout)
}

/// Resolves the effective timeout from the explicit option, env var, and default.
fn resolve_timeout(options: &HelperInvocation) -> Duration {
    if let Some(t) = options.timeout {
        return t;
    }
    let secs = std::env::var(HELPER_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(DEFAULT_HELPER_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Locates the JSON envelope in the child's stdout, validates it against
/// `skill.output_schema()`, and converts it into a [`HelperOutput`] (or a
/// [`HelperError::SemanticFailure`] when status is non-success).
fn parse_and_validate_output(
    skill: HelperSkill,
    stdout: &str,
) -> Result<HelperOutput, HelperError> {
    let envelope_str =
        extract_json_envelope(stdout).ok_or_else(|| HelperError::ProtocolViolation {
            category: "no_json_envelope".to_string(),
            detail: format!(
                "no JSON object found in helper stdout; stdout={}",
                truncate_for_error(stdout.trim(), ERROR_TRUNCATE_BYTES)
            ),
        })?;

    let envelope: serde_json::Value =
        serde_json::from_str(envelope_str).map_err(|e| HelperError::ProtocolViolation {
            category: "invalid_json".to_string(),
            detail: format!(
                "stdout JSON parse failed: {e}; envelope={}",
                truncate_for_error(envelope_str, ERROR_TRUNCATE_BYTES)
            ),
        })?;

    let validator = compiled_validator(skill)?;
    let first_violation = validator.iter_errors(&envelope).next().map(|err| {
        let path = err.instance_path().to_string();
        let detail = format!("schema rule violated at {}: {}", err.instance_path(), err);
        (path, detail)
    });
    if let Some((path, detail)) = first_violation {
        return Err(HelperError::ProtocolViolation {
            category: schema_violation_category(&path),
            detail,
        });
    }

    let parsed: HelperOutput =
        serde_json::from_value(envelope).map_err(|e| HelperError::ProtocolViolation {
            category: "envelope_shape".to_string(),
            detail: format!("decode validated envelope failed: {e}"),
        })?;

    match parsed.status {
        HelperStatus::Success => Ok(parsed),
        status @ (HelperStatus::FixRequired | HelperStatus::Blocked | HelperStatus::Abort) => {
            Err(HelperError::SemanticFailure {
                status,
                notes: parsed.notes,
            })
        }
    }
}

/// Extracts the first balanced top-level JSON object from `stdout`.
///
/// Helpers may print log lines around the envelope; we accept the first
/// `{ ... }` block whose braces balance and treat the rest as preamble /
/// trailer. Returns `None` if no balanced object is found.
fn extract_json_envelope(stdout: &str) -> Option<&str> {
    let bytes = stdout.as_bytes();
    let start = bytes.iter().position(|b| *b == b'{')?;
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escape = false;
    for (idx, b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if *b == b'\\' {
                escape = true;
            } else if *b == b'"' {
                in_string = false;
            }
            continue;
        }
        match *b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&stdout[start..=idx]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Categorizes a schema-violation instance path into a stable short tag the
/// corrective-prompt catalog (D1.x) keys on.
fn schema_violation_category(instance_path: &str) -> String {
    let trimmed = instance_path.trim_start_matches('/');
    if trimmed.is_empty() {
        return "envelope_shape".to_string();
    }
    let head = trimmed.split('/').next().unwrap_or(trimmed);
    match head {
        "status" => "status_enum".to_string(),
        "next_step" => "next_step_enum".to_string(),
        "notes" => "notes_shape".to_string(),
        "state_updates" => "state_updates_shape".to_string(),
        other => format!("schema_{other}"),
    }
}

/// Compiles the embedded schema for `skill` once per process.
fn compiled_validator(skill: HelperSkill) -> Result<&'static jsonschema::Validator, HelperError> {
    static RUN_REVIEWER_TEAM: OnceLock<jsonschema::Validator> = OnceLock::new();
    static REVIEW_EXECUTION_OUTPUT: OnceLock<jsonschema::Validator> = OnceLock::new();
    static VALIDATE_EXECUTION_PLAN: OnceLock<jsonschema::Validator> = OnceLock::new();
    static PR_FINALIZE: OnceLock<jsonschema::Validator> = OnceLock::new();

    let cell: &OnceLock<jsonschema::Validator> = match skill {
        HelperSkill::RunReviewerTeam => &RUN_REVIEWER_TEAM,
        HelperSkill::ReviewExecutionOutput => &REVIEW_EXECUTION_OUTPUT,
        HelperSkill::ValidateExecutionPlan => &VALIDATE_EXECUTION_PLAN,
        HelperSkill::PrFinalize => &PR_FINALIZE,
    };

    if let Some(v) = cell.get() {
        return Ok(v);
    }

    let schema_text = skill.output_schema();
    let schema_json: serde_json::Value = serde_json::from_str(schema_text).map_err(|e| {
        HelperError::HardInfra(format!(
            "embedded helper schema for {:?} is not valid JSON: {e}",
            skill
        ))
    })?;
    let validator = jsonschema::validator_for(&schema_json).map_err(|e| {
        HelperError::HardInfra(format!(
            "embedded helper schema for {:?} failed to compile: {e}",
            skill
        ))
    })?;
    Ok(cell.get_or_init(|| validator))
}
