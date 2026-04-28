//! Integration tests for the GitHub Actions dispatcher in
//! [`docs/remote-execution/execute-plan.yml`] (Task C2.1).
//!
//! The workflow contains three pieces of branching shell logic that must
//! match the executor CLI contract: the **kind-detection** step reads
//! `job-spec.json`'s `kind` field, the **pr-finalize metadata
//! validation** step enforces field shapes, and the **Run pr-finalize**
//! step composes the argv for `plan-executor run pr-finalize`.
//!
//! These tests extract those three `run:` blocks directly from the
//! committed YAML and exercise them under bash with deterministic
//! fixtures. No real GitHub Actions runner, no network, no real `gh`.
//!
//! ### Test design
//!
//! Two layers from the task brief are merged into one Rust target:
//!
//! 1. **Local invocation test** — fake `plan-executor` binary on PATH;
//!    drive the "Run pr-finalize" run-block and assert the recorded
//!    argv matches the expected shape per ECP partition.
//! 2. **Synthetic GHA round-trip** — extract the kind-detection and
//!    metadata-validation run-blocks, execute them against synthetic
//!    `job-spec.json` fixtures, assert on `$GITHUB_OUTPUT` content and
//!    process exit codes.
//!
//! ### ECP partitions — kind detection
//! | P1 | no job-spec.json present       | kind=plan, exit 0 |
//! | P2 | kind=plan                       | kind=plan, exit 0 |
//! | P3 | kind=pr-finalize                | kind=pr-finalize, exit 0 |
//! | P4 | kind missing/empty              | exit 1 |
//! | P5 | unknown kind                    | exit 1 |
//!
//! ### ECP partitions — pr-finalize metadata validation
//! | P1  | pr only                                     | exit 0     |
//! | P2  | missing pr                                  | exit 1     |
//! | P3  | non-numeric pr                              | exit 1     |
//! | P4  | merge && merge_admin                        | exit 1     |
//! | P5  | merge alone                                 | exit 0     |
//! | P6  | merge_admin alone                           | exit 0     |
//! | P7  | owner without repo                          | exit 1     |
//! | P8  | repo without owner                          | exit 1     |
//! | P9  | owner+repo together (charset valid)         | exit 0     |
//! | P10 | owner with shell metachar (`a/b`)           | exit 1     |
//! | P11 | repo with shell metachar (`r;m`)            | exit 1     |
//!
//! ### ECP partitions — argv composition (Run pr-finalize)
//! | P1 | pr only                | run pr-finalize --pr N                                   |
//! | P2 | pr + merge=true        | run pr-finalize --pr N --merge                           |
//! | P3 | pr + merge_admin=true  | run pr-finalize --pr N --merge-admin                     |
//! | P4 | pr + owner + repo      | run pr-finalize --pr N --owner X --repo Y                |

#![allow(clippy::unwrap_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Repo-root-relative path to the workflow YAML under test.
const WORKFLOW_PATH: &str = "docs/remote-execution/execute-plan.yml";

/// Fully-resolved path to the workflow YAML, anchored at `CARGO_MANIFEST_DIR`.
fn workflow_yaml_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(WORKFLOW_PATH)
}

/// Reads the entire YAML file as a string. Panics on I/O error so failures
/// surface in test output rather than silently masking a missing fixture.
fn read_workflow() -> String {
    fs::read_to_string(workflow_yaml_path()).expect("read workflow YAML")
}

/// Extracts the body of a `run: |` block belonging to the named workflow
/// step. The workflow uses a fixed two-space indent for steps and a
/// twelve-space indent for run-block bodies; we strip the latter so the
/// extracted body executes as standalone bash.
///
/// Uses simple line-anchored matching because the workflow is committed
/// with stable formatting. If the YAML reformats, this helper will fail
/// loudly — better than silently re-emitting stale fixture content.
fn extract_run_block(yaml: &str, step_name: &str) -> String {
    let header = format!("- name: {step_name}");
    let start = yaml
        .find(&header)
        .unwrap_or_else(|| panic!("step '{step_name}' not found in workflow YAML"));
    let after_header = &yaml[start..];
    // Locate `run: |` for this step, then capture all subsequent lines that
    // are indented (bash body) or blank, stopping at the next less-indented
    // step header or document end.
    let run_marker = "run: |";
    let run_offset = after_header
        .find(run_marker)
        .unwrap_or_else(|| panic!("'run: |' not found for step '{step_name}'"));
    let body_start = run_offset + run_marker.len();
    let body_region = &after_header[body_start..];
    let mut body = String::new();
    for line in body_region.lines().skip(1) {
        // The run body is indented with 10 spaces in this workflow
        // (steps at 6, body at 10). Blank lines have zero indent and
        // should be preserved verbatim.
        if line.is_empty() {
            body.push('\n');
            continue;
        }
        // Stop when we leave the run body (a new step or top-level key).
        let leading_spaces = line.len() - line.trim_start().len();
        if leading_spaces < 10 && !line.trim().is_empty() {
            break;
        }
        // Strip the 10-space body indent.
        let stripped = if line.len() >= 10 { &line[10..] } else { line };
        body.push_str(stripped);
        body.push('\n');
    }
    body
}

