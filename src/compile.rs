//! Fix-loop integration with the `plan-executor:compile-plan` skill.
//!
//! `append_fix_waves` takes an existing compiled manifest and a slice of
//! reviewer findings, invokes the compile-plan skill in APPEND mode, and
//! returns the path to the updated manifest. The skill subprocess is the
//! authority for fix-wave layout; this module owns: locating the manifest,
//! synthesizing the meta.json sidecar from the manifest's `plan` block,
//! writing findings.json, validating the post-append manifest, and
//! enforcing structural invariants (original waves preserved, fix-wave
//! IDs >= 100, fix-wave depends_on references existing waves).
//!
//! # Trust boundary
//!
//! `findings.json` is consumed by an LLM-driven skill (`plan-executor:compile-plan`).
//! Reviewer-supplied finding fields are sanitized in `sanitize_findings` before
//! the file is written: oversized strings are truncated, ASCII control chars
//! are stripped, and the array is capped. This is defense-in-depth against
//! prompt injection (OWASP LLM01) — an attacker who can influence finding
//! text MUST NOT be able to redirect the skill's behavior via embedded
//! instructions or by exhausting context.
//!
//! # Subprocess hardening
//!
//! `ClaudeInvoker` further restricts the spawned `claude` process: path args
//! are validated UTF-8/whitespace/leading-dash free, the slash-command prompt
//! is whitelist-only via `--allowed-tools`, sensitive credential env vars
//! (AWS, GH/GitHub, OpenAI, Google) are scrubbed from the child env, and the
//! wait is bounded by a timeout (default 600s, override via
//! `PLAN_EXECUTOR_COMPILE_TIMEOUT_SECS`).

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use thiserror::Error;
use wait_timeout::ChildExt;

use crate::finding::Finding;
use crate::schema::{validate_manifest, ValidationError};

/// Maximum bytes of any single subprocess stream embedded in error messages.
const ERROR_TRUNCATE_BYTES: usize = 2048;

/// Maximum size of an on-disk manifest the fix-loop will read into memory.
const MANIFEST_READ_CAP_BYTES: u64 = 16 * 1024 * 1024;

/// Maximum bytes captured from each subprocess stream after a timeout/exit.
const SUBPROCESS_STREAM_CAP_BYTES: usize = 16 * 1024 * 1024;

/// Maximum bytes per free-form reviewer field (description / suggested_fix / files entries).
const FINDING_FREEFORM_FIELD_CAP_BYTES: usize = 4 * 1024;

/// Maximum bytes per identifier-like reviewer field (id / category).
const FINDING_IDENT_FIELD_CAP_BYTES: usize = 256;

/// Maximum number of findings accepted in a single APPEND call.
const FINDINGS_MAX_ENTRIES: usize = 200;

/// Marker appended when a string was truncated by `sanitize_findings`.
const FINDING_TRUNCATION_MARKER: &str = "\n[...truncated for safety]";

/// Default subprocess timeout in seconds when the env override is absent or unparseable.
const DEFAULT_COMPILE_TIMEOUT_SECS: u64 = 600;

/// Env var consulted to override `DEFAULT_COMPILE_TIMEOUT_SECS`.
const COMPILE_TIMEOUT_ENV: &str = "PLAN_EXECUTOR_COMPILE_TIMEOUT_SECS";

/// Errors surfaced by `append_fix_waves`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AppendError {
    #[error("manifest read failed: {0}")]
    ManifestRead(std::io::Error),
    #[error("manifest JSON parse failed: {0}")]
    ManifestParse(serde_json::Error),
    #[error("manifest schema validation failed: {0}")]
    ManifestSchema(String),
    #[error("manifest is missing required field: {0}")]
    ManifestField(String),
    #[error("findings serialize failed: {0}")]
    FindingsSerialize(serde_json::Error),
    #[error("findings write failed: {0}")]
    FindingsWrite(std::io::Error),
    #[error("meta.json synthesize failed: {0}")]
    MetaSynthesize(String),
    #[error("meta.json write failed: {0}")]
    MetaWrite(std::io::Error),
    #[error("compile-plan skill invocation failed: {0}")]
    Invoke(String),
    #[error("compile-plan skill exited non-zero: {0}")]
    InvokeExit(String),
    #[error("post-append manifest re-read failed: {0}")]
    PostReread(std::io::Error),
    #[error("post-append manifest invalid: {0}")]
    PostInvalid(String),
    #[error("post-append semantic check failed: {0}")]
    PostSemantic(String),
    #[error("post-append invariant violated: {0}")]
    InvariantViolation(String),
    #[error("schema materialize failed: {0}")]
    SchemaMaterialize(std::io::Error),
    #[error("invalid path argument: {0}")]
    InvalidPathArg(String),
    #[error("too many findings: {n} (cap {cap})", cap = FINDINGS_MAX_ENTRIES)]
    TooManyFindings { n: usize },
    #[error("file too large: {path} is {size} bytes (cap {cap})")]
    FileTooLarge { path: String, size: u64, cap: u64 },
    #[error("compile-plan skill timed out after {0}s")]
    Timeout(u64),
}

/// Trait used to invoke the compile-plan skill. Production callers use the
/// default `ClaudeInvoker`; tests inject `FakeInvoker` writing a canned
/// post-append manifest.
pub trait CompileInvoker {
    /// Run the compile-plan skill with the given arguments.
    ///
    /// `args` is `[plan_path, schema_path, output_dir, meta_json_path, findings_json_path]`.
    ///
    /// # Errors
    ///
    /// Returns `Err(message)` on spawn failure, non-zero exit, timeout, or
    /// missing `COMPILED:` line in stdout.
    fn invoke(&self, args: &[&Path]) -> Result<(), String>;
}

