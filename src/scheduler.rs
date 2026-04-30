//! Wave-execution scheduler for `JobKind::Plan`.
//!
//! Replaces the orchestrator session that previously drove wave traversal via
//! `claude -p "/plan-executor:execute-plan-non-interactive"`. The Rust-side
//! [`run_wave_execution`] walks the manifest's wave DAG in topological order,
//! dispatches each wave's sub-agents through [`crate::handoff::dispatch_all`],
//! and folds the results into a single [`AttemptOutcome`].
//!
//! No `claude` orchestrator subprocess is spawned by this module. Sub-agent
//! `claude`/`codex`/`gemini`/`bash` invocations remain unchanged and continue
//! to flow through `handoff::dispatch_all`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::Config;
use crate::handoff::{self, AgentType, Handoff};
use crate::job::step::StepContext;
use crate::job::types::AttemptOutcome;

/// Errors surfaced while loading or scheduling a compiled manifest.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SchedulerError {
    /// Manifest file cannot be read from disk.
    #[error("manifest read failed at {path}: {source}")]
    ManifestRead {
        /// Path that failed to load.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Manifest is not valid JSON or does not deserialize into the expected shape.
    #[error("manifest parse failed at {path}: {source}")]
    ManifestParse {
        /// Path that failed to parse.
        path: PathBuf,
        /// Underlying serde error.
        #[source]
        source: serde_json::Error,
    },
    /// Manifest has structural issues that block deterministic dispatch.
    #[error("manifest invariant violated: {0}")]
    Invariant(String),
}

/// Compiled manifest as scheduled by [`run_wave_execution`].
///
/// Mirrors the on-disk `tasks.json` shape — the schema in
/// `src/schemas/tasks.schema.json` is the authority. Fields not consumed by
/// the scheduler are intentionally absent.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[non_exhaustive]
pub struct Manifest {
    /// Schema version; only `1` is currently supported.
    pub version: u32,
    /// Plan-level metadata (path, status, flags, ...).
    pub plan: PlanBlock,
    /// Ordered wave list. Wave `id` is unique within the manifest.
    pub waves: Vec<Wave>,
    /// Map from `task_id` to its prompt+agent spec.
    pub tasks: HashMap<String, TaskSpec>,
}

/// Plan-block subset the scheduler consults during dispatch.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[non_exhaustive]
pub struct PlanBlock {
    /// Execution state of the plan (READY / EXECUTING / COMPLETED / FAILED).
    pub status: String,
    /// Absolute path to the source plan markdown.
    pub path: String,
    /// Where to run the plan: "local" (default) runs the Rust scheduler in
    /// the current process; "remote" submits a job-spec to the configured
    /// remote_repo so a GitHub Actions runner executes it. Stored as the
    /// raw schema string; the manifest schema enforces the {local, remote}
    /// enum at validation time. Older manifests without the field
    /// deserialize as "local" via the `default_execution_mode` fallback.
    #[serde(default = "default_execution_mode")]
    pub execution_mode: String,
}

fn default_execution_mode() -> String {
    "local".to_string()
}

/// One wave: a parallel batch of tasks that runs after `depends_on` waves complete.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[non_exhaustive]
pub struct Wave {
    /// Unique wave id (>= 1, fix-loop waves use ids >= 100).
    pub id: u32,
    /// Tasks scheduled in this wave; dispatched in parallel.
    pub task_ids: Vec<String>,
    /// Wave ids that must complete before this wave starts.
    pub depends_on: Vec<u32>,
    /// Optional kind classifier (`implementation` | `fix` | `validation_fix`).
    #[serde(default = "default_wave_kind")]
    pub kind: String,
}

fn default_wave_kind() -> String {
    "implementation".to_string()
}

/// Per-task spec: which prompt file the agent runs and which CLI dispatches it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[non_exhaustive]
pub struct TaskSpec {
    /// Path to the sub-task prompt markdown, relative to the manifest dir.
    pub prompt_file: String,
    /// `"claude" | "codex" | "gemini" | "bash"`.
    pub agent_type: String,
    /// When `true`, a non-zero sub-agent exit does NOT fail the wave.
    #[serde(default)]
    pub can_fail: bool,
}

