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
