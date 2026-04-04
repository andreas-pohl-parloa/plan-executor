mod cli;
mod config;
mod daemon;
mod executor;
mod handoff;
mod ipc;
mod jobs;
mod notifications;
mod plan;
mod tui;
mod watcher;

pub use config::Config;

fn main() {
    cli::run();
}