/// Production implementation: spawns `claude -p "/plan-executor:compile-plan ..."`.
///
/// # Subprocess hardening
///
/// - Path args are validated (UTF-8, no whitespace, no leading dash).
/// - `--allowed-tools "Read,Write,Edit"` whitelists tool surface for the skill.
/// - Sensitive credential env vars are scrubbed from the child process:
///   `AWS_*`, `GITHUB_TOKEN`, `GH_TOKEN`, `OPENAI_*`, `GOOGLE_*`.
/// - Wait is bounded by `PLAN_EXECUTOR_COMPILE_TIMEOUT_SECS` (default 600s).
/// - stdout/stderr are size-capped (16 MiB each) and truncated in error
///   messages.
pub struct ClaudeInvoker;

impl CompileInvoker for ClaudeInvoker {
    fn invoke(&self, args: &[&Path]) -> Result<(), String> {
        if args.len() != 5 {
            return Err(format!("expected 5 args, got {}", args.len()));
        }
        let validated = validate_path_args(args).map_err(|e| e.to_string())?;
        let prompt = format!(
            "/plan-executor:compile-plan {} {} {} {} {}",
            validated[0], validated[1], validated[2], validated[3], validated[4],
        );

        let timeout_secs = timeout_seconds_from_env();
        let mut child = scrubbed_env_command()
            .arg("-p")
            .arg(&prompt)
            .arg("--allowed-tools")
            .arg("Read,Write,Edit")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn failed: {e}"))?;

        let status = match child.wait_timeout(Duration::from_secs(timeout_secs)) {
            Ok(Some(status)) => status,
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AppendError::Timeout(timeout_secs).to_string());
            }
            Err(e) => return Err(format!("wait failed: {e}")),
        };

        let stdout = drain_stream(child.stdout.take());
        let stderr = drain_stream(child.stderr.take());

        if !status.success() {
            return Err(AppendError::InvokeExit(format!(
                "claude exited {:?}; stderr={}",
                status.code(),
                truncate_for_error(&stderr, ERROR_TRUNCATE_BYTES)
            ))
            .to_string());
        }
        if !stdout.lines().any(|l| l.starts_with("COMPILED:")) {
            return Err(format!(
                "compile-plan did not emit COMPILED: line; stdout was: {}",
                truncate_for_error(stdout.trim(), ERROR_TRUNCATE_BYTES)
            ));
        }
        Ok(())
    }
}

/// Public entry point. Production callers pass `&ClaudeInvoker`.
///
/// # Errors
///
/// Returns any [`AppendError`] surfaced during read, sanitize, invoke, or
/// post-append validation steps.
pub fn append_fix_waves(
    manifest_path: &Path,
    findings: &[Finding],
) -> Result<PathBuf, AppendError> {
    append_fix_waves_with_invoker(&ClaudeInvoker, manifest_path, findings)
}

/// Test-injection variant. Splits invoker out so unit tests can supply a fake.
///
/// # Errors
///
/// Returns any [`AppendError`] surfaced during read, sanitize, invoke, or
/// post-append validation steps.
pub fn append_fix_waves_with_invoker(
    invoker: &dyn CompileInvoker,
    manifest_path: &Path,
    findings: &[Finding],
) -> Result<PathBuf, AppendError> {
    // Sanitize early — any over-cap or malformed input is rejected before
    // we touch the filesystem.
    let safe_findings = sanitize_findings(findings)?;

    // Step 1 — read existing manifest (size-capped).
    let manifest_bytes = read_capped(manifest_path, MANIFEST_READ_CAP_BYTES)?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).map_err(AppendError::ManifestParse)?;
    if let Err(errors) = validate_manifest(&manifest) {
        return Err(AppendError::ManifestSchema(errors_summary(&errors)));
    }

    // Step 2 — capture pre-append snapshot of waves + tasks for invariant check.
    let pre_waves = manifest
        .get("waves")
        .and_then(|v| v.as_array())
        .cloned()
        .ok_or_else(|| AppendError::ManifestField("waves".into()))?;
    let pre_tasks: serde_json::Map<String, serde_json::Value> = manifest
        .get("tasks")
        .and_then(|v| v.as_object())
        .cloned()
        .ok_or_else(|| AppendError::ManifestField("tasks".into()))?;
    let pre_max_fix_wave_id = max_fix_wave_id(&pre_waves);
    let pre_last_impl_wave_id = last_implementation_wave_id(&pre_waves)
        .ok_or_else(|| AppendError::ManifestField("no implementation wave found".into()))?;

    // Step 3 — locate plan markdown ($1) from manifest.
    let plan_path_str = manifest
        .pointer("/plan/path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppendError::ManifestField("plan.path".into()))?;
    let plan_path = PathBuf::from(plan_path_str);

    // Step 4 — schema path. Materialized from the embedded copy at runtime so
    // released binaries do not depend on `CARGO_MANIFEST_DIR`.
    let schema_path = embedded_schema_path()?.clone();

    // Step 5 — output-dir is the manifest's parent.
    let manifest_dir = manifest_path
        .parent()
        .ok_or_else(|| AppendError::ManifestField("manifest_path has no parent".into()))?
        .to_path_buf();

    // Step 6 — synthesize meta.json from manifest.plan block.
    let meta_path = manifest_dir.join(".append-meta.json");
    write_synthetic_meta(&meta_path, &manifest)?;

    // Step 7 — write findings.json (sanitized).
    let findings_path = manifest_dir.join("findings.json");
    let findings_doc = serde_json::json!({ "findings": safe_findings });
    let findings_bytes =
        serde_json::to_vec_pretty(&findings_doc).map_err(AppendError::FindingsSerialize)?;
    std::fs::write(&findings_path, &findings_bytes).map_err(AppendError::FindingsWrite)?;

    // Step 8 — invoke compile-plan APPEND mode via injected invoker.
    let args: [&Path; 5] = [
        &plan_path,
        &schema_path,
        &manifest_dir,
        &meta_path,
        &findings_path,
    ];
    invoker.invoke(&args).map_err(AppendError::Invoke)?;

    // Step 9 — re-read updated manifest (size-capped). Map IO failures to
    // `PostReread` so callers can distinguish step-1 vs step-9 read failures.
    let updated_bytes = read_capped(manifest_path, MANIFEST_READ_CAP_BYTES).map_err(|e| match e {
        AppendError::ManifestRead(io) => AppendError::PostReread(io),
        other => other,
    })?;
    let updated: serde_json::Value =
        serde_json::from_slice(&updated_bytes).map_err(AppendError::ManifestParse)?;
    if let Err(errors) = validate_manifest(&updated) {
        return Err(AppendError::PostInvalid(errors_summary(&errors)));
    }
    if let Err(sem_errors) = crate::validate::semantic_check(&updated, &manifest_dir) {
        return Err(AppendError::PostSemantic(semantic_errors_summary(&sem_errors)));
    }

    // Step 10 — invariants.
    enforce_post_append_invariants(
        &updated,
        &pre_waves,
        &pre_tasks,
        pre_last_impl_wave_id,
        pre_max_fix_wave_id,
    )?;

    Ok(manifest_path.to_path_buf())
}

