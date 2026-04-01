mod cli;
mod config;
mod daemon;
mod executor;
mod formatter;
mod handoff;
mod ipc;
mod jobs;
mod notifications;
mod plan;
mod pricing;
mod tui;
mod watcher;

pub use config::Config;

fn main() {
    cli::run();
}
