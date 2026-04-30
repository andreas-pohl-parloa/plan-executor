//! Single source of truth for every JSON Schema bundled into the binary.
//!
//! The crate keeps schema texts inline via `include_str!` so the daemon does
//! not depend on the on-disk plugin tree at runtime. Each subsystem used to
//! embed and compile its own copy; this registry collapses that into one
//! catalog so the CLI can offer a generic
//! `plan-executor validate --schema=<id> <path>` and every other call site
//! can share the same compiled validators.
//!
//! Schema IDs use the form `<namespace>` or `<namespace>:<sub>` where the
//! namespace is a coarse bucket (`tasks`, `handoffs`, `helper-output`) and
//! the sub-id is a stable kebab-case label. The label vocabulary is intended
//! to be plumbed into SKILL.md instructions, so changing it is a wire-format
//! break.
//!
//! ```text
//! tasks
//! handoffs
//! helper-output:run-reviewer-team
//! helper-output:review-execution-output
//! helper-output:validate-execution-plan
//! helper-output:pr-finalize
//! ```

use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Embedded schema texts — single source of truth.
// ---------------------------------------------------------------------------

const TASKS_SCHEMA: &str = include_str!("schemas/tasks.schema.json");
const HANDOFFS_SCHEMA: &str = include_str!("schemas/handoffs.schema.json");
const RUN_REVIEWER_TEAM_OUTPUT_SCHEMA: &str =
    include_str!("schemas/helpers/run_reviewer_team/output.schema.json");
const REVIEW_EXECUTION_OUTPUT_OUTPUT_SCHEMA: &str =
    include_str!("schemas/helpers/review_execution_output/output.schema.json");
const VALIDATE_EXECUTION_PLAN_OUTPUT_SCHEMA: &str =
    include_str!("schemas/helpers/validate_execution_plan/output.schema.json");
const PR_FINALIZE_OUTPUT_SCHEMA: &str =
    include_str!("schemas/helpers/pr_finalize/output.schema.json");

/// Identifies one of the bundled schemas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SchemaId {
    Tasks,
    Handoffs,
    HelperOutput(HelperOutputKind),
}

/// Sub-id for `helper-output:*` schemas. Mirrors `crate::helper::HelperSkill`
/// but lives here to keep the registry self-contained — the helper module
/// re-exports its own conversion in `crate::helper`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HelperOutputKind {
    RunReviewerTeam,
    ReviewExecutionOutput,
    ValidateExecutionPlan,
    PrFinalize,
}

impl SchemaId {
    /// Stable wire-format label, e.g. `"helper-output:run-reviewer-team"`.
    /// Reverse of [`SchemaId::parse`].
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tasks => "tasks",
            Self::Handoffs => "handoffs",
            Self::HelperOutput(k) => match k {
                HelperOutputKind::RunReviewerTeam => "helper-output:run-reviewer-team",
                HelperOutputKind::ReviewExecutionOutput => "helper-output:review-execution-output",
                HelperOutputKind::ValidateExecutionPlan => "helper-output:validate-execution-plan",
                HelperOutputKind::PrFinalize => "helper-output:pr-finalize",
            },
        }
    }

    /// Parse a wire-format label into a `SchemaId`. Unknown labels return
    /// `Err` with a list of valid IDs so the CLI can surface a useful error.
    pub fn parse(s: &str) -> Result<Self, String> {
        for id in ALL_IDS {
            if id.as_str() == s {
                return Ok(*id);
            }
        }
        let known = ALL_IDS
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!("unknown schema id: {s} (known: {known})"))
    }

    /// Raw embedded JSON text. Used by callers that need to materialize a
    /// schema to disk (e.g. compile_plan's temp-file pathway).
    #[must_use]
    pub fn embedded_text(self) -> &'static str {
        match self {
            Self::Tasks => TASKS_SCHEMA,
            Self::Handoffs => HANDOFFS_SCHEMA,
            Self::HelperOutput(HelperOutputKind::RunReviewerTeam) => RUN_REVIEWER_TEAM_OUTPUT_SCHEMA,
            Self::HelperOutput(HelperOutputKind::ReviewExecutionOutput) => {
                REVIEW_EXECUTION_OUTPUT_OUTPUT_SCHEMA
            }
            Self::HelperOutput(HelperOutputKind::ValidateExecutionPlan) => {
                VALIDATE_EXECUTION_PLAN_OUTPUT_SCHEMA
            }
            Self::HelperOutput(HelperOutputKind::PrFinalize) => PR_FINALIZE_OUTPUT_SCHEMA,
        }
    }
}

