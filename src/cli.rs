use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "plan-executor",
    about = "Monitor and execute Claude plan files"
)]
pub struct Cli {
    /// Path to config file. Default: ~/.plan-executor/config.json
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<std::path::PathBuf>,
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start the background daemon (detaches from terminal)
    Daemon {
        /// Run in the foreground without daemonizing.
        /// Use this when managed by launchd or another supervisor.
        #[arg(long)]
        foreground: bool,
    },
    /// Execute a compiled plan manifest. Pass tasks.json or its containing directory.
    /// (To compile a plan markdown into a manifest, use the `plan-executor:handover`
    /// + `plan-executor:compile-plan` skills from a Claude session.)
    Execute {
        /// Path to tasks.json or the manifest directory containing it
        manifest: String,
        /// Run in foreground without the daemon
        #[arg(short = 'f', long)]
        foreground: bool,
    },
    /// Stop the running daemon
    Stop,
    /// Start the daemon if it is not already running (used by shell hook)
    Ensure,
    /// Show daemon status
    Status,
    /// List, show, cancel, gc, or replay job records.
    Jobs {
        #[command(subcommand)]
        command: Option<JobsCommand>,
    },
    /// Kill a running job by job ID (prefix match)
    Kill { job_id: String },
    /// Pause a running job at the next handoff
    Pause { job_id: String },
    /// Resume a paused job
    Unpause { job_id: String },
    /// Show output of a job; use -f to follow a running job
    Output {
        /// Job ID prefix (from `plan-executor jobs`)
        job_id: String,
        /// Follow live output of a running job
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// Interactive wizard to configure remote execution secrets
    RemoteSetup,
    /// Validate a compiled tasks.json manifest against the schema and semantic rules.
    Validate {
        /// Path to tasks.json
        tasks_json: PathBuf,
    },
    /// Append fix waves to an existing tasks.json from reviewer findings.
    CompileFixWaves {
        /// Absolute path to the plan markdown file. MUST equal manifest.plan.path; the CLI hard-fails on mismatch.
        #[arg(long)]
        plan: PathBuf,
        /// Directory containing tasks.json. Manifest is read from `<execution_root>/tasks.json`.
        #[arg(long = "execution-root")]
        execution_root: PathBuf,
        /// Absolute path to the findings.json file (conforms to findings.schema.json).
        #[arg(long = "findings-json")]
        findings_json: PathBuf,
    },
    /// Run a standalone framework job (Phase C / Phase D entry point).
    Run {
        #[command(subcommand)]
        command: RunCommand,
    },
}

/// Subcommands for the `plan-executor run` group.
///
/// Each variant constructs and persists a [`crate::job::types::Job`] via
/// [`crate::job::storage::JobStore`] — the same storage pathway used by
/// `JobKind::Plan` in Phase A. The dispatch loop (running the steps) is
/// owned by the daemon and is added in Phase D; this CLI surface only
/// needs to materialize a valid `job.json` on disk.
#[derive(clap::Subcommand, Debug, Clone)]
pub enum RunCommand {
    /// Finalize a PR: lookup → mark-ready → monitor → merge (optional) → report.
    PrFinalize {
        /// Pull request number.
        #[arg(long)]
        pr: u32,
        /// Repository owner. Defaults to the `owner.login` reported by
        /// `gh repo view --json owner,name` in the current directory.
        #[arg(long)]
        owner: Option<String>,
        /// Repository name. Defaults to the `name` reported by
        /// `gh repo view --json owner,name` in the current directory.
        #[arg(long)]
        repo: Option<String>,
        /// Run `gh pr merge` after monitor succeeds. Mutually exclusive
        /// with `--merge-admin`.
        #[arg(long, conflicts_with = "merge_admin")]
        merge: bool,
        /// Run `gh pr merge --admin` after monitor succeeds (bypasses
        /// required-reviewer checks). Mutually exclusive with `--merge`.
        #[arg(long = "merge-admin")]
        merge_admin: bool,
        /// Dispatch to the configured execution repo via GitHub Actions
        /// instead of running locally. Requires `remote_repo` in
        /// `~/.plan-executor/config.json` (run `plan-executor remote-setup`
        /// first). Pushes a `job-spec.json` to a new `exec/` branch and
        /// opens a PR there; the GHA workflow runs the actual finalize.
        #[arg(long)]
        remote: bool,
    },
}

/// Subcommands for the `plan-executor jobs` group.
#[derive(clap::Subcommand, Debug, Clone)]
pub enum JobsCommand {
    /// List all jobs (new and legacy layouts). Default when no subcommand.
    List,
    /// Show full step/attempt history for a job (id prefix match).
    Show {
        /// Full or prefix-matched job id.
        id: String,
    },
    /// Mark a job as cancelled (new layout only; running daemon jobs use `kill`).
    Cancel {
        /// Full or prefix-matched job id.
        id: String,
    },
    /// Garbage-collect completed jobs older than the given duration.
    Gc {
        /// Duration like "7d", "24h", "30m". Default: "30d".
        #[arg(long)]
        older_than: Option<String>,
    },
    /// Replay a job from step N. Phase A: stub; full impl in Phase D.
    Replay {
        /// Full or prefix-matched job id.
        id: String,
        /// Step seq to start replay from (1-based).
        #[arg(long)]
        from_step: Option<u32>,
    },
    /// Aggregate persisted job metrics across the store.
    Metrics(crate::job::cli::MetricsArgs),
}

/// Prints a display line to the terminal, coloring plan-executor prefix lines
/// the same way the TUI does (yellow prefix, red for failures; green ⏺ bullet).
fn print_display_line(line: &str) {
    if let Some(rest) = line.strip_prefix("⏺ [plan-executor]") {
        let is_failure = rest.contains("failed");
        if is_failure {
            println!("\x1b[31m⏺ [plan-executor]{}\x1b[0m", rest);
        } else {
            println!("\x1b[33m⏺ [plan-executor]\x1b[0m{}", rest);
        }
    } else if let Some(rest) = line.strip_prefix("⏺") {
        println!("\x1b[32m⏺\x1b[0m{}", rest);
    } else {
        println!("{}", line);
    }
}

/// Dim-indented prefix used to nest one sub-agent's output under the
/// main agent's display. The agent index is embedded in the prefix so
/// that concurrent sub-agents (common on reviewer-team batches) are
/// visually distinguishable at a glance.
fn subagent_prefix(index: usize) -> String {
    format!("\x1b[2m│{}│ \x1b[0m", index)
}

/// Renders one sub-agent's persisted JSONL output via sjv and prints
/// each resulting visible line with an indent prefix that carries the
/// sub-agent index. Best-effort: if no file is found or reading fails,
/// the function silently skips (sub-agent output is optional context,
/// not critical signal).
fn render_subagent_output(job_id: &str, dispatch: u32, index: usize) {
    use std::path::PathBuf;
    let base: PathBuf = crate::config::Config::base_dir()
        .join("jobs")
        .join(job_id)
        .join("sub-agents");
    let Ok(entries) = std::fs::read_dir(&base) else {
        return;
    };

    let prefix = subagent_prefix(index);
    let prefix_stdout = format!("dispatch-{}-agent-{}-", dispatch, index);
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(&prefix_stdout) {
            continue;
        }
        let path = entry.path();
        let is_stderr = path.extension().and_then(|s| s.to_str()) == Some("stderr");
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let header = if is_stderr {
            format!("{}\x1b[2m─── sub-agent {} stderr ───\x1b[0m", prefix, index,)
        } else {
            format!("{}\x1b[2m─── sub-agent {} output ───\x1b[0m", prefix, index,)
        };
        println!("{}", header);
        for raw_line in content.lines() {
            if raw_line.is_empty() {
                continue;
            }
            let rendered = if is_stderr {
                // stderr is plain text — print raw.
                format!("\x1b[2m{}\x1b[0m", raw_line)
            } else {
                // stdout is JSONL from a streaming agent — run it
                // through sjv, same as the main agent.
                sjv::render_runtime_line(raw_line, false, true)
            };
            for visual in rendered.lines() {
                if visual.is_empty() {
                    continue;
                }
                println!("{}{}", prefix, visual);
            }
        }
    }
}

fn terminal_width() -> usize {
    #[cfg(unix)]
    {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0
            && ws.ws_col > 0
        {
            return ws.ws_col as usize;
        }
    }
    80 // fallback
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}…", &s[..max.saturating_sub(1)])
    } else {
        s.to_string()
    }
}

/// Lists `(pid, command)` pairs for every process in `pgid`. Uses
/// `ps -g <pgid> -o pid=,command=` which works on both macOS and Linux.
/// Returns an empty vec on any ps failure.
fn processes_in_pgid(pgid: u32) -> Vec<(u32, String)> {
    let output = std::process::Command::new("ps")
        .args(["-o", "pid=,command=", "-g", &pgid.to_string()])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| {
            let trimmed = l.trim_start();
            let (pid_str, cmd) = trimmed.split_once(char::is_whitespace)?;
            let pid = pid_str.parse::<u32>().ok()?;
            Some((pid, cmd.trim().to_string()))
        })
        .collect()
}

/// `pub`-callable wrapper that auto-resolves the terminal width before
/// delegating to `render_job_process_tree`. Used by `jobs list` to print a
/// sub-process tree under each running job without exposing the column
/// math to the caller.
pub fn render_job_process_tree_compat(procs: &crate::ipc::JobProcesses) {
    render_job_process_tree(procs, terminal_width());
}

