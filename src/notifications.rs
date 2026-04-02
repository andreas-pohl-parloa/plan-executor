use anyhow::Result;

fn icon_path() -> Option<String> {
    let p = crate::config::Config::base_dir().join("icon.png");
    if p.exists() { Some(p.to_string_lossy().into_owned()) } else { None }
}

/// Sends a native macOS notification for a READY plan.
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

/// Sends a macOS notification that a plan execution completed.
pub fn notify_execution_complete(plan_path: &str, success: bool, cost_usd: Option<f64>) -> Result<()> {
    let filename = std::path::Path::new(plan_path)
        .file_name().and_then(|n| n.to_str()).unwrap_or(plan_path);

    let status = if success { "succeeded" } else { "failed" };
    let cost_str = cost_usd.map(|c| format!(" (${:.4})", c)).unwrap_or_default();

    send(&format!("Plan {}", status), &format!("{}{}", filename, cost_str))?;
    Ok(())
}

fn send(title: &str, body: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let icon = icon_path();
        let icon_str = icon.as_deref().unwrap_or("");
        let mut n = mac_notification_sys::Notification::default();
        n.title(title).message(body);
        if !icon_str.is_empty() {
            n.app_icon(icon_str).content_image(icon_str);
        }
        n.send()?;
        return Ok(());
    }
    #[cfg(not(target_os = "macos"))]
    {
        notify_rust::Notification::new().summary(title).body(body).show()?;
        Ok(())
    }
}
