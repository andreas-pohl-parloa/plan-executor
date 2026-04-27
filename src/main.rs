mod cli;
mod compile;
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
mod validate;

pub use config::Config;

fn main() {
    cli::run();
}
