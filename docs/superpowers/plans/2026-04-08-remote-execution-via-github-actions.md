# Remote Execution via GitHub Actions — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add remote plan execution via GitHub Actions using a PR-per-execution model, a foreground execution mode, and a setup wizard.

**Architecture:** Plans declare `**execution:** remote` in their header. `plan-executor execute` routes to local (daemon) or remote (GitHub PR) execution based on this field. Remote execution creates a PR in a configured execution repo; a GitHub Actions workflow picks it up, clones the target repo, installs tooling, and runs `plan-executor execute -f` (new foreground mode) which reuses 100% of the existing handoff loop. A `remote-setup` wizard configures secrets.

**Tech Stack:** Rust (edition 2024), GitHub Actions, `gh` CLI

**Code Standards:** rust-services:production-code-recipe, rust-services:test-code-recipe

**Status:** READY

**non-interactive:** [x]

**Spec:** `docs/superpowers/specs/2026-04-08-remote-execution-via-github-actions-design.md`

---

## File Structure

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `src/plan.rs` | Add `ExecutionMode` enum and `parse_execution_mode()` |
| Modify | `src/lib.rs` | Export new `remote` module |
| Modify | `src/config.rs` | Add `remote_repo: Option<String>` field |
| Modify | `src/cli.rs` | Add `-f` flag to `Execute`, add `RemoteSetup` subcommand, foreground loop, remote trigger, remote status in `jobs` |
| Create | `src/remote.rs` | Remote execution logic: gather context, create branch/PR, query status |
| Modify | `src/daemon.rs` | Route remote plans through `remote::trigger_remote()` instead of local spawn |
| Modify | `src/main.rs` | Add `mod remote` |
| Create | `tests/plan_execution_mode_test.rs` | Tests for `parse_execution_mode()` |
| Create | `tests/config_remote_repo_test.rs` | Tests for `remote_repo` config field |
| Create | `tests/remote_test.rs` | Tests for `execution.json` generation and PR title formatting |

---

### Task 1: Add `ExecutionMode` to `src/plan.rs`

**Files:**
- Modify: `src/plan.rs:1-42`
- Test: `tests/plan_execution_mode_test.rs` (create)

- [ ] **Step 1: Write the failing tests**

Create `tests/plan_execution_mode_test.rs`:

```rust
use plan_executor::plan::{parse_execution_mode, ExecutionMode};
use tempfile::NamedTempFile;
use std::io::Write;

fn write_plan(content: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().unwrap();
    write!(f, "{}", content).unwrap();
    f
}

#[test]
fn test_parse_execution_mode_remote() {
    let f = write_plan("# Plan\n**execution:** remote\n**Status:** READY\n");
    assert_eq!(parse_execution_mode(f.path()), ExecutionMode::Remote);
}

#[test]
fn test_parse_execution_mode_local_explicit() {
    let f = write_plan("# Plan\n**execution:** local\n**Status:** READY\n");
    assert_eq!(parse_execution_mode(f.path()), ExecutionMode::Local);
}

#[test]
fn test_parse_execution_mode_missing_defaults_to_local() {
    let f = write_plan("# Plan\n**Status:** READY\n");
    assert_eq!(parse_execution_mode(f.path()), ExecutionMode::Local);
}

#[test]
fn test_parse_execution_mode_unknown_defaults_to_local() {
    let f = write_plan("# Plan\n**execution:** cloud\n");
    assert_eq!(parse_execution_mode(f.path()), ExecutionMode::Local);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test plan_execution_mode_test`
Expected: Compilation error — `ExecutionMode` and `parse_execution_mode` don't exist.

- [ ] **Step 3: Implement `ExecutionMode` and `parse_execution_mode`**

In `src/plan.rs`, add after line 19 (after `PlanStatus` enum):

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionMode {
    Local,
    Remote,
}
```

Add after the `parse_plan_status` function (after line 42):

```rust
/// Reads a plan file and extracts its **execution:** field.
/// Defaults to `ExecutionMode::Local` when absent or unrecognized.
pub fn parse_execution_mode(path: &Path) -> ExecutionMode {
    let Ok(content) = std::fs::read_to_string(path) else {
        return ExecutionMode::Local;
    };
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("**execution:**") {
            return match rest.trim() {
                "remote" => ExecutionMode::Remote,
                _ => ExecutionMode::Local,
            };
        }
    }
    ExecutionMode::Local
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test plan_execution_mode_test`
Expected: All 4 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/plan.rs tests/plan_execution_mode_test.rs
git commit -m "feat: add ExecutionMode enum and parse_execution_mode() to plan.rs"
```

---

### Task 2: Add `remote_repo` to `Config`

**Files:**
- Modify: `src/config.rs:52-64`
- Test: `tests/config_remote_repo_test.rs` (create)

- [ ] **Step 1: Write the failing tests**

Create `tests/config_remote_repo_test.rs`:

```rust
use plan_executor::config::Config;

#[test]
fn test_config_remote_repo_none_by_default() {
    let config = Config::default();
    assert!(config.remote_repo.is_none());
}

#[test]
fn test_config_remote_repo_from_json() {
    let json = r#"{
        "watch_dirs": ["~/workspace"],
        "plan_patterns": [".my/plans/*.md"],
        "auto_execute": false,
        "remote_repo": "owner/plan-executions"
    }"#;
    let config: Config = serde_json::from_str(json).unwrap();
    assert_eq!(config.remote_repo.as_deref(), Some("owner/plan-executions"));
}

#[test]
fn test_config_remote_repo_absent_in_json() {
    let json = r#"{
        "watch_dirs": ["~/workspace"],
        "plan_patterns": [".my/plans/*.md"],
        "auto_execute": false
    }"#;
    let config: Config = serde_json::from_str(json).unwrap();
    assert!(config.remote_repo.is_none());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test config_remote_repo_test`
