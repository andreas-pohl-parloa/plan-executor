mod cli;
mod config;
mod daemon;
mod executor;
mod handoff;
mod ipc;
mod remote;
mod jobs;
mod plan;

pub use config::Config;

fn main() {
    cli::run();
}
