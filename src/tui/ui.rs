//! TUI rendering logic.
use chrono::Utc;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, Paragraph, Tabs, Wrap},
};
use crate::tui::app::{App, Tab};
use crate::jobs::JobStatus;

/// Renders the full TUI frame.
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
                ListItem::new(format!("* {}{}", filename, countdown))
                    .style(Style::default().fg(Color::Yellow))
            }).collect();

            items.extend(app.running_jobs.iter().map(|j| {
                let filename = j.plan_path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                let elapsed = (Utc::now() - j.started_at).num_seconds();
                ListItem::new(format!(">> {} ({}s)", filename, elapsed))
                    .style(Style::default().fg(Color::Cyan))
            }));
            items
        }
        Tab::History => {
            app.history.iter().map(|j| {
                let filename = j.plan_path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                let status_icon = match j.status {
                    JobStatus::Success => "[OK]",
                    JobStatus::Failed => "[FAIL]",
                    JobStatus::Killed => "[KILL]",
                    JobStatus::Running => "[...]",
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
        // Use display output (formatted) for the TUI output pane
        let lines = app.job_display_output.get(&job.id).map(|v| v.as_slice()).unwrap_or(&[]);
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