Expected: Compilation error — `remote_repo` field doesn't exist on `Config`.

- [ ] **Step 3: Add `remote_repo` field to `Config`**

In `src/config.rs`, add to the `Config` struct (after line 63, the `agents` field):

```rust
    /// GitHub repo slug for remote execution (e.g. "owner/plan-executions").
    /// Set via `plan-executor remote-setup`.
    #[serde(default)]
    pub remote_repo: Option<String>,
```

No changes needed to `Default` impl — `Option<String>` defaults to `None` via `#[serde(default)]`, and the `Default` impl already sets explicit values for other fields. Add to the `Default` impl after `agents: AgentConfig::default(),`:

```rust
            remote_repo: None,
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test config_remote_repo_test`
Expected: All 3 tests PASS.

- [ ] **Step 5: Run all existing tests to check nothing broke**

Run: `cargo test`
Expected: All tests pass. The existing `test_config_serde_roundtrip` test uses a JSON without `remote_repo` — it still passes because the field has `#[serde(default)]`.

- [ ] **Step 6: Commit**

```bash
git add src/config.rs tests/config_remote_repo_test.rs
git commit -m "feat: add remote_repo config field for remote execution"
```

---

### Task 3: Add foreground mode (`execute -f`) to CLI

**Files:**
- Modify: `src/cli.rs:17-29` (Commands enum), `src/cli.rs:82-128` (run function), `src/cli.rs:257-273` (execute_plan function)

- [ ] **Step 1: Add `-f` flag to `Execute` command variant**

In `src/cli.rs`, change the `Execute` variant (lines 26-29) to:

```rust
    /// Execute a plan file or re-execute a job by ID prefix
    Execute {
        /// Plan file path or job ID prefix (from `plan-executor jobs`)
        plan: String,
        /// Run in foreground without the daemon
        #[arg(short = 'f', long)]
        foreground: bool,
    },
```

- [ ] **Step 2: Update all `Commands::Execute` match arms**

In `run()` at line 115, update:

```rust
        Commands::Execute { plan, foreground } => {
            if foreground {
                rt.block_on(execute_foreground(plan, config))
            } else {
                rt.block_on(execute_plan(plan, config))
            }
        }
```

- [ ] **Step 3: Implement `execute_foreground()`**

Add new function in `src/cli.rs`:

```rust
async fn execute_foreground(plan_path: String, config: crate::config::Config) -> Result<()> {
    use crate::executor::{spawn_execution, ExecEvent, find_state_file};
    use crate::handoff;
    use crate::jobs::JobMetadata;

    let resolved_path = std::fs::canonicalize(&plan_path)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&plan_path));
    if !resolved_path.exists() {
        anyhow::bail!("Plan file not found: {}", plan_path);
    }

    let execution_root = find_repo_root(&resolved_path)
        .unwrap_or_else(|| resolved_path.parent().unwrap_or(&resolved_path).to_path_buf());

    let job = JobMetadata::new(resolved_path.clone());
    let job_id = job.id.clone();

    let (mut child, _pgid, mut exec_rx) = spawn_execution(
        job, execution_root.clone(), &config.agents.main,
    )?;

    let mut last_display_blank = false;
    let mut final_status = None;

    'outer: loop {
        while let Some(event) = exec_rx.recv().await {
            match event {
                ExecEvent::OutputLine(_) => {} // raw output not shown in foreground
                ExecEvent::DisplayLine(line) => {
                    let is_blank = crate::executor::is_visually_blank(&line);
                    if is_blank && last_display_blank {
                        continue;
                    }
                    last_display_blank = is_blank;
                    print_display_line(&line);
                }
                ExecEvent::HandoffRequired { session_id, state_file } => {
                    let state_data = handoff::load_state(&state_file)?;

                    println!("⏺ [plan-executor] dispatching {} sub-agent(s) (phase: {})",
                        state_data.handoffs.len(), state_data.phase);

                    let (results, _pgids) = handoff::dispatch_all(
                        state_data.handoffs,
                        &config.agents.claude,
                        &config.agents.codex,
                        &config.agents.gemini,
                    ).await;

                    for r in &results {
                        if r.success {
                            println!("⏺ [plan-executor] sub-agent {} done ({} chars)",
                                r.index, r.stdout.len());
                        } else if r.can_fail {
                            println!("⏺ [plan-executor] sub-agent {} skipped (can-fail): {}",
                                r.index, r.stderr.lines().next().unwrap_or("(no stderr)"));
                        } else {
                            eprintln!("⏺ [plan-executor] sub-agent {} failed: {}",
                                r.index, r.stderr.lines().next().unwrap_or("(no stderr)"));
                        }
                    }

                    if results.iter().any(|r| !r.success && !r.can_fail) {
                        let _ = std::fs::remove_file(&state_file);
                        final_status = Some(false);
                        break 'outer;
                    }

                    let _ = std::fs::remove_file(&state_file);

                    println!("⏺ [plan-executor] resuming session {}",
                        &session_id[..session_id.len().min(16)]);

                    let continuation = handoff::build_continuation(&results);
                    match handoff::resume_execution(
                        &session_id,
                        &continuation,
                        execution_root.clone(),
                        Some(job_id.clone()),
                        Some(resolved_path.clone()),
                        &config.agents.main,
                    ).await {
                        Ok((new_child, _new_pgid, new_rx)) => {
                            child = new_child;
                            exec_rx = new_rx;
                            continue 'outer;
                        }
                        Err(e) => {
                            eprintln!("⏺ [plan-executor] failed to resume session: {}", e);
                            final_status = Some(false);
                            break 'outer;
                        }
                    }
                }
                ExecEvent::Finished(finished_job) => {
                    final_status = Some(finished_job.status == crate::jobs::JobStatus::Success);
                    break 'outer;
                }
            }
        }
        break;
    }

    let success = final_status.unwrap_or(false);
    if !success {
        std::process::exit(1);
    }
    Ok(())
}

/// Walk up from a path to find the closest directory containing `.git`.
fn find_repo_root(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut dir = if path.is_file() {
        path.parent()?.to_path_buf()
    } else {
        path.to_path_buf()
    };
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        dir = dir.parent()?.to_path_buf();
    }
}
```