/// Returns the maximum existing fix-wave id (id >= 100), or `None` if none.
fn max_fix_wave_id(waves: &[serde_json::Value]) -> Option<u64> {
    waves
        .iter()
        .filter_map(|w| w.get("id")?.as_u64())
        .filter(|id| *id >= 100)
        .max()
}

/// Returns the maximum impl-wave id (id < 100). `None` if no impl waves.
fn last_implementation_wave_id(waves: &[serde_json::Value]) -> Option<u64> {
    waves
        .iter()
        .filter_map(|w| w.get("id")?.as_u64())
        .filter(|id| *id < 100)
        .max()
}

/// Synthesizes a meta.json sidecar from `manifest.plan` and writes it to `path`.
fn write_synthetic_meta(path: &Path, manifest: &serde_json::Value) -> Result<(), AppendError> {
    let plan = manifest
        .get("plan")
        .ok_or_else(|| AppendError::MetaSynthesize("manifest.plan missing".into()))?;

    let plan_path = plan
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppendError::MetaSynthesize("plan.path missing".into()))?;
    let goal = plan
        .get("goal")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppendError::MetaSynthesize("plan.goal missing".into()))?;
    let plan_type = plan
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppendError::MetaSynthesize("plan.type missing".into()))?;

    let meta = serde_json::json!({
        "plan_path": plan_path,
        "goal": goal,
        "type": plan_type,
        "jira": plan.get("jira").and_then(|v| v.as_str()).unwrap_or(""),
        "target_repo": plan.get("target_repo").cloned().unwrap_or(serde_json::Value::Null),
        "target_branch": plan.get("target_branch").cloned().unwrap_or(serde_json::Value::Null),
        "flags": plan.get("flags").cloned().unwrap_or_else(|| serde_json::json!({
            "merge": false, "merge_admin": false, "skip_pr": false,
            "skip_code_review": false, "no_worktree": false, "draft_pr": false
        })),
    });

    let bytes =
        serde_json::to_vec_pretty(&meta).map_err(|e| AppendError::MetaSynthesize(e.to_string()))?;
    std::fs::write(path, &bytes).map_err(AppendError::MetaWrite)?;
    Ok(())
}

/// Materializes the embedded `tasks.schema.json` to a stable temp file once
/// per process and returns the path.
///
/// Replaces a previous design that baked `CARGO_MANIFEST_DIR` into the binary;
/// the build-host source path does not exist on the user's machine after
/// `cargo install` or any release distribution.
fn embedded_schema_path() -> Result<&'static PathBuf, AppendError> {
    static MATERIALIZED: OnceLock<PathBuf> = OnceLock::new();
    if let Some(p) = MATERIALIZED.get() {
        return Ok(p);
    }
    let target = std::env::temp_dir().join("plan-executor-tasks.schema.json");
    let payload = crate::schema::embedded_schema_json();
    if let Ok(existing) = std::fs::read(&target) {
        if existing == payload.as_bytes() {
            return Ok(MATERIALIZED.get_or_init(|| target));
        }
    }
    std::fs::write(&target, payload).map_err(AppendError::SchemaMaterialize)?;
    Ok(MATERIALIZED.get_or_init(|| target))
}