/// Prints a dimmed sub-tree of process groups and PIDs under a running
/// job. Emits nothing if the job has no tracked pgids or no live processes.
fn render_job_process_tree(procs: &crate::ipc::JobProcesses, term_w: usize) {
    const DIM: &str = "\x1b[2m";
    const RESET: &str = "\x1b[0m";

    let mut groups: Vec<(&str, u32)> = Vec::new();
    if let Some(pgid) = procs.main_pgid {
        groups.push(("main", pgid));
    }
    for pgid in &procs.sub_agent_pgids {
        groups.push(("sub-agent", *pgid));
    }
    if groups.is_empty() {
        return;
    }

    let resolved: Vec<(&str, u32, Vec<(u32, String)>)> = groups
        .into_iter()
        .map(|(label, pgid)| (label, pgid, processes_in_pgid(pgid)))
        .filter(|(_, _, procs)| !procs.is_empty())
        .collect();
    if resolved.is_empty() {
        return;
    }

    for (gi, (label, pgid, processes)) in resolved.iter().enumerate() {
        let is_last_group = gi == resolved.len() - 1;
        let group_branch = if is_last_group { "└─" } else { "├─" };
        println!(
            "{}  {} {} pgroup {}{}",
            DIM, group_branch, label, pgid, RESET
        );
        let spine = if is_last_group { " " } else { "│" };
        for (pi, (pid, cmd)) in processes.iter().enumerate() {
            let is_last_proc = pi == processes.len() - 1;
            let proc_branch = if is_last_proc { "└─" } else { "├─" };
            // 2 spaces + spine + 2 spaces + branch (2) + space + pid + space
            let pid_str = pid.to_string();
            let fixed = 2 + 1 + 2 + 2 + 1 + pid_str.len() + 1;
            let max_cmd = term_w.saturating_sub(fixed).max(20);
            let cmd_display = truncate_str(cmd, max_cmd);
            println!(
                "{}  {}  {} {} {}{}",
                DIM, spine, proc_branch, pid_str, cmd_display, RESET
            );
        }
    }
}

/// Resolve a user-supplied execute argument into an absolute `tasks.json` path.
/// Accepts either a file path ending in `tasks.json` or a directory containing one.
pub(crate) fn resolve_manifest_path(arg: &str) -> Result<PathBuf> {
    let raw = PathBuf::from(arg);
    let absolute = if raw.is_absolute() {
        raw
    } else {
        let cwd = std::env::current_dir()
            .with_context(|| "could not determine current working directory")?;
        cwd.join(&raw)
    };
    let resolved = std::fs::canonicalize(&absolute).unwrap_or(absolute);
    if resolved.is_file() && resolved.file_name().and_then(|n| n.to_str()) == Some("tasks.json") {
        return Ok(resolved);
    }
    if resolved.is_dir() {
        let candidate = resolved.join("tasks.json");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("Manifest not found at {}; expected tasks.json", arg)
}

/// Read `plan.path`, `plan.status`, and `plan.execution_mode` from a compiled
/// manifest. `execution_mode` defaults to `"local"` when absent so manifests
/// compiled before the field was added still load.
pub(crate) fn read_manifest_plan_block(
    manifest_path: &Path,
) -> Result<(PathBuf, String, String)> {
    let raw = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("read manifest {}", manifest_path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse manifest {}", manifest_path.display()))?;
    let plan_path = v
        .pointer("/plan/path")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow::anyhow!("manifest {} missing plan.path", manifest_path.display()))?
        .to_string();
    let status = v
        .pointer("/plan/status")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow::anyhow!("manifest {} missing plan.status", manifest_path.display()))?
        .to_string();
    let execution_mode = v
        .pointer("/plan/execution_mode")
        .and_then(|x| x.as_str())
        .unwrap_or("local")
        .to_string();
    Ok((PathBuf::from(plan_path), status, execution_mode))
}

fn run_validate(tasks_json: &std::path::Path) {
    let raw = match std::fs::read_to_string(tasks_json) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ERROR: cannot read {}: {}", tasks_json.display(), e);
            std::process::exit(1);
        }
    };
    let manifest: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ERROR: {} is not valid JSON: {}", tasks_json.display(), e);
            std::process::exit(1);
        }
    };

    let schema_errors = crate::schema::validate_manifest(&manifest)
        .err()
        .unwrap_or_default();
    let manifest_dir = tasks_json
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let semantic_errors = crate::validate::semantic_check(&manifest, manifest_dir)
        .err()
        .unwrap_or_default();

    if schema_errors.is_empty() && semantic_errors.is_empty() {
        println!("VALID: {}", tasks_json.display());
        return;
    }

    for e in &schema_errors {
        eprintln!("ERROR: schema: {} (at {})", e.message, e.path);
    }
    for e in &semantic_errors {
        eprintln!("ERROR: {}: {}", e.category, e.message);
    }
    std::process::exit(1);
}

fn run_compile_fix_waves(plan: &Path, execution_root: &Path, findings_json: &Path) {
    if let Err(e) = run_compile_fix_waves_with_invoker(
        &crate::compile::ClaudeInvoker,
        plan,
        execution_root,
        findings_json,
    ) {
        eprintln!("ERROR: {}", e);
        std::process::exit(1);
    }
}

/// Test-injectable variant. Production calls this with `&ClaudeInvoker`.
pub(crate) fn run_compile_fix_waves_with_invoker(
    invoker: &dyn crate::compile::CompileInvoker,
    plan: &Path,
    execution_root: &Path,
    findings_json: &Path,
) -> anyhow::Result<()> {
    use crate::finding::FindingsFile;
    use anyhow::Context;

    // Load and parse findings file.
    let findings_file = FindingsFile::from_path(findings_json)
        .with_context(|| format!("read findings {}", findings_json.display()))?;

    // Resolve manifest path.
    let manifest_path = execution_root.join("tasks.json");
    if !manifest_path.is_file() {
        anyhow::bail!(
            "manifest not found at {}; expected tasks.json in execution_root",
            manifest_path.display()
        );
    }

    // Cross-check plan path against manifest (hard-fail on explicit mismatch).
    // If the manifest can't be read or parsed here, fall through — the actual
    // `read_capped` in `append_fix_waves` will surface that error.
    //
    // Cap the preflight read at 16 MiB to match the production cap enforced
    // inside `append_fix_waves::read_capped`. Without this, a symlinked or
    // maliciously-crafted oversized manifest could OOM the CLI before append
    // begins. Over-cap manifests fall through to `append_fix_waves`, which
    // surfaces a definitive `FileTooLarge` error.
    const MANIFEST_PREFLIGHT_CAP_BYTES: u64 = 16 * 1024 * 1024;
    let raw = match std::fs::metadata(&manifest_path) {
        Ok(meta) if meta.len() <= MANIFEST_PREFLIGHT_CAP_BYTES => {
            std::fs::read_to_string(&manifest_path).ok()
        }
        _ => None,
    };
    if let Some(raw) = raw {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(manifest_plan_path) = v.pointer("/plan/path").and_then(|x| x.as_str()) {
                if std::path::PathBuf::from(manifest_plan_path) != plan {
                    anyhow::bail!(
                        "--plan {} disagrees with manifest.plan.path {}; either pass --plan matching the manifest, or re-derive the manifest first",
                        plan.display(),
                        manifest_plan_path
                    );
                }
            }
        }
    }

    // Invoke the actual append.
    let updated = crate::compile::append_fix_waves_with_invoker(
        invoker,
        &manifest_path,
        &findings_file.findings,
    )
    .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Stdout contract: single line with the updated manifest path.
    println!("{}", updated.display());
    Ok(())
}

