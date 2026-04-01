# Plan Executor Implementation Plan

**Goal:** Build a Rust tool (`plan-executor`) that monitors directories for ready plan files, executes them via the claude CLI, and provides a daemon + TUI interface for managing executions.
**Type:** Feature
**JIRA:** none
**Tech Stack:** Rust (edition 2024), ratatui + crossterm (TUI), tokio (async), notify (FSEvents), notify-rust (macOS notifications), Unix domain socket (IPC)
**Code Standards:** rust-services:production-code-recipe, rust-services:test-code-recipe
**Status:** COMPLETED

---

## Context

A standalone Rust binary that:
1. Runs as a background daemon monitoring configured directories for plan files matching configured glob patterns
2. Detects plans with `**Status:** READY` header and sends macOS native notifications
3. Supports `auto-execute` mode (15s countdown) or manual execute/cancel
4. Executes plans via: `claude --dangerously-skip-permissions --verbose --output-format stream-json -p "/my:execute-plan-non-interactive <path>"`
   The execution is multi-turn per `execute-plan-non-interactive/HANDOFF_PROTOCOL.md`: the skill stops and emits handoff lines (`call sub-agent N (agent-type: type): path`), writes `.tmp-execute-plan-state.json`, and waits for continuation. The daemon dispatches sub-agents (claude/codex/gemini) in parallel, collects outputs, and resumes via `claude --resume <session_id> -p "# output sub-agent N:\n..."` until no state file remains.
5. Parses stream-json output to track tokens/cost (using `~/.claude-code-proxy/pricing.json`)
6. Provides a TUI (ratatui) with two tabs: running executions and history
7. Daemon and TUI communicate via Unix domain socket at `~/.plan-executor/daemon.sock`
8. All data persisted to `~/.plan-executor/` (config, job outputs, job metadata)

**Out of scope:** multiple concurrent executions per plan, remote/SSH, plan editing in TUI, auto-install claude.

**Pricing JSON format** (from `~/.claude-code-proxy/pricing.json`):
```json
{
  "claude-sonnet-4-6": {
    "inputPerMtok": 3.0,
    "cacheWrite5mPerMtok": 3.75,
    "cacheWrite1hPerMtok": 6.0,
    "cacheReadPerMtok": 0.3,
    "outputPerMtok": 15.0,
    "inputPerMtokLong": 6.0,
    "outputPerMtokLong": 22.5
  }
}
```

**Claude stream-json events** (NDJSON lines, per Agent SDK spec):
- `{"type":"system","subtype":"init","model":"claude-...","session_id":"...",...}` — first event, contains model
- `{"type":"assistant","message":{"content":[...],"usage":{...},...}}` — assistant turn; content blocks:
  - `{"type":"text","text":"..."}` — text response
  - `{"type":"tool_use","id":"...","name":"ToolName","input":{...}}` — tool call
- `{"type":"user","message":{"content":[...]}}` — user turn; content blocks:
  - `{"type":"tool_result","tool_use_id":"...","content":[{"type":"text","text":"..."}]}` — tool output
- `{"type":"tool_progress","tool_name":"...","tool_use_id":"...","elapsed_time_seconds":N}` — tool in-progress
- `{"type":"result","subtype":"success","total_cost_usd":N,"usage":{...},"duration_ms":N,"modelUsage":{...}}` — final result
- `{"type":"result","subtype":"error_max_turns"|"error_during_execution"|...,"errors":[...]}` — failed result

Use `~/workspace/code/stream-json-view` for parsing the event stream and human readable visualization in the TUI.

**Config file** (`~/.plan-executor/config.json`):
```json
{
  "watch_dirs": ["~/workspace/code", "~/tools"],
  "plan_patterns": [".my/plans/*.md"],
  "auto_execute": false
}
```

**Job metadata** stored per-job at `~/.plan-executor/jobs/<job-id>/`:
- `metadata.json` — plan path, start time, end time, model, token counts, cost, status
- `output.jsonl` — raw stream-json lines from claude

## Acceptance Criteria

- [ ] Config loaded from `~/.plan-executor/config.json`; missing config uses defaults with user's home as base
- [ ] Daemon watches configured dirs using macOS FSEvents (via `notify` crate with fsevent backend)
- [ ] Plan files matching patterns are scanned on startup and on FS change events
- [ ] Plans with `**Status:** READY` trigger macOS notification
- [ ] auto-execute=false: notification has action buttons (Execute / Cancel via macOS UNNotification or terminal prompt)
- [ ] auto-execute=true: notification states execution starts in 15s; daemon waits then auto-runs
- [ ] Execution runs `claude` subprocess with correct flags, streams output to job output file
- [ ] `session_id` captured from `system/init` stream-json event and stored on `JobMetadata`
- [ ] Execution follows HANDOFF_PROTOCOL: daemon detects `.tmp-execute-plan-state.json` on process exit and enters handoff loop
- [ ] Sub-agents dispatched in parallel per `agent-type`: `claude` → `claude --dangerously-skip-permissions -p "..."`, `codex` → `codex --dangerously-bypass-approvals-and-sandbox exec "..."`, `gemini` → `gemini --yolo -p "..."`
- [ ] Sub-agent failures surfaced to TUI as job output lines with stderr context
- [ ] Orchestrator session resumed via `claude --resume <session_id> -p "<continuation>"` after each batch
- [ ] Handoff loop repeats until no `.tmp-execute-plan-state.json` present (execution complete)
- [ ] stream-json parsed: model extracted from `system/init`, usage from `result` event, cost calculated from pricing.json
- [ ] Job metadata written to `~/.plan-executor/jobs/<job-id>/metadata.json`
- [ ] Completion notification sent after execution (shown for 15s, done via OS)
- [ ] Daemon listens on `~/.plan-executor/daemon.sock` Unix socket
- [ ] TUI connects to daemon socket and shows live state
- [ ] TUI Tab 1: running executions with live output (scrollable); kill button
- [ ] TUI Tab 2: history of completed jobs with time, tokens, cost; select to view output
- [ ] Binary has subcommands: `daemon` (start daemon), `tui` (attach TUI), `status` (print daemon status)

---

### Task 1: Project scaffold and Cargo.toml

**Files:**
- Create: `plan-executor/Cargo.toml`
- Create: `plan-executor/src/main.rs`
- Create: `plan-executor/src/lib.rs`
- Create: `plan-executor/.gitignore`

**Step 1: Create the Cargo workspace member**

In `/Users/andreas.pohl/tools/`, check if there is a `Cargo.toml` workspace. If not, create a new standalone project. Create `plan-executor/` directory.

Create `plan-executor/Cargo.toml`:
```toml
[package]
name = "plan-executor"
version = "0.1.0"
edition = "2024"
description = "Monitor and execute Claude plan files"
default-run = "plan-executor"

[[bin]]
name = "plan-executor"
path = "src/main.rs"

[dependencies]
# Async runtime (includes process feature for subprocess spawning)
tokio = { version = "1", features = ["full"] }

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# CLI parsing
clap = { version = "4", features = ["derive"] }

# Logging
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# File system watching (macOS FSEvents backend)
notify = "8"

# Glob pattern matching
glob = "0.3"

# macOS native notifications
notify-rust = { version = "4", features = ["d"] }

# TUI
ratatui = "0.29"
crossterm = "0.28"

# Error handling
thiserror = "2"
anyhow = "1"

# Home directory
dirs = "6"

# Time
chrono = { version = "0.4", features = ["serde"] }

# UUID for job IDs
uuid = { version = "1", features = ["v4"] }

[dev-dependencies]
tempfile = "3"
tokio-test = "0.4"
```

Create `plan-executor/src/main.rs`:
```rust
mod cli;
mod config;
mod daemon;
mod handoff;
mod ipc;
mod jobs;
mod notifications;
mod plan;
mod tui;
mod watcher;

pub use config::Config;

fn main() {
    cli::run();
}
```

Create `plan-executor/src/lib.rs` as empty (or re-export modules for tests).

Create `plan-executor/.gitignore`:
```
/target
```

**Step 2: Verify**

```
cd /Users/andreas.pohl/tools/plan-executor && cargo check
```
Expected: compiles with no errors (empty modules may need stub files).

**Step 3: Commit**
```
feat(plan-executor): initial project scaffold with Cargo.toml
```

---

### Task 2: Config module

**Files:**
- Create: `plan-executor/src/config.rs`

**Step 1: Implement config**

Create `plan-executor/src/config.rs`:
```rust
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use anyhow::Result;

/// Application configuration loaded from ~/.plan-executor/config.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Directories to watch for plan files (tilde-expanded)
    pub watch_dirs: Vec<String>,
    /// Glob patterns relative to each watch_dir, e.g. [".my/plans/*.md"]
    pub plan_patterns: Vec<String>,
    /// If true, auto-execute READY plans after 15s countdown
    pub auto_execute: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            watch_dirs: vec!["~/tools".to_string()],
            plan_patterns: vec![".my/plans/*.md".to_string()],
            auto_execute: false,
        }
    }
}

impl Config {
    /// Returns the base directory: ~/.plan-executor/
    pub fn base_dir() -> PathBuf {
        dirs::home_dir()
            .expect("home dir must exist")
            .join(".plan-executor")
    }

    /// Returns the config file path: ~/.plan-executor/config.json
    pub fn config_path() -> PathBuf {
        Self::base_dir().join("config.json")
    }

    /// Loads config from disk; returns Default if file does not exist.
    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)?;
        let config: Self = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// Expands tilde in watch_dirs to absolute paths.
    pub fn expanded_watch_dirs(&self) -> Vec<PathBuf> {
        self.watch_dirs
            .iter()
            .map(|d| expand_tilde(d))
            .collect()
    }
}

/// Expands a leading `~/` to the home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .expect("home dir must exist")
            .join(rest)
    } else {
        PathBuf::from(path)
    }
}
```

**Step 2: Verify**
```
cd /Users/andreas.pohl/tools/plan-executor && cargo check
```

**Step 3: Commit**
```
feat(plan-executor): add config module with JSON loading and tilde expansion
```

---

### Task 3: Plan file parsing module

**Files:**
- Create: `plan-executor/src/plan.rs`

**Step 1: Implement plan scanner**

