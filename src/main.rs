mod cli;
mod compile;
mod config;
mod daemon;
mod finding;
mod handoff;
mod helper;
mod ipc;
mod job;
mod jobs;
mod plan;
mod proctree;
mod remote;
mod schema;
mod schema_registry;
mod scheduler;
mod validate;

pub use config::Config;

fn main() {
    cli::run();
}