Note: `find_repo_root` duplicates the one in `daemon.rs:741`. This is acceptable — the daemon version is not `pub` and the function is 8 lines. Extracting it to a shared module is not worth the coupling.

- [ ] **Step 4: Verify it compiles**

Run: `cargo build`
Expected: Compiles with zero errors. There may be an unused `child` warning — suppress with `let _ = child;` at the end of the function if needed.

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs
git commit -m "feat: add foreground execution mode (execute -f)"
```

---

### Task 4: Create `src/remote.rs` — execution metadata and PR creation

**Files:**
- Create: `src/remote.rs`
- Modify: `src/main.rs` (add `mod remote`)
- Modify: `src/lib.rs` (add `pub mod remote`)
- Test: `tests/remote_test.rs` (create)

- [ ] **Step 1: Write the failing tests**

Create `tests/remote_test.rs`:

```rust
use plan_executor::remote::{ExecutionMetadata, pr_title};

#[test]
fn test_pr_title_format() {
    let meta = ExecutionMetadata {
        target_repo: "owner/my-service".to_string(),
        target_ref: "abc123def456".to_string(),
        target_branch: "feat/cool".to_string(),
        plan_filename: "plan-add-feature.md".to_string(),
        started_at: "2026-04-08T14:30:00Z".to_string(),
    };
    assert_eq!(pr_title(&meta), "exec: plan-add-feature.md @ owner/my-service");
}

#[test]
fn test_execution_metadata_serialization() {
    let meta = ExecutionMetadata {
        target_repo: "owner/repo".to_string(),
        target_ref: "abc123".to_string(),
        target_branch: "main".to_string(),
        plan_filename: "plan-foo.md".to_string(),
        started_at: "2026-04-08T14:30:00Z".to_string(),
    };
    let json = serde_json::to_string_pretty(&meta).unwrap();
    let parsed: ExecutionMetadata = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.target_repo, "owner/repo");
    assert_eq!(parsed.target_ref, "abc123");
    assert_eq!(parsed.plan_filename, "plan-foo.md");
}