Create `plan-executor/src/plan.rs`:
```rust
use std::path::{Path, PathBuf};
use anyhow::Result;

/// Represents a discovered plan file.
#[derive(Debug, Clone)]
pub struct PlanFile {
    pub path: PathBuf,
    pub status: PlanStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlanStatus {
    Ready,
    Wip,
    Executing,
    Completed,
    Unknown(String),
}

impl PlanStatus {
    fn from_str(s: &str) -> Self {
        match s.trim() {
            "READY" => PlanStatus::Ready,
            "WIP" => PlanStatus::Wip,
            "EXECUTING" => PlanStatus::Executing,
            "COMPLETED" => PlanStatus::Completed,
            other => PlanStatus::Unknown(other.to_string()),
        }
    }
}

/// Reads a plan file and extracts its **Status:** field.
pub fn parse_plan_status(path: &Path) -> Result<PlanStatus> {
    let content = std::fs::read_to_string(path)?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("**Status:**") {
            return Ok(PlanStatus::from_str(rest));
        }
    }
    Ok(PlanStatus::Unknown("missing".to_string()))
}

/// Scans a directory for files matching a glob pattern.
/// Returns all matching paths.
pub fn scan_for_plans(base_dir: &Path, pattern: &str) -> Vec<PathBuf> {
    let full_pattern = base_dir.join(pattern);
    let pattern_str = full_pattern.to_string_lossy();
    match glob::glob(&pattern_str) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|p| p.is_file())
            .collect(),
        Err(_) => vec![],
    }
}

/// Scans all watch_dirs with all patterns and returns READY plan files.
pub fn find_ready_plans(watch_dirs: &[PathBuf], patterns: &[String]) -> Vec<PlanFile> {
    let mut results = Vec::new();
    for dir in watch_dirs {
        for pattern in patterns {
            for path in scan_for_plans(dir, pattern) {
                if let Ok(status) = parse_plan_status(&path) {
                    if status == PlanStatus::Ready {
                        results.push(PlanFile { path, status });
                    }
                }
            }
        }
    }
    results
}
```

**Step 2: Verify**
```
cd /Users/andreas.pohl/tools/plan-executor && cargo check
```

**Step 3: Commit**
```
feat(plan-executor): add plan file parsing with status detection
```

---

### Task 4: Job management module

**Depends on:** Task 2 (Config), Task 3 (PlanFile)

**Files:**
- Create: `plan-executor/src/jobs.rs`

**Step 1: Implement job types and persistence**

Jobs are stored at `~/.plan-executor/jobs/<uuid>/`:
- `metadata.json`: job state
- `output.jsonl`: raw NDJSON lines from claude

Create `plan-executor/src/jobs.rs`:
```rust
use std::path::PathBuf;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use anyhow::Result;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum JobStatus {
    Running,
    Success,
    Failed,
    Killed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobMetadata {
    pub id: String,
    pub plan_path: PathBuf,
    pub status: JobStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Model used (from stream-json system/init event)
    pub model: Option<String>,
    /// Total input tokens (from result event)
    pub input_tokens: Option<u64>,
    /// Total output tokens (from result event)
    pub output_tokens: Option<u64>,
    /// Cache write tokens
    pub cache_creation_tokens: Option<u64>,
    /// Cache read tokens
    pub cache_read_tokens: Option<u64>,
    /// Calculated cost in USD
    pub cost_usd: Option<f64>,
    /// Duration in milliseconds (from result event)
    pub duration_ms: Option<u64>,
    /// Claude session ID (from stream-json system/init), used for --resume in handoff loop
    pub session_id: Option<String>,
}

impl JobMetadata {
    pub fn new(plan_path: PathBuf) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            plan_path,
            status: JobStatus::Running,
            started_at: Utc::now(),
            finished_at: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            cache_creation_tokens: None,
            cache_read_tokens: None,
            cost_usd: None,
            duration_ms: None,
            session_id: None,
        }
    }

    /// Returns the job's directory under ~/.plan-executor/jobs/<id>/
    pub fn job_dir(&self) -> PathBuf {
        crate::Config::base_dir().join("jobs").join(&self.id)
    }

    pub fn metadata_path(&self) -> PathBuf {
        self.job_dir().join("metadata.json")
    }

    pub fn output_path(&self) -> PathBuf {
        self.job_dir().join("output.jsonl")
    }

    /// Persists metadata to disk.
    pub fn save(&self) -> Result<()> {
        let dir = self.job_dir();
        std::fs::create_dir_all(&dir)?;
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(self.metadata_path(), json)?;
        Ok(())
    }

    /// Loads all jobs from ~/.plan-executor/jobs/
    pub fn load_all() -> Vec<Self> {
        let jobs_dir = crate::Config::base_dir().join("jobs");
        let Ok(entries) = std::fs::read_dir(&jobs_dir) else {
            return vec![];
        };
        let mut jobs = Vec::new();
        for entry in entries.flatten() {
            let meta_path = entry.path().join("metadata.json");
            if let Ok(content) = std::fs::read_to_string(&meta_path) {
                if let Ok(meta) = serde_json::from_str::<Self>(&content) {
                    jobs.push(meta);
                }
            }
        }
        // Sort by started_at descending
        jobs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        jobs
    }
}
```

**Step 2: Verify**
```
cd /Users/andreas.pohl/tools/plan-executor && cargo check
```

**Step 3: Commit**
```
feat(plan-executor): add job metadata and persistence module
```

---

### Task 5: Pricing and cost calculation

**Files:**
- Create: `plan-executor/src/pricing.rs`

**Step 1: Implement pricing loader and cost calculator**

Pricing file at `~/.claude-code-proxy/pricing.json` has format:
```json
{
  "claude-sonnet-4-6": {
    "inputPerMtok": 3.0,
    "cacheWrite5mPerMtok": 3.75,
    "outputPerMtok": 15.0,
    "cacheReadPerMtok": 0.3
  }
}
```
Cost = `(input_tokens * inputPerMtok + output_tokens * outputPerMtok + cache_creation * cacheWrite5mPerMtok + cache_read * cacheReadPerMtok) / 1_000_000`

Create `plan-executor/src/pricing.rs`:
```rust
use std::collections::HashMap;
use std::path::PathBuf;
use serde::Deserialize;
use anyhow::Result;

#[derive(Debug, Clone, Deserialize)]
pub struct ModelPricing {
    #[serde(rename = "inputPerMtok")]
    pub input_per_mtok: f64,
    #[serde(rename = "outputPerMtok")]
    pub output_per_mtok: f64,
    #[serde(rename = "cacheWrite5mPerMtok", default)]
    pub cache_write_per_mtok: f64,
    #[serde(rename = "cacheReadPerMtok", default)]
    pub cache_read_per_mtok: f64,
}

pub type PricingTable = HashMap<String, ModelPricing>;

pub fn pricing_path() -> PathBuf {
    dirs::home_dir()
        .expect("home dir must exist")
        .join(".claude-code-proxy")
        .join("pricing.json")
}

/// Loads pricing from ~/.claude-code-proxy/pricing.json.
/// Returns empty table if file does not exist.
pub fn load_pricing() -> PricingTable {
    let path = pricing_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// Calculates cost in USD for a job given token counts and model.
pub fn calculate_cost(
    pricing: &PricingTable,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
) -> Option<f64> {
    // Try exact match, then prefix match (e.g. "claude-sonnet-4-6[1m]" -> "claude-sonnet-4-6")
    let p = pricing.get(model).or_else(|| {
        pricing.iter().find(|(k, _)| model.starts_with(k.as_str())).map(|(_, v)| v)
    })?;

    let cost = (input_tokens as f64 * p.input_per_mtok
        + output_tokens as f64 * p.output_per_mtok
        + cache_creation_tokens as f64 * p.cache_write_per_mtok
        + cache_read_tokens as f64 * p.cache_read_per_mtok)
        / 1_000_000.0;
    Some(cost)
}
```

**Step 2: Verify**
```
cd /Users/andreas.pohl/tools/plan-executor && cargo check
```

**Step 3: Commit**
```
feat(plan-executor): add pricing module with cost calculation from pricing.json
```

---

### Task 6: IPC protocol (daemon ↔ TUI)

**Files:**
- Create: `plan-executor/src/ipc.rs`

**Step 1: Define IPC message types and socket helpers**

Communication over Unix socket is NDJSON (one JSON object per line).

Create `plan-executor/src/ipc.rs`:
```rust
use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use crate::jobs::JobMetadata;
use crate::Config;

pub fn socket_path() -> PathBuf {
    Config::base_dir().join("daemon.sock")
}

/// Messages sent from TUI → Daemon
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TuiRequest {
    /// Subscribe to state updates (daemon streams responses)
    Subscribe,
    /// Execute a plan immediately
    Execute { plan_path: String },
    /// Cancel pending execution (within 15s window)
    CancelPending { plan_path: String },
    /// Kill a running job
    KillJob { job_id: String },
    /// Request full state snapshot
    GetState,
}

/// Messages sent from Daemon → TUI
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonEvent {
    /// Full state snapshot (response to Subscribe/GetState)
    State {
        running_jobs: Vec<JobMetadata>,
        pending_plans: Vec<PendingPlan>,
        history: Vec<JobMetadata>,
    },
    /// A job's output line arrived
    JobOutput { job_id: String, line: String },
    /// A job's metadata changed (status, tokens, cost)
    JobUpdated { job: JobMetadata },
    /// A new READY plan was detected
    PlanReady { plan_path: String, auto_execute: bool },
    /// Error response
    Error { message: String },
}

/// A plan detected as READY, pending user action or countdown
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPlan {
    pub plan_path: String,
    /// Seconds remaining before auto-execute (None if manual mode)
    pub auto_execute_remaining_secs: Option<u64>,
}
```

**Step 2: Add async read/write helpers** — add to `ipc.rs`:
```rust
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use anyhow::Result;

/// Sends a message as a JSON line over a UnixStream.
pub async fn send_msg<T: Serialize>(stream: &mut UnixStream, msg: &T) -> Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;
    Ok(())
}

/// Reads one JSON line from a BufReader<UnixStream>.
pub async fn recv_msg<T: for<'de> Deserialize<'de>>(
    reader: &mut BufReader<UnixStream>,
) -> Result<T> {
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let msg = serde_json::from_str(line.trim())?;
    Ok(msg)
}
```