/// Every schema currently bundled into the binary, in stable display order.
/// Used by the CLI's `--list-schemas` flag and by [`SchemaId::parse`] for
/// error messages.
pub const ALL_IDS: &[SchemaId] = &[
    SchemaId::Tasks,
    SchemaId::Handoffs,
    SchemaId::HelperOutput(HelperOutputKind::RunReviewerTeam),
    SchemaId::HelperOutput(HelperOutputKind::ReviewExecutionOutput),
    SchemaId::HelperOutput(HelperOutputKind::ValidateExecutionPlan),
    SchemaId::HelperOutput(HelperOutputKind::PrFinalize),
];

/// Returns a process-cached `jsonschema::Validator` for the requested schema.
///
/// The validator is compiled at most once per process; subsequent calls
/// return the same `&'static` reference. A schema-compile failure (i.e. a
/// shipped schema that does not satisfy JSON-Schema-meta) is treated as a
/// build-time bug and surfaced as `Err` for the caller to render.
pub fn compiled(id: SchemaId) -> Result<&'static jsonschema::Validator, String> {
    static TASKS: OnceLock<jsonschema::Validator> = OnceLock::new();
    static HANDOFFS: OnceLock<jsonschema::Validator> = OnceLock::new();
    static RUN_REVIEWER_TEAM: OnceLock<jsonschema::Validator> = OnceLock::new();
    static REVIEW_EXECUTION_OUTPUT: OnceLock<jsonschema::Validator> = OnceLock::new();
    static VALIDATE_EXECUTION_PLAN: OnceLock<jsonschema::Validator> = OnceLock::new();
    static PR_FINALIZE: OnceLock<jsonschema::Validator> = OnceLock::new();

    let cell: &OnceLock<jsonschema::Validator> = match id {
        SchemaId::Tasks => &TASKS,
        SchemaId::Handoffs => &HANDOFFS,
        SchemaId::HelperOutput(HelperOutputKind::RunReviewerTeam) => &RUN_REVIEWER_TEAM,
        SchemaId::HelperOutput(HelperOutputKind::ReviewExecutionOutput) => &REVIEW_EXECUTION_OUTPUT,
        SchemaId::HelperOutput(HelperOutputKind::ValidateExecutionPlan) => &VALIDATE_EXECUTION_PLAN,
        SchemaId::HelperOutput(HelperOutputKind::PrFinalize) => &PR_FINALIZE,
    };

    if let Some(v) = cell.get() {
        return Ok(v);
    }
    let raw = id.embedded_text();
    let json: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| format!("embedded schema {} is not valid JSON: {e}", id.as_str()))?;
    let validator = jsonschema::validator_for(&json)
        .map_err(|e| format!("embedded schema {} failed to compile: {e}", id.as_str()))?;
    Ok(cell.get_or_init(|| validator))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_id_round_trips_and_compiles() {
        for id in ALL_IDS {
            let label = id.as_str();
            let parsed = SchemaId::parse(label).expect("known id parses");
            assert_eq!(parsed, *id, "round-trip for {label}");
            compiled(*id).unwrap_or_else(|e| panic!("compile {label}: {e}"));
        }
    }

    #[test]
    fn unknown_id_lists_known_ones() {
        let err = SchemaId::parse("nope").unwrap_err();
        assert!(err.contains("tasks"), "{err}");
        assert!(err.contains("handoffs"), "{err}");
        assert!(err.contains("helper-output:run-reviewer-team"), "{err}");
    }
}
