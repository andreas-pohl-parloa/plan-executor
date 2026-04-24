mod cli;
mod config;
mod daemon;
mod executor;
mod handoff;
mod ipc;
mod remote;
mod jobs;
mod plan;
mod proctree;
mod schema;

pub use config::Config;

fn main() {
    cli::run();
}