/// Writes `body` to `path` with a `#!/bin/bash` shebang and 0o755 perms.
fn write_bash_script(path: &Path, body: &str) {
    let full = format!("#!/bin/bash\nset -e\n{body}");
    fs::write(path, full).expect("write script");
    let mut perms = fs::metadata(path).expect("stat script").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod script");
}

/// On macOS, BSD `grep` lacks `-P` (Perl regex). The workflow runs on
/// Ubuntu where GNU grep is the default, but the tests need to execute
/// the same script bodies on developer machines too. This helper
/// installs a `grep` shim in `dir` that forwards to `ggrep` when it
/// exists (homebrew coreutils provides it). Returns the dir path so
/// callers can prepend it to `PATH`.
///
/// On Linux (or when `ggrep` is absent) this is a no-op and the system
/// `grep` is used unchanged.
fn ensure_gnu_grep_shim(dir: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        let ggrep = std::process::Command::new("which")
            .arg("ggrep")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        if let Some(ggrep_path) = ggrep {
            let shim = dir.join("grep");
            let body = format!("exec {ggrep_path} \"$@\"\n");
            write_bash_script(&shim, &body);
        }
    }
    dir.to_path_buf()
}

/// Builds a `PATH` value that prepends `dir` to the existing PATH so a
/// shim binary in `dir` (e.g. `grep` or `plan-executor`) takes
/// precedence over the system equivalent.
fn prepended_path(dir: &Path) -> String {
    format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// Outcome of a single dispatcher script invocation. `PartialEq` lets the
/// tests compare the entire result structure with one `assert_eq!` per
/// the test-code-recipe rule. `error_annotation_emitted` checks combined
/// stdout+stderr for the GitHub Actions `::error::` annotation, which
/// the workflow scripts write via `echo` (stdout).
#[derive(Debug, PartialEq, Eq)]
struct DispatchResult {
    exit_code: i32,
    github_output: String,
    error_annotation_emitted: bool,
}

/// Runs the kind-detection script in `script_dir` (which must already
/// contain whatever `job-spec.json` fixture the case under test
/// requires). Captures stdout, stderr, exit, and the contents of the
/// per-invocation `GITHUB_OUTPUT` file.
fn run_kind_detection(script_dir: &Path, kind_script_body: &str) -> DispatchResult {
    let script = script_dir.join("kind.sh");
    write_bash_script(&script, kind_script_body);
    let github_output = script_dir.join("gha_output.txt");
    fs::write(&github_output, "").expect("init GITHUB_OUTPUT");
    let shim_dir = ensure_gnu_grep_shim(script_dir);

    let out = Command::new("bash")
        .arg(&script)
        .current_dir(script_dir)
        .env("GITHUB_OUTPUT", &github_output)
        .env("PATH", prepended_path(&shim_dir))
        .output()
        .expect("spawn bash");

    DispatchResult {
        exit_code: out.status.code().unwrap_or(-1),
        github_output: fs::read_to_string(&github_output).unwrap_or_default(),
        error_annotation_emitted: format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)).contains("::error::"),
    }
}

