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
//! Per-helper wrapper functions ([`invoke_review_team`],
//! [`invoke_review_triage`], [`invoke_validator`], [`invoke_pr_finalize`])
//! provide strongly-typed input/output structs around [`invoke_helper`].
//! Tests live in D2.3.
//!
//! # Subprocess hardening
//!
//! Mirrors [`crate::compile::ClaudeInvoker`]:
//! - `--allowed-tools "Read,Write,Edit"` whitelists tool surface entirely
//!   so a prompt-injected child cannot reach Bash / Grep / WebFetch.
//! - `--add-dir <ctx.workdir>` jails the whitelisted Read/Write/Edit tools
//!   to the per-step workdir so a prompt-injected child cannot exfiltrate
//!   files outside the job's working tree (e.g. `~/.ssh/`,
//!   `~/.aws/credentials`, `~/.codex/auth.json`). The helper sidecar
//!   directory (`<ctx.workdir>/.plan-executor/helpers`) is nested inside
//!   `ctx.workdir`, so a single `--add-dir` covers both the sidecar I/O and
//!   the working tree the helper legitimately needs.
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

use crate::compile::{join_drainer, scrubbed_env_command, truncate_for_error};
use crate::handoff::SubAgentLine;
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
            Self::ValidateExecutionPlan => VALIDATE_EXECUTION_PLAN_OUTPUT_SCHEMA,
            Self::PrFinalize => PR_FINALIZE_OUTPUT_SCHEMA,
        }
    }

    /// Stable lower_snake_case label used for sidecar file names.
    fn input_file_stem(self) -> &'static str {
        match self {
            Self::RunReviewerTeam => "run_reviewer_team",
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
    /// Helper wrote prompt files for one or more sub-agents and is asking the
    /// caller to dispatch them. The caller reads `state_updates.handoffs[]`,
    /// dispatches each prompt file via `handoff::dispatch_all`, then re-invokes
    /// the helper with the captured outputs in `handoff_outputs[]`. The helper
    /// re-enters with that input and produces the final `Success` envelope.
    WaitingForHandoffs,
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
        /// Helper-supplied `state_updates` payload, preserved verbatim so that
        /// callers handling `fix_required` can inspect helper-specific
        /// fields (e.g. validator `gaps`). Set to [`serde_json::Value::Null`]
        /// when the helper did not emit a `state_updates` block.
        state_updates: serde_json::Value,
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
    let stdout = run_claude_subprocess(skill, &sidecar_path, ctx, options)?;
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
///
/// `workdir` is passed verbatim to `claude --add-dir <workdir>` so the
/// whitelisted Read/Write/Edit tools cannot escape the per-step working
/// directory. This is the security boundary that prevents a prompt-injected
/// helper from reading host secrets such as `~/.ssh/`, `~/.aws/credentials`,
/// or `~/.codex/auth.json`. The helper sidecar directory lives inside
/// `workdir`, so one `--add-dir` covers both the sidecar I/O and the
/// working tree.
fn run_claude_subprocess(
    skill: HelperSkill,
    sidecar_path: &std::path::Path,
    ctx: &StepContext,
    options: &HelperInvocation,
) -> Result<String, HelperError> {
    let workdir = ctx.workdir.as_path();
    let sidecar_str = sidecar_path.to_str().ok_or_else(|| {
        HelperError::HardInfra(format!(
            "sidecar path is not valid UTF-8: {}",
            sidecar_path.display()
        ))
    })?;
    let workdir_str = workdir.to_str().ok_or_else(|| {
        HelperError::HardInfra(format!(
            "workdir path is not valid UTF-8: {}",
            workdir.display()
        ))
    })?;
    let prompt = format!(
        "/plan-executor:{skill_id} {sidecar}",
        skill_id = skill.skill_id(),
        sidecar = sidecar_str,
    );

    let timeout = resolve_timeout(options);

    let mut command = scrubbed_env_command();
    command
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("-p")
        .arg(&prompt)
        .arg("--allowed-tools")
        .arg("Read,Write,Edit")
        .arg("--add-dir")
        .arg(workdir_str)
        .arg("--dangerously-skip-permissions")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Put the helper child in its own process group so the daemon's
    // `KillJob` can SIGKILL it (and any descendants) via the pgid the
    // dispatcher path already uses for wave sub-agents. Without this, the
    // helper inherits the daemon's pgid and kill -PGID would target the
    // daemon itself.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = match command.spawn()
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

    // Wire the helper child into the daemon's sub-agent rendering and
    // kill paths:
    //   * pgid_registrar so `plan-executor kill` SIGKILLs the helper's
    //     process group via the same pathway the wave dispatcher uses;
    //   * subagent_writer so each JSONL event the helper streams is
    //     persisted under `<job>/sub-agents/dispatch-<N>-...` and
    //     broadcast as a `SubAgentLine` event for `plan-executor output
    //     -f` to render live.
    // No-op when ctx.daemon_hooks is unset (foreground / tests).
    let subagent_tx = if let Some(hooks) = ctx.daemon_hooks.as_ref() {
        let pgid_tx = hooks.spawn_pgid_registrar();
        // With process_group(0) above, the child's PID equals its PGID.
        let _ = pgid_tx.send(child.id());
        let dispatch_num =
            hooks.announce_helper_dispatch(1, &format!("helper:{}", skill.skill_id()));
        Some(hooks.spawn_subagent_writer(dispatch_num))
    } else {
        None
    };

    let stdout_handle = child.stdout.take().map(|s| {
        spawn_streaming_drainer(
            s,
            SUBPROCESS_STREAM_CAP_BYTES,
            subagent_tx.clone(),
            false,
        )
    });
    let stderr_handle = child.stderr.take().map(|s| {
        spawn_streaming_drainer(
            s,
            SUBPROCESS_STREAM_CAP_BYTES,
            subagent_tx.clone(),
            true,
        )
    });

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

/// Streams `reader` line-by-line, simultaneously appending each line to a
/// local byte buffer (returned via the join handle) and forwarding each
/// line to the daemon's `subagent_tx` channel as a `SubAgentLine` (so
/// `plan-executor output -f` can render helper progress live, the same
/// way it renders wave sub-agent output).
///
/// `is_stderr` selects the channel marker so the renderer can split
/// stdout (JSONL stream-json) from stderr (raw text) into sibling files.
/// `cap` bounds the buffer to defend against runaway helpers; bytes past
/// the cap are streamed into the channel but not retained for envelope
/// extraction (envelope must arrive before the cap is hit, which the
/// 5 MiB default leaves ample room for).
fn spawn_streaming_drainer<R: std::io::Read + Send + 'static>(
    reader: R,
    cap: u64,
    subagent_tx: Option<tokio::sync::mpsc::UnboundedSender<SubAgentLine>>,
    is_stderr: bool,
) -> std::thread::JoinHandle<Vec<u8>> {
    use std::io::{BufRead, BufReader};
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut buf = Vec::with_capacity(8 * 1024);
        let mut line = String::new();
        let mut over_cap = false;
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if !over_cap {
                        buf.extend_from_slice(line.as_bytes());
                        if buf.len() as u64 > cap {
                            over_cap = true;
                        }
                    }
                    if let Some(tx) = subagent_tx.as_ref() {
                        let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
                        if !trimmed.is_empty() {
                            let _ = tx.send(SubAgentLine {
                                index: 1,
                                agent_type: "claude",
                                is_stderr,
                                line: trimmed,
                            });
                        }
                    }
                }
                Err(_) => break,
            }
        }
        buf
    })
}

