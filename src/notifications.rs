use anyhow::Result;

/// Sends a native notification for a READY plan.
pub fn notify_plan_ready(plan_path: &str, auto_execute: bool) -> Result<()> {
    let filename = std::path::Path::new(plan_path)
        .file_name().and_then(|n| n.to_str()).unwrap_or(plan_path);

    let body = if auto_execute {
        "Auto-executing in 15 seconds. Open TUI to cancel."
    } else {
        "Open TUI to execute or cancel."
    };

    send("Plan Ready", &format!("{}\n{}", filename, body))?;
    Ok(())
}

/// Sends a notification that a plan execution completed.
pub fn notify_execution_complete(plan_path: &str, success: bool) -> Result<()> {
    let filename = std::path::Path::new(plan_path)
        .file_name().and_then(|n| n.to_str()).unwrap_or(plan_path);
    let status = if success { "succeeded" } else { "failed" };
    send(&format!("Plan {}", status), filename)?;
    Ok(())
}

fn send(title: &str, body: &str) -> Result<()> {
    notify_rust::Notification::new().summary(title).body(body).show()?;
    Ok(())
}