/// Loads and parses a `tasks.json` manifest from disk.
///
/// # Errors
///
/// Returns [`SchedulerError::ManifestRead`] when the file cannot be read,
/// [`SchedulerError::ManifestParse`] when JSON deserialization fails, and
/// [`SchedulerError::Invariant`] when structural invariants required by the
/// scheduler (referenced task ids exist, wave dependencies resolve) are not
/// satisfied.
pub fn load_manifest(path: &Path) -> Result<Manifest, SchedulerError> {
    let raw = std::fs::read_to_string(path).map_err(|source| SchedulerError::ManifestRead {
        path: path.to_path_buf(),
        source,
    })?;
    let manifest: Manifest =
        serde_json::from_str(&raw).map_err(|source| SchedulerError::ManifestParse {
            path: path.to_path_buf(),
            source,
        })?;
    validate_invariants(&manifest)?;
    Ok(manifest)
}

/// Confirms cross-references and schema invariants the scheduler relies on.
/// Schema-level validation also happens upstream in
/// [`crate::schema::validate_manifest`]; the duplication here is a defense in
/// depth so the scheduler never accepts a manifest that violates the
/// `src/schemas/tasks.schema.json` contract (`version == 1`, restricted
/// `prompt_file` shape).
fn validate_invariants(m: &Manifest) -> Result<(), SchedulerError> {
    if m.version != 1 {
        return Err(SchedulerError::Invariant(format!(
            "manifest version must be 1; got {}",
            m.version
        )));
    }
    let wave_ids: HashSet<u32> = m.waves.iter().map(|w| w.id).collect();
    if wave_ids.len() != m.waves.len() {
        return Err(SchedulerError::Invariant(
            "duplicate wave ids in manifest".to_string(),
        ));
    }
    for wave in &m.waves {
        for tid in &wave.task_ids {
            if !m.tasks.contains_key(tid) {
                return Err(SchedulerError::Invariant(format!(
                    "wave {} references unknown task `{}`",
                    wave.id, tid
                )));
            }
        }
        for dep in &wave.depends_on {
            if !wave_ids.contains(dep) {
                return Err(SchedulerError::Invariant(format!(
                    "wave {} depends on missing wave {}",
                    wave.id, dep
                )));
            }
        }
    }
    for (tid, task) in &m.tasks {
        validate_prompt_file_shape(tid, &task.prompt_file)?;
    }
    Ok(())
}

/// Enforces the `tasks.schema.json` `prompt_file` regex
/// (`^tasks/[A-Za-z0-9._/-]+\.(md|sh)$`) without pulling in a regex dep.
///
/// Equivalent semantics: starts with `tasks/`, ends in `.md` or `.sh`, only
/// the documented character class is allowed, and no `..` segments slip
/// through. Also rejects absolute paths and back-slash separators that could
/// be misread as Windows drive specifiers.
fn validate_prompt_file_shape(task_id: &str, prompt_file: &str) -> Result<(), SchedulerError> {
    let invariant = |msg: String| SchedulerError::Invariant(msg);
    if prompt_file.is_empty() {
        return Err(invariant(format!("task `{task_id}` has empty prompt_file")));
    }
    if Path::new(prompt_file).is_absolute() {
        return Err(invariant(format!(
            "task `{task_id}` prompt_file `{prompt_file}` must be a relative path"
        )));
    }
    if !prompt_file.starts_with("tasks/") {
        return Err(invariant(format!(
            "task `{task_id}` prompt_file `{prompt_file}` must start with `tasks/`"
        )));
    }
    if !(prompt_file.ends_with(".md") || prompt_file.ends_with(".sh")) {
        return Err(invariant(format!(
            "task `{task_id}` prompt_file `{prompt_file}` must end with .md or .sh"
        )));
    }
    if prompt_file.contains('\\') {
        return Err(invariant(format!(
            "task `{task_id}` prompt_file `{prompt_file}` may not contain backslashes"
        )));
    }
    // Guard against `..` segments and characters outside [A-Za-z0-9._/-].
    for segment in prompt_file.split('/') {
        if segment == ".." {
            return Err(invariant(format!(
                "task `{task_id}` prompt_file `{prompt_file}` may not contain `..` segments"
            )));
        }
        if segment.is_empty() {
            return Err(invariant(format!(
                "task `{task_id}` prompt_file `{prompt_file}` may not contain empty path segments"
            )));
        }
    }
    for ch in prompt_file.chars() {
        let allowed =
            ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '/' | '-');
        if !allowed {
            return Err(invariant(format!(
                "task `{task_id}` prompt_file `{prompt_file}` contains disallowed character `{ch}`"
            )));
        }
    }
    Ok(())
}