/// Validates path arguments before they are interpolated into a slash-command
/// prompt. Each path must be UTF-8 and contain no whitespace, no control
/// chars, and no leading `-` (which could re-bind to a flag inside the skill).
fn validate_path_args<'a>(args: &'a [&'a Path]) -> Result<Vec<&'a str>, AppendError> {
    args.iter()
        .enumerate()
        .map(|(idx, p)| {
            let s = p.to_str().ok_or_else(|| {
                AppendError::InvalidPathArg(format!(
                    "arg {idx}: path is not valid UTF-8 ({})",
                    p.display()
                ))
            })?;
            if s.is_empty() {
                return Err(AppendError::InvalidPathArg(format!("arg {idx}: empty path")));
            }
            if s.starts_with('-') {
                return Err(AppendError::InvalidPathArg(format!(
                    "arg {idx}: path may not start with `-` ({s})"
                )));
            }
            if let Some(c) = s.chars().find(|c| c.is_whitespace() || c.is_control()) {
                return Err(AppendError::InvalidPathArg(format!(
                    "arg {idx}: path contains forbidden character {c:?} ({s})"
                )));
            }
            Ok(s)
        })
        .collect()
}

/// Truncates `s` to at most `max_bytes` (cut on a char boundary) and appends
/// a `... (truncated, N more bytes)` suffix when truncation occurred.
fn truncate_for_error(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let extra = s.len() - cut;
    format!("{}... (truncated, {} more bytes)", &s[..cut], extra)
}

/// Sanitizes reviewer-supplied findings before they are written to disk and
/// consumed by the LLM-driven compile-plan skill.
///
/// - Caps free-form fields (`description`, `suggested_fix`, `files[i]`) at 4 KiB.
/// - Caps identifier fields (`id`, `category`) at 256 B.
/// - Strips ASCII control chars except `\n`.
/// - Caps the array at `FINDINGS_MAX_ENTRIES` entries.
fn sanitize_findings(findings: &[Finding]) -> Result<Vec<Finding>, AppendError> {
    if findings.len() > FINDINGS_MAX_ENTRIES {
        return Err(AppendError::TooManyFindings { n: findings.len() });
    }
    Ok(findings
        .iter()
        .map(|f| Finding {
            id: clamp_field(&f.id, FINDING_IDENT_FIELD_CAP_BYTES, false),
            severity: f.severity,
            category: clamp_field(&f.category, FINDING_IDENT_FIELD_CAP_BYTES, false),
            description: clamp_field(&f.description, FINDING_FREEFORM_FIELD_CAP_BYTES, true),
            files: f
                .files
                .iter()
                .map(|p| clamp_field(p, FINDING_FREEFORM_FIELD_CAP_BYTES, false))
                .collect(),
            suggested_fix: f
                .suggested_fix
                .as_deref()
                .map(|s| clamp_field(s, FINDING_FREEFORM_FIELD_CAP_BYTES, true)),
        })
        .collect())
}

/// Strips ASCII control chars (preserving `\n` only when `keep_newline`) and
/// truncates to `cap` bytes (cut on a char boundary), appending the safety
/// marker on truncation.
fn clamp_field(input: &str, cap: usize, keep_newline: bool) -> String {
    let cleaned: String = input
        .chars()
        .filter(|c| !c.is_control() || (keep_newline && *c == '\n'))
        .collect();
    if cleaned.len() <= cap {
        return cleaned;
    }
    let mut cut = cap;
    while cut > 0 && !cleaned.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + FINDING_TRUNCATION_MARKER.len());
    out.push_str(&cleaned[..cut]);
    out.push_str(FINDING_TRUNCATION_MARKER);
    out
}

/// Reads `path` after asserting its size is at most `max_bytes`. Avoids
/// loading attacker-influenced files into memory unbounded.
fn read_capped(path: &Path, max_bytes: u64) -> Result<Vec<u8>, AppendError> {
    let metadata = std::fs::metadata(path).map_err(AppendError::ManifestRead)?;
    if metadata.len() > max_bytes {
        return Err(AppendError::FileTooLarge {
            path: path.display().to_string(),
            size: metadata.len(),
            cap: max_bytes,
        });
    }
    std::fs::read(path).map_err(AppendError::ManifestRead)
}

