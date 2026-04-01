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
            should_quit: false,
        }
    }

    /// Applies a daemon event to the application state.
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
}