#[test]
fn test_branch_name_format() {
    let name = plan_executor::remote::branch_name("plan-add-feature.md", "2026-04-08T14:30:22Z");
    // Should be exec/<date-time>-<plan-stem>
    assert!(name.starts_with("exec/"));
    assert!(name.contains("plan-add-feature"));
    // No .md extension in branch name
    assert!(!name.ends_with(".md"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test remote_test`
Expected: Compilation error — `remote` module doesn't exist.

- [ ] **Step 3: Create `src/remote.rs`**

```rust
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use anyhow::{Context, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionMetadata {
    pub target_repo: String,
    pub target_ref: String,
    pub target_branch: String,
    pub plan_filename: String,
    pub started_at: String,
}

/// Formats the PR title for an execution.
pub fn pr_title(meta: &ExecutionMetadata) -> String {
    format!("exec: {} @ {}", meta.plan_filename, meta.target_repo)
}

/// Generates the branch name from the plan filename and ISO timestamp.
/// Format: `exec/<YYYYMMDD-HHMMSS>-<plan-stem>`
pub fn branch_name(plan_filename: &str, iso_timestamp: &str) -> String {
    let stem = Path::new(plan_filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(plan_filename);
    // Parse "2026-04-08T14:30:22Z" → "20260408-143022"
    let ts = iso_timestamp
        .replace('-', "")
        .replace(':', "")
        .replace('T', "-")
        .replace('Z', "");
    // Truncate to YYYYMMDD-HHMMSS (15 chars)
    let ts_short = &ts[..ts.len().min(15)];
    format!("exec/{}-{}", ts_short, stem)
}

/// Gathers git context from the current working directory.
/// Returns (owner/repo, HEAD SHA, branch name).
pub fn gather_git_context() -> Result<(String, String, String)> {
    let origin_url = run_git(&["remote", "get-url", "origin"])?;
    let repo_slug = parse_repo_slug(&origin_url)
        .context("Could not parse owner/repo from git remote URL")?;
    let head_sha = run_git(&["rev-parse", "HEAD"])?;
    let branch = run_git(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    Ok((repo_slug, head_sha, branch))
}

/// Extracts `owner/repo` from a git remote URL.
/// Supports HTTPS (`https://github.com/owner/repo.git`) and
/// SSH (`git@github.com:owner/repo.git`) formats.
fn parse_repo_slug(url: &str) -> Option<String> {
    let url = url.trim();
    if let Some(path) = url.strip_prefix("https://github.com/") {
        let slug = path.trim_end_matches(".git");
        Some(slug.to_string())
    } else if let Some(path) = url.strip_prefix("git@github.com:") {
        let slug = path.trim_end_matches(".git");
        Some(slug.to_string())
    } else {
        None
    }
}

/// Finds `.tmp-subtask-*.md` files co-located with the plan file.
pub fn find_prompt_files(plan_path: &Path) -> Vec<PathBuf> {
    let Some(dir) = plan_path.parent() else { return vec![] };
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(".tmp-subtask-") && name.ends_with(".md") {
                Some(e.path())
            } else {
                None
            }
        })
        .collect()
}

/// Pushes the Codex OAuth token to the execution repo secrets (idempotent).
pub fn push_codex_auth(remote_repo: &str) -> Result<()> {
    let auth_path = dirs::home_dir()
        .expect("home dir must exist")
        .join(".codex")
        .join("auth.json");
    if !auth_path.exists() {
        return Ok(()); // no auth file, skip
    }
    let content = std::fs::read_to_string(&auth_path)?;
    run_gh(&["secret", "set", "CODEX_AUTH", "--repo", remote_repo, "--body", &content])?;
    Ok(())
}

/// Creates a branch with plan files and execution metadata in the execution repo,
/// then opens a PR. Returns the PR URL.
pub fn trigger_remote_execution(
    remote_repo: &str,
    plan_path: &Path,
    meta: &ExecutionMetadata,
) -> Result<String> {
    let plan_content = std::fs::read_to_string(plan_path)?;
    let meta_json = serde_json::to_string_pretty(meta)?;
    let branch = branch_name(&meta.plan_filename, &meta.started_at);
    let title = pr_title(meta);
    let prompt_files = find_prompt_files(plan_path);

    // Create branch from main
    run_gh(&[
        "api", &format!("repos/{}/git/refs", remote_repo),
        "-X", "POST",
        "-f", &format!("ref=refs/heads/{}", branch),
        "-f", &format!("sha={}", get_main_sha(remote_repo)?),
    ])?;

    // Push execution.json
    push_file_to_branch(remote_repo, &branch, "execution.json", &meta_json)?;

    // Push plan.md
    push_file_to_branch(remote_repo, &branch, "plan.md", &plan_content)?;

    // Push prompt files
    for pf in &prompt_files {
        let name = pf.file_name().and_then(|n| n.to_str()).unwrap_or("prompt.md");
        let content = std::fs::read_to_string(pf)?;
        let dest = format!("prompt-files/{}", name);
        push_file_to_branch(remote_repo, &branch, &dest, &content)?;
    }

    // Create PR
    let pr_url = run_gh(&[
        "pr", "create",
        "--repo", remote_repo,
        "--head", &branch,
        "--title", &title,
        "--body", &format!(
            "## Remote Execution\n\n\
             **Target:** {repo}@{ref_short}\n\
             **Branch:** {branch}\n\
             **Plan:** {plan}\n\
             **Started:** {started}",
            repo = meta.target_repo,
            ref_short = &meta.target_ref[..meta.target_ref.len().min(12)],
            branch = meta.target_branch,
            plan = meta.plan_filename,
            started = meta.started_at,
        ),
    ])?;

    Ok(pr_url.trim().to_string())
}

/// Queries recent remote execution PRs from the execution repo.
/// Returns formatted lines for display.
pub fn list_remote_executions(remote_repo: &str) -> Result<Vec<RemoteJob>> {
    let output = run_gh(&[
        "pr", "list",
        "--repo", remote_repo,
        "--state", "all",
        "--limit", "20",
        "--json", "number,title,state,labels",
    ])?;
    let prs: Vec<serde_json::Value> = serde_json::from_str(&output)?;
    let mut jobs = Vec::new();
    for pr in prs {
        let number = pr["number"].as_u64().unwrap_or(0);
        let title = pr["title"].as_str().unwrap_or("");
        let state = pr["state"].as_str().unwrap_or("UNKNOWN");
        let labels: Vec<&str> = pr["labels"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|l| l["name"].as_str()).collect())
            .unwrap_or_default();

        // Parse title: "exec: plan-foo.md @ owner/repo"
        let (plan_name, target) = if let Some(rest) = title.strip_prefix("exec: ") {
            if let Some((plan, tgt)) = rest.split_once(" @ ") {
                (plan.to_string(), tgt.to_string())
            } else {
                (rest.to_string(), "?".to_string())
            }
        } else {
            (title.to_string(), "?".to_string())
        };

        let status = match state {
            "OPEN" => "running".to_string(),
            "CLOSED" | "MERGED" => {
                if labels.contains(&"succeeded") {
                    "succeeded".to_string()
                } else if labels.contains(&"failed") {
                    "failed".to_string()
                } else {
                    "closed".to_string()
                }
            }
            other => other.to_lowercase(),
        };

        jobs.push(RemoteJob { number, plan_name, status, target });
    }
    Ok(jobs)
}

#[derive(Debug)]
pub struct RemoteJob {
    pub number: u64,
    pub plan_name: String,
    pub status: String,
    pub target: String,
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn run_git(args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_gh(args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("gh")
        .args(args)
        .output()
        .context("failed to run gh — is the GitHub CLI installed and authenticated?")?;
    if !output.status.success() {
        anyhow::bail!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn get_main_sha(remote_repo: &str) -> Result<String> {
    let output = run_gh(&[
        "api", &format!("repos/{}/git/ref/heads/main", remote_repo),
        "--jq", ".object.sha",
    ])?;
    Ok(output.trim().to_string())
}

fn push_file_to_branch(repo: &str, branch: &str, path: &str, content: &str) -> Result<()> {
    // GitHub Contents API requires base64-encoded content
    let encoded = base64_encode(content.as_bytes());
    run_gh(&[
        "api", &format!("repos/{}/contents/{}", repo, path),
        "-X", "PUT",
        "-f", &format!("message=add {}", path),
        "-f", &format!("branch={}", branch),
        "-f", &format!("content={}", encoded),
    ]).map(|_| ())
}

fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}
```

- [ ] **Step 4: Register the module**

In `src/main.rs`, add after line 5 (`mod handoff;`):

```rust
mod remote;
```

In `src/lib.rs`, add after `pub mod handoff;`:

```rust
pub mod remote;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test remote_test`
Expected: All 3 tests PASS.

- [ ] **Step 6: Commit**

```bash
git add src/remote.rs src/main.rs src/lib.rs tests/remote_test.rs
git commit -m "feat: add remote execution module with metadata, PR creation, and status query"
```

---

### Task 5: Wire remote execution into `execute` command

**Files:**
- Modify: `src/cli.rs:257-273` (execute_plan function)

- [ ] **Step 1: Update `execute_plan` to check execution mode and route**

Replace the `execute_plan` function in `src/cli.rs`:

```rust
async fn execute_plan(plan_path: String, config: crate::config::Config) -> Result<()> {
    if !crate::ipc::socket_path().exists() {
        anyhow::bail!("Daemon not running. Start with: plan-executor daemon");
    }

    // If the argument looks like a job ID prefix, resolve it to a plan path.
    let resolved_path = resolve_plan_path(&plan_path);

    // Canonicalize to absolute path.
    let plan = std::fs::canonicalize(&resolved_path)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&resolved_path));
    if !plan.exists() {
        anyhow::bail!("Plan file not found: {}", resolved_path);
    }

    // Check execution mode
    if crate::plan::parse_execution_mode(&plan) == crate::plan::ExecutionMode::Remote {
        return trigger_remote(plan, config).await;
    }

    execute_via_daemon(plan, config).await
}
```

- [ ] **Step 2: Add `trigger_remote` function**

Add in `src/cli.rs`:

```rust
async fn trigger_remote(plan: PathBuf, config: crate::config::Config) -> Result<()> {
    let remote_repo = config.remote_repo.as_deref()
        .ok_or_else(|| anyhow::anyhow!(
            "remote execution requires 'remote_repo' in config — run 'plan-executor remote-setup'"
        ))?;

    let plan_filename = plan.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("plan.md")
        .to_string();

    let (target_repo, target_ref, target_branch) = crate::remote::gather_git_context()?;
    let started_at = chrono::Utc::now().to_rfc3339();

    let meta = crate::remote::ExecutionMetadata {
        target_repo,
        target_ref,
        target_branch,
        plan_filename,
        started_at,
    };

    // Push Codex OAuth token (idempotent, skips if no auth file)
    let _ = crate::remote::push_codex_auth(remote_repo);

    let pr_url = crate::remote::trigger_remote_execution(remote_repo, &plan, &meta)?;

    println!("Remote execution triggered.");
    println!("PR: {}", pr_url);

    Ok(())
}
```

- [ ] **Step 3: Also route foreground mode through remote check**

Update `execute_foreground` to check for remote mode at the top, before the foreground loop:

```rust
async fn execute_foreground(plan_path: String, config: crate::config::Config) -> Result<()> {
    use crate::executor::{spawn_execution, ExecEvent, find_state_file};
    use crate::handoff;
    use crate::jobs::JobMetadata;

    let resolved_path = std::fs::canonicalize(&plan_path)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&plan_path));
    if !resolved_path.exists() {
        anyhow::bail!("Plan file not found: {}", plan_path);
    }

    // Remote plans always trigger remotely, even with -f
    if crate::plan::parse_execution_mode(&resolved_path) == crate::plan::ExecutionMode::Remote {
        return trigger_remote(resolved_path, config).await;
    }

    // ... rest of foreground execution unchanged ...
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo build`
Expected: Compiles with zero errors.

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs
git commit -m "feat: route remote plans to GitHub PR trigger from execute command"
```

---

### Task 6: Wire remote execution into daemon auto-execute

**Files:**
- Modify: `src/daemon.rs:287-326` (trigger_execution function)

- [ ] **Step 1: Add remote routing to `trigger_execution`**

In `src/daemon.rs`, modify `trigger_execution` to check execution mode before spawning locally. Add after line 288 (`let plan = PathBuf::from(plan_path);`):

```rust
    // Route remote plans to GitHub PR trigger instead of local execution.
    if crate::plan::parse_execution_mode(&plan) == crate::plan::ExecutionMode::Remote {
        let config = { state.lock().await.config.clone() };
        if let Some(remote_repo) = config.remote_repo.as_deref() {
            let plan_filename = plan.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("plan.md")
                .to_string();
            // Gather git context from the plan's repo root
            let result = std::process::Command::new("git")
                .args(["-C", &plan.parent().unwrap_or(&plan).to_string_lossy(), "remote", "get-url", "origin"])
                .output();
            if let Ok(output) = result {
                if output.status.success() {
                    let origin = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    let head = std::process::Command::new("git")
                        .args(["-C", &plan.parent().unwrap_or(&plan).to_string_lossy(), "rev-parse", "HEAD"])
                        .output()
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                        .unwrap_or_default();
                    let branch = std::process::Command::new("git")
                        .args(["-C", &plan.parent().unwrap_or(&plan).to_string_lossy(), "rev-parse", "--abbrev-ref", "HEAD"])
                        .output()
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                        .unwrap_or_default();

                    let meta = crate::remote::ExecutionMetadata {
                        target_repo: origin.trim_start_matches("https://github.com/")
                            .trim_start_matches("git@github.com:")
                            .trim_end_matches(".git")
                            .to_string(),
                        target_ref: head,
                        target_branch: branch,
                        plan_filename,
                        started_at: chrono::Utc::now().to_rfc3339(),
                    };
                    let _ = crate::remote::push_codex_auth(remote_repo);
                    match crate::remote::trigger_remote_execution(remote_repo, &plan, &meta) {
                        Ok(url) => tracing::info!("remote execution triggered: {}", url),
                        Err(e) => tracing::error!("remote execution failed: {}", e),
                    }
                    // Remove from pending
                    let mut st = state.lock().await;
                    st.pending_plans.remove(plan_path);
                    let event = st.snapshot_state();
                    let _ = st.event_tx.send(event);
                    return;
                }
            }
            tracing::error!("remote execution: could not determine git origin for {}", plan_path);
        } else {
            tracing::error!("remote execution: remote_repo not configured");
        }
        return;
    }
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build`
Expected: Compiles with zero errors.

- [ ] **Step 3: Commit**

```bash
git add src/daemon.rs
git commit -m "feat: route remote plans in daemon auto-execute to GitHub PR trigger"
```

---

### Task 7: Add remote job status to `jobs` command

**Files:**
- Modify: `src/cli.rs:428-512` (list_jobs function)

- [ ] **Step 1: Add remote execution listing after local jobs**

In `src/cli.rs`, at the end of the `list_jobs` function (before the closing `}`), add:

```rust
    // Show remote executions if remote_repo is configured
    let config = crate::config::Config::load(None).ok();
    if let Some(remote_repo) = config.and_then(|c| c.remote_repo) {
        match crate::remote::list_remote_executions(&remote_repo) {
            Ok(remote_jobs) if !remote_jobs.is_empty() => {
                println!();
                println!("Remote ({}):", remote_repo);
                let pr_w = 6;
                let r_plan_w = 34;
                let r_status_w = 10;
                let target_w = 30;
                println!(
                    "{:<pr_w$}  {:<r_plan_w$}  {:<r_status_w$}  {}",
                    "PR", "PLAN", "STATUS", "TARGET",
                    pr_w = pr_w, r_plan_w = r_plan_w, r_status_w = r_status_w,
                );
                println!("{}", "─".repeat(pr_w + 2 + r_plan_w + 2 + r_status_w + 2 + target_w));
                for rj in &remote_jobs {
                    let plan_truncated = if rj.plan_name.len() > r_plan_w {
                        format!("{}…", &rj.plan_name[..r_plan_w - 1])
                    } else {
                        rj.plan_name.clone()
                    };
                    let target_truncated = if rj.target.len() > target_w {
                        format!("{}…", &rj.target[..target_w - 1])
                    } else {
                        rj.target.clone()
                    };
                    println!(
                        "#{:<width$}  {:<r_plan_w$}  {:<r_status_w$}  {}",
                        rj.number, plan_truncated, rj.status, target_truncated,
                        width = pr_w - 1, r_plan_w = r_plan_w, r_status_w = r_status_w,
                    );
                }
            }
            Ok(_) => {} // no remote jobs, don't print header
            Err(e) => {
                eprintln!("(could not fetch remote jobs: {})", e);
            }
        }
    }
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build`
Expected: Compiles with zero errors.

- [ ] **Step 3: Commit**

```bash
git add src/cli.rs
git commit -m "feat: show remote execution status in jobs command"
```

---

### Task 8: Add `remote-setup` CLI command

**Files:**
- Modify: `src/cli.rs:17-59` (Commands enum, run function)

- [ ] **Step 1: Add `RemoteSetup` variant to `Commands`**

In `src/cli.rs`, add to the `Commands` enum (after `Retry`):

```rust
    /// Interactive wizard to configure remote execution secrets
    RemoteSetup,
```

- [ ] **Step 2: Add synchronous handling in `run()`**

In the `run()` function, add `RemoteSetup` to the synchronous commands block (after the `Ensure` case, around line 86):

```rust
        Commands::RemoteSetup => { remote_setup(); return; }
```

Update the `unreachable!()` arm to include `RemoteSetup`:

```rust
        Commands::Stop | Commands::Jobs | Commands::Ensure | Commands::RemoteSetup
        | Commands::Kill { .. } | Commands::Pause { .. } | Commands::Unpause { .. } => unreachable!(),
```

- [ ] **Step 3: Implement `remote_setup()`**

Add in `src/cli.rs`:

```rust
fn remote_setup() {
    use std::io::{self, BufRead, Write};

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    // Check gh CLI
    if std::process::Command::new("gh").arg("--version").output().is_err() {
        eprintln!("Error: gh CLI not found. Install: https://cli.github.com");
        std::process::exit(1);
    }

    // Step 1: Execution repo
    let current_repo = crate::config::Config::load(None)
        .ok()
        .and_then(|c| c.remote_repo);
    let default_display = current_repo.as_deref().unwrap_or("owner/plan-executions");
    print!("Execution repo [{}]: ", default_display);
    let _ = stdout.flush();
    let mut repo_input = String::new();
    stdin.lock().read_line(&mut repo_input).unwrap();
    let repo_input = repo_input.trim();
    let remote_repo = if repo_input.is_empty() {
        default_display.to_string()
    } else {
        repo_input.to_string()
    };

    // Save to config
    match crate::config::Config::load(None) {
        Ok(mut config) => {
            config.remote_repo = Some(remote_repo.clone());
            let config_path = crate::config::Config::config_path();
            if let Ok(json) = serde_json::to_string_pretty(&config) {
                let _ = std::fs::write(&config_path, json);
                println!("  Saved to {}", config_path.display());
            }
        }
        Err(e) => {
            eprintln!("  Warning: could not update config: {}", e);
        }
    }

    // Step 2: GitHub PAT
    println!();
    println!("GitHub PAT for cloning org repos:");
    println!("  Create one at: https://github.com/settings/personal-access-tokens/new");
    println!("  Scope: your org, permission: Contents -> Read");
    print!("  Paste token: ");
    let _ = stdout.flush();
    let mut pat = String::new();
    stdin.lock().read_line(&mut pat).unwrap();
    let pat = pat.trim();
    if !pat.is_empty() {
        match gh_secret_set(&remote_repo, "TARGET_REPO_TOKEN", pat) {
            Ok(()) => println!("  Stored as TARGET_REPO_TOKEN"),
            Err(e) => eprintln!("  Error: {}", e),
        }
    } else {
        println!("  Skipped.");
    }

    // Step 3: Anthropic API key
    println!();
    print!("Anthropic API key: ");
    let _ = stdout.flush();
    let mut anthropic = String::new();
    stdin.lock().read_line(&mut anthropic).unwrap();
    let anthropic = anthropic.trim();
    if !anthropic.is_empty() {
        match gh_secret_set(&remote_repo, "ANTHROPIC_API_KEY", anthropic) {
            Ok(()) => println!("  Stored as ANTHROPIC_API_KEY"),
            Err(e) => eprintln!("  Error: {}", e),
        }
    } else {
        println!("  Skipped.");
    }

    // Step 4: Codex auth
    println!();
    print!("Codex auth — (o)auth / (a)pi key / (s)kip: ");
    let _ = stdout.flush();
    let mut codex_choice = String::new();
    stdin.lock().read_line(&mut codex_choice).unwrap();
    match codex_choice.trim() {
        "o" | "oauth" => {
            let auth_path = dirs::home_dir()
                .expect("home dir")
                .join(".codex")
                .join("auth.json");
            if auth_path.exists() {
                match std::fs::read_to_string(&auth_path) {
                    Ok(content) => {
                        println!("  Read {}", auth_path.display());
                        match gh_secret_set(&remote_repo, "CODEX_AUTH", &content) {
                            Ok(()) => println!("  Stored as CODEX_AUTH"),
                            Err(e) => eprintln!("  Error: {}", e),
                        }
                    }
                    Err(e) => eprintln!("  Error reading {}: {}", auth_path.display(), e),
                }
            } else {
                eprintln!("  {} not found. Run codex login first.", auth_path.display());
            }
        }
        "a" | "api" => {
            print!("  OpenAI API key: ");
            let _ = stdout.flush();
            let mut openai = String::new();
            stdin.lock().read_line(&mut openai).unwrap();
            let openai = openai.trim();
            if !openai.is_empty() {
                match gh_secret_set(&remote_repo, "OPENAI_API_KEY", openai) {
                    Ok(()) => println!("  Stored as OPENAI_API_KEY"),
                    Err(e) => eprintln!("  Error: {}", e),
                }
            }
        }
        _ => println!("  Skipped."),
    }

    // Step 5: Gemini API key
    println!();
    print!("Gemini API key (enter to skip): ");
    let _ = stdout.flush();
    let mut gemini = String::new();
    stdin.lock().read_line(&mut gemini).unwrap();
    let gemini = gemini.trim();
    if !gemini.is_empty() {
        match gh_secret_set(&remote_repo, "GEMINI_API_KEY", gemini) {
            Ok(()) => println!("  Stored as GEMINI_API_KEY"),
            Err(e) => eprintln!("  Error: {}", e),
        }
    } else {
        println!("  Skipped.");
    }

    println!();
    println!("Setup complete. Remote execution ready.");
}

fn gh_secret_set(repo: &str, name: &str, value: &str) -> Result<()> {
    let output = std::process::Command::new("gh")
        .args(["secret", "set", name, "--repo", repo, "--body", value])
        .output()
        .context("failed to run gh")?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo build`
Expected: Compiles with zero errors.

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs
git commit -m "feat: add remote-setup wizard for configuring execution repo secrets"
```

---

### Task 9: Create GitHub Actions workflow file

**Files:**
- Create: `.github/workflows/execute-plan.yml` (this goes in the execution repo, but we create a template here)

- [ ] **Step 1: Create the workflow template**

Create `docs/remote-execution/execute-plan.yml`:

```yaml
name: Execute Plan

on:
  pull_request:
    types: [opened]
    branches: ['exec/**']

permissions:
  contents: read
  pull-requests: write

jobs:
  execute:
    runs-on: ubuntu-latest
    timeout-minutes: 120

    steps:
      - name: Checkout execution repo
        uses: actions/checkout@v4

      - name: Parse execution metadata
        id: meta
        run: |
          TARGET_REPO=$(jq -r '.target_repo' execution.json)
          TARGET_REF=$(jq -r '.target_ref' execution.json)
          TARGET_BRANCH=$(jq -r '.target_branch' execution.json)
          PLAN_FILENAME=$(jq -r '.plan_filename' execution.json)
          echo "target_repo=${TARGET_REPO}" >> "$GITHUB_OUTPUT"
          echo "target_ref=${TARGET_REF}" >> "$GITHUB_OUTPUT"
          echo "target_branch=${TARGET_BRANCH}" >> "$GITHUB_OUTPUT"
          echo "plan_filename=${PLAN_FILENAME}" >> "$GITHUB_OUTPUT"

      - name: Clone target repo
        run: |
          git clone "https://x-access-token:${{ secrets.TARGET_REPO_TOKEN }}@github.com/${{ steps.meta.outputs.target_repo }}.git" workspace
          cd workspace
          git checkout "${{ steps.meta.outputs.target_ref }}"

      - name: Setup Rust
        uses: dtolnay/rust-toolchain@stable

      - name: Setup Node.js
        uses: actions/setup-node@v4
        with:
          node-version: '22'

      - name: Install Claude CLI
        run: |
          npm install -g @anthropic-ai/claude-code

      - name: Install Codex CLI
        run: |
          npm install -g @openai/codex

      - name: Install Gemini CLI
        run: |
          npm install -g @anthropic-ai/gemini-cli || true

      - name: Install my-coding plugin
        run: |
          bash -c "$(curl -fsSL https://raw.githubusercontent.com/andreas-pohl-parloa/my-coding/main/install.sh)" || \
          bash -c "$(gh api 'repos/andreas-pohl-parloa/my-coding/contents/install.sh' --header 'Accept: application/vnd.github.raw')"
        env:
          GH_TOKEN: ${{ secrets.TARGET_REPO_TOKEN }}

      - name: Restore agent auth
        run: |
          if [ -n "$CODEX_AUTH" ]; then
            mkdir -p ~/.codex
            echo "$CODEX_AUTH" > ~/.codex/auth.json
          fi
        env:
          CODEX_AUTH: ${{ secrets.CODEX_AUTH }}

      - name: Copy plan into target repo
        run: |
          PLAN_DIR="workspace/.my/plans"
          mkdir -p "$PLAN_DIR"
          cp plan.md "$PLAN_DIR/${{ steps.meta.outputs.plan_filename }}"
          if [ -d prompt-files ]; then
            cp prompt-files/* "$PLAN_DIR/" 2>/dev/null || true
          fi

      - name: Execute plan
        id: execute
        run: |
          cd workspace
          plan-executor execute -f ".my/plans/${{ steps.meta.outputs.plan_filename }}"
        env:
          ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
          OPENAI_API_KEY: ${{ secrets.OPENAI_API_KEY }}
          GEMINI_API_KEY: ${{ secrets.GEMINI_API_KEY }}

      - name: Post result comment
        if: always()
        run: |
          if [ "${{ steps.execute.outcome }}" = "success" ]; then
            STATUS="succeeded"
          else
            STATUS="failed"
          fi
          REF_SHORT=$(echo "${{ steps.meta.outputs.target_ref }}" | cut -c1-12)
          gh pr comment "${{ github.event.pull_request.number }}" \
            --repo "${{ github.repository }}" \
            --body "## Execution Complete

          **Status:** ${STATUS}
          **Target:** ${{ steps.meta.outputs.target_repo }}@${REF_SHORT}
          **Branch:** ${{ steps.meta.outputs.target_branch }}
          **Plan:** ${{ steps.meta.outputs.plan_filename }}"

          gh pr close "${{ github.event.pull_request.number }}" \
            --repo "${{ github.repository }}" || true
          gh pr edit "${{ github.event.pull_request.number }}" \
            --repo "${{ github.repository }}" \
            --add-label "${STATUS}" || true
        env:
          GH_TOKEN: ${{ github.token }}
```

- [ ] **Step 2: Commit**

```bash
mkdir -p docs/remote-execution
git add docs/remote-execution/execute-plan.yml
git commit -m "feat: add GitHub Actions workflow template for remote execution"
```

---

### Task 10: Final integration test and cleanup

**Files:**
- All modified files

- [ ] **Step 1: Run the full test suite**

Run: `cargo test`
Expected: All tests pass, including the new ones from Tasks 1, 2, and 4.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy -- -D warnings`
Expected: No warnings.

- [ ] **Step 3: Fix any warnings or issues found in steps 1-2**

Apply fixes as needed. Common issues:
- Unused imports in `cli.rs` (may need `#[allow(unused)]` for `child` variable in foreground mode)
- Missing `use` statements for `anyhow::Context` in `cli.rs`

- [ ] **Step 4: Verify the binary runs**

Run: `cargo run -- --help`
Expected: Shows help with `execute` (including `-f` flag), `remote-setup`, and all existing commands.

Run: `cargo run -- remote-setup --help`
Expected: Shows help for the remote-setup command.

- [ ] **Step 5: Commit any fixes**

```bash
git add -A
git commit -m "chore: fix clippy warnings and finalize remote execution integration"
```

---

## Acceptance Criteria

- [ ] `**execution:** remote` header is parsed correctly; absent/unknown defaults to `local`
- [ ] `remote_repo` config field is optional, deserialized correctly, absent defaults to `None`
- [ ] `plan-executor execute -f <plan>` runs the full handoff loop without the daemon
- [ ] `plan-executor execute <plan>` with `**execution:** remote` creates a PR in the execution repo
- [ ] Daemon auto-execute routes remote plans to GitHub PR trigger
- [ ] `plan-executor jobs` shows remote executions when `remote_repo` is configured
- [ ] `plan-executor remote-setup` walks through secrets configuration
- [ ] GitHub Actions workflow template is complete and functional
- [ ] All tests pass, clippy clean