**Step 3: Verify**
```
cd /Users/andreas.pohl/tools/plan-executor && cargo check
```

**Step 4: Commit**
```
feat(plan-executor): add IPC protocol with Unix socket message types
```

---

### Task 7: Notifications module

**Files:**
- Create: `plan-executor/src/notifications.rs`

**Step 1: Implement macOS notification helpers**

`notify-rust` on macOS uses the native notification center.

Create `plan-executor/src/notifications.rs`:
```rust
use anyhow::Result;

/// Sends a native macOS notification for a READY plan.
/// Shows plan filename and either auto-execute countdown or action hint.
pub fn notify_plan_ready(plan_path: &str, auto_execute: bool) -> Result<()> {
    let filename = std::path::Path::new(plan_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(plan_path);

    let body = if auto_execute {
        "Auto-executing in 15 seconds. Open TUI to cancel.".to_string()
    } else {
        "Open TUI to execute or cancel.".to_string()
    };

    notify_rust::Notification::new()
        .summary("Plan Ready")
        .body(&format!("{}\n{}", filename, body))
        .show()?;
    Ok(())
}

/// Sends a macOS notification that a plan execution completed.
pub fn notify_execution_complete(plan_path: &str, success: bool, cost_usd: Option<f64>) -> Result<()> {
    let filename = std::path::Path::new(plan_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(plan_path);

    let status = if success { "succeeded" } else { "failed" };
    let cost_str = cost_usd
        .map(|c| format!(" (${:.4})", c))
        .unwrap_or_default();

    notify_rust::Notification::new()
        .summary(&format!("Plan {}", status))
        .body(&format!("{}{}", filename, cost_str))
        .show()?;
    Ok(())
}
```

**Step 2: Verify**
```
cd /Users/andreas.pohl/tools/plan-executor && cargo check
```

**Step 3: Commit**
```
feat(plan-executor): add macOS notification helpers via notify-rust
```

---

### Task 8: File system watcher module

**Depends on:** Task 2 (Config), Task 3 (plan)

**Files:**
- Create: `plan-executor/src/watcher.rs`

**Step 1: Implement FSEvents watcher**

The `notify` crate's `RecommendedWatcher` on macOS uses FSEvents. We watch top-level dirs (not recursive) and filter events for paths that match our plan patterns.

Create `plan-executor/src/watcher.rs`:
```rust
use std::path::PathBuf;
use notify::{RecommendedWatcher, RecursiveMode, Watcher, Config as NotifyConfig, EventKind};
use notify::event::{CreateKind, ModifyKind};
use tokio::sync::mpsc;
use anyhow::Result;

/// Event from the watcher: a path was created or modified.
#[derive(Debug)]
pub struct WatchEvent {
    pub path: PathBuf,
}

/// Starts watching the given directories (non-recursive).
/// Returns a channel receiver for watch events.
pub fn start_watcher(
    watch_dirs: Vec<PathBuf>,
) -> Result<(RecommendedWatcher, mpsc::Receiver<WatchEvent>)> {
    let (tx, rx) = mpsc::channel::<WatchEvent>(64);

    let mut watcher = RecommendedWatcher::new(
        move |result: notify::Result<notify::Event>| {
            if let Ok(event) = result {
                // Only care about creates and modifications
                let relevant = matches!(
                    event.kind,
                    EventKind::Create(CreateKind::File)
                        | EventKind::Modify(ModifyKind::Data(_))
                        | EventKind::Modify(ModifyKind::Any)
                );
                if relevant {
                    for path in event.paths {
                        if path.extension().and_then(|e| e.to_str()) == Some("md") {
                            let _ = tx.blocking_send(WatchEvent { path });
                        }
                    }
                }
            }
        },
        NotifyConfig::default(),
    )?;

    for dir in &watch_dirs {
        // Non-recursive: only watch the top-level dir
        // Events for subdirs come through because FSEvents reports full paths
        // We use NonRecursive to avoid deep tree scanning overhead
        watcher.watch(dir, RecursiveMode::Recursive)?;
        // Note: use Recursive here because .my/plans/ is a subdir of watch_dirs.
        // FSEvents does not do recursive inode scanning; it uses kernel events.
        // This is efficient on macOS regardless of depth.
    }

    Ok((watcher, rx))
}
```

Note: `notify` with FSEvents backend on macOS is inherently event-driven — no polling. `RecursiveMode::Recursive` on macOS uses FSEvents stream which is efficient and kernel-driven, not a recursive inode scan.

**Step 2: Verify**
```
cd /Users/andreas.pohl/tools/plan-executor && cargo check
```

**Step 3: Commit**
```
feat(plan-executor): add FSEvents-based directory watcher
```

---

### Task 9: Execution engine (claude subprocess + stream-json parser)

**Depends on:** Task 4 (jobs), Task 5 (pricing)

**Files:**
- Create: `plan-executor/src/executor.rs`

**Step 1: Implement claude subprocess runner**

Create `plan-executor/src/executor.rs`:
```rust
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use serde::Deserialize;
use anyhow::Result;
use crate::jobs::{JobMetadata, JobStatus};
use crate::pricing::{calculate_cost, load_pricing};
use crate::Config;

/// Events emitted during execution
#[derive(Debug)]
pub enum ExecEvent {
    OutputLine(String),
    /// Emitted when the claude process exits and `.tmp-execute-plan-state.json` is present.
    /// The daemon must dispatch sub-agents and resume via `handoff::resume_execution`.
    HandoffRequired { session_id: String, state_file: PathBuf },
    Finished(JobMetadata),
}

/// Parsed fields from claude stream-json
#[derive(Debug, Deserialize, Default)]
struct StreamEvent {
    #[serde(rename = "type")]
    event_type: String,
    // For "system" type
    model: Option<String>,
    session_id: Option<String>,
    // For "result" type
    total_cost_usd: Option<f64>,
    duration_ms: Option<u64>,
    usage: Option<UsageBlock>,
}

#[derive(Debug, Deserialize, Default)]
struct UsageBlock {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

/// Spawns claude and returns a child handle and an event receiver.
/// `execution_root` is the repo/worktree root — where `.tmp-execute-plan-state.json` will be written.
pub fn spawn_execution(
    mut job: JobMetadata,
    execution_root: PathBuf,
) -> Result<(Child, mpsc::Receiver<ExecEvent>)> {
    let plan_path = job.plan_path.to_string_lossy().to_string();
    let cmd_arg = format!("/my:execute-plan-non-interactive {}", plan_path);

    let mut child = Command::new("claude")
        .args([
            "--dangerously-skip-permissions",
            "--verbose",
            "--output-format",
            "stream-json",
            "-p",
            &cmd_arg,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let stdout = child.stdout.take().expect("stdout must be piped");
    let (tx, rx) = mpsc::channel::<ExecEvent>(256);

    // Prepare output file
    std::fs::create_dir_all(job.job_dir())?;
    let output_path = job.output_path();
    job.save()?;

    tokio::spawn(async move {
        let pricing = load_pricing();
        let mut reader = BufReader::new(stdout).lines();
        let mut output_file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&output_path)
            .await
            .ok();

        while let Ok(Some(line)) = reader.next_line().await {
            // Write to output file
            if let Some(ref mut f) = output_file {
                use tokio::io::AsyncWriteExt;
                let _ = f.write_all(format!("{}\n", line).as_bytes()).await;
            }

            // Parse stream-json
            if let Ok(event) = serde_json::from_str::<StreamEvent>(&line) {
                match event.event_type.as_str() {
                    "system" => {
                        if let Some(model) = event.model {
                            job.model = Some(model);
                        }
                        if let Some(sid) = event.session_id {
                            job.session_id = Some(sid);
                        }
                    }
                    "result" => {
                        if let Some(usage) = event.usage {
                            job.input_tokens = usage.input_tokens;
                            job.output_tokens = usage.output_tokens;
                            job.cache_creation_tokens = usage.cache_creation_input_tokens;
                            job.cache_read_tokens = usage.cache_read_input_tokens;
                        }
                        job.duration_ms = event.duration_ms;
                        // Calculate cost from pricing.json
                        if let Some(model) = &job.model {
                            job.cost_usd = calculate_cost(
                                &pricing,
                                model,
                                job.input_tokens.unwrap_or(0),
                                job.output_tokens.unwrap_or(0),
                                job.cache_creation_tokens.unwrap_or(0),
                                job.cache_read_tokens.unwrap_or(0),
                            );
                        }
                    }
                    _ => {}
                }
            }

            let _ = tx.send(ExecEvent::OutputLine(line)).await;
        }

        // stdout closed — check for handoff pause before declaring finished
        let state_file = execution_root.join(".tmp-execute-plan-state.json");
        if state_file.exists() {
            if let Some(sid) = job.session_id.clone() {
                let _ = job.save();
                let _ = tx.send(ExecEvent::HandoffRequired {
                    session_id: sid,
                    state_file,
                }).await;
                return; // caller loop will resume; do NOT emit Finished here
            }
        }

        job.status = JobStatus::Success;
        job.finished_at = Some(chrono::Utc::now());
        let _ = job.save();
        let _ = tx.send(ExecEvent::Finished(job)).await;
    });

    Ok((child, rx))
}
```

**Step 2: Verify**
```
cd /Users/andreas.pohl/tools/plan-executor && cargo check
```

**Step 3: Commit**
```
feat(plan-executor): add execution engine with claude subprocess and stream-json parsing
```

---

### Task 9b: Handoff protocol module

**Depends on:** Task 9 (executor — provides `ExecEvent`, `JobMetadata`)

**Reference:** `~/tools/claude/my-plugin/plugins/my/skills/execute-plan-non-interactive/HANDOFF_PROTOCOL.md` — authoritative contract for handoff line format, state file schema, agent-type values, and continuation format.

**Files:**
- Create: `plan-executor/src/handoff.rs`

**Step 1: Create `handoff.rs`**

