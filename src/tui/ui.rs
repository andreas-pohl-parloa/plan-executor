//! TUI rendering logic.
use chrono::Utc;
use std::path::Path;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};
use crate::tui::app::{App, Tab};
use crate::jobs::JobStatus;

const HELP: &str =
    " enter/e: execute  c: cancel  p: pause  u: unpause  x: kill  r: reload  tab: switch  q: quit";

/// Renders the full TUI frame. Takes `&mut App` so it can lazily load
/// job output from disk when a history job is selected.
pub fn render(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // tab bar
            Constraint::Min(0),    // content
            Constraint::Length(1), // help bar
        ])
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
    // Lazily load output from disk for the selected job before rendering.
    if let Some(id) = app.selected_job().map(|j| j.id.clone()) {
        app.ensure_output_loaded(&id);
    }
    render_output(frame, app, content_chunks[1]);

    // Help bar
    let help = Paragraph::new(HELP)
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(help, chunks[2]);
}

fn status_col(label: &str, style: Style) -> Span<'static> {
    // Fixed 8-char status column, e.g. "READY   " or "RUNNING " or "PAUSED  "
    Span::styled(format!("{:<8}", label), style)
}

fn render_list(frame: &mut Frame, app: &App, area: Rect) {
    let sel      = app.selected;
    let normal   = Style::default().fg(Color::Gray);
    let selected = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let dim      = Style::default().fg(Color::DarkGray);

    let st_ready   = Style::default().fg(Color::Green);
    let st_running = Style::default().fg(Color::Cyan);
    let st_paused  = Style::default().fg(Color::Yellow);

    let items: Vec<ListItem> = match app.current_tab {
        Tab::Running => {
            let n_pending = app.pending_plans.len();
            let mut items: Vec<ListItem> = app.pending_plans.iter().enumerate().map(|(i, p)| {
                let title_style = if i == sel { selected } else { normal };
                let filename = std::path::Path::new(&p.plan_path)
                    .file_name().and_then(|n| n.to_str()).unwrap_or(&p.plan_path);
                let countdown = p.auto_execute_remaining_secs
                    .map(|s| format!(" [auto {}s]", s))
                    .unwrap_or_default();
                ListItem::new(Text::from(vec![
                    Line::from(vec![
                        status_col("READY", st_ready),
                        Span::styled(format!("{}{}", filename, countdown), title_style),
                    ]),
                    Line::from(Span::styled(format!("        {}", project_label(&p.plan_path)), dim)),
                ]))
            }).collect();

            // Inner width = list area minus borders (2) and status col (8)
            let inner_w = area.width.saturating_sub(2 + 8) as usize;

            items.extend(app.running_jobs.iter().enumerate().map(|(i, j)| {
                let title_style = if i + n_pending == sel { selected } else { normal };
                let filename = j.plan_path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                let elapsed = (Utc::now() - j.started_at).num_seconds();
                let time_str = format_elapsed(elapsed);
                let (status_label, st_style) = if app.is_paused(&j.id) {
                    ("PAUSED", st_paused)
                } else {
                    ("RUNNING", st_running)
                };
                // Pad filename so time_str lands at the right edge of inner_w
                let pad = inner_w.saturating_sub(filename.len() + time_str.len() + 1);
                let spacer = " ".repeat(pad.max(1));
                ListItem::new(Text::from(vec![
                    Line::from(vec![
                        status_col(status_label, st_style),
                        Span::styled(filename.to_string(), title_style),
                        Span::styled(spacer, normal),
                        Span::styled(time_str, dim),
                    ]),
                    Line::from(Span::styled(format!("        {}", project_label(&j.plan_path.to_string_lossy())), dim)),
                ]))
            }));
            items
        }
        Tab::History => {
            app.history.iter().enumerate().map(|(i, j)| {
                let title_style = if i == sel { selected } else { normal };
                let filename = j.plan_path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                let (status_label, st_style) = match j.status {
                    JobStatus::Success => ("OK",     Style::default().fg(Color::Green)),
                    JobStatus::Failed  => ("FAILED", Style::default().fg(Color::Red)),
                    JobStatus::Killed  => ("KILLED", Style::default().fg(Color::Red)),
                    JobStatus::Running => ("RUN",    Style::default().fg(Color::Cyan)),
                };
                let cost = j.cost_usd.map(|c| format!("  ${:.4}", c)).unwrap_or_default();
                let secs = j.duration_ms.map(|ms| format!("  {}", format_elapsed((ms / 1000) as i64))).unwrap_or_default();
                ListItem::new(Text::from(vec![
                    Line::from(vec![
                        status_col(status_label, st_style),
                        Span::styled(format!("{}{}{}", filename, secs, cost), title_style),
                    ]),
                    Line::from(Span::styled(format!("        {}", project_label(&j.plan_path.to_string_lossy())), dim)),
                ]))
            }).collect()
        }
    };

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(match app.current_tab {
            Tab::Running => "Running / Pending",
            Tab::History => "History",
        }))
        .highlight_style(Style::default()); // no-op: selection colour applied per-span above

    let mut state = ListState::default();
    state.select(Some(app.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

/// Returns `<repo-name>` for a plan path, or `<repo-name> [wt]` when the
/// plan lives inside a git worktree. Falls back to the file name when no
/// git root is found.
fn project_label(path: &str) -> String {
    let p = Path::new(path);
    let mut dir = if p.is_file() { p.parent() } else { Some(p) };
    while let Some(d) = dir {
        let git = d.join(".git");
        if git.is_dir() {
            // Regular repo
            return d.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        }
        if git.is_file() {
            // Worktree: .git is a file with `gitdir: /main/repo/.git/worktrees/…`
            let main_name = std::fs::read_to_string(&git)
                .ok()
                .and_then(|s| {
                    let gitdir = s.trim().strip_prefix("gitdir:")?.trim().to_string();
                    // Walk the gitdir path backwards to find the component before ".git"
                    let gp = Path::new(&gitdir).to_path_buf();
                    let mut cur: &Path = &gp;
                    loop {
                        if cur.file_name().map(|n| n == ".git").unwrap_or(false) {
                            return cur.parent()
                                .and_then(|r| r.file_name())
                                .and_then(|n| n.to_str())
                                .map(String::from);
                        }
                        cur = cur.parent()?;
                    }
                })
                .unwrap_or_else(|| {
                    d.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string()
                });
            return format!("{} [wt]", main_name);
        }
        dir = d.parent();
    }
    p.file_name().and_then(|n| n.to_str()).unwrap_or(path).to_string()
}

fn format_elapsed(secs: i64) -> String {
    let s = secs.unsigned_abs();
    if s < 60 {
        format!("{}s", s)
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m{:02}s", s / 3600, (s % 3600) / 60, s % 60)
    }
}

/// Convert a string containing sjv ANSI codes into a ratatui `Line`.
/// sjv uses only: ESC[0m reset, ESC[1m bold, ESC[2m dim, ESC[3m italic,
/// ESC[31m red, ESC[32m green, ESC[34m blue, ESC[36m cyan.
fn ansi_line(s: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = Style::default().fg(Color::Gray);
    let mut seg_start = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            if i > seg_start {
                spans.push(Span::styled(s[seg_start..i].to_string(), style));
            }
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'm' { j += 1; }
            style = match s.get(i + 2..j).unwrap_or("") {
                "0"  => Style::default().fg(Color::Gray),
                "1"  => style.add_modifier(Modifier::BOLD),
                "2"  => style.add_modifier(Modifier::DIM),
                "3"  => style.add_modifier(Modifier::ITALIC),
                "31" => Style::default().fg(Color::Red),
                "32" => Style::default().fg(Color::Green),
                "34" => Style::default().fg(Color::Blue),
                "36" => Style::default().fg(Color::Cyan),
                _    => style,
            };
            i = j + 1;
            seg_start = i;
        } else {
            i += 1;
        }
    }
    if seg_start < s.len() {
        spans.push(Span::styled(s[seg_start..].to_string(), style));
    }
    Line::from(spans)
}

fn render_output(frame: &mut Frame, app: &App, area: Rect) {
    // Subtract 2 for top/bottom borders so the tail is truly visible.
    let visible = area.height.saturating_sub(2) as usize;

    let content = if let Some(job) = app.selected_job() {
        let lines = app.job_display_output.get(&job.id).map(|v| v.as_slice()).unwrap_or(&[]);
        let start = lines.len().saturating_sub(visible + app.output_scroll);
        Text::from(lines[start..].iter().map(|l| ansi_line(l)).collect::<Vec<_>>())
    } else {
        Text::from(Line::from(Span::styled(
            "Select a job to view output",
            Style::default().fg(Color::DarkGray),
        )))
    };

    let paragraph = Paragraph::new(content)
        .block(Block::default().borders(Borders::ALL).title("Output"))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}