/// Resolve `(owner, repo)` for the current working directory by invoking
/// `gh repo view --json owner,name`. Returns `None` if `gh` is missing,
/// the command fails, or the JSON shape is unexpected.
///
/// We deliberately use `gh` (not `git remote get-url`) because gh respects
/// the user's auth context and `gh-resolved` remote settings, which matters
/// when the local clone has multiple remotes (fork + upstream). This matches
/// the recommendation in the C1.2 task description.
fn detect_owner_repo_via_gh() -> Option<(String, String)> {
    let output = std::process::Command::new("gh")
        .args(["repo", "view", "--json", "owner,name"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let owner = v
        .pointer("/owner/login")
        .and_then(|x| x.as_str())?
        .to_string();
    let name = v.pointer("/name").and_then(|x| x.as_str())?.to_string();
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some((owner, name))
}

/// Build a [`crate::job::types::Job`] for the standalone `pr-finalize`
/// CLI subcommand and persist it via [`crate::job::storage::JobStore`].
///
/// Steps are sourced from [`crate::job::registry::steps_for`] so this CLI
/// surface stays in sync with the registry's step list. Each
/// `Box<dyn Step>` is projected into a [`crate::job::types::StepInstance`]
/// (`status: Pending`, no attempts yet) before persisting; the daemon
/// dispatch loop (Phase D) reads `job.json` back and re-hydrates the
/// runtime steps via the registry.
///
/// # Errors
///
/// Returns an error when `gh repo view` cannot resolve the repo (and the
/// caller did not pass `--owner`/`--repo`), when the job store cannot be
/// opened, when persisting `job.json` fails, or when the synchronous
/// pipeline reports a non-success outcome on any step.
async fn run_pr_finalize(
    pr: u32,
    owner: Option<String>,
    repo: Option<String>,
    merge: bool,
    merge_admin: bool,
) -> Result<()> {
    use crate::job::registry;
    use crate::job::storage::JobStore;
    use crate::job::types::{
        Job, JobId, JobKind, JobState, MergeMode as WireMergeMode, StepInstance, StepStatus,
    };

    // Resolve owner/repo: prefer explicit CLI args, fall back to gh detection.
    let (resolved_owner, resolved_repo) = match (owner, repo) {
        (Some(o), Some(r)) => (o, r),
        (o_opt, r_opt) => {
            let detected = detect_owner_repo_via_gh().ok_or_else(|| {
                anyhow::anyhow!(
                    "could not detect owner/repo from `gh repo view` — pass --owner and --repo explicitly, or run from a directory with a configured gh remote"
                )
            })?;
            (o_opt.unwrap_or(detected.0), r_opt.unwrap_or(detected.1))
        }
    };

    // Defense-in-depth: validate owner/repo charset against the same
    // shape the GHA workflow enforces (`^[A-Za-z0-9._-]+$`). Reuses
    // `crate::remote::validate_repo_slug`, which checks both halves of the
    // slug and rejects `..`, slashes, or other injection-prone characters.
    let combined_slug = format!("{resolved_owner}/{resolved_repo}");
    if !crate::remote::validate_repo_slug(&combined_slug) {
        anyhow::bail!(
            "invalid owner/repo: '{combined_slug}' — must match ^[A-Za-z0-9._-]+$ for both owner and repo"
        );
    }

    // Clap's `conflicts_with` already rejects `--merge` + `--merge-admin`,
    // but a defensive check here makes the precondition explicit and
    // shields the registry against future plumbing changes.
    let merge_mode = match (merge, merge_admin) {
        (false, false) => WireMergeMode::None,
        (true, false) => WireMergeMode::Merge,
        (false, true) => WireMergeMode::MergeAdmin,
        (true, true) => {
            anyhow::bail!("--merge and --merge-admin are mutually exclusive")
        }
    };

    let kind = JobKind::PrFinalize {
        owner: resolved_owner,
        repo: resolved_repo,
        pr,
        merge_mode,
    };

    let runtime_steps = registry::steps_for(&kind);
    let step_instances: Vec<StepInstance> = runtime_steps
        .iter()
        .enumerate()
        .map(|(idx, step)| {
            let seq = u32::try_from(idx + 1).unwrap_or(u32::MAX);
            StepInstance {
                seq,
                name: step.name().to_string(),
                status: StepStatus::Pending,
                attempts: Vec::new(),
                idempotent: step.idempotent(),
            }
        })
        .collect();

    let job = Job {
        id: JobId(uuid::Uuid::new_v4().to_string()),
        kind,
        state: JobState::Running,
        created_at: chrono::Utc::now().to_rfc3339(),
        steps: step_instances,
    };

    let store = JobStore::new().context("opening job store")?;
    let dir = store.create(&job).context("persisting job.json")?;

    let short_id = &job.id.0[..job.id.0.len().min(8)];
    println!(
        "Running pr-finalize job (id {}) at {}",
        short_id,
        dir.path().display()
    );

    // Run the 5-step pipeline synchronously. The daemon's dispatcher only
    // hydrates `JobKind::Plan` today, so deferring to the daemon would
    // dead-letter the job. Workdir is the current dir — gh subprocesses
    // resolve owner/repo via the args we already validated above, not via
    // a git remote inside workdir.
    let workdir = std::env::current_dir().context("resolving current working directory")?;
    let success =
        run_rust_scheduler_pipeline(runtime_steps, dir.path().to_path_buf(), workdir).await;
    if !success {
        anyhow::bail!("pr-finalize pipeline failed (see job dir for per-step logs)");
    }
    println!("pr-finalize job {short_id} succeeded");
    Ok(())
}

/// Run a `gh` invocation, returning trimmed stdout. Wraps spawn failures and
/// non-zero exits into `anyhow::Error` so `?` propagates cleanly.
fn run_gh_capture(args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("gh")
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run gh: {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Dispatch a `pr-finalize` job to the configured remote execution repo
/// instead of running it locally.
///
/// Pushes a single `job-spec.json` file to a fresh `exec/pr-finalize-…`
/// branch on `config.remote_repo`, opens a PR against the default branch,
/// and prints the resulting PR URL. The `kind: pr-finalize` arm of the
/// execute-plan GHA workflow then runs `plan-executor run pr-finalize`
/// inside a runner.
///
/// # Errors
///
/// Returns an error when:
/// * `remote_repo` is missing from the loaded config (operator hasn't
///   completed `plan-executor remote-setup`),
/// * `--owner`/`--repo` are absent and `gh repo view` cannot resolve
///   the current directory,
/// * either slug fails [`crate::remote::validate_repo_slug`],
/// * any of the underlying `gh api` / `gh pr create` calls fail.
fn run_pr_finalize_remote(
    pr: u32,
    owner: Option<String>,
    repo: Option<String>,
    merge: bool,
    merge_admin: bool,
    config_path: Option<&Path>,
) -> Result<()> {
    // Mirror the local path's mutual-exclusion guard so the same error
    // surface applies whether or not `--remote` is passed.
    if merge && merge_admin {
        anyhow::bail!("--merge and --merge-admin are mutually exclusive");
    }

    // Resolve the execution repo from config.
    let cfg = crate::config::Config::load(config_path).context("loading config")?;
    let remote_repo = cfg.remote_repo.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "remote_repo is not set in plan-executor config — run `plan-executor remote-setup` first"
        )
    })?;
    if !crate::remote::validate_repo_slug(remote_repo) {
        anyhow::bail!(
            "invalid remote_repo in config: '{remote_repo}' — must match owner/name with chars [A-Za-z0-9._-]"
        );
    }

    // Resolve target owner/repo: explicit args win; otherwise auto-detect
    // the cwd via gh repo view (matches the local path's contract).
    let (resolved_owner, resolved_repo) = match (owner, repo) {
        (Some(o), Some(r)) => (o, r),
        (o_opt, r_opt) => {
            let detected = detect_owner_repo_via_gh().ok_or_else(|| {
                anyhow::anyhow!(
                    "could not detect owner/repo from `gh repo view` — pass --owner and --repo explicitly, or run from a directory with a configured gh remote"
                )
            })?;
            (o_opt.unwrap_or(detected.0), r_opt.unwrap_or(detected.1))
        }
    };

    let combined_slug = format!("{resolved_owner}/{resolved_repo}");
    if !crate::remote::validate_repo_slug(&combined_slug) {
        anyhow::bail!(
            "invalid owner/repo: '{combined_slug}' — must match ^[A-Za-z0-9._-]+$ for both owner and repo"
        );
    }

    // Build job-spec.json. Field order matches the GHA workflow contract
    // (kind, pr, merge, merge_admin, owner, repo) — the workflow parses
    // these via jq, so the on-disk order is informational, not load-bearing.
    let job_spec = serde_json::json!({
        "kind": "pr-finalize",
        "pr": pr,
        "merge": merge,
        "merge_admin": merge_admin,
        "owner": resolved_owner,
        "repo": resolved_repo,
    });
    let job_spec_str =
        serde_json::to_string_pretty(&job_spec).context("serializing job-spec.json")?;

    // Branch name: `exec/pr-finalize-<owner>-<repo>-<pr>-<UTC-timestamp>`.
    // Sanitize the slug components defensively even though
    // `validate_repo_slug` already guarantees they match `[A-Za-z0-9._-]+`.
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let branch = format!(
        "exec/pr-finalize-{owner}-{repo}-{pr}-{ts}",
        owner = resolved_owner,
        repo = resolved_repo,
    );

    // 1. Look up the default branch SHA on remote_repo. The execution repo
    //    is created with `main` as the default by `remote-setup`; reuse the
    //    existing helper that hard-codes that assumption.
    let base_sha = crate::remote::get_main_sha(remote_repo)
        .context("looking up base branch SHA on remote_repo")?;

    // 2. Create the new ref pointing at base_sha.
    run_gh_capture(&[
        "api",
        &format!("repos/{}/git/refs", remote_repo),
        "-X",
        "POST",
        "-f",
        &format!("ref=refs/heads/{}", branch),
        "-f",
        &format!("sha={}", base_sha),
    ])
    .context("creating exec branch ref")?;

    // 3. Push job-spec.json to the new branch via the Contents API.
    crate::remote::push_file_to_branch(remote_repo, &branch, "job-spec.json", &job_spec_str)
        .context("pushing job-spec.json")?;

    // 4. Open the PR against the default branch.
    let title = format!("pr-finalize {resolved_owner}/{resolved_repo}#{pr}");
    let body = format!(
        "Remote pr-finalize for {resolved_owner}/{resolved_repo}#{pr}.\n\n\
         Triggered by `plan-executor run pr-finalize --remote`.",
    );
    let pr_url = run_gh_capture(&[
        "pr",
        "create",
        "--repo",
        remote_repo,
        "--head",
        &branch,
        "--title",
        &title,
        "--body",
        &body,
    ])
    .context("opening pr-finalize PR on remote_repo")?;

    println!("{}", pr_url.trim());
    Ok(())
}

/// Dispatch entry point for the `plan-executor run <subcommand>` CLI group.
///
/// `config_path` is the canonicalized `--config` override (or `None` for the
/// default location). Forwarded to remote dispatch handlers that need to
/// read `remote_repo` from the user's config.
///
/// # Errors
///
/// Propagates the underlying handler's error (currently only
/// `RunCommand::PrFinalize`).
fn run_subcommand(command: RunCommand, config_path: Option<&Path>) -> Result<()> {
    match command {
        RunCommand::PrFinalize {
            pr,
            owner,
            repo,
            merge,
            merge_admin,
            remote,
        } => {
            if remote {
                run_pr_finalize_remote(pr, owner, repo, merge, merge_admin, config_path)
            } else {
                // Only honour an explicit RUST_LOG opt-in. Default output
                // is the eprintln status lines from run_rust_scheduler_pipeline;
                // tracing's INFO duplicates them. Set
                // `RUST_LOG=plan_executor=info` to surface structured tracing.
                if std::env::var("RUST_LOG").is_ok() {
                    let _ = tracing_subscriber::fmt::try_init();
                }

                // The pipeline awaits sub-step async work (gh subprocesses,
                // monitor wait). Spin up a multi-thread runtime locally
                // so this stays a synchronous CLI entry point.
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .context("creating tokio runtime for pr-finalize pipeline")?;
                rt.block_on(run_pr_finalize(pr, owner, repo, merge, merge_admin))
            }
        }
    }
}

pub fn run() {
    let cli = Cli::parse();

    // Synchronous commands — handle before creating the async runtime.
    match &cli.command {
        Commands::Stop => {
            stop_daemon();
            return;
        }
        Commands::Jobs { command } => {
            let cmd = command.clone().unwrap_or(JobsCommand::List);
            match crate::job::cli::dispatch(cmd) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("Error: {:#}", e);
                    std::process::exit(1);
                }
            }
            return;
        }
        Commands::Ensure => {
            ensure_daemon();
            return;
        }
        Commands::RemoteSetup => {
            remote_setup();
            return;
        }
        Commands::Kill { job_id } => {
            daemon_job_request("kill", job_id);
            return;
        }
        Commands::Pause { job_id } => {
            daemon_job_request("pause", job_id);
            return;
        }
        Commands::Unpause { job_id } => {
            daemon_job_request("unpause", job_id);
            return;
        }
        Commands::Validate { tasks_json } => {
            run_validate(tasks_json);
            return;
        }
        Commands::CompileFixWaves {
            plan,
            execution_root,
            findings_json,
        } => {
            run_compile_fix_waves(plan, execution_root, findings_json);
            return;
        }
        Commands::Run { command } => {
            // Resolve --config to an absolute path before dispatch so the
            // remote dispatch path can read `remote_repo` from the user's
            // config file.
            let config_path: Option<std::path::PathBuf> = cli.config.as_ref().map(|p| {
                std::fs::canonicalize(p)
                    .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(p))
            });
            if let Err(e) = run_subcommand(command.clone(), config_path.as_deref()) {
                eprintln!("Error: {:#}", e);
                std::process::exit(1);
            }
            return;
        }
        _ => {}
    }

    // Resolve --config to an absolute path NOW, before daemonize() changes
    // the working directory to `/`. Relative paths become invalid after fork.
    let config_path: Option<std::path::PathBuf> = cli.config.as_ref().map(|p| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(p))
    });

    // Daemonize before creating the Tokio runtime — forking after Tokio's
    // thread pool is initialized is undefined behavior.
    if let Commands::Daemon { foreground } = &cli.command {
        if !foreground {
            daemonize();
        }
    }

    // Default to info-level logging when RUST_LOG is not set.
    // After daemonize(), stderr points to ~/.plan-executor/daemon.log.
    if std::env::var("RUST_LOG").is_err() {
        unsafe {
            std::env::set_var("RUST_LOG", "plan_executor=info");
        }
    }
    tracing_subscriber::fmt::init();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    let config =
        crate::config::Config::load(config_path.as_deref()).expect("failed to load config");

    let result: Result<()> = match cli.command {
        Commands::Daemon { .. } => rt.block_on(crate::daemon::run_daemon(config)),
        Commands::Execute {
            manifest,
            foreground,
        } => {
            if foreground {
                rt.block_on(execute_foreground(manifest, config))
            } else {
                rt.block_on(execute_plan(manifest, config))
            }
        }
        Commands::Status => rt.block_on(show_status()),
        Commands::Output { job_id, follow } => rt.block_on(output_job(job_id, follow)),
        Commands::Stop
        | Commands::Jobs { .. }
        | Commands::Ensure
        | Commands::RemoteSetup
        | Commands::Kill { .. }
        | Commands::Pause { .. }
        | Commands::Unpause { .. }
        | Commands::Validate { .. }
        | Commands::CompileFixWaves { .. }
        | Commands::Run { .. } => unreachable!(),
    };

    if let Err(e) = result {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

async fn output_job(job_id_prefix: String, follow: bool) -> Result<()> {
    use crate::config::Config;
    use crate::ipc::{DaemonEvent, TuiRequest};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    if !crate::ipc::socket_path().exists() {
        anyhow::bail!("Daemon not running. Start with: plan-executor daemon");
    }

    // Resolve job ID prefix → full ID via daemon state.
    let stream = UnixStream::connect(crate::ipc::socket_path()).await?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half).lines();

    let gs = serde_json::to_string(&TuiRequest::GetState)?;
    write_half.write_all(format!("{}\n", gs).as_bytes()).await?;

    let state_line = reader.next_line().await?.unwrap_or_default();
    let (job_id, is_running) = if let Ok(DaemonEvent::State {
        running_jobs,
        history,
        ..
    }) = serde_json::from_str::<DaemonEvent>(&state_line)
    {
        let running_match = running_jobs
            .iter()
            .find(|j| j.id.starts_with(&job_id_prefix));
        let history_match = history.iter().find(|j| j.id.starts_with(&job_id_prefix));
        match (running_match, history_match) {
            (Some(j), _) => (j.id.clone(), true),
            (_, Some(j)) => (j.id.clone(), false),
            _ => anyhow::bail!("No job matching '{}'", job_id_prefix),
        }
    } else {
        anyhow::bail!("Unexpected response from daemon");
    };

    // Print stored display output from display.log (pre-rendered, includes
    // [plan-executor] lines). Sub-agent output is interleaved inline: each
    // `dispatching N sub-agent(s)` line advances a dispatch counter, and
    // before each `sub-agent <N> done` / `sub-agent <N> failed` line we
    // render the matching persisted JSONL file with a dim indented prefix
    // so the context is clear.
    let display_path = Config::base_dir()
        .join("jobs")
        .join(&job_id)
        .join("display.log");
    if display_path.exists() {
        let content = std::fs::read_to_string(&display_path)?;
        let mut dispatch_counter: u32 = 0;
        for line in content.lines() {
            if line.contains("⏺ [plan-executor] dispatching") && line.contains("sub-agent(s)") {
                dispatch_counter += 1;
            }
            if let Some(idx) = parse_subagent_done_index(line) {
                render_subagent_output(&job_id, dispatch_counter, idx);
            }
            print_display_line(line);
        }
    }

    if !follow || !is_running {
        return Ok(());
    }

    // Follow mode: stream live JobDisplayLine and SubAgentLine events.
    // Sub-agent output is rendered live as each line arrives rather than
    // batch-rendered at `sub-agent N done` (which is how replay works).
    // We track (dispatch, index) pairs we've seen live so that if a
    // sub-agent finished before follow began, we still batch-render its
    // file at the done marker; otherwise we skip the batch to avoid
    // double-rendering what we already streamed.
    eprintln!(
        "[following {} — Ctrl+C to stop]",
        &job_id[..job_id.len().min(8)]
    );
    let mut dispatch_counter: u32 = 0;
    let mut live_streamed: std::collections::HashSet<(u32, usize)> =
        std::collections::HashSet::new();
    while let Some(line) = reader.next_line().await? {
        if let Ok(DaemonEvent::JobDisplayLine {
            job_id: jid,
            line: text,
        }) = serde_json::from_str::<DaemonEvent>(&line)
        {
            if jid == job_id {
                if text.contains("⏺ [plan-executor] dispatching") && text.contains("sub-agent(s)")
                {
                    dispatch_counter += 1;
                }
                if let Some(idx) = parse_subagent_done_index(&text) {
                    if !live_streamed.contains(&(dispatch_counter, idx)) {
                        render_subagent_output(&job_id, dispatch_counter, idx);
                    }
                }
                print_display_line(&text);
            }
        } else if let Ok(DaemonEvent::SubAgentLine {
            job_id: jid,
            index,
            is_stderr,
            line: sa_line,
            ..
        }) = serde_json::from_str::<DaemonEvent>(&line)
        {
            if jid == job_id {
                live_streamed.insert((dispatch_counter, index));
                render_subagent_live(index, is_stderr, &sa_line);
            }
        } else if let Ok(DaemonEvent::JobUpdated { job }) =
            serde_json::from_str::<DaemonEvent>(&line)
        {
            if job.id == job_id && job.status != crate::jobs::JobStatus::Running {
                eprintln!("[job finished: {:?}]", job.status);
                break;
            }
        }
    }
    Ok(())
}

