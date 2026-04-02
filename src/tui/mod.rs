//! Terminal user interface for monitoring the plan-executor daemon.
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

/// Connects to the daemon and runs the interactive TUI.
///
/// # Errors
///
/// Returns an error if the daemon socket is unreachable or terminal setup fails.
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
    let mut dirty = true; // draw immediately on first iteration
    let mut last_tick = std::time::Instant::now();

    loop {
        // Redraw only when something changed, or once per second for elapsed
        // time counters on running jobs.
        let tick_due = last_tick.elapsed() >= Duration::from_secs(1);
        if dirty || tick_due {
            terminal.draw(|f| ui::render(f, app))?;
            dirty = false;
            if tick_due {
                last_tick = std::time::Instant::now();
            }
        }

        // Block up to 100ms waiting for a keyboard event. Returns immediately
        // when a key is pressed, so interaction feels instant.
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                dirty = true;
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
                        if let Some(pending) = app.pending_plans.get(app.selected) {
                            let _ = app.daemon_tx.send(TuiRequest::Execute {
                                plan_path: pending.plan_path.clone(),
                            }).await;
                        }
                    }
                    KeyCode::Char('c') => {
                        if let Some(pending) = app.pending_plans.get(app.selected) {
                            let _ = app.daemon_tx.send(TuiRequest::CancelPending {
                                plan_path: pending.plan_path.clone(),
                            }).await;
                        }
                    }
                    KeyCode::Char('x') => {
                        if app.current_tab == app::Tab::Running {
                            if let Some(job) = app.running_jobs.get(app.selected) {
                                let _ = app.daemon_tx.send(TuiRequest::KillJob {
                                    job_id: job.id.clone(),
                                }).await;
                            }
                        }
                    }
                    KeyCode::Char('p') => {
                        if app.current_tab == app::Tab::Running {
                            if let Some(job) = app.running_jobs.get(app.selected) {
                                let _ = app.daemon_tx.send(TuiRequest::PauseJob {
                                    job_id: job.id.clone(),
                                }).await;
                            }
                        }
                    }
                    KeyCode::Char('u') => {
                        if app.current_tab == app::Tab::Running {
                            if let Some(job) = app.running_jobs.get(app.selected) {
                                let _ = app.daemon_tx.send(TuiRequest::ResumeJob {
                                    job_id: job.id.clone(),
                                }).await;
                            }
                        }
                    }
                    KeyCode::Char('r') => {
                        let _ = app.daemon_tx.send(TuiRequest::GetState).await;
                    }
                    KeyCode::PageDown => {
                        app.output_scroll = app.output_scroll.saturating_add(10);
                    }
                    KeyCode::PageUp => {
                        app.output_scroll = app.output_scroll.saturating_sub(10);
                    }
                    _ => { dirty = false; } // no-op key: don't force a redraw
                }
            }
        }

        // Drain daemon events (non-blocking)
        while let Ok(event) = event_rx.try_recv() {
            app.apply_event(event);
            dirty = true;
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}