/// Runs the pr-finalize metadata-validation script in `script_dir`.
/// Identical capture semantics to [`run_kind_detection`]; the validation
/// step writes to `GITHUB_OUTPUT` only on the success path.
fn run_pr_finalize_validation(script_dir: &Path, validation_body: &str) -> DispatchResult {
    let script = script_dir.join("validate.sh");
    write_bash_script(&script, validation_body);
    let github_output = script_dir.join("gha_output.txt");
    fs::write(&github_output, "").expect("init GITHUB_OUTPUT");
    let shim_dir = ensure_gnu_grep_shim(script_dir);

    let out = Command::new("bash")
        .arg(&script)
        .current_dir(script_dir)
        .env("GITHUB_OUTPUT", &github_output)
        .env("PATH", prepended_path(&shim_dir))
        .output()
        .expect("spawn bash");

    DispatchResult {
        exit_code: out.status.code().unwrap_or(-1),
        github_output: fs::read_to_string(&github_output).unwrap_or_default(),
        error_annotation_emitted: format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)).contains("::error::"),
    }
}

/// Helper: writes a `job-spec.json` fixture in `dir` with arbitrary JSON.
fn write_job_spec(dir: &Path, json: &serde_json::Value) {
    fs::write(
        dir.join("job-spec.json"),
        serde_json::to_string_pretty(json).unwrap(),
    )
    .expect("write job-spec.json");
}

/// Runs the kind-detection step against an optional fixture. `Some`
/// installs that JSON as `job-spec.json`; `None` leaves the directory
/// without one to exercise the missing-file branch.
fn drive_kind_detection(fixture: Option<serde_json::Value>) -> DispatchResult {
    let yaml = read_workflow();
    let body = extract_run_block(&yaml, "Detect job kind");
    let dir = TempDir::new().unwrap();
    if let Some(json) = fixture {
        write_job_spec(dir.path(), &json);
    }
    run_kind_detection(dir.path(), &body)
}

/// Runs the pr-finalize metadata-validation step against `fixture` and
/// returns the result plus parsed `GITHUB_OUTPUT` pairs.
fn drive_pr_finalize_validation(
    fixture: serde_json::Value,
) -> (DispatchResult, Vec<(String, String)>) {
    let yaml = read_workflow();
    let body = extract_run_block(&yaml, "Parse and validate pr-finalize metadata");
    let dir = TempDir::new().unwrap();
    write_job_spec(dir.path(), &fixture);
    let result = run_pr_finalize_validation(dir.path(), &body);
    let outputs = parse_outputs(&result.github_output);
    (result, outputs)
}

/// Sentinel for failure-path tests: the dispatcher exits non-zero, emits
/// an `::error::` annotation, and writes nothing to `GITHUB_OUTPUT`.
fn failure_dispatch_result() -> DispatchResult {
    DispatchResult {
        exit_code: 1,
        github_output: String::new(),
        error_annotation_emitted: true,
    }
}

// ---------------------------------------------------------------------
// Sanity check: the workflow extractor still recognizes the named steps
// after any future YAML reformat. If this breaks, the extractor needs a
// matching update.
// ---------------------------------------------------------------------

#[test]
fn workflow_yaml_exposes_expected_dispatch_steps() {
    let yaml = read_workflow();
    let kind_body = extract_run_block(&yaml, "Detect job kind");
    let validation_body = extract_run_block(&yaml, "Parse and validate pr-finalize metadata");
    let argv_body = extract_run_block(&yaml, "Run pr-finalize");

    let extracted = (
        kind_body.contains("jq -r '.kind"),
        validation_body.contains("'pr' must be a positive integer"),
        argv_body.contains("ARGS=(run pr-finalize --pr"),
    );
    assert_eq!(extracted, (true, true, true));
}

// ---------------------------------------------------------------------
// Kind-detection tests (synthetic GHA round-trip layer)
// ---------------------------------------------------------------------

/// Sentinel: success path with kind=`expected_kind`.
fn kind_success(expected_kind: &str) -> DispatchResult {
    DispatchResult {
        exit_code: 0,
        github_output: format!("kind={expected_kind}\n"),
        error_annotation_emitted: false,
    }
}

#[test]
fn kind_detection_falls_back_to_plan_when_job_spec_missing() {
    let result = drive_kind_detection(None);
    assert_eq!(result, kind_success("plan"));
}

#[test]
fn kind_detection_selects_plan_when_kind_is_plan() {
    let result = drive_kind_detection(Some(serde_json::json!({"kind": "plan"})));
    assert_eq!(result, kind_success("plan"));
}

#[test]
fn kind_detection_selects_pr_finalize_when_kind_is_pr_finalize() {
    let result = drive_kind_detection(Some(serde_json::json!({"kind": "pr-finalize", "pr": 42})));
    assert_eq!(result, kind_success("pr-finalize"));
}