```rust
use std::path::{Path, PathBuf};
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::mpsc;
use anyhow::Result;

use crate::executor::ExecEvent;
use crate::jobs::{JobMetadata, JobStatus};
use crate::pricing::load_pricing;

// ── Types ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum AgentType {
    Claude,
    Codex,
    Gemini,
}

#[derive(Debug, Clone)]
pub struct Handoff {
    pub index: usize,
    pub agent_type: AgentType,
    pub prompt_file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct HandoffState {
    pub phase: String,
    pub handoffs: Vec<Handoff>,
}

#[derive(Debug, Clone)]
pub struct HandoffResult {
    pub index: usize,
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

// ── State file deserialization ─────────────────────────────────────────────

#[derive(Deserialize)]
struct RawHandoff {
    index: usize,
    #[serde(rename = "agentType")]
    agent_type: String,
    #[serde(rename = "promptFile")]
    prompt_file: String,
}

#[derive(Deserialize)]
struct RawState {
    phase: String,
    handoffs: Vec<RawHandoff>,
}

/// Reads `.tmp-execute-plan-state.json` and returns parsed `HandoffState`.
pub fn load_state(state_file: &Path) -> Result<HandoffState> {
    let content = std::fs::read_to_string(state_file)?;
    let raw: RawState = serde_json::from_str(&content)?;
    let handoffs = raw
        .handoffs
        .into_iter()
        .map(|h| Handoff {
            index: h.index,
            agent_type: match h.agent_type.as_str() {
                "claude" => AgentType::Claude,
                "codex"  => AgentType::Codex,
                "gemini" => AgentType::Gemini,
                other => {
                    tracing::warn!("unknown agent-type '{}', defaulting to claude", other);
                    AgentType::Claude
                }
            },
            prompt_file: PathBuf::from(h.prompt_file),
        })
        .collect();
    Ok(HandoffState { phase: raw.phase, handoffs })
}

// ── Sub-agent dispatch ─────────────────────────────────────────────────────

async fn dispatch_agent(handoff: Handoff) -> HandoffResult {
    let prompt = match std::fs::read_to_string(&handoff.prompt_file) {
        Ok(c) => c,
        Err(e) => return HandoffResult {
            index: handoff.index,
            stdout: String::new(),
            stderr: format!("failed to read prompt file {:?}: {}", handoff.prompt_file, e),
            success: false,
        },
    };

    let output = match &handoff.agent_type {
        AgentType::Claude => Command::new("claude")
            .args(["--dangerously-skip-permissions", "-p", &prompt])
            .output().await,
        AgentType::Codex => Command::new("codex")
            .args(["--dangerously-bypass-approvals-and-sandbox", "exec", &prompt])
            .output().await,
        AgentType::Gemini => Command::new("gemini")
            .args(["--yolo", "-p", &prompt])
            .output().await,
    };

    match output {
        Ok(out) => HandoffResult {
            index: handoff.index,
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            success: out.status.success(),
        },
        Err(e) => HandoffResult {
            index: handoff.index,
            stdout: String::new(),
            stderr: format!("failed to spawn agent: {}", e),
            success: false,
        },
    }
}

/// Dispatches all handoffs in a batch concurrently. Returns results sorted by index.
pub async fn dispatch_all(handoffs: Vec<Handoff>) -> Vec<HandoffResult> {
    let handles: Vec<_> = handoffs.into_iter()
        .map(|h| tokio::spawn(dispatch_agent(h)))
        .collect();
    let mut results = Vec::new();
    for handle in handles {
        if let Ok(r) = handle.await { results.push(r); }
    }
    results.sort_by_key(|r| r.index);
    results
}

// ── Continuation builder ───────────────────────────────────────────────────

/// Builds the `--resume` continuation payload per HANDOFF_PROTOCOL §6.
/// Format: `# output sub-agent N:\n<stdout>\n\n# output sub-agent M:\n<stdout>`
pub fn build_continuation(results: &[HandoffResult]) -> String {
    let mut sorted = results.to_vec();
    sorted.sort_by_key(|r| r.index);
    let mut out = String::new();
    for r in &sorted {
        out.push_str(&format!("# output sub-agent {}:\n{}\n\n", r.index, r.stdout));
    }
    out.trim_end().to_string()
}

// ── Resume ─────────────────────────────────────────────────────────────────

/// Resumes the orchestrator session via `claude --resume <session_id> -p <continuation>`.
/// Returns a new (Child, Receiver<ExecEvent>) with the same shape as `spawn_execution`.
pub async fn resume_execution(
    session_id: &str,
    continuation: &str,
    execution_root: PathBuf,
) -> Result<(tokio::process::Child, mpsc::Receiver<ExecEvent>)> {
    use tokio::io::AsyncBufReadExt;

    let mut child = Command::new("claude")
        .args([
            "--dangerously-skip-permissions",
            "--verbose",
            "--output-format",
            "stream-json",
            "--resume",
            session_id,
            "-p",
            continuation,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let stdout = child.stdout.take().expect("stdout must be piped");
    let (tx, rx) = mpsc::channel::<ExecEvent>(256);
    let session_id_owned = session_id.to_string();

    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stdout).lines();
        let mut resumed_session_id = session_id_owned.clone();
        let mut resumed_model: Option<String> = None;
        let mut resumed_cost: Option<f64> = None;

        while let Ok(Some(line)) = reader.next_line().await {
            let _ = tx.send(ExecEvent::OutputLine(line.clone())).await;

            if let Ok(ev) = serde_json::from_str::<serde_json::Value>(&line) {
                match ev.get("type").and_then(|t| t.as_str()) {
                    Some("system") => {
                        if let Some(sid) = ev.get("session_id").and_then(|s| s.as_str()) {
                            resumed_session_id = sid.to_string();
                        }
                        if let Some(m) = ev.get("model").and_then(|m| m.as_str()) {
                            resumed_model = Some(m.to_string());
                        }
                    }
                    Some("result") => {
                        if let Some(c) = ev.get("total_cost_usd").and_then(|c| c.as_f64()) {
                            resumed_cost = Some(c);
                        }
                    }
                    _ => {}
                }
            }
        }

        // Check for another handoff pause
        let state_file = execution_root.join(".tmp-execute-plan-state.json");
        if state_file.exists() {
            let _ = tx.send(ExecEvent::HandoffRequired {
                session_id: resumed_session_id,
                state_file,
            }).await;
            return;
        }

        // Execution complete
        let mut placeholder = JobMetadata::new(PathBuf::from("<resumed>"));
        placeholder.session_id = Some(session_id_owned);
        placeholder.model = resumed_model;
        placeholder.cost_usd = resumed_cost;
        placeholder.status = JobStatus::Success;
        placeholder.finished_at = Some(chrono::Utc::now());
        let _ = tx.send(ExecEvent::Finished(placeholder)).await;
    });

    Ok((child, rx))
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn build_continuation_orders_by_index() {
        let results = vec![
            HandoffResult { index: 2, stdout: "out2".to_string(), stderr: String::new(), success: true },
            HandoffResult { index: 1, stdout: "out1".to_string(), stderr: String::new(), success: true },
        ];
        let s = build_continuation(&results);
        assert!(s.find("# output sub-agent 1:").unwrap() < s.find("# output sub-agent 2:").unwrap());
        assert!(s.contains("out1"));
        assert!(s.contains("out2"));
    }

    #[test]
    fn build_continuation_includes_empty_stdout_for_failed_agents() {
        let results = vec![
            HandoffResult { index: 1, stdout: String::new(), stderr: "error".to_string(), success: false },
        ];
        let s = build_continuation(&results);
        assert!(s.contains("# output sub-agent 1:"));
    }

    #[test]
    fn load_state_parses_all_agent_types() {
        let json = r#"{
            "phase": "wave_execution",
            "wave": 1, "attempt": 1, "batch": 1,
            "handoffs": [
                {"index": 1, "agentType": "claude", "promptFile": "/tmp/p1.md"},
                {"index": 2, "agentType": "codex",  "promptFile": "/tmp/p2.md"},
                {"index": 3, "agentType": "gemini", "promptFile": "/tmp/p3.md"}
            ]
        }"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        let state = load_state(f.path()).unwrap();
        assert_eq!(state.handoffs.len(), 3);
        assert!(matches!(state.handoffs[0].agent_type, AgentType::Claude));
        assert!(matches!(state.handoffs[1].agent_type, AgentType::Codex));
        assert!(matches!(state.handoffs[2].agent_type, AgentType::Gemini));
    }
}
```

**Step 2: Verify**
```
cd /Users/andreas.pohl/tools/plan-executor && cargo test -p plan-executor handoff
```
Expected: 3 tests pass.

**Step 3: Commit**
```
feat(plan-executor): add handoff module with state parsing, sub-agent dispatch, and session resume
```

---

### Task 10: Daemon module

**Depends on:** Task 6 (ipc), Task 7 (notifications), Task 8 (watcher), Task 9 (executor), Task 9b (handoff), Task 3 (plan), Task 2 (config)

**Files:**
- Create: `plan-executor/src/daemon.rs`

**Step 1: Implement daemon event loop**

The daemon:
1. Loads config
2. Scans for READY plans on startup
3. Starts FSEvents watcher
4. Listens on Unix socket for TUI clients
5. Manages pending plans (auto-execute countdown or waiting for user action)
6. Spawns execution when triggered
7. Broadcasts state updates to all connected TUI clients

Create `plan-executor/src/daemon.rs` — this is the core state machine. Key structures:

```rust
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::{broadcast, Mutex};
use tokio::time::{Duration, Instant};
use anyhow::Result;

use crate::config::Config;
use crate::executor::{spawn_execution, ExecEvent};
use crate::handoff;
use crate::ipc::{socket_path, DaemonEvent, PendingPlan, TuiRequest};
use crate::jobs::{JobMetadata, JobStatus};
use crate::notifications::{notify_execution_complete, notify_plan_ready};
use crate::plan::{find_ready_plans, parse_plan_status, PlanStatus};
use crate::watcher::start_watcher;

/// Shared daemon state
pub struct DaemonState {
    pub config: Config,
    pub running_jobs: HashMap<String, JobMetadata>, // job_id -> metadata
    pub pending_plans: HashMap<String, PendingInfo>, // plan_path -> info
    pub history: Vec<JobMetadata>,
    /// Per-job output buffers (last N lines)
    pub job_output: HashMap<String, Vec<String>>,
    /// broadcast channel for DaemonEvent to all TUI clients
    pub event_tx: broadcast::Sender<DaemonEvent>,
}