impl Manifest {
    /// Topologically orders wave ids using Kahn's algorithm. Returns an error
    /// when the wave DAG contains a cycle.
    ///
    /// Ties between waves whose dependencies have all been satisfied are
    /// broken by ascending wave id so dispatch is deterministic.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerError::Invariant`] when the wave graph has a cycle.
    pub fn topological_wave_order(&self) -> Result<Vec<u32>, SchedulerError> {
        let mut in_degree: HashMap<u32, usize> =
            self.waves.iter().map(|w| (w.id, w.depends_on.len())).collect();

        // Use a descending-sorted stack; pop() yields the smallest id first.
        let mut ready: Vec<u32> = in_degree
            .iter()
            .filter_map(|(&id, &deg)| (deg == 0).then_some(id))
            .collect();
        ready.sort_unstable_by(|a, b| b.cmp(a));

        let mut order: Vec<u32> = Vec::with_capacity(self.waves.len());
        while let Some(id) = ready.pop() {
            order.push(id);
            for w in &self.waves {
                if w.depends_on.contains(&id) {
                    if let Some(deg) = in_degree.get_mut(&w.id) {
                        *deg = deg.saturating_sub(1);
                        if *deg == 0 {
                            ready.push(w.id);
                        }
                    }
                }
            }
            ready.sort_unstable_by(|a, b| b.cmp(a));
        }

        if order.len() != self.waves.len() {
            return Err(SchedulerError::Invariant(
                "wave graph contains a cycle".to_string(),
            ));
        }
        Ok(order)
    }

    /// Returns a borrow of the wave with the given id, if any.
    #[must_use]
    pub fn wave(&self, id: u32) -> Option<&Wave> {
        self.waves.iter().find(|w| w.id == id)
    }
}

/// Per-wave outcome captured for the step output JSON.
#[derive(Debug, Clone, Serialize)]
struct WaveOutcome {
    wave_id: u32,
    kind: String,
    dispatched: usize,
    succeeded: usize,
    failed_required: Vec<usize>,
    skipped_can_fail: Vec<usize>,
}

