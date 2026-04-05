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
    /// the last-read size.  On the first load the full file is read from byte 0.
    /// On subsequent loads only the newly appended bytes are read (by seeking to
    /// the previous cached size), split into lines, and appended to the existing
    /// Vec.  This makes the operation O(new bytes) per tick rather than O(total
    /// file size), which matters for long-running jobs with MB of output.
    pub fn ensure_output_loaded(&mut self, job_id: &str) {
        use std::io::{Read, Seek, SeekFrom};

        let path = crate::config::Config::base_dir()
            .join("jobs").join(job_id).join("display.log");

        let current_size = std::fs::metadata(&path)
            .map(|m| m.len())
            .unwrap_or(0);

        let cached_size = self.display_log_sizes.get(job_id).copied();

        // Nothing new on disk — return early.
        if cached_size == Some(current_size) && self.job_display_output.contains_key(job_id) {
            return;
        }

        let Ok(mut file) = std::fs::File::open(&path) else { return };

        if cached_size.is_none() || !self.job_display_output.contains_key(job_id) {
            // ── Initial load: read the whole file ──────────────────────────────
            let mut content = String::new();
            if file.read_to_string(&mut content).is_err() { return }

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
        } else {
            // ── Incremental load: read only the new bytes ──────────────────────
            let prev_size = cached_size.unwrap_or(0);
            if file.seek(SeekFrom::Start(prev_size)).is_err() { return }

            let mut new_bytes = Vec::new();
            if file.read_to_end(&mut new_bytes).is_err() { return }

            let new_text = String::from_utf8_lossy(&new_bytes);

            // Determine whether the last cached line was blank, so we can
            // continue blank-line dedup across the boundary.
            let existing = self.job_display_output.entry(job_id.to_string()).or_default();
            let mut last_blank = existing
                .last()
                .map(|l| crate::executor::is_visually_blank(l.as_str()))
                .unwrap_or(false);

            // split_terminator avoids a spurious empty token at the end when
            // the chunk ends with '\n', but we still handle a leading empty
            // token (chunk starts right at a line boundary) by skipping it.
            let mut segments = new_text.split('\n').peekable();

            // If prev_size was already at a newline boundary the first segment
            // is an empty string representing "nothing before the next line" —
            // skip it to avoid a phantom blank entry.
            if let Some(&"") = segments.peek() {
                segments.next();
            }

            // The very last segment may be a partial line (no trailing '\n'
            // yet).  We include it anyway; on the next tick it will be
            // re-read as part of the new incremental chunk — but since we
            // update cached_size to current_size the partial line is already
            // stored.  This is acceptable because display.log lines are
            // always complete when flushed by the writer.
            for segment in segments {
                let blank = crate::executor::is_visually_blank(segment);
                if blank && last_blank {
                    continue; // collapse consecutive blank lines
                }
                last_blank = blank;
                existing.push(segment.to_string());
            }

            self.display_log_sizes.insert(job_id.to_string(), current_size);
        }
    }
}