/// Locates the JSON envelope in the child's stdout, validates it against
/// `skill.output_schema()`, and converts it into a [`HelperOutput`] (or a
/// [`HelperError::SemanticFailure`] when status is non-success).
fn parse_and_validate_output(
    skill: HelperSkill,
    stdout: &str,
) -> Result<HelperOutput, HelperError> {
    // The helper child runs with `--output-format stream-json --verbose`, so
    // its stdout is JSONL (one event per line). The agent's actual textual
    // response — which is where the envelope lives — sits in the terminal
    // `{"type":"result","result":"..."}` event. Unwrap that first; if the
    // stream looks like raw text (no recognizable result event), fall back
    // to scanning stdout directly so the test fixtures and any non-stream
    // execution path keep working.
    let candidate: String = {
        let lines: Vec<String> = stdout.lines().map(str::to_string).collect();
        crate::handoff::extract_result_text(&lines).unwrap_or_else(|| stdout.to_string())
    };
    let envelope_str =
        extract_json_envelope(&candidate).ok_or_else(|| HelperError::ProtocolViolation {
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
        status @ (HelperStatus::FixRequired
        | HelperStatus::Blocked
        | HelperStatus::Abort
        | HelperStatus::WaitingForHandoffs) => Err(HelperError::SemanticFailure {
            status,
            notes: parsed.notes,
            state_updates: parsed.state_updates,
        }),
    }
}

/// Extracts the helper response envelope from `stdout`.
///
/// Helpers may print prose, descriptive object literals, or markdown around
/// the envelope. We scan every balanced `{ ... }` block in `stdout` and
/// return the first one that:
///
/// 1. Parses as JSON (tolerates `{ ... }` around the envelope but rejects
///    e.g. JS-object-literal descriptions where keys aren't quoted).
/// 2. Carries a top-level `"status"` key — the helper protocol mandates it
///    on every envelope, so an object without `"status"` is by definition
///    not the envelope.
///
/// When no candidate matches both, falls back to the first balanced block
/// (so existing callers with prose-free output still work). The downstream
/// schema validator catches genuine shape errors with a precise category.
fn extract_json_envelope(stdout: &str) -> Option<&str> {
    let mut first_balanced: Option<&str> = None;
    for candidate in balanced_object_blocks(stdout) {
        if first_balanced.is_none() {
            first_balanced = Some(candidate);
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) {
            if value.get("status").is_some() {
                return Some(candidate);
            }
        }
    }
    first_balanced
}

/// Yields every balanced `{ ... }` substring in `stdout` in order, treating
/// quoted strings opaquely so braces inside string literals don't count.
/// Used by [`extract_json_envelope`] to enumerate candidate envelopes.
fn balanced_object_blocks(stdout: &str) -> impl Iterator<Item = &str> {
    let bytes = stdout.as_bytes();
    let mut cursor: usize = 0;
    std::iter::from_fn(move || {
        while cursor < bytes.len() {
            let start = bytes
                .iter()
                .enumerate()
                .skip(cursor)
                .find(|(_, b)| **b == b'{')
                .map(|(i, _)| i)?;
            let mut depth: usize = 0;
            let mut in_string = false;
            let mut escape = false;
            let mut end_inclusive: Option<usize> = None;
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
                            end_inclusive = Some(idx);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            match end_inclusive {
                Some(end) => {
                    cursor = end + 1;
                    return Some(&stdout[start..=end]);
                }
                None => return None,
            }
        }
        None
    })
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
    static VALIDATE_EXECUTION_PLAN: OnceLock<jsonschema::Validator> = OnceLock::new();
    static PR_FINALIZE: OnceLock<jsonschema::Validator> = OnceLock::new();

    let cell: &OnceLock<jsonschema::Validator> = match skill {
        HelperSkill::RunReviewerTeam => &RUN_REVIEWER_TEAM,
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

// ---------------------------------------------------------------------------
// Per-helper typed wrappers (Task D2.2)
//
// Each wrapper:
//   1. Accepts a strongly-typed input struct (Serialize).
//   2. Builds the JSON envelope and calls `invoke_helper`.
//   3. Decodes `HelperOutput::state_updates` into a typed output struct.
//   4. Surfaces `HelperError` unchanged.
//
// The input shapes follow the corresponding helper SKILL contracts'
// `Required Inputs`. The output shapes mirror the `state_updates` shape in
// `src/schemas/helpers/<helper>/output.schema.json` (Task D1.1).
// ---------------------------------------------------------------------------

/// Decodes `state_updates` from a [`HelperOutput`] into the caller's typed
/// output struct, mapping serde failures into [`HelperError::ProtocolViolation`].
fn decode_state_updates<T: serde::de::DeserializeOwned>(
    output: HelperOutput,
) -> Result<T, HelperError> {
    serde_json::from_value(output.state_updates).map_err(|e| HelperError::ProtocolViolation {
        category: "state_updates_shape".to_string(),
        detail: format!("decode state_updates failed: {e}"),
    })
}

/// Serializes a wrapper input struct into the JSON envelope passed to
/// [`invoke_helper`], mapping serde failures into
/// [`HelperError::ProtocolViolation`] so callers see a uniform error type.
fn serialize_wrapper_input<T: Serialize>(input: &T) -> Result<serde_json::Value, HelperError> {
    serde_json::to_value(input).map_err(|e| HelperError::ProtocolViolation {
        category: "input_serialize".to_string(),
        detail: format!("serialize wrapper input failed: {e}"),
    })
}

// ----- run-reviewer-team-non-interactive -----------------------------------

/// Input envelope for `plan-executor:run-reviewer-team-non-interactive`.
///
/// Fields mirror the `Required Inputs` listed in the helper's SKILL contract.
/// Production code in `job::steps::plan` builds one of these and passes it
/// through `invoke_helper`; the helper protocol does not require the typed
/// output side, so only the input is plumbed.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewTeamInput {
    /// Plan path or relevant plan excerpts that define the expected implementation.
    pub plan_context: String,
    /// Description or summary of what was built or changed during execution.
    pub execution_outputs: String,
    /// Files created or modified during the wave under review.
    pub changed_files: Vec<PathBuf>,
    /// Detected primary language of the changed files (lower-case, e.g. `"rust"`).
    pub language: String,
    /// Recipe skills relevant to the changed code (used to build reviewer prompts).
    pub recipe_list: Vec<String>,
    /// Prior triage history for this review loop. Pass an empty object on the first run.
    pub prior_review_context: serde_json::Value,
    /// Absolute path to the directory where prompt files are written.
    pub execution_root: PathBuf,
    /// 1-based attempt number; used in prompt-file names to prevent clobbering.
    pub attempt: u32,
    /// Absolute path to the JSON sidecar carrying the dispatched sub-agent
    /// outputs the orchestrator just collected. Empty string on the first
    /// invocation (dispatch mode); non-empty on the re-invocation that
    /// follows a `waiting_for_handoffs` envelope, signaling the skill to
    /// enter triage mode and parse the sidecar at the given path.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prior_handoff_outputs_path: String,
}

// ----- validate-execution-plan-non-interactive -----------------------------

/// Input envelope for `plan-executor:validate-execution-plan-non-interactive`.
#[derive(Debug, Clone, Serialize)]
pub struct ValidatorInput {
    /// Full plan path.
    pub plan_path: PathBuf,
    /// Execution root.
    pub execution_root: PathBuf,
    /// Changed files.
    pub changed_files: Vec<PathBuf>,
    /// Language.
    pub language: String,
    /// Recipe list.
    pub recipe_list: Vec<String>,
    /// Skip-code-review flag.
    pub skip_code_review: bool,
    /// State file path.
    pub state_file_path: PathBuf,
    /// Execution orchestration state.
    pub execution_state: serde_json::Value,
    /// Current helper-owned validation state when already available.
    pub validation_state: serde_json::Value,
    /// Persisted helper-owned validation-state path when state is resumed from storage.
    pub validation_state_path: Option<PathBuf>,
    /// Current validation attempt.
    pub current_validation_attempt: u32,
    /// Prior validation notes, including prior GAPS and DEVIATIONS.
    pub prior_validation_notes: serde_json::Value,
    /// Prior helper outcomes needed to continue the same validation loop deterministically.
    pub prior_helper_outcomes: serde_json::Value,
    /// Absolute path to the JSON sidecar carrying the dispatched sub-agent
    /// outputs the orchestrator just collected. Empty string on the first
    /// invocation; non-empty on the re-invocation that follows a
    /// `waiting_for_handoffs` envelope (triage mode).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prior_handoff_outputs_path: String,
}

/// One row of the validator helper's `state_updates.gaps[*]` payload. The
/// `job::steps::plan` validation loop decodes the SemanticFailure
/// `state_updates.gaps` into `Vec<ValidationGap>` to surface remaining
/// plan-vs-output divergences in fix-loop iterations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationGap {
    /// Plan goal or acceptance item that is not yet satisfied.
    pub goal: String,
    /// Description of the missing evidence preventing the goal from being satisfied.
    pub missing_evidence: String,
}

// ----- pr-finalize ---------------------------------------------------------

/// Merge intent passed to [`invoke_pr_finalize`].
///
/// Mirrors the CLI surface of the `pr-finalize` skill (`--merge`,
/// `--merge-admin`); kept as an enum (not a `bool`) so the wire shape is
/// extensible without breaking callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PrFinalizeMergeMode {
    /// Do not merge after finalization.
    None,
    /// Merge with `gh pr merge --merge` after finalization.
    Merge,
    /// Merge with `gh pr merge --merge --admin` after finalization.
    MergeAdmin,
}

