use std::path::PathBuf;
use notify::{RecommendedWatcher, RecursiveMode, Watcher, Config as NotifyConfig, EventKind};
use notify::event::{CreateKind, ModifyKind};
use tokio::sync::mpsc;
use anyhow::Result;

/// Event from the watcher: a path was created or modified.
#[derive(Debug)]
pub struct WatchEvent {
    pub path: PathBuf,
}

/// Starts watching the given directories (recursive, using FSEvents on macOS).
/// Returns a channel receiver for watch events.
pub fn start_watcher(
    watch_dirs: Vec<PathBuf>,
) -> Result<(RecommendedWatcher, mpsc::Receiver<WatchEvent>)> {
    let (tx, rx) = mpsc::channel::<WatchEvent>(64);

    let mut watcher = RecommendedWatcher::new(
        move |result: notify::Result<notify::Event>| {
            if let Ok(event) = result {
                // Only care about creates and modifications
                let relevant = matches!(
                    event.kind,
                    EventKind::Create(CreateKind::File)
                        | EventKind::Modify(ModifyKind::Data(_))
                        | EventKind::Modify(ModifyKind::Any)
                );
                if relevant {
                    for path in event.paths {
                        if path.extension().and_then(|e| e.to_str()) == Some("md") {
                            let _ = tx.blocking_send(WatchEvent { path });
                        }
                    }
                }
            }
        },
        NotifyConfig::default(),
    )?;

    for dir in &watch_dirs {
        // Use Recursive because .my/plans/ is a subdir of watch_dirs.
        // FSEvents on macOS is kernel-driven and efficient regardless of depth.
        watcher.watch(dir, RecursiveMode::Recursive)?;
    }

    Ok((watcher, rx))
}