pub struct PendingInfo {
    pub plan_path: String,
    pub detected_at: Instant,
    pub auto_execute: bool,
}

impl DaemonState {
    pub fn snapshot_state(&self) -> DaemonEvent {
        DaemonEvent::State {
            running_jobs: self.running_jobs.values().cloned().collect(),
            pending_plans: self.pending_plans.values().map(|p| PendingPlan {
                plan_path: p.plan_path.clone(),
                auto_execute_remaining_secs: if p.auto_execute {
                    let elapsed = p.detected_at.elapsed().as_secs();
                    Some(15u64.saturating_sub(elapsed))
                } else {
                    None
                },
            }).collect(),
            history: self.history.clone(),
        }
    }
}

/// Main daemon entry point
pub async fn run_daemon() -> Result<()> {
    let config = Config::load()?;
    let watch_dirs = config.expanded_watch_dirs();

    // Ensure socket cleanup on start
    let sock_path = socket_path();
    if sock_path.exists() {
        std::fs::remove_file(&sock_path)?;
    }
    std::fs::create_dir_all(sock_path.parent().unwrap())?;

    let (event_tx, _) = broadcast::channel::<DaemonEvent>(256);

    let state = Arc::new(Mutex::new(DaemonState {
        config: config.clone(),
        running_jobs: HashMap::new(),
        pending_plans: HashMap::new(),
        history: JobMetadata::load_all()
            .into_iter()
            .filter(|j| j.status != JobStatus::Running)
            .collect(),
        job_output: HashMap::new(),
        event_tx: event_tx.clone(),
    }));

    // Scan on startup
    {
        let ready = find_ready_plans(&watch_dirs, &config.plan_patterns);
        let mut st = state.lock().await;
        for plan in ready {
            let path_str = plan.path.to_string_lossy().to_string();
            let _ = notify_plan_ready(&path_str, config.auto_execute);
            st.pending_plans.insert(path_str.clone(), PendingInfo {
                plan_path: path_str,
                detected_at: Instant::now(),
                auto_execute: config.auto_execute,
            });
        }
    }

    // Start watcher
    let (watcher, mut watch_rx) = start_watcher(watch_dirs.clone())?;
    let _watcher = watcher; // keep alive

    // Unix socket listener
    let listener = UnixListener::bind(&sock_path)?;

    // Ticker for auto-execute countdown (check every second)
    let mut interval = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            // FS event
            Some(watch_event) = watch_rx.recv() => {
                handle_watch_event(&state, &watch_dirs, &config, watch_event.path).await;
            }

            // New TUI client
            Ok((stream, _)) = listener.accept() => {
                let state_clone = Arc::clone(&state);
                let rx = event_tx.subscribe();
                tokio::spawn(handle_tui_client(stream, state_clone, rx));
            }

            // Auto-execute tick
            _ = interval.tick() => {
                handle_auto_execute_tick(&state).await;
            }
        }
    }
}

async fn handle_watch_event(
    state: &Arc<Mutex<DaemonState>>,
    watch_dirs: &[PathBuf],
    config: &Config,
    path: PathBuf,
) {
    let Ok(status) = parse_plan_status(&path) else { return };
    if status != PlanStatus::Ready { return }

    let path_str = path.to_string_lossy().to_string();
    let mut st = state.lock().await;

    // Skip if already pending or running
    if st.pending_plans.contains_key(&path_str) { return }
    if st.running_jobs.values().any(|j| j.plan_path == path) { return }

    let _ = notify_plan_ready(&path_str, config.auto_execute);
    st.pending_plans.insert(path_str.clone(), PendingInfo {
        plan_path: path_str.clone(),
        detected_at: Instant::now(),
        auto_execute: config.auto_execute,
    });
    let event = st.snapshot_state();
    let _ = st.event_tx.send(event);
}

async fn handle_auto_execute_tick(state: &Arc<Mutex<DaemonState>>) {
    let mut to_execute = Vec::new();
    {
        let st = state.lock().await;
        for (path, info) in &st.pending_plans {
            if info.auto_execute && info.detected_at.elapsed() >= Duration::from_secs(15) {
                to_execute.push(path.clone());
            }
        }
    }
    for path in to_execute {
        trigger_execution(state, &path).await;
    }
}

pub async fn trigger_execution(state: &Arc<Mutex<DaemonState>>, plan_path: &str) {
    let plan = PathBuf::from(plan_path);
    let execution_root = find_repo_root(&plan)
        .unwrap_or_else(|| plan.parent().unwrap_or(&plan).to_path_buf());

    let job = JobMetadata::new(plan.clone());
    let job_id = job.id.clone();

    let Ok((mut _child, mut exec_rx)) = spawn_execution(job.clone(), execution_root.clone()) else { return };

    {
        let mut st = state.lock().await;
        st.pending_plans.remove(plan_path);
        st.running_jobs.insert(job_id.clone(), job.clone());
        st.job_output.insert(job_id.clone(), Vec::new());
        let event = st.snapshot_state();
        let _ = st.event_tx.send(event);
    }

    let state_clone = Arc::clone(state);
    let plan_path_owned = plan_path.to_string();
    tokio::spawn(async move {
        'outer: loop {
        while let Some(event) = exec_rx.recv().await {
            match event {
                ExecEvent::OutputLine(line) => {
                    let mut st = state_clone.lock().await;
                    let buf = st.job_output.entry(job_id.clone()).or_default();
                    buf.push(line.clone());
                    // Keep last 10000 lines in memory
                    if buf.len() > 10000 { buf.remove(0); }
                    let _ = st.event_tx.send(DaemonEvent::JobOutput {
                        job_id: job_id.clone(),
                        line,
                    });
                }
                ExecEvent::HandoffRequired { session_id, state_file } => {
                    let state_data = match handoff::load_state(&state_file) {
                        Ok(s) => s,
                        Err(e) => {
                            let mut st = state_clone.lock().await;
                            let _ = st.event_tx.send(DaemonEvent::JobOutput {
                                job_id: job_id.clone(),
                                line: format!("[plan-executor] failed to read state file: {}", e),
                            });
                            break 'outer;
                        }
                    };

                    {
                        let mut st = state_clone.lock().await;
                        let _ = st.event_tx.send(DaemonEvent::JobOutput {
                            job_id: job_id.clone(),
                            line: format!(
                                "[plan-executor] dispatching {} sub-agent(s) (phase: {})",
                                state_data.handoffs.len(), state_data.phase
                            ),
                        });
                    }

                    let results = handoff::dispatch_all(state_data.handoffs).await;

                    for r in &results {
                        if !r.success {
                            let mut st = state_clone.lock().await;
                            let _ = st.event_tx.send(DaemonEvent::JobOutput {
                                job_id: job_id.clone(),
                                line: format!(
                                    "[plan-executor] sub-agent {} failed: {}",
                                    r.index,
                                    r.stderr.lines().next().unwrap_or("(no stderr)")
                                ),
                            });
                        }
                    }

                    let continuation = handoff::build_continuation(&results);
                    match handoff::resume_execution(&session_id, &continuation, execution_root.clone()).await {
                        Ok((_new_child, new_rx)) => {
                            exec_rx = new_rx;
                            continue 'outer;
                        }
                        Err(e) => {
                            let mut st = state_clone.lock().await;
                            let _ = st.event_tx.send(DaemonEvent::JobOutput {
                                job_id: job_id.clone(),
                                line: format!("[plan-executor] failed to resume session: {}", e),
                            });
                            break 'outer;
                        }
                    }
                }
                ExecEvent::Finished(finished_job) => {
                    let success = finished_job.status == JobStatus::Success;
                    let cost = finished_job.cost_usd;
                    let mut st = state_clone.lock().await;
                    st.running_jobs.remove(&job_id);
                    st.history.insert(0, finished_job.clone());
                    let _ = notify_execution_complete(&plan_path_owned, success, cost);
                    let _ = st.event_tx.send(DaemonEvent::JobUpdated { job: finished_job });
                    break 'outer;
                }
            }
        }
        break; // exec_rx closed without Finished
        } // end 'outer loop
    });
}