/// Drains a captured subprocess stream up to `SUBPROCESS_STREAM_CAP_BYTES`.
fn drain_stream<R: Read>(reader: Option<R>) -> String {
    let Some(mut r) = reader else { return String::new() };
    let mut buf = Vec::with_capacity(8 * 1024);
    let _ = (&mut r).take(SUBPROCESS_STREAM_CAP_BYTES as u64).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Returns the configured subprocess timeout in seconds, falling back to
/// `DEFAULT_COMPILE_TIMEOUT_SECS` when the env var is unset or unparseable.
fn timeout_seconds_from_env() -> u64 {
    std::env::var(COMPILE_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(DEFAULT_COMPILE_TIMEOUT_SECS)
}

/// Returns a `Command::new("claude")` with sensitive credential env vars
/// removed. Inherits everything else (the child still needs `PATH`, `HOME`,
/// claude config dirs, and the user's chosen authentication env).
fn scrubbed_env_command() -> Command {
    let mut cmd = Command::new("claude");
    let to_remove: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| {
            k.starts_with("AWS_")
                || k == "GITHUB_TOKEN"
                || k == "GH_TOKEN"
                || k.starts_with("OPENAI_")
                || k.starts_with("GOOGLE_")
        })
        .collect();
    for k in to_remove {
        cmd.env_remove(k);
    }
    cmd
}

/// Enforces the post-append invariants the plan's APPEND-mode rules require.
fn enforce_post_append_invariants(
    updated: &serde_json::Value,
    pre_waves: &[serde_json::Value],
    pre_tasks: &serde_json::Map<String, serde_json::Value>,
    pre_last_impl_wave_id: u64,
    pre_max_fix_wave_id: Option<u64>,
) -> Result<(), AppendError> {
    let post_waves = updated
        .get("waves")
        .and_then(|v| v.as_array())
        .ok_or_else(|| AppendError::InvariantViolation("post manifest missing waves".into()))?;
    let post_tasks_obj = updated
        .get("tasks")
        .and_then(|v| v.as_object())
        .ok_or_else(|| AppendError::InvariantViolation("post manifest missing tasks".into()))?;

    // Invariant 1: every original wave preserved verbatim.
    for pre in pre_waves {
        let pre_id = pre.get("id").and_then(|v| v.as_u64());
        let matched = post_waves
            .iter()
            .find(|w| w.get("id").and_then(|v| v.as_u64()) == pre_id);
        match matched {
            Some(post) if post == pre => {}
            Some(_) => {
                return Err(AppendError::InvariantViolation(format!(
                    "wave id {pre_id:?} was modified by APPEND"
                )));
            }
            None => {
                return Err(AppendError::InvariantViolation(format!(
                    "wave id {pre_id:?} was dropped by APPEND"
                )));
            }
        }
    }

    // Invariant 2: every original task preserved verbatim (full value equality).
    for (k, pre_val) in pre_tasks {
        match post_tasks_obj.get(k) {
            Some(post_val) if post_val == pre_val => {}
            Some(_) => {
                return Err(AppendError::InvariantViolation(format!(
                    "original task `{k}` was modified by APPEND"
                )));
            }
            None => {
                return Err(AppendError::InvariantViolation(format!(
                    "original task `{k}` was dropped by APPEND"
                )));
            }
        }
    }

    // Build the post-append wave-id set up front for invariant 5.
    let post_wave_ids: HashSet<u64> = post_waves
        .iter()
        .filter_map(|w| w.get("id").and_then(serde_json::Value::as_u64))
        .collect();

    // Invariant 3: there is at least one new wave with id >= 100 and kind == "fix".
    let new_fix_waves: Vec<&serde_json::Value> = post_waves
        .iter()
        .filter(|w| {
            let id = w.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let kind = w
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("implementation");
            id >= 100 && kind == "fix"
        })
        .filter(|w| {
            // exclude waves that already existed pre-append
            let id = w.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            !pre_waves
                .iter()
                .any(|p| p.get("id").and_then(|v| v.as_u64()) == Some(id))
        })
        .collect();
    if new_fix_waves.is_empty() {
        return Err(AppendError::InvariantViolation(
            "no new fix-wave appended (expected at least one wave with id>=100 and kind=fix)"
                .into(),
        ));
    }

    // Invariant 4: each new fix-wave's id is strictly greater than pre_max_fix_wave_id.
    let threshold = pre_max_fix_wave_id.unwrap_or(99);
    for w in &new_fix_waves {
        let id = w.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        if id <= threshold {
            return Err(AppendError::InvariantViolation(format!(
                "new fix-wave id {id} is not greater than prior max {threshold}"
            )));
        }
    }

    // Invariant 5: every new fix-wave's depends_on includes pre_last_impl_wave_id
    // OR an id present in the post-append wave set. (Round-2 fix-waves may
    // depend on round-1 fix-waves; non-existent ids are rejected.)
    for w in &new_fix_waves {
        let deps: Vec<u64> = w
            .get("depends_on")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(serde_json::Value::as_u64).collect())
            .unwrap_or_default();
        let ok = deps
            .iter()
            .any(|d| *d == pre_last_impl_wave_id || post_wave_ids.contains(d));
        if !ok {
            let id = w.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            return Err(AppendError::InvariantViolation(format!(
                "fix-wave {id} depends_on must include the last impl wave \
                 ({pre_last_impl_wave_id}) or a wave id present in the post-append manifest; got {deps:?}"
            )));
        }
    }

    Ok(())
}

fn errors_summary(errors: &[ValidationError]) -> String {
    errors
        .iter()
        .take(5)
        .map(|e| format!("{}: {}", e.path, e.message))
        .collect::<Vec<_>>()
        .join("; ")
}

