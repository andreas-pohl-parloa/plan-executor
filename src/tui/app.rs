//! TUI application state and event processing.
use crate::ipc::{DaemonEvent, PendingPlan, TuiRequest};
use crate::jobs::JobMetadata;
use tokio::sync::mpsc;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Tab {
    Running,
    History,
}

pub struct App {
    pub current_tab: Tab,
    pub running_jobs: Vec<JobMetadata>,
    pub pending_plans: Vec<PendingPlan>,
    pub history: Vec<JobMetadata>,
    /// job_id → display lines loaded from display.log
    pub job_display_output: std::collections::HashMap<String, Vec<String>>,
    /// job_id → byte length of display.log at last read; re-read when it grows
    display_log_sizes: std::collections::HashMap<String, u64>,
    pub selected: usize,
    pub output_scroll: usize,
    pub daemon_tx: mpsc::Sender<TuiRequest>,
    pub paused_job_ids: std::collections::HashSet<String>,
    pub should_quit: bool,
}

impl App {
    pub fn new(daemon_tx: mpsc::Sender<TuiRequest>) -> Self {
        Self {
            current_tab: Tab::Running,
            running_jobs: vec![],
            pending_plans: vec![],
            history: vec![],
            job_display_output: Default::default(),
            display_log_sizes: Default::default(),
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

    pub fn apply_event(&mut self, event: DaemonEvent) {
        match event {
            DaemonEvent::State { running_jobs, pending_plans, history, paused_job_ids } => {
                self.running_jobs = running_jobs;
                self.pending_plans = pending_plans;
                self.history = history;
                self.paused_job_ids = paused_job_ids.into_iter().collect();

                let list_len = match self.current_tab {
                    Tab::Running => self.pending_plans.len() + self.running_jobs.len(),
                    Tab::History => self.history.len(),
                };
                if list_len > 0 {
                    self.selected = self.selected.min(list_len - 1);
                }
            }
            DaemonEvent::JobOutput { .. } => {}
            DaemonEvent::JobDisplayLine { .. } => {
                // display.log is the source of truth; size-check polling in
                // ensure_output_loaded handles refresh automatically.
            }
            DaemonEvent::JobUpdated { job } => {
                self.running_jobs.retain(|j| j.id != job.id);
                if job.status == crate::jobs::JobStatus::Running {
                    self.running_jobs.push(job);
                } else {
                    // Evict so the finished display.log is loaded fresh.
                    self.job_display_output.remove(&job.id);
                    self.display_log_sizes.remove(&job.id);
                    self.history.insert(0, job);
                }
            }
            DaemonEvent::PlanReady { .. } => {}
            DaemonEvent::Error { .. } => {}
        }
    }

    pub fn selected_job(&self) -> Option<&JobMetadata> {
        match self.current_tab {
            Tab::Running => self.running_jobs.get(self.selected),
            Tab::History => self.history.get(self.selected),
        }
    }

    /// Reads display.log for `job_id` into `job_display_output`.
    ///
    /// Uses a single `stat` call to compare the file's current byte length against
    /// the last-read size.  The file is re-read only when it has grown (new lines
    /// appended) or when no cache entry exists.  This makes both running and
    /// history jobs always show the current content of display.log — the same
    /// source the `output` CLI command reads — with no manual invalidation.
    pub fn ensure_output_loaded(&mut self, job_id: &str) {
        let path = crate::config::Config::base_dir()
            .join("jobs").join(job_id).join("display.log");

        let current_size = std::fs::metadata(&path)
            .map(|m| m.len())
            .unwrap_or(0);

        let cached_size = self.display_log_sizes.get(job_id).copied();
        if cached_size == Some(current_size) && self.job_display_output.contains_key(job_id) {
            return; // nothing new on disk
        }

        let Ok(content) = std::fs::read_to_string(&path) else { return };

        let mut last_blank = false;
        let lines: Vec<String> = content
            .lines()
            .filter(|l| {
                let blank = crate::executor::is_visually_blank(l);
                let skip = blank && last_blank;
                last_blank = blank;
                !skip
            })
            .map(String::from)
            .collect();

        self.display_log_sizes.insert(job_id.to_string(), current_size);
        self.job_display_output.insert(job_id.to_string(), lines);
    }
}