fn find_repo_root(path: &Path) -> Option<PathBuf> {
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

async fn handle_tui_client(
    stream: tokio::net::UnixStream,
    state: Arc<Mutex<DaemonState>>,
    mut event_rx: broadcast::Receiver<DaemonEvent>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half).lines();

    // Send initial state snapshot
    {
        let st = state.lock().await;
        let snapshot = st.snapshot_state();
        if let Ok(json) = serde_json::to_string(&snapshot) {
            let _ = write_half.write_all(format!("{}\n", json).as_bytes()).await;
        }
    }

    loop {
        tokio::select! {
            // Incoming TUI request
            line = reader.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        if let Ok(req) = serde_json::from_str::<TuiRequest>(&l) {
                            handle_tui_request(req, &state, &mut write_half).await;
                        }
                    }
                    _ => break, // client disconnected
                }
            }
            // Outgoing daemon event
            Ok(event) = event_rx.recv() => {
                if let Ok(json) = serde_json::to_string(&event) {
                    if write_half.write_all(format!("{}\n", json).as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
}

async fn handle_tui_request(
    req: TuiRequest,
    state: &Arc<Mutex<DaemonState>>,
    write_half: &mut (impl tokio::io::AsyncWrite + Unpin),
) {
    use tokio::io::AsyncWriteExt;
    match req {
        TuiRequest::Execute { plan_path } => {
            trigger_execution(state, &plan_path).await;
        }
        TuiRequest::CancelPending { plan_path } => {
            let mut st = state.lock().await;
            st.pending_plans.remove(&plan_path);
            let event = st.snapshot_state();
            let _ = st.event_tx.send(event);
        }
        TuiRequest::KillJob { job_id } => {
            // Note: killing requires storing child handles; simplified here.
            // Mark job as Killed in metadata.
            let mut st = state.lock().await;
            if let Some(job) = st.running_jobs.remove(&job_id) {
                let mut killed = job;
                killed.status = crate::jobs::JobStatus::Killed;
                killed.finished_at = Some(chrono::Utc::now());
                let _ = killed.save();
                st.history.insert(0, killed.clone());
                let _ = st.event_tx.send(DaemonEvent::JobUpdated { job: killed });
            }
        }
        TuiRequest::GetState => {
            let st = state.lock().await;
            let snapshot = st.snapshot_state();
            if let Ok(json) = serde_json::to_string(&snapshot) {
                let _ = write_half.write_all(format!("{}\n", json).as_bytes()).await;
            }
        }
        TuiRequest::Subscribe => {
            // Already subscribed via broadcast channel; no-op
        }
    }
}
```

**Important note on kill**: To properly kill child processes, the executor needs to store the child PID. Add a `KillJob` enhancement in a follow-up: store child PIDs in `DaemonState` and send SIGTERM on kill.

**Step 2: Verify**
```
cd /Users/andreas.pohl/tools/plan-executor && cargo check
```

**Step 3: Commit**
```
feat(plan-executor): add daemon with FSEvents watcher, auto-execute, and IPC handler
```

---

### Task 11: TUI module

**Depends on:** Task 6 (ipc), Task 4 (jobs)

**Files:**
- Create: `plan-executor/src/tui/mod.rs` (NOT `src/tui.rs` — Rust requires either a file OR a directory with `mod.rs`, not both)
- Create: `plan-executor/src/tui/app.rs`
- Create: `plan-executor/src/tui/ui.rs`

**Step 1: TUI app state**

Create `plan-executor/src/tui/app.rs`:
```rust
use crate::ipc::{DaemonEvent, PendingPlan, TuiRequest};
use crate::jobs::JobMetadata;
use tokio::sync::mpsc;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Tab {
    Running,   // Tab index 0
    History,   // Tab index 1
}

pub struct App {
    pub current_tab: Tab,
    pub running_jobs: Vec<JobMetadata>,
    pub pending_plans: Vec<PendingPlan>,
    pub history: Vec<JobMetadata>,
    /// job_id -> output lines
    pub job_output: std::collections::HashMap<String, Vec<String>>,
    /// Index of selected item in current tab
    pub selected: usize,
    /// Scroll offset for output view
    pub output_scroll: usize,
    /// Sender to daemon
    pub daemon_tx: mpsc::Sender<TuiRequest>,
    pub should_quit: bool,
}

impl App {
    pub fn new(daemon_tx: mpsc::Sender<TuiRequest>) -> Self {
        Self {
            current_tab: Tab::Running,
            running_jobs: vec![],
            pending_plans: vec![],
            history: vec![],
            job_output: Default::default(),
            selected: 0,
            output_scroll: 0,
            daemon_tx,
            should_quit: false,
        }
    }

    pub fn apply_event(&mut self, event: DaemonEvent) {
        match event {
            DaemonEvent::State { running_jobs, pending_plans, history } => {
                self.running_jobs = running_jobs;
                self.pending_plans = pending_plans;
                self.history = history;
            }
            DaemonEvent::JobOutput { job_id, line } => {
                self.job_output.entry(job_id).or_default().push(line);
            }
            DaemonEvent::JobUpdated { job } => {
                // Update or move from running to history
                self.running_jobs.retain(|j| j.id != job.id);
                if job.status == crate::jobs::JobStatus::Running {
                    self.running_jobs.push(job);
                } else {
                    self.history.insert(0, job);
                }
            }
            DaemonEvent::PlanReady { .. } => {
                // State snapshot will follow; handled via State event
            }
            DaemonEvent::Error { .. } => {}
        }
    }

    pub fn selected_job(&self) -> Option<&JobMetadata> {
        match self.current_tab {
            Tab::Running => self.running_jobs.get(self.selected),
            Tab::History => self.history.get(self.selected),
        }
    }
}
```

**Step 2: UI rendering**

Create `plan-executor/src/tui/ui.rs`:
```rust
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Tabs, Wrap},
};
use crate::tui::app::{App, Tab};
use crate::jobs::JobStatus;

pub fn render(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(frame.area());

    // Tab bar
    let tab_titles = vec![
        Line::from("Running"),
        Line::from("History"),
    ];
    let selected_tab = match app.current_tab {
        Tab::Running => 0,
        Tab::History => 1,
    };
    let tabs = Tabs::new(tab_titles)
        .block(Block::default().borders(Borders::ALL).title("Plan Executor"))
        .select(selected_tab)
        .highlight_style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Yellow));
    frame.render_widget(tabs, chunks[0]);

    // Main content split: list (left) + output (right)
    let content_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(chunks[1]);

    render_list(frame, app, content_chunks[0]);
    render_output(frame, app, content_chunks[1]);
}

fn render_list(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = match app.current_tab {
        Tab::Running => {
            // Show pending plans first, then running jobs
            let mut items: Vec<ListItem> = app.pending_plans.iter().map(|p| {
                let filename = std::path::Path::new(&p.plan_path)
                    .file_name().and_then(|n| n.to_str()).unwrap_or(&p.plan_path);
                let countdown = p.auto_execute_remaining_secs
                    .map(|s| format!(" [auto in {}s]", s))
                    .unwrap_or_else(|| " [press e to execute]".to_string());
                ListItem::new(format!("⏳ {}{}", filename, countdown))
                    .style(Style::default().fg(Color::Yellow))
            }).collect();

            items.extend(app.running_jobs.iter().map(|j| {
                let filename = j.plan_path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                let elapsed = j.started_at.elapsed().map(|d| d.as_secs()).unwrap_or(0);
                ListItem::new(format!("🔄 {} ({}s)", filename, elapsed))
                    .style(Style::default().fg(Color::Cyan))
            }));
            items
        }
        Tab::History => {
            app.history.iter().map(|j| {
                let filename = j.plan_path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                let status_icon = match j.status {
                    JobStatus::Success => "✓",
                    JobStatus::Failed => "✗",
                    JobStatus::Killed => "⊘",
                    JobStatus::Running => "…",
                };
                let cost = j.cost_usd.map(|c| format!(" ${:.4}", c)).unwrap_or_default();
                let secs = j.duration_ms.map(|ms| format!(" {}s", ms / 1000)).unwrap_or_default();
                ListItem::new(format!("{} {}{}{}", status_icon, filename, secs, cost))
            }).collect()
        }
    };

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(match app.current_tab {
            Tab::Running => "Running / Pending",
            Tab::History => "History",
        }))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    frame.render_widget(list, area);
}

fn render_output(frame: &mut Frame, app: &App, area: Rect) {
    let output_text = if let Some(job) = app.selected_job() {
        let lines = app.job_output.get(&job.id).map(|v| v.as_slice()).unwrap_or(&[]);
        let start = lines.len().saturating_sub(area.height as usize + app.output_scroll);
        lines[start..].join("\n")
    } else {
        "Select a job to view output".to_string()
    };

    let paragraph = Paragraph::new(output_text)
        .block(Block::default().borders(Borders::ALL).title("Output"))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}
```

**Step 3: TUI main loop**

Create `plan-executor/src/tui/mod.rs`:
```rust
pub mod app;
pub mod ui;

use std::time::Duration;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::net::UnixStream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use anyhow::Result;
use app::App;
use crate::ipc::{socket_path, DaemonEvent, TuiRequest};

pub async fn run_tui() -> Result<()> {
    // Connect to daemon
    let stream = UnixStream::connect(socket_path()).await
        .map_err(|_| anyhow::anyhow!("Daemon not running. Start with: plan-executor daemon"))?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half).lines();

    // Channel: daemon events -> app
    let (event_tx, mut event_rx) = mpsc::channel::<DaemonEvent>(64);

    // Spawn daemon reader task
    tokio::spawn(async move {
        while let Ok(Some(line)) = reader.next_line().await {
            if let Ok(event) = serde_json::from_str::<DaemonEvent>(&line) {
                let _ = event_tx.send(event).await;
            }
        }
    });

    // Channel: TUI requests -> daemon
    let (req_tx, mut req_rx) = mpsc::channel::<TuiRequest>(64);

    // Spawn daemon writer task
    tokio::spawn(async move {
        while let Some(req) = req_rx.recv().await {
            if let Ok(json) = serde_json::to_string(&req) {
                let _ = write_half.write_all(format!("{}\n", json).as_bytes()).await;
            }
        }
    });

    let mut app = App::new(req_tx.clone());

    // Request initial state
    let _ = req_tx.send(TuiRequest::GetState).await;

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut app, &mut event_rx).await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;

    result
}

async fn run_loop(
    terminal: &mut ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    event_rx: &mut mpsc::Receiver<DaemonEvent>,
) -> Result<()> {
    loop {
        terminal.draw(|f| ui::render(f, app))?;

        // Non-blocking event poll (16ms = ~60fps)
        if event::poll(Duration::from_millis(16))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => {
                        app.should_quit = true;
                    }
                    KeyCode::Tab => {
                        app.current_tab = match app.current_tab {
                            app::Tab::Running => app::Tab::History,
                            app::Tab::History => app::Tab::Running,
                        };
                        app.selected = 0;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        app.selected = app.selected.saturating_add(1);
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        app.selected = app.selected.saturating_sub(1);
                    }
                    KeyCode::Char('e') => {
                        // Execute selected pending plan
                        if let Some(pending) = app.pending_plans.get(app.selected) {
                            let _ = app.daemon_tx.send(TuiRequest::Execute {
                                plan_path: pending.plan_path.clone(),
                            }).await;
                        }
                    }
                    KeyCode::Char('c') => {
                        // Cancel selected pending plan
                        if let Some(pending) = app.pending_plans.get(app.selected) {
                            let _ = app.daemon_tx.send(TuiRequest::CancelPending {
                                plan_path: pending.plan_path.clone(),
                            }).await;
                        }
                    }
                    KeyCode::Char('x') => {
                        // Kill selected running job
                        if app.current_tab == app::Tab::Running {
                            if let Some(job) = app.running_jobs.get(app.selected) {
                                let _ = app.daemon_tx.send(TuiRequest::KillJob {
                                    job_id: job.id.clone(),
                                }).await;
                            }
                        }
                    }
                    KeyCode::PageDown => {
                        app.output_scroll = app.output_scroll.saturating_add(10);
                    }
                    KeyCode::PageUp => {
                        app.output_scroll = app.output_scroll.saturating_sub(10);
                    }
                    _ => {}
                }
            }
        }

        // Drain daemon events (non-blocking)
        while let Ok(event) = event_rx.try_recv() {
            app.apply_event(event);
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}
```

**Step 4: Verify**
```
cd /Users/andreas.pohl/tools/plan-executor && cargo check
```

**Step 5: Commit**
```
feat(plan-executor): add ratatui TUI with running/history tabs and output viewer
```

---

### Task 12: CLI subcommands (main entry point)

**Depends on:** Task 10 (daemon), Task 11 (tui)

**Files:**
- Create: `plan-executor/src/cli.rs`
- Modify: `plan-executor/src/main.rs`

**Step 1: Implement CLI**

Create `plan-executor/src/cli.rs`:
```rust
use clap::{Parser, Subcommand};
use anyhow::Result;

