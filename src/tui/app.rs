//! TUI application state and event processing.
use crate::ipc::{DaemonEvent, PendingPlan, TuiRequest};
use crate::jobs::JobMetadata;
use tokio::sync::mpsc;

/// Active tab selection in the TUI.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Tab {
    /// Tab index 0: currently running jobs and pending plans.
    Running,
    /// Tab index 1: completed job history.
    History,
}

/// Root application state for the TUI.
pub struct App {
    pub current_tab: Tab,
    pub running_jobs: Vec<JobMetadata>,
    pub pending_plans: Vec<PendingPlan>,
    pub history: Vec<JobMetadata>,
    /// job_id -> raw output lines (written to disk)
    pub job_output: std::collections::HashMap<String, Vec<String>>,
    /// job_id -> formatted display lines (for TUI output pane)
    pub job_display_output: std::collections::HashMap<String, Vec<String>>,
    /// Index of selected item in current tab
    pub selected: usize,
    /// Scroll offset for output view
    pub output_scroll: usize,
    /// Sender to daemon
    pub daemon_tx: mpsc::Sender<TuiRequest>,
    /// IDs of jobs currently paused at a handoff
    pub paused_job_ids: std::collections::HashSet<String>,
    pub should_quit: bool,
}

impl App {
    /// Creates a new `App` with the given daemon request sender.
    pub fn new(daemon_tx: mpsc::Sender<TuiRequest>) -> Self {
        Self {
            current_tab: Tab::Running,
            running_jobs: vec![],
            pending_plans: vec![],
            history: vec![],
            job_output: Default::default(),
            job_display_output: Default::default(),
            selected: 0,
            output_scroll: 0,
            daemon_tx,
            paused_job_ids: Default::default(),
            should_quit: false,
        }
    }

    pub fn is_paused(&self, job_id: &str) -> bool {
        self.paused_job_ids.contains(job_id)
    }

    /// Applies a daemon event to the application state.
    pub fn apply_event(&mut self, event: DaemonEvent) {
        match event {
            DaemonEvent::State { running_jobs, pending_plans, history, paused_job_ids } => {
                self.running_jobs = running_jobs;
                self.pending_plans = pending_plans;
                self.history = history;
                self.paused_job_ids = paused_job_ids.into_iter().collect();
                // Clamp selection so it never points past the end of the list.
                let list_len = match self.current_tab {
                    Tab::Running => self.pending_plans.len() + self.running_jobs.len(),
                    Tab::History => self.history.len(),
                };
                if list_len > 0 {
                    self.selected = self.selected.min(list_len - 1);
                }
            }
            DaemonEvent::JobOutput { job_id, line } => {
                self.job_output.entry(job_id).or_default().push(line);
            }
            DaemonEvent::JobDisplayLine { job_id, line } => {
                self.job_display_output.entry(job_id).or_default().push(line);
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

    /// Returns the currently selected job, if any.
    pub fn selected_job(&self) -> Option<&JobMetadata> {
        match self.current_tab {
            Tab::Running => self.running_jobs.get(self.selected),
            Tab::History => self.history.get(self.selected),
        }
    }

    /// Ensures the output for a job is loaded into `job_display_output`.
    /// For history jobs the live events are gone — load from the stored
    /// `output.jsonl` file and format each line through sjv.
    pub fn ensure_output_loaded(&mut self, job_id: &str) {
        if self.job_display_output.contains_key(job_id) {
            return;
        }
        let path = crate::config::Config::base_dir()
            .join("jobs").join(job_id).join("output.jsonl");
        if let Ok(content) = std::fs::read_to_string(&path) {
            let lines: Vec<String> = content
                .lines()
                .flat_map(|raw| {
                    // sjv may return multi-line strings; split so each entry
                    // is one visual line and colorize_line sees the right prefix.
                    let rendered = sjv::render_runtime_line(raw, false, false);
                    rendered.lines()
                        .filter(|l| !l.is_empty())
                        .map(String::from)
                        .collect::<Vec<_>>()
                })
                .collect();
            self.job_display_output.insert(job_id.to_string(), lines);
        }
    }
}