#[test]
fn kind_detection_fails_when_kind_field_is_empty() {
    let result = drive_kind_detection(Some(serde_json::json!({"other": "x"})));
    assert_eq!(result, failure_dispatch_result());
}

#[test]
fn kind_detection_fails_on_unknown_kind() {
    let result = drive_kind_detection(Some(serde_json::json!({"kind": "deploy"})));
    assert_eq!(result, failure_dispatch_result());
}

// ---------------------------------------------------------------------
// pr-finalize metadata-validation tests
// ---------------------------------------------------------------------

/// Parses key=value lines from `GITHUB_OUTPUT` content into a sorted
/// `Vec` so the entire expected output can be compared with a single
/// `assert_eq!`.
fn parse_outputs(s: &str) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = s
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    pairs.sort();
    pairs
}

/// Builds the expected `(exit, outputs)` tuple for a successful
/// validation run. Workflow always writes all five keys; defaults are
/// `false` / empty string per the script.
fn ok_validation(
    pr: &str,
    merge: &str,
    merge_admin: &str,
    owner: &str,
    repo: &str,
) -> (i32, Vec<(String, String)>) {
    (
        0,
        vec![
            ("merge".to_string(), merge.to_string()),
            ("merge_admin".to_string(), merge_admin.to_string()),
            ("owner".to_string(), owner.to_string()),
            ("pr".to_string(), pr.to_string()),
            ("repo".to_string(), repo.to_string()),
        ],
    )
}

#[test]
fn pr_finalize_validation_accepts_valid_pr_only() {
    let (result, outputs) =
        drive_pr_finalize_validation(serde_json::json!({"kind": "pr-finalize", "pr": 7}));
    assert_eq!(
        (result.exit_code, outputs),
        ok_validation("7", "false", "false", "", "")
    );
}

#[test]
fn pr_finalize_validation_rejects_missing_pr() {
    let (result, _) = drive_pr_finalize_validation(serde_json::json!({"kind": "pr-finalize"}));
    assert_eq!(result, failure_dispatch_result());
}

#[test]
fn pr_finalize_validation_rejects_non_numeric_pr() {
    let (result, _) = drive_pr_finalize_validation(
        serde_json::json!({"kind": "pr-finalize", "pr": "abc"}),
    );
    assert_eq!(result, failure_dispatch_result());
}

#[test]
fn pr_finalize_validation_rejects_merge_and_merge_admin_together() {
    let (result, _) = drive_pr_finalize_validation(serde_json::json!({
        "kind": "pr-finalize",
        "pr": 9,
        "merge": true,
        "merge_admin": true,
    }));
    assert_eq!(result, failure_dispatch_result());
}

#[test]
fn pr_finalize_validation_accepts_merge_alone() {
    let (result, outputs) = drive_pr_finalize_validation(
        serde_json::json!({"kind": "pr-finalize", "pr": 9, "merge": true}),
    );
    assert_eq!(
        (result.exit_code, outputs),
        ok_validation("9", "true", "false", "", "")
    );
}

#[test]
fn pr_finalize_validation_accepts_merge_admin_alone() {
    let (result, outputs) = drive_pr_finalize_validation(
        serde_json::json!({"kind": "pr-finalize", "pr": 11, "merge_admin": true}),
    );
    assert_eq!(
        (result.exit_code, outputs),
        ok_validation("11", "false", "true", "", "")
    );
}

#[test]
fn pr_finalize_validation_rejects_owner_without_repo() {
    let (result, _) = drive_pr_finalize_validation(
        serde_json::json!({"kind": "pr-finalize", "pr": 1, "owner": "andreas"}),
    );
    assert_eq!(result, failure_dispatch_result());
}

#[test]
fn pr_finalize_validation_rejects_repo_without_owner() {
    let (result, _) = drive_pr_finalize_validation(
        serde_json::json!({"kind": "pr-finalize", "pr": 1, "repo": "plan-executor"}),
    );
    assert_eq!(result, failure_dispatch_result());
}

#[test]
fn pr_finalize_validation_accepts_owner_and_repo_together() {
    let (result, outputs) = drive_pr_finalize_validation(serde_json::json!({
        "kind": "pr-finalize",
        "pr": 5,
        "owner": "andreas-pohl-parloa",
        "repo": "plan-executor",
    }));
    assert_eq!(
        (result.exit_code, outputs),
        ok_validation("5", "false", "false", "andreas-pohl-parloa", "plan-executor")
    );
}