#[derive(Parser)]
#[command(name = "plan-executor", about = "Monitor and execute Claude plan files")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start the background daemon
    Daemon,
    /// Attach TUI to running daemon
    Tui,
    /// Show daemon status
    Status,
}

pub fn run() {
    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    let result: Result<()> = match cli.command {
        Commands::Daemon => rt.block_on(crate::daemon::run_daemon()),
        Commands::Tui => rt.block_on(crate::tui::run_tui()),
        Commands::Status => rt.block_on(show_status()),
    };

    if let Err(e) = result {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

async fn show_status() -> Result<()> {
    use crate::ipc::socket_path;
    let sock = socket_path();
    if sock.exists() {
        println!("Daemon running (socket: {})", sock.display());
    } else {
        println!("Daemon not running");
    }
    Ok(())
}
```

Update `plan-executor/src/main.rs` to simply call `cli::run()` as shown in Task 1.

**Step 2: Verify full build**
```
cd /Users/andreas.pohl/tools/plan-executor && cargo build
```
Expected: binary builds successfully at `target/debug/plan-executor`.

**Step 3: Smoke test**
```
cd /Users/andreas.pohl/tools/plan-executor && ./target/debug/plan-executor --help
```
Expected: shows `daemon`, `tui`, `status` subcommands.

**Step 4: Commit**
```
feat(plan-executor): add CLI subcommands (daemon, tui, status) via clap
```

---

### Task 13: Kill job with SIGTERM (child PID tracking)

**Depends on:** Task 9 (executor), Task 10 (daemon)

**Files:**
- Modify: `plan-executor/src/executor.rs`
- Modify: `plan-executor/src/daemon.rs`
- Modify: `plan-executor/src/ipc.rs`

**Step 1: Store child handle in daemon state**

Add to `DaemonState` in `daemon.rs`:
```rust
/// Child process handles for running jobs (job_id -> child)
pub running_children: HashMap<String, tokio::process::Child>,
```

In `spawn_execution`, return the `Child` separately. In `trigger_execution`, store it in `DaemonState::running_children`.

**Step 2: Kill on `KillJob` request**

In `handle_tui_request` for `KillJob`:
```rust
TuiRequest::KillJob { job_id } => {
    let mut st = state.lock().await;
    // Send SIGTERM to child
    if let Some(mut child) = st.running_children.remove(&job_id) {
        let _ = child.kill().await; // tokio's kill() sends SIGKILL
        // For SIGTERM: use nix::sys::signal::kill
    }
    if let Some(mut job) = st.running_jobs.remove(&job_id) {
        job.status = JobStatus::Killed;
        job.finished_at = Some(chrono::Utc::now());
        let _ = job.save();
        st.history.insert(0, job.clone());
        let _ = st.event_tx.send(DaemonEvent::JobUpdated { job });
    }
}
```

**Step 3: Verify**
```
cd /Users/andreas.pohl/tools/plan-executor && cargo build
```

**Step 4: Commit**
```
fix(plan-executor): implement SIGKILL for running jobs via child handle tracking
```

---

### Task 14: Stream-JSON formatter (human-readable output)

**Depends on:** Task 9 (executor, defines stream-json parsing)

**Files:**
- Create: `plan-executor/src/formatter.rs`

**Step 1: Implement formatter**

The formatter converts raw NDJSON lines from claude into human-readable strings for display in the TUI.

Create `plan-executor/src/formatter.rs`:
```rust
use serde::Deserialize;
use serde_json::Value;

/// Converts a raw stream-json NDJSON line to zero or more human-readable display lines.
/// Returns empty vec for lines that should be suppressed (e.g. session metadata).
pub fn format_stream_line(raw: &str) -> Vec<String> {
    let Ok(val): Result<Value, _> = serde_json::from_str(raw) else {
        // Not JSON — show as-is
        return vec![raw.to_string()];
    };

    let event_type = val["type"].as_str().unwrap_or("");
    let subtype = val["subtype"].as_str().unwrap_or("");

    match event_type {
        "system" => match subtype {
            "init" => {
                let model = val["model"].as_str().unwrap_or("unknown");
                vec![format!("[Session] Using model: {}", model)]
            }
            "compact_boundary" => vec!["[Context] Conversation compacted".to_string()],
            _ => vec![], // suppress other system events
        },

        "assistant" => {
            let mut lines = Vec::new();
            if let Some(content) = val["message"]["content"].as_array() {
                for block in content {
                    match block["type"].as_str().unwrap_or("") {
                        "text" => {
                            let text = block["text"].as_str().unwrap_or("").trim();
                            if !text.is_empty() {
                                // Prefix each line of multi-line text
                                for line in text.lines() {
                                    lines.push(format!("[Claude] {}", line));
                                }
                            }
                        }
                        "tool_use" => {
                            let name = block["name"].as_str().unwrap_or("?");
                            let input = &block["input"];
                            let summary = summarize_tool_input(name, input);
                            lines.push(format!("[Tool: {}] {}", name, summary));
                        }
                        _ => {}
                    }
                }
            }
            lines
        }

        "user" => {
            let mut lines = Vec::new();
            if let Some(content) = val["message"]["content"].as_array() {
                for block in content {
                    if block["type"].as_str() == Some("tool_result") {
                        let output = extract_tool_result_text(block);
                        if !output.is_empty() {
                            // Show first 5 lines of tool output, truncate rest
                            let all_lines: Vec<&str> = output.lines().collect();
                            let limit = 5;
                            for line in all_lines.iter().take(limit) {
                                lines.push(format!("  → {}", line));
                            }
                            if all_lines.len() > limit {
                                lines.push(format!("  → ... ({} more lines)", all_lines.len() - limit));
                            }
                        }
                    }
                }
            }
            lines
        }

        "tool_progress" => {
            let name = val["tool_name"].as_str().unwrap_or("?");
            let secs = val["elapsed_time_seconds"].as_f64().unwrap_or(0.0);
            vec![format!("[⏳ {}] running ({:.1}s)…", name, secs)]
        }

        "result" => {
            let cost = val["total_cost_usd"].as_f64().unwrap_or(0.0);
            let ms = val["duration_ms"].as_u64().unwrap_or(0);
            let input = val["usage"]["input_tokens"].as_u64().unwrap_or(0);
            let output = val["usage"]["output_tokens"].as_u64().unwrap_or(0);
            let total_tokens = input + output;

            match subtype {
                "success" => vec![format!(
                    "[✓] Completed in {}s — ${:.4} ({} tokens)",
                    ms / 1000, cost, total_tokens
                )],
                other => {
                    let errors = val["errors"]
                        .as_array()
                        .map(|e| e.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join("; "))
                        .unwrap_or_default();
                    vec![format!("[✗] Failed ({}) — {}", other, errors)]
                }
            }
        }

        _ => vec![], // suppress unknown types
    }
}

/// Produces a short one-line summary of a tool call's input for display.
fn summarize_tool_input(tool_name: &str, input: &Value) -> String {
    match tool_name {
        "Bash" => input["command"].as_str().unwrap_or("").to_string(),
        "Read" => input["file_path"].as_str().unwrap_or("").to_string(),
        "Write" => {
            let path = input["file_path"].as_str().unwrap_or("?");
            let content_len = input["content"].as_str().map(|s| s.len()).unwrap_or(0);
            format!("{} ({} bytes)", path, content_len)
        }
        "Edit" => {
            let path = input["file_path"].as_str().unwrap_or("?");
            format!("{}", path)
        }
        "Glob" => {
            let pattern = input["pattern"].as_str().unwrap_or("?");
            format!("{}", pattern)
        }
        "Grep" => {
            let pattern = input["pattern"].as_str().unwrap_or("?");
            let path = input["path"].as_str().unwrap_or(".");
            format!("{} in {}", pattern, path)
        }
        "Agent" => {
            let desc = input["description"].as_str().unwrap_or("?");
            format!("{}", &desc[..desc.len().min(60)])
        }
        "WebSearch" => input["query"].as_str().unwrap_or("?").to_string(),
        "WebFetch" => input["url"].as_str().unwrap_or("?").to_string(),
        _ => {
            // Generic: show first string field value or JSON truncated
            if let Some(obj) = input.as_object() {
                if let Some((_, v)) = obj.iter().next() {
                    if let Some(s) = v.as_str() {
                        return s[..s.len().min(80)].to_string();
                    }
                }
            }
            let json = serde_json::to_string(input).unwrap_or_default();
            json[..json.len().min(80)].to_string()
        }
    }
}

/// Extracts text content from a tool_result block.
fn extract_tool_result_text(block: &Value) -> String {
    // content can be a string or array of blocks
    if let Some(s) = block["content"].as_str() {
        return s.to_string();
    }
    if let Some(arr) = block["content"].as_array() {
        return arr.iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}
```

**Step 2: Declare formatter module in main.rs**

Add `mod formatter;` to `plan-executor/src/main.rs` alongside the other module declarations:
```rust
mod cli;
mod config;
mod daemon;
mod executor;
mod formatter;   // ← add this
mod ipc;
mod jobs;
mod notifications;
mod plan;
mod tui;
mod watcher;
mod pricing;
```

**Step 4: Update executor to produce formatted lines alongside raw**

In `executor.rs`, when processing `ExecEvent::OutputLine`, also call `format_stream_line` and emit a `FormattedLine` variant so the daemon can send both raw (for log) and formatted (for TUI display).

Update `ExecEvent` in `executor.rs`:
```rust
pub enum ExecEvent {
    OutputLine(String),           // raw NDJSON (written to disk)
    DisplayLine(String),          // human-readable formatted line (for TUI)
    Finished(JobMetadata),
}
```

In the tokio spawn loop, after parsing, push formatted lines:
```rust
// After handling stream event:
for display_line in crate::formatter::format_stream_line(&line) {
    let _ = tx.send(ExecEvent::DisplayLine(display_line)).await;
}
let _ = tx.send(ExecEvent::OutputLine(line)).await;
```

**Step 5: Update daemon to store display lines separately**

In `DaemonState`, add:
```rust
/// Display-formatted output per job (for TUI rendering)
pub job_display_output: HashMap<String, Vec<String>>,
```

In `trigger_execution`, handle `ExecEvent::DisplayLine`:
```rust
ExecEvent::DisplayLine(line) => {
    let mut st = state_clone.lock().await;
    let buf = st.job_display_output.entry(job_id.clone()).or_default();
    buf.push(line.clone());
    if buf.len() > 10000 { buf.remove(0); }
    // Reuse JobOutput IPC event but with formatted line
    let _ = st.event_tx.send(DaemonEvent::JobDisplayLine {
        job_id: job_id.clone(),
        line,
    });
}
```

**Step 6: Add `JobDisplayLine` to `DaemonEvent` in `ipc.rs`**

```rust
/// A formatted human-readable display line for a job
JobDisplayLine { job_id: String, line: String },
```

**Step 7: Update TUI `app.rs` to use display output**

In `App`, add `pub job_display_output: HashMap<String, Vec<String>>`.
In `apply_event`, handle `DaemonEvent::JobDisplayLine` same as `JobOutput` but into `job_display_output`.
In `ui.rs` `render_output`, use `job_display_output` instead of `job_output`.
Keep `job_output` for raw log (historical view, not in TUI main output pane).

**Step 8: Verify**
```
cd /Users/andreas.pohl/tools/plan-executor && cargo check
```

**Step 9: Commit**
```
feat(plan-executor): add stream-json formatter for human-readable TUI output
```

---

### Task 15: Tests

**Files:**
- Create: `plan-executor/tests/config_test.rs`
- Create: `plan-executor/tests/plan_test.rs`
- Create: `plan-executor/tests/pricing_test.rs`

**Step 1: Config tests**

Create `plan-executor/tests/config_test.rs`:
```rust
use plan_executor::config::{Config, expand_tilde};
use tempfile::TempDir;

#[test]
fn test_expand_tilde_replaces_home() {
    let result = expand_tilde("~/foo/bar");
    let home = dirs::home_dir().unwrap();
    assert_eq!(result, home.join("foo/bar"));
}

#[test]
fn test_expand_tilde_no_tilde() {
    let result = expand_tilde("/absolute/path");
    assert_eq!(result, std::path::PathBuf::from("/absolute/path"));
}

#[test]
fn test_config_default() {
    let config = Config::default();
    assert!(!config.auto_execute);
    assert!(!config.watch_dirs.is_empty());
    assert!(!config.plan_patterns.is_empty());
}

#[test]
fn test_config_load_missing_returns_default() {
    // No config file exists in test environment (or temp dir)
    // This test validates that missing config.json returns Default without error
    // We can't easily test with home dir, so test serde parsing instead
    let json = r#"{"watch_dirs": ["~/workspace"], "plan_patterns": [".my/plans/*.md"], "auto_execute": true}"#;
    let config: Config = serde_json::from_str(json).unwrap();
    assert!(config.auto_execute);
    assert_eq!(config.watch_dirs, vec!["~/workspace"]);
}
```

**Step 2: Plan parsing tests**

Create `plan-executor/tests/plan_test.rs`:
```rust
use plan_executor::plan::{parse_plan_status, PlanStatus};
use tempfile::NamedTempFile;
use std::io::Write;

fn write_plan(content: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().unwrap();
    write!(f, "{}", content).unwrap();
    f
}

#[test]
fn test_parse_ready_status() {
    let f = write_plan("# My Plan\n\n**Status:** READY\n\n## Tasks\n");
    let status = parse_plan_status(f.path()).unwrap();
    assert_eq!(status, PlanStatus::Ready);
}

#[test]
fn test_parse_wip_status() {
    let f = write_plan("**Status:** WIP\n");
    let status = parse_plan_status(f.path()).unwrap();
    assert_eq!(status, PlanStatus::Wip);
}

#[test]
fn test_parse_missing_status() {
    let f = write_plan("# No status here\n");
    let status = parse_plan_status(f.path()).unwrap();
    assert!(matches!(status, PlanStatus::Unknown(_)));
}
```

**Step 3: Pricing tests**

Create `plan-executor/tests/pricing_test.rs`:
```rust
use plan_executor::pricing::{calculate_cost, ModelPricing, PricingTable};

fn make_table() -> PricingTable {
    let mut table = PricingTable::new();
    table.insert("claude-sonnet-4-6".to_string(), ModelPricing {
        input_per_mtok: 3.0,
        output_per_mtok: 15.0,
        cache_write_per_mtok: 3.75,
        cache_read_per_mtok: 0.3,
    });
    table
}

#[test]
fn test_cost_calculation() {
    let table = make_table();
    // 1M input + 1M output = $3 + $15 = $18
    let cost = calculate_cost(&table, "claude-sonnet-4-6", 1_000_000, 1_000_000, 0, 0).unwrap();
    assert!((cost - 18.0).abs() < 0.001);
}

#[test]
fn test_cost_prefix_match() {
    let table = make_table();
    // Model with suffix like "[1m]" should match via prefix
    let cost = calculate_cost(&table, "claude-sonnet-4-6[1m]", 1_000_000, 0, 0, 0).unwrap();
    assert!((cost - 3.0).abs() < 0.001);
}

#[test]
fn test_unknown_model_returns_none() {
    let table = make_table();
    let cost = calculate_cost(&table, "unknown-model", 1_000_000, 0, 0, 0);
    assert!(cost.is_none());
}
```

**Step 4: Formatter tests**

Create `plan-executor/tests/formatter_test.rs`:
```rust
use plan_executor::formatter::format_stream_line;

#[test]
fn test_format_text_message() {
    let line = r#"{"type":"assistant","uuid":"x","session_id":"s","message":{"content":[{"type":"text","text":"Hello world"}],"usage":{}}}"#;
    let lines = format_stream_line(line);
    assert_eq!(lines, vec!["[Claude] Hello world"]);
}

#[test]
fn test_format_tool_use_bash() {
    let line = r#"{"type":"assistant","uuid":"x","session_id":"s","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la"}}],"usage":{}}}"#;
    let lines = format_stream_line(line);
    assert_eq!(lines, vec!["[Tool: Bash] ls -la"]);
}

#[test]
fn test_format_tool_result() {
    let line = r#"{"type":"user","uuid":"x","session_id":"s","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"file1.rs\nfile2.rs\n"}]}}"#;
    let lines = format_stream_line(line);
    assert!(lines.iter().any(|l| l.contains("file1.rs")));
}

#[test]
fn test_format_result_success() {
    let line = r#"{"type":"result","subtype":"success","uuid":"x","session_id":"s","total_cost_usd":0.05,"duration_ms":45000,"usage":{"input_tokens":10000,"output_tokens":5000}}"#;
    let lines = format_stream_line(line);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("[✓]"));
    assert!(lines[0].contains("45s"));
    assert!(lines[0].contains("$0.0500"));
}

#[test]
fn test_format_system_init() {
    let line = r#"{"type":"system","subtype":"init","uuid":"x","session_id":"s","model":"claude-sonnet-4-6","tools":[],"mcp_servers":[],"slash_commands":[],"output_style":"auto","skills":[],"plugins":[],"apiKeySource":"env","cwd":"/tmp","permissionMode":"bypassPermissions","claude_code_version":"1.0"}"#;
    let lines = format_stream_line(line);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("claude-sonnet-4-6"));
}

#[test]
fn test_suppress_unknown_types() {
    let line = r#"{"type":"tool_use_summary","summary":"Did stuff","preceding_tool_use_ids":[],"uuid":"x","session_id":"s"}"#;
    let lines = format_stream_line(line);
    assert!(lines.is_empty());
}
```

For tests to access internal modules, add to `src/lib.rs`:
```rust
pub mod config;
pub mod plan;
pub mod pricing;
pub mod jobs;
pub mod ipc;
pub mod formatter;
```

**Step 5: Run tests**
```
cd /Users/andreas.pohl/tools/plan-executor && cargo test
```
Expected: all tests pass.

**Step 6: Commit**
```
test(plan-executor): add unit tests for config, plan parsing, pricing, and formatter
```

---

## Task Dependency Graph

```
Task 1 (scaffold)
  └─> Task 2 (config)
        ├─> Task 3 (plan)
        │     └─> Task 8 (watcher)
        │           └─> Task 10 (daemon) <──────────────────────────┐
        ├─> Task 4 (jobs)                                           │
        │     └─> Task 9 (executor) ──> Task 14 (formatter) ──> Task 10 (daemon)
        └─> Task 5 (pricing)          Task 7 (notifications) ─────▶│
              └─> Task 9 (executor)   Task 6 (ipc) ───────────────▶│
Task 6 (ipc)                                                        │
  └─> Task 11 (TUI) ──────────────────────────────────────────────▶│
Task 10 (daemon) + Task 11 (TUI) ──> Task 12 (CLI)
Task 9 (executor) + Task 10 (daemon) ──> Task 13 (kill)
Task 14 (formatter) ──> Task 11 (TUI output display)
Tasks 2,3,5,14 ──> Task 15 (tests)
```

## Open Questions

- The `notify` crate in v8.x uses `RecursiveMode::Recursive` — this leverages FSEvents which is kernel-driven on macOS and efficient. However verify that `RecursiveMode::NonRecursive` would still work if someone prefers it for conceptual clarity.
- macOS notification action buttons (Execute/Cancel) via `notify-rust` require macOS 10.14+ UNUserNotificationCenter. The crate supports this but may require app bundling for actions. For now, buttons will be TUI-only (`e`/`c` keys); OS notifications are informational only.
- The auto-execute countdown in the TUI shows seconds remaining but requires the daemon to broadcast state every second (done via the 1s interval tick).