/// Renders one streamed sub-agent line in follow mode, using the same
/// sjv pipeline and agent-indexed dim prefix as the batch replay so
/// concurrent sub-agents interleave without losing attribution.
fn render_subagent_live(index: usize, is_stderr: bool, line: &str) {
    if line.is_empty() {
        return;
    }
    let prefix = subagent_prefix(index);
    let rendered = if is_stderr {
        format!("\x1b[2m{}\x1b[0m", line)
    } else {
        sjv::render_runtime_line(line, false, true)
    };
    for visual in rendered.lines() {
        if visual.is_empty() {
            continue;
        }
        println!("{}{}", prefix, visual);
    }
}

/// Parses `⏺ [plan-executor] sub-agent <N> done` / `failed` /
/// `skipped (can-fail)` lines and returns the sub-agent index. Used by
/// `output_job` to look up the matching persisted sub-agent output.
fn parse_subagent_done_index(line: &str) -> Option<usize> {
    // Match any of: "sub-agent <N> done", "sub-agent <N> failed:",
    // "sub-agent <N> skipped (can-fail):".
    let after = line.strip_prefix("⏺ [plan-executor] sub-agent ")?;
    let (num, rest) = after.split_once(' ')?;
    let idx: usize = num.parse().ok()?;
    if rest.starts_with("done") || rest.starts_with("failed") || rest.starts_with("skipped") {
        Some(idx)
    } else {
        None
    }
}