fn semantic_errors_summary(errors: &[crate::validate::SemanticError]) -> String {
    errors
        .iter()
        .take(5)
        .map(|e| format!("{}: {}", e.category, e.message))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::Severity;
    use std::cell::RefCell;
    use std::fs;

    /// Minimal valid pre-append manifest used by tests. Two impl waves.
    fn fixture_pre_append_manifest() -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "plan": {
                "goal": "test plan for fix-loop",
                "type": "feature",
                "jira": "",
                "target_repo": null,
                "target_branch": null,
                "path": "/tmp/fixtures/plan.md",
                "status": "READY",
                "flags": {
                    "merge": false, "merge_admin": false, "skip_pr": false,
                    "skip_code_review": false, "no_worktree": false, "draft_pr": false
                }
            },
            "waves": [
                { "id": 1, "task_ids": ["1.1"], "depends_on": [], "kind": "implementation" },
                { "id": 2, "task_ids": ["2.1"], "depends_on": [1], "kind": "implementation" }
            ],
            "tasks": {
                "1.1": { "prompt_file": "tasks/1.1.md", "agent_type": "claude" },
                "2.1": { "prompt_file": "tasks/2.1.md", "agent_type": "claude" }
            }
        })
    }

    /// Fake invoker that writes a caller-supplied manifest to the output dir
    /// and (optionally) materializes prompt_file paths so semantic_check passes.
    struct FakeInvoker {
        write_manifest: serde_json::Value,
        capture_args: RefCell<Vec<PathBuf>>,
    }
    impl CompileInvoker for FakeInvoker {
        fn invoke(&self, args: &[&Path]) -> Result<(), String> {
            *self.capture_args.borrow_mut() = args.iter().map(|p| p.to_path_buf()).collect();
            let output_dir = args[2];
            let target = output_dir.join("tasks.json");
            let bytes = serde_json::to_vec_pretty(&self.write_manifest).unwrap();
            fs::write(&target, &bytes).unwrap();
            // Materialize every referenced prompt_file so semantic_check passes.
            if let Some(tasks) = self.write_manifest.get("tasks").and_then(|v| v.as_object()) {
                for (_tid, spec) in tasks {
                    if let Some(pf) = spec.get("prompt_file").and_then(|v| v.as_str()) {
                        let full = output_dir.join(pf);
                        if let Some(parent) = full.parent() {
                            let _ = fs::create_dir_all(parent);
                        }
                        let _ = fs::write(&full, "dummy");
                    }
                }
            }
            Ok(())
        }
    }

    fn write_pre_manifest(dir: &Path, manifest: &serde_json::Value) -> PathBuf {
        let path = dir.join("tasks.json");
        fs::write(&path, serde_json::to_vec_pretty(manifest).unwrap()).unwrap();
        // Materialize prompt_files so semantic_check accepts the post-manifest.
        if let Some(tasks) = manifest.get("tasks").and_then(|v| v.as_object()) {
            for (_tid, spec) in tasks {
                if let Some(pf) = spec.get("prompt_file").and_then(|v| v.as_str()) {
                    let full = dir.join(pf);
                    if let Some(parent) = full.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::write(&full, "dummy");
                }
            }
        }
        path
    }

    fn fresh_findings() -> Vec<Finding> {
        vec![Finding {
            id: "F1".into(),
            severity: Severity::Major,
            category: "error-handling".into(),
            description: "swallow error".into(),
            files: vec!["src/compile.rs".into()],
            suggested_fix: Some("propagate Err".into()),
        }]
    }

    #[test]
    fn fresh_findings_produce_fix_wave_with_id_100() {
        let tmp = tempfile::tempdir().unwrap();
        let pre = fixture_pre_append_manifest();
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        // Construct post-append manifest the fake invoker will write.
        let mut post = pre.clone();
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100,
            "task_ids": ["fix-100-1"],
            "depends_on": [2],
            "kind": "fix"
        }));
        post["tasks"]["fix-100-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-100-1.md",
            "agent_type": "claude"
        });

        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        let result = append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings())
            .expect("must succeed");
        assert_eq!(result, manifest_path);

        let reread: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        let waves = reread["waves"].as_array().unwrap();
        assert!(waves
            .iter()
            .any(|w| w["id"].as_u64() == Some(100) && w["kind"] == "fix"));

        // findings.json was written
        assert!(tmp.path().join("findings.json").exists());
        // synthetic meta.json was written
        assert!(tmp.path().join(".append-meta.json").exists());
    }

    #[test]
    fn original_waves_and_tasks_preserved_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let pre = fixture_pre_append_manifest();
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        let mut post = pre.clone();
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100, "task_ids": ["fix-100-1"], "depends_on": [2], "kind": "fix"
        }));
        post["tasks"]["fix-100-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-100-1.md", "agent_type": "claude"
        });

        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings()).unwrap();

        let reread: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        // Original waves identical
        assert_eq!(reread["waves"][0], pre["waves"][0]);
        assert_eq!(reread["waves"][1], pre["waves"][1]);
        // Original tasks identical
        assert_eq!(reread["tasks"]["1.1"], pre["tasks"]["1.1"]);
        assert_eq!(reread["tasks"]["2.1"], pre["tasks"]["2.1"]);
    }

    #[test]
    fn second_round_fix_wave_id_increments() {
        let tmp = tempfile::tempdir().unwrap();
        let mut pre = fixture_pre_append_manifest();
        // Pre-existing fix-wave from round 1
        pre["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100, "task_ids": ["fix-100-1"], "depends_on": [2], "kind": "fix"
        }));
        pre["tasks"]["fix-100-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-100-1.md", "agent_type": "claude"
        });
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        // Round 2 must use id >= 101
        let mut post = pre.clone();
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 101, "task_ids": ["fix-101-1"], "depends_on": [100], "kind": "fix"
        }));
        post["tasks"]["fix-101-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-101-1.md", "agent_type": "claude"
        });

        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings()).unwrap();
    }

    #[test]
    fn rejects_post_append_that_drops_original_wave() {
        let tmp = tempfile::tempdir().unwrap();
        let pre = fixture_pre_append_manifest();
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        // Bad: drop wave 1 entirely
        let post = serde_json::json!({
            "version": 1,
            "plan": pre["plan"],
            "waves": [
                pre["waves"][1].clone(),
                {
                    "id": 100, "task_ids": ["fix-100-1"], "depends_on": [2], "kind": "fix"
                }
            ],
            "tasks": {
                "1.1": pre["tasks"]["1.1"].clone(),
                "2.1": pre["tasks"]["2.1"].clone(),
                "fix-100-1": {
                    "prompt_file": "tasks/task-fix-100-1.md", "agent_type": "claude"
                }
            }
        });

        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        let err = append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings())
            .expect_err("dropping a pre-existing wave must fail");
        // PostSemantic may fire first (wave 2's dangling depends_on:[1])
        // before the invariant check spots the dropped wave; either is correct.
        assert!(matches!(
            err,
            AppendError::InvariantViolation(_) | AppendError::PostSemantic(_)
        ));
    }

    #[test]
    fn rejects_post_append_with_no_new_fix_wave() {
        let tmp = tempfile::tempdir().unwrap();
        let pre = fixture_pre_append_manifest();
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        // Bad: no fix-wave added
        let post = pre.clone();
        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        let err = append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings())
            .expect_err("no new fix-wave must fail");
        assert!(matches!(err, AppendError::InvariantViolation(_)));
    }

    #[test]
    fn rejects_post_append_with_misnumbered_fix_wave() {
        let tmp = tempfile::tempdir().unwrap();
        let mut pre = fixture_pre_append_manifest();
        pre["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100, "task_ids": ["fix-100-1"], "depends_on": [2], "kind": "fix"
        }));
        pre["tasks"]["fix-100-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-100-1.md", "agent_type": "claude"
        });
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        // Bad: round-2 fix-wave reuses id 100 (must be >=101)
        let mut post = pre.clone();
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100, "task_ids": ["fix-100-1b"], "depends_on": [2], "kind": "fix"
        }));
        // skip adding fix-100-1b to tasks → schema will fail; actually, schema validation
        // catches the duplicate wave first. Use 99 instead to trigger our invariant only.
        post["waves"].as_array_mut().unwrap().pop();
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 99, "task_ids": ["fix-99-1"], "depends_on": [2], "kind": "fix"
        }));
        post["tasks"]["fix-99-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-99-1.md", "agent_type": "claude"
        });

        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        let err = append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings())
            .expect_err("fix-wave id <= prior max must fail");
        assert!(matches!(err, AppendError::InvariantViolation(_)));
    }

    #[test]
    fn rejects_post_append_with_bad_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        let pre = fixture_pre_append_manifest();
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        // Bad: fix-wave depends_on a wave id that does not exist in the post
        // manifest. Per F4 the rule changed from ">=100 OR pre-last-impl" to
        // "exists in post-wave-set OR pre-last-impl"; a phantom id satisfies
        // neither.
        let mut post = pre.clone();
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100, "task_ids": ["fix-100-1"], "depends_on": [42], "kind": "fix"
        }));
        post["tasks"]["fix-100-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-100-1.md", "agent_type": "claude"
        });

        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        let err = append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings())
            .expect_err("fix-wave with bad depends_on must fail");
        assert!(matches!(
            err,
            AppendError::InvariantViolation(_) | AppendError::PostSemantic(_)
        ));
    }

    #[test]
    fn synthesizes_meta_json_from_manifest_plan_block() {
        let tmp = tempfile::tempdir().unwrap();
        let pre = fixture_pre_append_manifest();
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        let mut post = pre.clone();
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100, "task_ids": ["fix-100-1"], "depends_on": [2], "kind": "fix"
        }));
        post["tasks"]["fix-100-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-100-1.md", "agent_type": "claude"
        });

        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings()).unwrap();

        let meta_bytes = fs::read(tmp.path().join(".append-meta.json")).unwrap();
        let meta: serde_json::Value = serde_json::from_slice(&meta_bytes).unwrap();
        assert_eq!(meta["plan_path"], "/tmp/fixtures/plan.md");
        assert_eq!(meta["goal"], "test plan for fix-loop");
        assert_eq!(meta["type"], "feature");
        assert_eq!(meta["jira"], "");
        assert_eq!(meta["flags"]["merge"], false);
    }

    // ---- F1: embedded_schema_path materialization ----

    #[test]
    fn embedded_schema_path_materializes_to_temp_file_with_id() {
        let p1 = embedded_schema_path().unwrap().clone();
        let p2 = embedded_schema_path().unwrap().clone();
        assert_eq!(p1, p2);
        assert!(p1.is_file(), "schema must exist on disk");
        let body = fs::read_to_string(&p1).unwrap();
        assert!(body.contains("\"$id\""), "materialized schema must contain $id");
    }

    // ---- F2: ClaudeInvoker path-arg validation ----

    #[test]
    fn validate_path_args_rejects_path_with_space() {
        let p = PathBuf::from("/tmp/has space/file");
        let plain = PathBuf::from("/tmp/ok.md");
        let args: [&Path; 5] = [&plain, &p, &plain, &plain, &plain];
        let err = validate_path_args(&args).expect_err("space must reject");
        assert!(matches!(err, AppendError::InvalidPathArg(_)));
    }

    #[test]
    fn validate_path_args_rejects_leading_dash() {
        let p = PathBuf::from("-rf");
        let plain = PathBuf::from("/tmp/ok.md");
        let args: [&Path; 5] = [&plain, &plain, &plain, &plain, &p];
        let err = validate_path_args(&args).expect_err("leading dash must reject");
        assert!(matches!(err, AppendError::InvalidPathArg(_)));
    }

    // ---- F3: original tasks preserved verbatim (full equality) ----

    #[test]
    fn rejects_post_append_that_modifies_original_task_prompt_file() {
        let tmp = tempfile::tempdir().unwrap();
        let pre = fixture_pre_append_manifest();
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        // Bad: rewrite original task `1.1`'s prompt_file.
        let mut post = pre.clone();
        post["tasks"]["1.1"] = serde_json::json!({
            "prompt_file": "tasks/hijacked.md",
            "agent_type": "claude"
        });
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100, "task_ids": ["fix-100-1"], "depends_on": [2], "kind": "fix"
        }));
        post["tasks"]["fix-100-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-100-1.md", "agent_type": "claude"
        });

        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        let err = append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings())
            .expect_err("modifying an original task must fail");
        assert!(matches!(err, AppendError::InvariantViolation(_)));
    }

    // ---- F4: dep-target existence + duplicate wave id ----

    #[test]
    fn rejects_post_append_with_dep_on_nonexistent_wave() {
        let tmp = tempfile::tempdir().unwrap();
        let pre = fixture_pre_append_manifest();
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        let mut post = pre.clone();
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100, "task_ids": ["fix-100-1"], "depends_on": [999], "kind": "fix"
        }));
        post["tasks"]["fix-100-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-100-1.md", "agent_type": "claude"
        });

        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        let err = append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings())
            .expect_err("dep on non-existent wave must fail");
        assert!(matches!(
            err,
            AppendError::InvariantViolation(_) | AppendError::PostSemantic(_)
        ));
    }

    #[test]
    fn rejects_post_append_with_duplicate_wave_id() {
        let tmp = tempfile::tempdir().unwrap();
        let pre = fixture_pre_append_manifest();
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        // Two waves both with id 100.
        let mut post = pre.clone();
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100, "task_ids": ["fix-100-1"], "depends_on": [2], "kind": "fix"
        }));
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100, "task_ids": ["fix-100-2"], "depends_on": [2], "kind": "fix"
        }));
        post["tasks"]["fix-100-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-100-1.md", "agent_type": "claude"
        });
        post["tasks"]["fix-100-2"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-100-2.md", "agent_type": "claude"
        });

        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        let err = append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings())
            .expect_err("duplicate wave id must fail");
        assert!(matches!(err, AppendError::PostSemantic(_)));
    }

    // ---- F6: truncate_for_error helper ----

    #[test]
    fn truncate_for_error_appends_suffix_with_remaining_byte_count() {
        let input: String = "a".repeat(5000);
        let out = truncate_for_error(&input, 2048);
        assert!(out.starts_with(&"a".repeat(2048)));
        assert!(
            out.contains("(truncated, 2952 more bytes)"),
            "missing suffix; got: {}",
            &out[out.len().saturating_sub(80)..]
        );
    }

    // ---- F7: sanitize_findings ----

    #[test]
    fn sanitize_findings_truncates_overlong_description() {
        let big = "x".repeat(10_000);
        let f = vec![Finding {
            id: "F1".into(),
            severity: Severity::Major,
            category: "c".into(),
            description: big.clone(),
            files: vec![],
            suggested_fix: None,
        }];
        let out = sanitize_findings(&f).unwrap();
        assert!(out[0].description.len() < big.len());
        assert!(out[0].description.ends_with(FINDING_TRUNCATION_MARKER));
    }

    #[test]
    fn sanitize_findings_strips_control_chars_in_description() {
        let f = vec![Finding {
            id: "F1".into(),
            severity: Severity::Minor,
            category: "c".into(),
            description: "ok\u{0007}\u{001b}[31mbad".into(),
            files: vec![],
            suggested_fix: None,
        }];
        let out = sanitize_findings(&f).unwrap();
        assert_eq!(out[0].description, "ok[31mbad");
    }

    #[test]
    fn sanitize_findings_rejects_too_many_entries() {
        let one = Finding {
            id: "F".into(),
            severity: Severity::Nit,
            category: "c".into(),
            description: "d".into(),
            files: vec![],
            suggested_fix: None,
        };
        let many: Vec<Finding> = (0..(FINDINGS_MAX_ENTRIES + 1)).map(|_| one.clone()).collect();
        let err = sanitize_findings(&many).expect_err("over-cap must reject");
        assert!(matches!(err, AppendError::TooManyFindings { .. }));
    }

    // ---- F9: read_capped ----

    #[test]
    fn read_capped_rejects_oversize_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.bin");
        let cap: u64 = 16 * 1024 * 1024;
        // Write cap + 1 MiB to comfortably exceed.
        let buf = vec![0u8; (cap as usize) + 1024 * 1024];
        fs::write(&path, &buf).unwrap();
        let err = read_capped(&path, cap).expect_err("oversize must reject");
        assert!(matches!(err, AppendError::FileTooLarge { .. }));
    }

    // ---- F10: timeout_seconds_from_env ----

    #[test]
    fn timeout_seconds_from_env_defaults_when_unset() {
        // SAFETY: tests in this module are not parallel-sensitive to this env
        // var; we remove it for the duration of the assertion. Restoration is
        // best-effort.
        let prior = std::env::var(COMPILE_TIMEOUT_ENV).ok();
        unsafe { std::env::remove_var(COMPILE_TIMEOUT_ENV); }
        let got = timeout_seconds_from_env();
        if let Some(p) = prior {
            unsafe { std::env::set_var(COMPILE_TIMEOUT_ENV, p); }
        }
        assert_eq!(got, DEFAULT_COMPILE_TIMEOUT_SECS);
    }

    #[test]
    fn timeout_seconds_from_env_uses_override_when_set() {
        let prior = std::env::var(COMPILE_TIMEOUT_ENV).ok();
        unsafe { std::env::set_var(COMPILE_TIMEOUT_ENV, "42"); }
        let got = timeout_seconds_from_env();
        match prior {
            Some(p) => unsafe { std::env::set_var(COMPILE_TIMEOUT_ENV, p); },
            None => unsafe { std::env::remove_var(COMPILE_TIMEOUT_ENV); },
        }
        assert_eq!(got, 42);
    }

    #[test]
    fn timeout_seconds_from_env_defaults_on_unparseable() {
        let prior = std::env::var(COMPILE_TIMEOUT_ENV).ok();
        unsafe { std::env::set_var(COMPILE_TIMEOUT_ENV, "not-a-number"); }
        let got = timeout_seconds_from_env();
        match prior {
            Some(p) => unsafe { std::env::set_var(COMPILE_TIMEOUT_ENV, p); },
            None => unsafe { std::env::remove_var(COMPILE_TIMEOUT_ENV); },
        }
        assert_eq!(got, DEFAULT_COMPILE_TIMEOUT_SECS);
    }
}