/// Executes every wave in the manifest's topological order.
///
/// The function is the sole entry point for wave traversal and replaces the
/// previous LLM-driven orchestrator. It uses [`handoff::dispatch_all`] for
/// sub-agent dispatch and respects [`StepContext::workdir`] for all I/O paths.
///
/// # Errors
///
/// Never returns a `Result` — protocol violations are surfaced by writing the
/// summary JSON to the step's attempt scratch dir and returning the matching
/// [`AttemptOutcome`] variant.
#[tracing::instrument(skip(ctx, manifest, manifest_dir), fields(waves = manifest.waves.len()))]
pub async fn run_wave_execution(
    ctx: &mut StepContext,
    manifest: &Manifest,
    manifest_dir: &Path,
) -> AttemptOutcome {
    let order = match manifest.topological_wave_order() {
        Ok(o) => o,
        Err(e) => {
            return AttemptOutcome::ProtocolViolation {
                category: "manifest_invariant".to_string(),
                detail: e.to_string(),
            };
        }
    };

    let config = match Config::load(None) {
        Ok(c) => c,
        Err(e) => {
            return AttemptOutcome::HardInfra {
                error: format!("config load failed: {e}"),
            };
        }
    };

    let manifest_dir = manifest_dir.to_path_buf();
    let mut wave_outcomes: Vec<WaveOutcome> = Vec::with_capacity(order.len());

    for wave_id in order {
        let Some(wave) = manifest.wave(wave_id) else {
            return AttemptOutcome::ProtocolViolation {
                category: "manifest_invariant".to_string(),
                detail: format!("topological order referenced unknown wave {wave_id}"),
            };
        };

        let handoffs = match build_handoffs(wave, manifest, &manifest_dir) {
            Ok(h) => h,
            Err(e) => {
                return AttemptOutcome::ProtocolViolation {
                    category: "manifest_invariant".to_string(),
                    detail: e.to_string(),
                };
            }
        };

        let dispatched = handoffs.len();
        tracing::info!(
            wave_id, kind = %wave.kind, dispatched,
            "dispatching wave",
        );

        let (results, _pgids) = handoff::dispatch_all(
            handoffs,
            &config.agents.claude,
            &config.agents.codex,
            &config.agents.gemini,
            &config.agents.bash,
            None,
            None,
            None,
        )
        .await;

        let mut succeeded = 0_usize;
        let mut failed_required: Vec<usize> = Vec::new();
        let mut skipped_can_fail: Vec<usize> = Vec::new();
        for r in &results {
            if r.success {
                succeeded += 1;
            } else if r.can_fail {
                skipped_can_fail.push(r.index);
            } else {
                failed_required.push(r.index);
            }
        }

        let outcome = WaveOutcome {
            wave_id,
            kind: wave.kind.clone(),
            dispatched,
            succeeded,
            failed_required: failed_required.clone(),
            skipped_can_fail,
        };
        wave_outcomes.push(outcome);

        if !failed_required.is_empty() {
            tracing::warn!(
                wave_id,
                failed = ?failed_required,
                "wave failed; aborting traversal",
            );
            let _ = write_step_summary(ctx, &wave_outcomes, false);
            return AttemptOutcome::SemanticMistake { fix_loop_round: 0 };
        }
    }

    let _ = write_step_summary(ctx, &wave_outcomes, true);
    AttemptOutcome::Success
}

/// Builds the [`Handoff`] vector for a single wave from its [`TaskSpec`]s.
///
/// # Errors
///
/// Returns [`SchedulerError::Invariant`] when a task's `agent_type` does not
/// map to a known [`AgentType`] or the prompt file path cannot be resolved.
fn build_handoffs(
    wave: &Wave,
    manifest: &Manifest,
    manifest_dir: &Path,
) -> Result<Vec<Handoff>, SchedulerError> {
    wave.task_ids
        .iter()
        .enumerate()
        .map(|(idx, tid)| {
            let task = manifest.tasks.get(tid).ok_or_else(|| {
                SchedulerError::Invariant(format!(
                    "wave {} references unknown task `{}`",
                    wave.id, tid
                ))
            })?;
            let agent_type = parse_agent_type(&task.agent_type).ok_or_else(|| {
                SchedulerError::Invariant(format!(
                    "task `{}` has unsupported agent_type `{}`",
                    tid, task.agent_type
                ))
            })?;
            // Defense in depth: re-validate the prompt_file shape and
            // confirm the joined path stays under `manifest_dir`. The
            // path-shape check already rejects `..` and absolute prompts,
            // but canonicalize gives us a real-filesystem guard against
            // symlink-based escapes when both paths exist on disk.
            validate_prompt_file_shape(tid, &task.prompt_file)?;
            if Path::new(&task.prompt_file).is_absolute() {
                return Err(SchedulerError::Invariant(format!(
                    "task `{tid}` prompt_file `{}` must be a relative path",
                    task.prompt_file
                )));
            }
            let prompt_file = manifest_dir.join(&task.prompt_file);
            if let (Ok(canon_prompt), Ok(canon_dir)) =
                (std::fs::canonicalize(&prompt_file), std::fs::canonicalize(manifest_dir))
            {
                if !canon_prompt.starts_with(&canon_dir) {
                    return Err(SchedulerError::Invariant(format!(
                        "task `{tid}` prompt_file `{}` resolves outside manifest_dir `{}`",
                        canon_prompt.display(),
                        canon_dir.display()
                    )));
                }
            }
            Ok(Handoff {
                index: idx + 1,
                agent_type,
                prompt_file,
                can_fail: task.can_fail,
            })
        })
        .collect()
}