/// PR lifecycle state reported by [`invoke_pr_finalize`].
///
/// Mirrors the `state_updates.pr_state` enum in the pr-finalize output schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
#[non_exhaustive]
pub enum PrState {
    /// PR is open and not merged.
    Open,
    /// PR has been merged.
    Merged,
    /// PR was closed without merging.
    Closed,
    /// PR state could not be determined from the helper output.
    Unknown,
}

/// Input envelope for [`invoke_pr_finalize`].
///
/// The pr-finalize skill is currently CLI-shaped; the wrapper exposes the
/// minimal subset the orchestrator needs to identify the PR plus an explicit
/// merge intent.
#[derive(Debug, Clone, Serialize)]
pub struct PrFinalizeInput {
    /// PR owner (e.g. `parloa`).
    pub owner: String,
    /// PR repository name (e.g. `plan-executor`).
    pub repo: String,
    /// PR number.
    pub pr: u32,
    /// Whether (and how) to attempt the merge after finalization.
    pub merge_mode: PrFinalizeMergeMode,
}

/// Typed output for [`invoke_pr_finalize`].
///
/// Mirrors the `state_updates` shape in the pr-finalize output schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PrFinalizeOutput {
    /// Lifecycle state of the PR after finalization.
    pub pr_state: PrState,
    /// 40-character lower-hex SHA of the merge commit, when the PR was merged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_sha: Option<String>,
    /// Number of Bugbot comments the helper resolved during finalization.
    pub bugbot_comments_addressed: u32,
}