async fn execute_plan(manifest_arg: String, config: crate::config::Config) -> Result<()> {
    let manifest_path = resolve_manifest_path(&manifest_arg)?;
    let (plan_path, status, _execution_mode) = read_manifest_plan_block(&manifest_path)?;

    if status != "READY" {
        anyhow::bail!("Manifest plan.status is {}, expected READY", status);
    }

    if !plan_path.exists() {
        anyhow::bail!(
            "Plan file referenced by manifest not found: {}",
            plan_path.display()
        );
    }

    if !crate::ipc::socket_path().exists() {
        anyhow::bail!("Daemon not running. Start with: plan-executor daemon");
    }

    execute_via_daemon(plan_path, manifest_path, config).await
}

async fn trigger_remote(
    plan: PathBuf,
    manifest_path: PathBuf,
    config: crate::config::Config,
) -> Result<()> {
    let remote_repo = config.remote_repo.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "remote execution requires 'remote_repo' in config — run 'plan-executor remote-setup'"
        )
    })?;

    let plan_filename = plan
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("plan.md")
        .to_string();

    let repo_root = find_repo_root(&plan)
        .ok_or_else(|| anyhow::anyhow!("could not find git repo root for {}", plan.display()))?;
    let (target_repo, target_ref, target_branch) = crate::remote::gather_git_context(&repo_root)?;
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

    let pr_url =
        crate::remote::trigger_remote_execution(remote_repo, &plan, &manifest_path, &meta)?;
    let pr_num = crate::remote::pr_number_from_url(&pr_url);

    // Store PR number for daemon-side polling.
    if let Some(n) = pr_num {
        let _ = crate::plan::set_plan_header(&plan, "remote-pr", &n.to_string());
    }

    // Create a job entry and persist it
    if let Some(n) = pr_num {
        let job = crate::jobs::JobMetadata::new_remote(plan.clone(), remote_repo.to_string(), n);
        let short_id = job.id[..job.id.len().min(8)].to_string();
        let _ = job.save();

        // Notify daemon to start monitoring (if running)
        if crate::ipc::socket_path().exists() {
            let _ = notify_daemon_track_remote(
                plan.to_string_lossy().to_string(),
                remote_repo.to_string(),
                n,
            );
        }

        println!("Remote execution triggered (job {}).", short_id);
    } else {
        println!("Remote execution triggered.");
    }
    println!("PR: {}", pr_url);

    Ok(())
}

/// Foreground-mode entry point: drives the Rust scheduler pipeline directly.
///
/// Builds a `Job { JobKind::Plan { manifest_path } }`, hydrates its steps via
/// the registry, runs each step against a fresh `StepContext`, and exits with
/// status 1 on the first non-success [`AttemptOutcome`]. Sub-agent dispatch
/// flows through `handoff::dispatch_all` inside each step.
async fn execute_foreground(manifest_arg: String, config: crate::config::Config) -> Result<()> {
    use crate::job::registry;
    use crate::job::storage::JobStore;
    use crate::job::types::{Job, JobId, JobKind, JobState, StepInstance, StepStatus};

    let manifest_path = resolve_manifest_path(&manifest_arg)?;
    let (resolved_path, status, execution_mode) = read_manifest_plan_block(&manifest_path)?;

    if status != "READY" {
        anyhow::bail!("Manifest plan.status is {}, expected READY", status);
    }
    if !resolved_path.exists() {
        anyhow::bail!(
            "Plan file referenced by manifest not found: {}",
            resolved_path.display()
        );
    }

    if execution_mode == "remote" && std::env::var("PLAN_EXECUTOR_LOCAL").as_deref() != Ok("1") {
        return trigger_remote(resolved_path, manifest_path, config).await;
    }

    let execution_root = find_repo_root(&resolved_path).unwrap_or_else(|| {
        resolved_path
            .parent()
            .unwrap_or(&resolved_path)
            .to_path_buf()
    });

    tracing::info!("dispatching plan job (foreground)");

    let kind = JobKind::Plan {
        manifest_path: manifest_path.clone(),
    };
    let runtime_steps = registry::steps_for(&kind);
    let step_instances: Vec<StepInstance> = runtime_steps
        .iter()
        .enumerate()
        .map(|(idx, step)| {
            let seq = u32::try_from(idx + 1).unwrap_or(u32::MAX);
            StepInstance {
                seq,
                name: step.name().to_string(),
                status: StepStatus::Pending,
                attempts: Vec::new(),
                idempotent: step.idempotent(),
            }
        })
        .collect();

    let job = Job {
        id: JobId(uuid::Uuid::new_v4().to_string()),
        kind,
        state: JobState::Running,
        created_at: chrono::Utc::now().to_rfc3339(),
        steps: step_instances,
    };

    let store = JobStore::new().context("opening job store")?;
    let job_dir = store
        .create(&job)
        .context("persisting job.json for foreground rust scheduler run")?;

    let _ = resolved_path; // kept in scope to mirror prior contract

    let success =
        run_rust_scheduler_pipeline(runtime_steps, job_dir.path().to_path_buf(), execution_root)
            .await;
    if !success {
        std::process::exit(1);
    }
    Ok(())
}

/// Sequentially runs every step in `steps` and reports whether all of them
/// reached [`AttemptOutcome::Success`]. Each step gets a fresh
/// [`StepContext`] anchored at `job_dir` and `workdir`. Recovery / retry
/// per step is the registry's responsibility in later phases; D4-step-1 is
/// a "happy-path wiring" change — any non-success outcome aborts the run.
pub(crate) async fn run_rust_scheduler_pipeline(
    steps: Vec<Box<dyn crate::job::step::Step>>,
    job_dir: PathBuf,
    workdir: PathBuf,
) -> bool {
    use crate::job::step::StepContext;
    use crate::job::types::AttemptOutcome;

    for (idx, step) in steps.iter().enumerate() {
        let seq = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        let mut ctx = StepContext {
            job_dir: job_dir.clone(),
            step_seq: seq,
            attempt_n: 1,
            workdir: workdir.clone(),
            daemon_hooks: None,
        };
        let step_name = step.name();
        // Yellow `[plan-executor]` prefix matches format_message_line;
        // status keyword colored to match severity.
        let prefix = "\x1b[33m[plan-executor]\x1b[0m";
        eprintln!("{prefix} step {seq:03} {step_name}: running");
        let outcome = step.run(&mut ctx).await;
        match outcome {
            AttemptOutcome::Success => {
                eprintln!("{prefix} step {seq:03} {step_name}: \x1b[32msuccess\x1b[0m");
            }
            AttemptOutcome::Pending => {
                // `Pending` is currently emitted by the placeholder
                // `PreflightStep` and `PrFinalizeStep` shells. Treat as
                // a no-op pass so the rest of the pipeline can run.
                eprintln!("{prefix} step {seq:03} {step_name}: pending (placeholder)");
            }
            other => {
                eprintln!(
                    "{prefix} step {seq:03} {step_name}: \x1b[31mFAILED\x1b[0m — {other:?}\n{prefix} aborting pipeline. attempt log: {}/steps/{seq:03}-{step_name}/attempts/1/",
                    job_dir.display()
                );
                return false;
            }
        }
    }
    true
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

/// Sends Execute to the daemon, waits just long enough to identify the new
/// job ID, prints it, and returns immediately.  Use `plan-executor output -f
/// <job-id>` to watch the live output of local jobs.
async fn execute_via_daemon(
    plan: PathBuf,
    manifest_path: PathBuf,
    _config: crate::config::Config,
) -> Result<()> {
    use crate::ipc::{DaemonEvent, TuiRequest};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    // Re-read execution_mode from the manifest. This function is only
    // reachable from execute_plan, which already loaded the block once,
    // but execute_via_daemon is also invoked from a TUI path that doesn't
    // pass the parsed value through. Reading it here keeps the function
    // self-contained.
    let (_, _, execution_mode) = read_manifest_plan_block(&manifest_path)
        .context("re-reading manifest to determine execution_mode")?;
    let is_remote = execution_mode == "remote";

    let stream = UnixStream::connect(crate::ipc::socket_path())
        .await
        .context("Daemon not reachable")?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half).lines();

    // Snapshot current running job IDs before we trigger execution.
    let gs = serde_json::to_string(&TuiRequest::GetState)?;
    write_half.write_all(format!("{}\n", gs).as_bytes()).await?;

    let mut existing_ids = std::collections::HashSet::<String>::new();
    if let Ok(Some(line)) = reader.next_line().await {
        if let Ok(DaemonEvent::State {
            running_jobs,
            history,
            ..
        }) = serde_json::from_str(&line)
        {
            existing_ids = running_jobs
                .iter()
                .chain(history.iter())
                .map(|j| j.id.clone())
                .collect();
        }
    }

    // Trigger execution.
    let manifest_str = manifest_path.to_string_lossy().to_string();
    let req = serde_json::to_string(&TuiRequest::Execute {
        manifest_path: manifest_str,
    })?;
    write_half
        .write_all(format!("{}\n", req).as_bytes())
        .await?;

    let filename = plan.file_name().and_then(|n| n.to_str()).unwrap_or("?");

    // Remote plans need longer: creating branch + pushing files + opening PR via
    // the GitHub API can take 10-20 seconds.  Local plans resolve in <1 second.
    let timeout_secs = if is_remote { 30 } else { 2 };
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            line = reader.next_line() => {
                let Ok(Some(line)) = line else { break };
                if let Ok(event) = serde_json::from_str::<DaemonEvent>(&line) {
                    match event {
                        DaemonEvent::State { running_jobs, history, .. } => {
                            // Check both running_jobs (local) and history (remote)
                            // for a newly created job.
                            let new_job = running_jobs.iter().chain(history.iter())
                                .find(|j| !existing_ids.contains(&j.id));
                            if let Some(j) = new_job {
                                if let (Some(repo), Some(pr)) = (&j.remote_repo, j.remote_pr) {
                                    println!("https://github.com/{}/pull/{}", repo, pr);
                                } else {
                                    let short_id = &j.id[..j.id.len().min(8)];
                                    println!("Queued {} (job {})", filename, short_id);
                                    println!("Watch: plan-executor output -f {}", short_id);
                                }
                                return Ok(());
                            }
                        }
                        DaemonEvent::Error { message } => {
                            eprintln!("Error: {}", message);
                            return Ok(());
                        }
                        _ => {}
                    }
                }
            }
            _ = &mut timeout => {
                if is_remote {
                    eprintln!("Timed out waiting for PR creation. Check: plan-executor jobs");
                } else {
                    println!("Queued {}", filename);
                    println!("Watch: plan-executor output -f <job-id>");
                }
                return Ok(());
            }
        }
    }

    println!("Queued {}", filename);
    Ok(())
}

