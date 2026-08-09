//! AgentRT command-line entry point.

mod audit;
mod cli;
mod eval;
mod exec;
mod model;
mod run;
mod sandbox;
mod store;

fn main() -> std::process::ExitCode {
    cli::run()
}