/// Invokes `plan-executor:pr-finalize` and returns its typed `state_updates`
/// payload.
///
/// # Errors
///
/// Same as [`invoke_review_team`] but for the pr-finalize helper.
pub fn invoke_pr_finalize(
    input: PrFinalizeInput,
    ctx: &StepContext,
) -> Result<PrFinalizeOutput, HelperError> {
    let json = serialize_wrapper_input(&input)?;
    // pr-finalize delegates to `pr-monitor.sh`, a poll loop with a
    // hardcoded 30-minute upper bound (`MAX_POLL_ITERATIONS=120` × 15s).
    // The default 10-minute helper timeout kills the helper before the
    // script can finish under any non-trivial CI / Bugbot wait. Give
    // pr-finalize 35 minutes so the script's own deadline fires first.
    let options = HelperInvocation {
        timeout: Some(Duration::from_secs(2100)),
    };
    let raw = invoke_helper_with(HelperSkill::PrFinalize, json, ctx, &options)?;
    decode_state_updates(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_simple_envelope_when_no_prose() {
        let stdout = r#"{"status":"success","next_step":"done","notes":"ok"}"#;
        let env = extract_json_envelope(stdout).expect("envelope");
        assert!(serde_json::from_str::<serde_json::Value>(env).is_ok());
        assert!(env.contains("\"status\""));
    }

    #[test]
    fn skips_descriptive_object_literal_before_real_envelope() {
        // Reproduces the run-reviewer-team-non-interactive failure where Claude
        // printed a JS-object-literal description of reviewer_set[0] before
        // the actual JSON envelope. The descriptor parses neither as JSON nor
        // contains a `status` key; the second balanced block is the real
        // envelope and must win.
        let stdout = r#"
{ index: 1, name: claude, handoff_type: claude, required: true,
  skill: rust-services:production-code-recipe + rust-services:test-code-recipe }

{"status":"waiting_for_handoffs","next_step":"dispatch","notes":"ok",
 "state_updates":{"reviewer_set":[]}}
"#;
        let env = extract_json_envelope(stdout).expect("envelope");
        let parsed: serde_json::Value = serde_json::from_str(env).expect("parse");
        assert_eq!(parsed.get("status").and_then(|v| v.as_str()), Some("waiting_for_handoffs"));
    }

    #[test]
    fn falls_back_to_first_block_when_no_status_field_anywhere() {
        // Pre-existing callers may parse blocks that schema-validate without
        // a top-level `status` (defensive); we still hand back the first
        // balanced block so the schema validator emits its own precise
        // error rather than a confusing "no envelope found".
        let stdout = r#"{"foo": 1}"#;
        let env = extract_json_envelope(stdout).expect("envelope");
        assert_eq!(env, r#"{"foo": 1}"#);
    }

    #[test]
    fn returns_none_when_no_balanced_object() {
        assert!(extract_json_envelope("just prose, no braces").is_none());
        assert!(extract_json_envelope("{ unbalanced").is_none());
    }

    #[test]
    fn ignores_braces_inside_string_literals() {
        // The `}` inside the string value must not close the outer object.
        let stdout = r#"{"status":"success","notes":"closing brace } here"}"#;
        let env = extract_json_envelope(stdout).expect("envelope");
        assert_eq!(env, stdout);
    }

    #[test]
    fn balanced_object_blocks_yields_each_top_level_object() {
        let stdout = "{a} {b} text {c}";
        let blocks: Vec<&str> = balanced_object_blocks(stdout).collect();
        assert_eq!(blocks, vec!["{a}", "{b}", "{c}"]);
    }
}