/// Resolves a job ID prefix to a full ID from running jobs, sends the
/// corresponding daemon request, and prints the result.
fn daemon_job_request(action: &str, job_id_prefix: &str) {
    use crate::ipc::{DaemonEvent, TuiRequest};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let sock = crate::ipc::socket_path();
    if !sock.exists() {
        eprintln!("Daemon not running.");
        std::process::exit(1);
    }

    let mut stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Cannot connect to daemon: {}", e);
            std::process::exit(1);
        }
    };

    // Get state to resolve job ID prefix.
    let gs = serde_json::to_string(&TuiRequest::GetState).unwrap();
    let _ = stream.write_all(format!("{}\n", gs).as_bytes());
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    let _ = reader.read_line(&mut line);

    let full_id = if let Ok(DaemonEvent::State { running_jobs, .. }) = serde_json::from_str(&line) {
        running_jobs
            .into_iter()
            .find(|j| j.id.starts_with(job_id_prefix))
            .map(|j| j.id)
    } else {
        None
    };

    let Some(job_id) = full_id else {
        eprintln!("No running job matching '{}'.", job_id_prefix);
        std::process::exit(1);
    };

    let req = match action {
        "kill" => TuiRequest::KillJob {
            job_id: job_id.clone(),
        },
        "pause" => TuiRequest::PauseJob {
            job_id: job_id.clone(),
        },
        "unpause" => TuiRequest::ResumeJob {
            job_id: job_id.clone(),
        },
        _ => unreachable!(),
    };

    let _ = stream.write_all(format!("{}\n", serde_json::to_string(&req).unwrap()).as_bytes());
    println!("{} job {}.", action, &job_id[..job_id.len().min(8)]);
}

/// Start the daemon if it is not already running. Used by the shell hook.
fn ensure_daemon() {
    use crate::ipc::socket_path;
    if socket_path().exists() {
        return; // already running, nothing to do
    }
    // Daemonize and start — same path as `plan-executor daemon`
    daemonize();
    // After daemonize() the child continues here; start the runtime and daemon.
    tracing_subscriber::fmt::init();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let config = crate::config::Config::load(None).expect("failed to load config");
    if let Err(e) = rt.block_on(crate::daemon::run_daemon(config)) {
        tracing::error!("daemon error: {:#}", e);
        std::process::exit(1);
    }
}

fn stop_daemon() {
    use crate::config::Config;
    let pid_path = Config::base_dir().join("daemon.pid");

    let pid = match std::fs::read_to_string(&pid_path) {
        Ok(s) => s.trim().to_string(),
        Err(_) => {
            println!("Daemon is not running (no PID file).");
            return;
        }
    };

    let pid: u32 = match pid.trim().parse() {
        Ok(n) => n,
        Err(_) => {
            eprintln!("Invalid PID in pid file: {:?}", pid);
            std::process::exit(1);
        }
    };

    // Safety: pid > 0 guaranteed by parse into u32; we only send SIGTERM.
    let ret = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if ret == 0 {
        let _ = std::fs::remove_file(&pid_path);
        println!("Daemon stopped (pid={}).", pid);
    } else {
        eprintln!(
            "Failed to stop daemon (pid={}). It may have already exited.",
            pid
        );
        std::process::exit(1);
    }
}

/// Forks the process, exits the parent, and redirects stdout/stderr to the
/// daemon log file. The child process continues past this call.
fn daemonize() {
    use crate::config::Config;
    let base_dir = Config::base_dir();
    std::fs::create_dir_all(&base_dir).expect("failed to create daemon base directory");

    let log_path = base_dir.join("daemon.log");
    let pid_path = base_dir.join("daemon.pid");

    // Kill ALL running plan-executor daemon processes, not just the one in
    // the PID file (there may be leftover instances from previous runs).
    let our_pid = std::process::id().to_string();
    if let Ok(out) = std::process::Command::new("pgrep")
        .args(["-f", "plan-executor.*daemon"])
        .output()
    {
        let killed = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|p| p.trim() != our_pid)
            .filter_map(|p| p.trim().parse::<libc::pid_t>().ok())
            .inspect(|&pid| unsafe {
                libc::kill(pid, libc::SIGTERM);
            })
            .count();
        if killed > 0 {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }

    eprintln!(
        "Starting daemon. PID file: {}  Logs: {}",
        pid_path.display(),
        log_path.display()
    );

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("failed to open daemon log file");
    let log_stderr = log_file
        .try_clone()
        .expect("failed to clone log file handle");

    // No .pid_file() — we write the PID ourselves in run_daemon() after fork.
    // Using pid_file() here creates a lock that conflicts when restarting.
    daemonize::Daemonize::new()
        .stdout(log_file)
        .stderr(log_stderr)
        .start()
        .expect("failed to daemonize");
}

async fn show_status() -> Result<()> {
    use crate::config::Config;
    use crate::ipc::socket_path;

    let sock = socket_path();
    let pid_path = Config::base_dir().join("daemon.pid");

    if sock.exists() {
        let pid = std::fs::read_to_string(&pid_path)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "?".to_string());
        println!("Daemon running  pid={}  socket={}", pid, sock.display());
    } else {
        println!("Daemon not running");
    }
    Ok(())
}

fn remote_setup() {
    use std::io::{self, BufRead, Write};

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    // Check gh CLI
    if std::process::Command::new("gh")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("Error: gh CLI not found. Install: https://cli.github.com");
        std::process::exit(1);
    }

    // Step 1: Execution repo
    let current_repo = crate::config::Config::load(None)
        .ok()
        .and_then(|c| c.remote_repo);
    let default_display = current_repo.unwrap_or_else(|| {
        // Use the current gh account as the default owner.
        let gh_user = std::process::Command::new("gh")
            .args(["api", "user", "--jq", ".login"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    String::from_utf8(o.stdout)
                        .ok()
                        .map(|s| s.trim().to_string())
                } else {
                    None
                }
            });
        match gh_user {
            Some(user) if !user.is_empty() => format!("{}/plan-executions", user),
            _ => "owner/plan-executions".to_string(),
        }
    });
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

    if !crate::remote::validate_repo_slug(&remote_repo) {
        eprintln!(
            "Error: invalid repo slug '{}'. Expected format: owner/repo",
            remote_repo
        );
        std::process::exit(1);
    }

    // Ensure repo exists
    if crate::remote::repo_exists(&remote_repo) {
        println!("  Repo exists.");
    } else {
        println!("  Repo not found. Creating...");
        match crate::remote::create_repo(&remote_repo) {
            Ok(()) => println!("  Created {}", remote_repo),
            Err(e) => {
                eprintln!("  Error creating repo: {}", e);
                std::process::exit(1);
            }
        }
    }

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
    print!("  Paste token (enter to skip): ");
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
    print!("Anthropic API key (enter to skip): ");
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
                eprintln!(
                    "  {} not found. Run codex login first.",
                    auth_path.display()
                );
            }
        }
        "a" | "api" => {
            print!("  OpenAI API key (enter to skip): ");
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

    // Step 6: Commit signing
    println!();
    configure_commit_signing(&remote_repo, &stdin, &mut stdout);

    // Step 7: Push workflow to execution repo
    println!("Pushing execute-plan workflow...");
    match crate::remote::push_workflow(&remote_repo) {
        Ok(()) => println!("  Pushed to .github/workflows/execute-plan.yml"),
        Err(e) => eprintln!("  Error pushing workflow: {}", e),
    }

    println!();
    println!("Setup complete. Remote execution ready.");
}

