mod cli;
mod compile;
mod config;
mod daemon;
mod executor;
mod finding;
mod handoff;
mod ipc;
mod job;
mod jobs;
mod plan;
mod proctree;
mod remote;
mod schema;
mod supervisor;
mod validate;

pub use config::Config;

fn main() {
    cli::run();
}