/// Maps the manifest's `agent_type` enum value onto the runtime [`AgentType`].
fn parse_agent_type(s: &str) -> Option<AgentType> {
    match s {
        "claude" => Some(AgentType::Claude),
        "codex" => Some(AgentType::Codex),
        "gemini" => Some(AgentType::Gemini),
        "bash" => Some(AgentType::Bash),
        _ => None,
    }
}

/// Writes a JSON summary of the wave traversal to the per-attempt scratch
/// directory. Best-effort — failures here do not change the attempt outcome.
fn write_step_summary(
    ctx: &StepContext,
    outcomes: &[WaveOutcome],
    success: bool,
) -> std::io::Result<()> {
    let dir = ctx
        .job_dir
        .join("steps")
        .join(format!("{:03}-wave_execution", ctx.step_seq))
        .join("attempts")
        .join(ctx.attempt_n.to_string());
    std::fs::create_dir_all(&dir)?;
    let payload = serde_json::json!({
        "success": success,
        "waves": outcomes,
    });
    std::fs::write(
        dir.join("scheduler_summary.json"),
        serde_json::to_string_pretty(&payload)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with_waves(waves: Vec<Wave>, tasks: Vec<(&str, TaskSpec)>) -> Manifest {
        Manifest {
            version: 1,
            plan: PlanBlock {
                status: "READY".to_string(),
                path: "/tmp/plan.md".to_string(),
                execution_mode: "local".to_string(),
            },
            waves,
            tasks: tasks
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        }
    }

    fn task(prompt: &str, agent: &str, can_fail: bool) -> TaskSpec {
        TaskSpec {
            prompt_file: prompt.to_string(),
            agent_type: agent.to_string(),
            can_fail,
        }
    }

    #[test]
    fn topological_order_visits_smaller_ids_first() {
        let m = manifest_with_waves(
            vec![
                Wave {
                    id: 2,
                    task_ids: vec!["t2".into()],
                    depends_on: vec![1],
                    kind: "implementation".into(),
                },
                Wave {
                    id: 1,
                    task_ids: vec!["t1".into()],
                    depends_on: vec![],
                    kind: "implementation".into(),
                },
            ],
            vec![
                ("t1", task("tasks/t1.md", "claude", false)),
                ("t2", task("tasks/t2.md", "claude", false)),
            ],
        );
        assert_eq!(m.topological_wave_order().unwrap(), vec![1, 2]);
    }

    #[test]
    fn topological_order_orders_diamond_dependency() {
        let m = manifest_with_waves(
            vec![
                Wave {
                    id: 1,
                    task_ids: vec!["t1".into()],
                    depends_on: vec![],
                    kind: "implementation".into(),
                },
                Wave {
                    id: 2,
                    task_ids: vec!["t2".into()],
                    depends_on: vec![1],
                    kind: "implementation".into(),
                },
                Wave {
                    id: 3,
                    task_ids: vec!["t3".into()],
                    depends_on: vec![1],
                    kind: "implementation".into(),
                },
                Wave {
                    id: 4,
                    task_ids: vec!["t4".into()],
                    depends_on: vec![2, 3],
                    kind: "implementation".into(),
                },
            ],
            vec![
                ("t1", task("tasks/t1.md", "claude", false)),
                ("t2", task("tasks/t2.md", "claude", false)),
                ("t3", task("tasks/t3.md", "claude", false)),
                ("t4", task("tasks/t4.md", "claude", false)),
            ],
        );
        let order = m.topological_wave_order().unwrap();
        assert_eq!(order.first(), Some(&1));
        assert_eq!(order.last(), Some(&4));
        let pos = |w: u32| order.iter().position(|&x| x == w).unwrap();
        assert!(pos(1) < pos(2));
        assert!(pos(1) < pos(3));
        assert!(pos(2) < pos(4));
        assert!(pos(3) < pos(4));
    }

    #[test]
    fn topological_order_detects_cycle() {
        let m = manifest_with_waves(
            vec![
                Wave {
                    id: 1,
                    task_ids: vec!["t1".into()],
                    depends_on: vec![2],
                    kind: "implementation".into(),
                },
                Wave {
                    id: 2,
                    task_ids: vec!["t2".into()],
                    depends_on: vec![1],
                    kind: "implementation".into(),
                },
            ],
            vec![
                ("t1", task("tasks/t1.md", "claude", false)),
                ("t2", task("tasks/t2.md", "claude", false)),
            ],
        );
        let err = m.topological_wave_order().unwrap_err();
        match err {
            SchedulerError::Invariant(msg) => assert!(msg.contains("cycle")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn validate_invariants_rejects_unknown_task_id() {
        let m = manifest_with_waves(
            vec![Wave {
                id: 1,
                task_ids: vec!["missing".into()],
                depends_on: vec![],
                kind: "implementation".into(),
            }],
            vec![("t1", task("tasks/t1.md", "claude", false))],
        );
        let err = validate_invariants(&m).unwrap_err();
        match err {
            SchedulerError::Invariant(msg) => assert!(msg.contains("unknown task")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn validate_invariants_rejects_missing_dep_wave() {
        let m = manifest_with_waves(
            vec![Wave {
                id: 1,
                task_ids: vec!["t1".into()],
                depends_on: vec![999],
                kind: "implementation".into(),
            }],
            vec![("t1", task("tasks/t1.md", "claude", false))],
        );
        let err = validate_invariants(&m).unwrap_err();
        match err {
            SchedulerError::Invariant(msg) => assert!(msg.contains("missing wave")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_agent_type_recognizes_all_four() {
        assert!(matches!(parse_agent_type("claude"), Some(AgentType::Claude)));
        assert!(matches!(parse_agent_type("codex"), Some(AgentType::Codex)));
        assert!(matches!(parse_agent_type("gemini"), Some(AgentType::Gemini)));
        assert!(matches!(parse_agent_type("bash"), Some(AgentType::Bash)));
        assert!(parse_agent_type("perl").is_none());
    }

    #[test]
    fn build_handoffs_resolves_prompt_paths_against_manifest_dir() {
        let m = manifest_with_waves(
            vec![Wave {
                id: 1,
                task_ids: vec!["t1".into(), "t2".into()],
                depends_on: vec![],
                kind: "implementation".into(),
            }],
            vec![
                ("t1", task("tasks/t1.md", "claude", false)),
                ("t2", task("tasks/t2.md", "codex", true)),
            ],
        );
        let dir = Path::new("/tmp/manifest-dir");
        let handoffs = build_handoffs(&m.waves[0], &m, dir).unwrap();
        assert_eq!(handoffs.len(), 2);
        assert_eq!(handoffs[0].index, 1);
        assert_eq!(handoffs[1].index, 2);
        assert_eq!(handoffs[0].prompt_file, dir.join("tasks/t1.md"));
        assert_eq!(handoffs[1].prompt_file, dir.join("tasks/t2.md"));
        assert!(matches!(handoffs[0].agent_type, AgentType::Claude));
        assert!(matches!(handoffs[1].agent_type, AgentType::Codex));
        assert!(!handoffs[0].can_fail);
        assert!(handoffs[1].can_fail);
    }

    #[test]
    fn load_manifest_round_trips_minimal_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let json = r#"{
            "version": 1,
            "plan": {
                "goal": "g", "type": "feature", "path": "/tmp/p.md", "status": "READY",
                "flags": {
                    "merge": false, "merge_admin": false, "skip_pr": false,
                    "skip_code_review": false, "no_worktree": false, "draft_pr": false
                }
            },
            "waves": [{"id": 1, "task_ids": ["t1"], "depends_on": []}],
            "tasks": {"t1": {"prompt_file": "tasks/t1.md", "agent_type": "claude"}}
        }"#;
        std::fs::write(&path, json).unwrap();
        let m = load_manifest(&path).unwrap();
        assert_eq!(m.version, 1);
        assert_eq!(m.waves.len(), 1);
        assert_eq!(m.waves[0].kind, "implementation");
        assert!(m.tasks.contains_key("t1"));
    }
}