/// Orchestrates the commit-signing portion of `remote-setup`.
///
/// Layered decisions:
///   1. If the execution repo already has `GPG_SIGNING_KEY`, default to
///      skip; offer rotate.
///   2. Otherwise (or on rotate), look for an existing `plan-executor CI`
///      key on the local keyring and default to reusing it across repos.
///   3. If neither applies, generate a fresh passphraseless ed25519 key
///      whose uid carries the CI marker for future runs to find.
///   4. Ensure the public key is on the current gh user, then set
///      `GPG_SIGNING_KEY` and `GPG_SIGNING_KEY_ID` on the execution repo.
fn configure_commit_signing(
    remote_repo: &str,
    stdin: &std::io::Stdin,
    stdout: &mut std::io::Stdout,
) {
    use std::io::{BufRead, Write};

    println!("Commit signing:");

    let already_set = crate::remote::gh_secret_exists(remote_repo, "GPG_SIGNING_KEY")
        .unwrap_or_else(|e| {
            eprintln!("  Warning: could not query existing secrets ({e}); assuming not set.");
            false
        });

    let mut rotating = false;
    if already_set {
        print!(
            "  GPG_SIGNING_KEY already set on {}. (r)otate / (s)kip [default: skip]: ",
            remote_repo
        );
        let _ = stdout.flush();
        let mut choice = String::new();
        let _ = stdin.lock().read_line(&mut choice);
        match choice.trim() {
            "r" | "rotate" => rotating = true,
            _ => {
                println!("  Skipped.");
                return;
            }
        }
    }

    // Prefer reusing an existing CI key across execution repos unless we
    // were explicitly asked to rotate.
    let fingerprint = if rotating {
        match generate_new_ci_key(stdin, stdout) {
            Some(fp) => fp,
            None => {
                println!("  Skipped.");
                return;
            }
        }
    } else if let Some(existing) = crate::remote::find_ci_signing_key() {
        let short = short_fingerprint(&existing.fingerprint);
        let label = if existing.email.is_empty() {
            existing.name.clone()
        } else {
            format!("{} <{}>", existing.name, existing.email)
        };
        print!(
            "  Found existing CI key {} ({label}). (u)se / (g)enerate new / (s)kip [default: use]: ",
            short
        );
        let _ = stdout.flush();
        let mut choice = String::new();
        let _ = stdin.lock().read_line(&mut choice);
        match choice.trim() {
            "" | "u" | "use" => existing.fingerprint,
            "g" | "generate" => match generate_new_ci_key(stdin, stdout) {
                Some(fp) => fp,
                None => {
                    println!("  Skipped.");
                    return;
                }
            },
            _ => {
                println!("  Skipped.");
                return;
            }
        }
    } else {
        match generate_new_ci_key(stdin, stdout) {
            Some(fp) => fp,
            None => {
                println!("  Skipped.");
                return;
            }
        }
    };

    // Public key on GitHub — upload if the current gh user doesn't have
    // it yet. Cross-repo rotations skip this when the key was already
    // uploaded from a previous run.
    use crate::remote::{GithubGpgKeyCheck, GithubGpgUploadResult};
    match crate::remote::github_check_gpg_key(&fingerprint) {
        Ok(GithubGpgKeyCheck::Present) => println!("  Public key already on GitHub."),
        Ok(GithubGpgKeyCheck::Absent) => match crate::remote::gpg_export_public(&fingerprint) {
            Ok(pub_armored) => match crate::remote::github_upload_gpg_key(&pub_armored) {
                Ok(GithubGpgUploadResult::Uploaded) => {
                    println!("  Uploaded public key to GitHub user account.");
                }
                Ok(GithubGpgUploadResult::MissingScope) => {
                    print_gpg_upload_fallback(&pub_armored);
                }
                Err(e) => eprintln!("  Warning: could not upload public key: {e}"),
            },
            Err(e) => eprintln!("  Warning: could not export public key: {e}"),
        },
        Ok(GithubGpgKeyCheck::MissingScope) => match crate::remote::gpg_export_public(&fingerprint)
        {
            Ok(pub_armored) => print_gpg_upload_fallback(&pub_armored),
            Err(e) => eprintln!("  Warning: could not export public key: {e}"),
        },
        Err(e) => eprintln!("  Warning: could not check user GPG keys: {e}"),
    }

    // Private key secret.
    match crate::remote::gpg_export_secret(&fingerprint) {
        Ok(secret) => {
            match crate::remote::gh_secret_set_stdin("GPG_SIGNING_KEY", remote_repo, &secret) {
                Ok(()) => println!("  Stored GPG_SIGNING_KEY."),
                Err(e) => eprintln!("  Error storing GPG_SIGNING_KEY: {e}"),
            }
        }
        Err(e) => eprintln!("  Error exporting secret key: {e}"),
    }
    if let Err(e) =
        crate::remote::gh_secret_set_stdin("GPG_SIGNING_KEY_ID", remote_repo, &fingerprint)
    {
        eprintln!("  Error storing GPG_SIGNING_KEY_ID: {e}");
    } else {
        println!(
            "  Stored GPG_SIGNING_KEY_ID ({}).",
            short_fingerprint(&fingerprint)
        );
    }
}

fn generate_new_ci_key(stdin: &std::io::Stdin, stdout: &mut std::io::Stdout) -> Option<String> {
    use std::io::{BufRead, Write};

    let default_name = default_commit_name();
    let default_email = default_commit_email();

    print!("  Commit name [{}]: ", default_name);
    let _ = stdout.flush();
    let mut name_input = String::new();
    let _ = stdin.lock().read_line(&mut name_input);
    let name = match name_input.trim() {
        "" => default_name.clone(),
        s => s.to_string(),
    };

    print!("  Commit email [{}]: ", default_email);
    let _ = stdout.flush();
    let mut email_input = String::new();
    let _ = stdin.lock().read_line(&mut email_input);
    let email = match email_input.trim() {
        "" => default_email.clone(),
        s => s.to_string(),
    };

    // The `plan-executor CI` marker goes into the GPG uid's comment
    // field by gpg_generate_ci_key, not into the Name-Real we pass here.
    // That keeps commit author strings clean while still letting
    // find_ci_signing_key identify the key on the keyring.
    println!("  Generating ed25519 signing key (no passphrase)...");
    match crate::remote::gpg_generate_ci_key(&name, &email) {
        Ok(fp) => {
            println!(
                "  Generated {} ({} <{}>).",
                short_fingerprint(&fp),
                name,
                email
            );
            Some(fp)
        }
        Err(e) => {
            eprintln!("  Error generating key: {e}");
            None
        }
    }
}

