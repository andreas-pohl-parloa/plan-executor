mod cli;
mod config;
mod daemon;
mod executor;
mod finding;
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