#[test]
fn pr_finalize_validation_rejects_owner_with_slash() {
    let (result, _) = drive_pr_finalize_validation(serde_json::json!({
        "kind": "pr-finalize",
        "pr": 1,
        "owner": "andreas/evil",
        "repo": "plan-executor",
    }));
    assert_eq!(result, failure_dispatch_result());
}

#[test]
fn pr_finalize_validation_rejects_repo_with_semicolon() {
    let (result, _) = drive_pr_finalize_validation(serde_json::json!({
        "kind": "pr-finalize",
        "pr": 1,
        "owner": "andreas",
        "repo": "plan-executor;rm",
    }));
    assert_eq!(result, failure_dispatch_result());
}

// ---------------------------------------------------------------------
// argv composition tests (local invocation layer)
//
// The "Run pr-finalize" step builds an `ARGS=(...)` array from the
// validated GHA outputs and then invokes `plan-executor "${ARGS[@]}"`.
// We replace `plan-executor` with a fake shim that records its argv to
// a counter file, then assert on the recorded argv.
// ---------------------------------------------------------------------

/// Captured argv from the fake `plan-executor` shim. The argv is space-
/// joined to keep the comparison structure flat.
#[derive(Debug, PartialEq, Eq)]
struct ArgvCapture {
    exit_code: i32,
    recorded_argv: String,
}

/// Drives the "Run pr-finalize" run-block under a fake `plan-executor`
/// on PATH. Substitutes `${{ steps.meta_pr_finalize.outputs.* }}`
/// expressions with concrete env values before running.
fn run_argv_composition(
    pr: &str,
    merge: &str,
    merge_admin: &str,
    owner: &str,
    repo: &str,
) -> ArgvCapture {
    let yaml = read_workflow();
    let raw_body = extract_run_block(&yaml, "Run pr-finalize");

    let dir = TempDir::new().unwrap();
    let counter = dir.path().join("argv.log");
    fs::write(&counter, "").unwrap();

    // Fake `plan-executor` records argv (one space-joined line per call).
    let fake = dir.path().join("plan-executor");
    let fake_body = format!(
        "echo \"$@\" >> \"{path}\"\nexit 0\n",
        path = counter.display()
    );
    write_bash_script(&fake, &fake_body);

    let script = dir.path().join("run.sh");
    write_bash_script(&script, &raw_body);

    let path_env = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let out = Command::new("bash")
        .arg(&script)
        .current_dir(dir.path())
        .env("PATH", path_env)
        .env("PR", pr)
        .env("MERGE", merge)
        .env("MERGE_ADMIN", merge_admin)
        .env("OWNER", owner)
        .env("REPO", repo)
        .output()
        .expect("spawn bash");

    let recorded = fs::read_to_string(&counter)
        .unwrap_or_default()
        .trim_end_matches('\n')
        .to_string();
    ArgvCapture {
        exit_code: out.status.code().unwrap_or(-1),
        recorded_argv: recorded,
    }
}

#[test]
fn argv_composition_pr_only() {
    let result = run_argv_composition("42", "false", "false", "", "");
    assert_eq!(
        result,
        ArgvCapture {
            exit_code: 0,
            recorded_argv: "run pr-finalize --pr 42".to_string(),
        }
    );
}

#[test]
fn argv_composition_with_merge_flag() {
    let result = run_argv_composition("42", "true", "false", "", "");
    assert_eq!(
        result,
        ArgvCapture {
            exit_code: 0,
            recorded_argv: "run pr-finalize --pr 42 --merge".to_string(),
        }
    );
}

#[test]
fn argv_composition_with_merge_admin_flag() {
    let result = run_argv_composition("42", "false", "true", "", "");
    assert_eq!(
        result,
        ArgvCapture {
            exit_code: 0,
            recorded_argv: "run pr-finalize --pr 42 --merge-admin".to_string(),
        }
    );
}

#[test]
fn argv_composition_with_owner_and_repo() {
    let result =
        run_argv_composition("42", "false", "false", "andreas-pohl-parloa", "plan-executor");
    assert_eq!(
        result,
        ArgvCapture {
            exit_code: 0,
            recorded_argv:
                "run pr-finalize --pr 42 --owner andreas-pohl-parloa --repo plan-executor"
                    .to_string(),
        }
    );
}