/// Resolves the best default commit author name, in order:
///   1. `git config --global user.name`
///   2. `gh api user --jq .name` (GitHub profile display name)
///   3. `gh api user --jq .login` (GitHub username)
///   4. `"plan-executor"` (last-resort placeholder)
fn default_commit_name() -> String {
    git_config_get("user.name")
        .filter(|s| !s.is_empty())
        .or_else(|| gh_user_field("name"))
        .or_else(|| gh_user_field("login"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "plan-executor".to_string())
}

/// Resolves the best default commit email, in order:
///   1. `git config --global user.email`
///   2. GitHub's canonical noreply form `<id>+<login>@users.noreply.github.com`
///      (derived from `gh api user`)
///   3. `"plan-executor@noreply"` (last-resort placeholder)
fn default_commit_email() -> String {
    if let Some(e) = git_config_get("user.email").filter(|s| !s.is_empty()) {
        return e;
    }
    if let (Some(id), Some(login)) = (gh_user_field("id"), gh_user_field("login")) {
        if !id.is_empty() && !login.is_empty() {
            return format!("{id}+{login}@users.noreply.github.com");
        }
    }
    "plan-executor@noreply".to_string()
}

fn git_config_get(key: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["config", "--global", "--get", key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Reads a single field from `gh api user`. Returns `None` when gh is not
/// authenticated, the field is absent, or the value is null.
fn gh_user_field(field: &str) -> Option<String> {
    let output = std::process::Command::new("gh")
        .args(["api", "user", "--jq", &format!(".{field}")])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if v.is_empty() || v == "null" {
        None
    } else {
        Some(v)
    }
}

fn short_fingerprint(fp: &str) -> String {
    // Show the last 16 hex chars — that's the long key id historically
    // printed by `gpg --list-keys --keyid-format=long`.
    if fp.len() > 16 {
        fp[fp.len() - 16..].to_string()
    } else {
        fp.to_string()
    }
}

/// Printed when the automatic upload path via `gh api user/gpg_keys` is
/// blocked by a missing OAuth scope. Gives the operator both a
/// gh-auth-refresh command and a copy-paste option so the signing flow
/// never hard-blocks on scope.
fn print_gpg_upload_fallback(armored_public: &str) {
    println!("  Public key upload requires the `admin:gpg_key` scope, which gh auth");
    println!("  does not request by default. Two ways to complete the upload:");
    println!();
    println!("  (1) Refresh scope, then re-run this setup to upload automatically:");
    println!("      gh auth refresh -h github.com -s admin:gpg_key");
    println!();
    println!("  (2) Paste the key at https://github.com/settings/gpg/new");
    println!();
    for line in armored_public.lines() {
        println!("    {}", line);
    }
    println!();
    println!("  (Setup continues — the private key secret is already stored.)");
}

fn notify_daemon_track_remote(
    plan_path: String,
    remote_repo: String,
    pr_number: u64,
) -> Result<()> {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    let mut s = UnixStream::connect(crate::ipc::socket_path())?;
    let req = serde_json::to_string(&crate::ipc::TuiRequest::TrackRemote {
        plan_path,
        remote_repo,
        pr_number,
    })?;
    s.write_all(format!("{}\n", req).as_bytes())?;
    Ok(())
}

fn gh_secret_set(repo: &str, name: &str, value: &str) -> Result<()> {
    crate::remote::gh_secret_set_stdin(name, repo, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;

    /// Local fake invoker — duplicates the test pattern from compile.rs to
    /// avoid exposing a public test type from that module.
    struct FakeInvoker {
        post_manifest: serde_json::Value,
        captured_args: RefCell<Vec<std::path::PathBuf>>,
    }
    impl crate::compile::CompileInvoker for FakeInvoker {
        fn invoke(&self, args: &[&Path]) -> Result<(), String> {
            *self.captured_args.borrow_mut() = args.iter().map(|p| p.to_path_buf()).collect();
            let output_dir = args[2];
            let target = output_dir.join("tasks.json");
            fs::write(
                target,
                serde_json::to_vec_pretty(&self.post_manifest).unwrap(),
            )
            .map_err(|e| format!("fake write: {e}"))?;
            // Materialize every referenced prompt_file so the post-append
            // semantic_check (added per F4) accepts the synthetic manifest.
            if let Some(tasks) = self.post_manifest.get("tasks").and_then(|v| v.as_object()) {
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

    fn pre_manifest() -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "plan": {
                "goal": "g", "type": "feature", "jira": "",
                "target_repo": null, "target_branch": null,
                "path": "/tmp/plan.md", "status": "READY",
                "flags": {
                    "merge": false, "merge_admin": false, "skip_pr": false,
                    "skip_code_review": false, "no_worktree": false, "draft_pr": false
                }
            },
            "waves": [
                {"id": 1, "task_ids": ["1.1"], "depends_on": [], "kind": "implementation"}
            ],
            "tasks": {
                "1.1": {"prompt_file": "tasks/task-1.1.md", "agent_type": "claude"}
            }
        })
    }

    #[test]
    fn end_to_end_with_fake_invoker_writes_manifest_and_returns_path() {
        let tmp = tempfile::tempdir().unwrap();
        let exec_root = tmp.path().to_path_buf();
        // Pre-write tasks.json
        let pre = pre_manifest();
        fs::write(
            exec_root.join("tasks.json"),
            serde_json::to_vec_pretty(&pre).unwrap(),
        )
        .unwrap();

        // Pre-write findings.json
        let findings_path = exec_root.join("findings-input.json");
        fs::write(
            &findings_path,
            br#"{"findings":[{"id":"F1","severity":"major","category":"x","description":"y"}]}"#,
        )
        .unwrap();

        // Build the post-append manifest the fake will write.
        let mut post = pre.clone();
        post["waves"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "id": 100, "task_ids": ["fix-100-1"], "depends_on": [1], "kind": "fix"
            }));
        post["tasks"]["fix-100-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-100-1.md", "agent_type": "claude"
        });

        let invoker = FakeInvoker {
            post_manifest: post,
            captured_args: RefCell::new(vec![]),
        };

        let plan_path = std::path::PathBuf::from("/tmp/plan.md");
        run_compile_fix_waves_with_invoker(&invoker, &plan_path, &exec_root, &findings_path)
            .expect("must succeed");

        // Verify manifest was rewritten with fix-wave 100
        let reread: serde_json::Value =
            serde_json::from_slice(&fs::read(exec_root.join("tasks.json")).unwrap()).unwrap();
        assert!(
            reread["waves"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["id"].as_u64() == Some(100) && w["kind"] == "fix")
        );

        // Verify the fake captured 5 args
        let captured = invoker.captured_args.borrow();
        assert_eq!(captured.len(), 5);
    }

    #[test]
    fn missing_manifest_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let exec_root = tmp.path().to_path_buf();
        let findings_path = exec_root.join("f.json");
        fs::write(&findings_path, br#"{"findings":[]}"#).unwrap();

        struct NeverInvoker;
        impl crate::compile::CompileInvoker for NeverInvoker {
            fn invoke(&self, _: &[&Path]) -> Result<(), String> {
                panic!("must not be called when manifest missing")
            }
        }

        let plan_path = std::path::PathBuf::from("/tmp/plan.md");
        let err = run_compile_fix_waves_with_invoker(
            &NeverInvoker,
            &plan_path,
            &exec_root,
            &findings_path,
        )
        .expect_err("missing manifest must error");
        let msg = format!("{err}");
        assert!(msg.contains("manifest not found"), "msg was: {msg}");
    }

    #[test]
    fn malformed_findings_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let exec_root = tmp.path().to_path_buf();
        // Pre-write a valid manifest
        fs::write(
            exec_root.join("tasks.json"),
            serde_json::to_vec_pretty(&pre_manifest()).unwrap(),
        )
        .unwrap();

        let findings_path = exec_root.join("f.json");
        fs::write(&findings_path, b"not json {}").unwrap();

        struct NeverInvoker;
        impl crate::compile::CompileInvoker for NeverInvoker {
            fn invoke(&self, _: &[&Path]) -> Result<(), String> {
                panic!("must not be called when findings malformed")
            }
        }

        let plan_path = std::path::PathBuf::from("/tmp/plan.md");
        let err = run_compile_fix_waves_with_invoker(
            &NeverInvoker,
            &plan_path,
            &exec_root,
            &findings_path,
        )
        .expect_err("malformed findings must error");
        let _ = err;
    }

    #[test]
    fn plan_mismatch_with_manifest_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let exec_root = tmp.path().to_path_buf();
        // Pre-write a manifest whose plan.path is /tmp/plan.md.
        fs::write(
            exec_root.join("tasks.json"),
            serde_json::to_vec_pretty(&pre_manifest()).unwrap(),
        )
        .unwrap();

        let findings_path = exec_root.join("f.json");
        fs::write(&findings_path, br#"{"findings":[]}"#).unwrap();

        struct NeverInvoker;
        impl crate::compile::CompileInvoker for NeverInvoker {
            fn invoke(&self, _: &[&Path]) -> Result<(), String> {
                panic!("must not be called when --plan disagrees with manifest")
            }
        }

        // Caller passes a different --plan than what is recorded in manifest.plan.path.
        let plan_path = std::path::PathBuf::from("/tmp/different-plan.md");
        let err = run_compile_fix_waves_with_invoker(
            &NeverInvoker,
            &plan_path,
            &exec_root,
            &findings_path,
        )
        .expect_err("plan vs manifest mismatch must hard-fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("/tmp/different-plan.md")
                && msg.contains("/tmp/plan.md")
                && msg.contains("disagrees"),
            "msg was: {msg}"
        );
    }

    /// N2: oversized manifest must skip the preflight cross-check and let
    /// `append_fix_waves` reject it definitively, rather than OOM-ing the CLI
    /// via an uncapped `read_to_string` in the preflight path.
    #[test]
    fn oversized_manifest_skips_preflight_and_errors_via_append() {
        let tmp = tempfile::tempdir().unwrap();
        let exec_root = tmp.path().to_path_buf();
        // Write a manifest just barely over 16 MiB so the preflight cap
        // skips the read and append's own read_capped surfaces FileTooLarge.
        let oversized: Vec<u8> = vec![b'a'; 16 * 1024 * 1024 + 8];
        fs::write(exec_root.join("tasks.json"), &oversized).unwrap();

        let findings_path = exec_root.join("f.json");
        fs::write(&findings_path, br#"{"findings":[]}"#).unwrap();

        struct NeverInvoker;
        impl crate::compile::CompileInvoker for NeverInvoker {
            fn invoke(&self, _: &[&Path]) -> Result<(), String> {
                panic!("must not be called when manifest exceeds cap")
            }
        }

        let plan_path = std::path::PathBuf::from("/tmp/plan.md");
        let err = run_compile_fix_waves_with_invoker(
            &NeverInvoker,
            &plan_path,
            &exec_root,
            &findings_path,
        )
        .expect_err("oversized manifest must error");
        let _ = err;
    }

    #[test]
    fn parse_subagent_done_index_accepts_done() {
        assert_eq!(
            parse_subagent_done_index("⏺ [plan-executor] sub-agent 3 done (1234 chars)"),
            Some(3)
        );
    }

    #[test]
    fn parse_subagent_done_index_accepts_failed() {
        assert_eq!(
            parse_subagent_done_index("⏺ [plan-executor] sub-agent 1 failed: boom"),
            Some(1)
        );
    }

    #[test]
    fn parse_subagent_done_index_accepts_skipped() {
        assert_eq!(
            parse_subagent_done_index("⏺ [plan-executor] sub-agent 2 skipped (can-fail): reason"),
            Some(2)
        );
    }

    #[test]
    fn parse_subagent_done_index_ignores_dispatching() {
        assert_eq!(
            parse_subagent_done_index("⏺ [plan-executor] dispatching 4 sub-agent(s) (phase: x)"),
            None
        );
    }

    #[test]
    fn parse_subagent_done_index_ignores_unrelated() {
        assert_eq!(parse_subagent_done_index("some other line"), None);
    }

    fn write_manifest(json: serde_json::Value) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "{}", json).unwrap();
        f
    }

    #[test]
    fn read_manifest_plan_block_returns_explicit_remote_execution_mode() {
        let manifest = write_manifest(serde_json::json!({
            "version": 1,
            "plan": {
                "path": "/tmp/plan.md",
                "status": "READY",
                "execution_mode": "remote"
            },
            "waves": [],
            "tasks": {}
        }));
        let (path, status, mode) = read_manifest_plan_block(manifest.path()).unwrap();
        assert_eq!(path, std::path::PathBuf::from("/tmp/plan.md"));
        assert_eq!(status, "READY");
        assert_eq!(mode, "remote");
    }

    #[test]
    fn read_manifest_plan_block_defaults_execution_mode_to_local_when_missing() {
        // Pre-execution_mode manifests must still load. The reader treats a
        // missing field as "local" so older compiled tasks.json files don't
        // need recompilation just to be readable.
        let manifest = write_manifest(serde_json::json!({
            "version": 1,
            "plan": {
                "path": "/tmp/plan.md",
                "status": "READY"
            },
            "waves": [],
            "tasks": {}
        }));
        let (_, _, mode) = read_manifest_plan_block(manifest.path()).unwrap();
        assert_eq!(mode, "local");
    }
}
