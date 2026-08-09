use std::path::PathBuf;
use std::process::ExitCode;

use crate::audit;
use crate::exec::{self, ExecutionError};
use crate::run::{StepDefinition, new_run_id};
use crate::store::{Store, StoreError};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "Usage:\n  agentrt version\n  agentrt --version\n  agentrt --help";

const RUN_USAGE: &str = "Usage:\n  agentrt run [--store <path>] [--steps <count>] [--crash-after <count>]\n  agentrt resume --run-id <id> [--store <path>]\n  agentrt status --run-id <id> [--store <path>]\n  agentrt audit --run-id <id> [--store <path>]";

pub(crate) fn run() -> ExitCode {
    let mut args = std::env::args().skip(1);

    match args.next().as_deref() {
        None | Some("help") | Some("--help") | Some("-h") => {
            println!("AgentRT — durable, sandboxed, auditable LLM execution");
            println!();
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("version") | Some("--version") | Some("-V") => {
            if args.next().is_some() {
                print_unknown_command()
            } else {
                println!("agentrt {VERSION}");
                ExitCode::SUCCESS
            }
        }
        Some("run") => run_command(args.collect()),
        Some("resume") => resume_command(args.collect()),
        Some("status") => status_command(args.collect()),
        Some("audit") => audit_command(args.collect()),
        Some(command) => {
            eprintln!("error: unknown command `{command}`");
            println!();
            println!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn run_command(arguments: Vec<String>) -> ExitCode {
    let options = match parse_options(arguments, true, false) {
        Ok(options) => options,
        Err(error) => return usage_error(error),
    };
    let run_id = options.run_id.unwrap_or_else(new_run_id);
    let definitions = StepDefinition::sequence(options.steps);

    match Store::open(&options.store).and_then(|store| {
        store.create_run(&run_id, &definitions)?;
        match exec::execute(&store, &run_id, &definitions, options.crash_after) {
            Ok(()) => {
                println!("run_id={run_id}");
                println!("status=succeeded");
                Ok(())
            }
            Err(ExecutionError::SimulatedCrash { .. }) => {
                println!("run_id={run_id}");
                println!("status=running");
                Err(StoreError::RunNotFound("simulated crash".to_owned()))
            }
            Err(ExecutionError::Store(error)) => Err(error),
        }
    }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(StoreError::RunNotFound(message)) if message == "simulated crash" => ExitCode::from(75),
        Err(error) => command_error(error),
    }
}

fn resume_command(arguments: Vec<String>) -> ExitCode {
    let options = match parse_options(arguments, false, false) {
        Ok(options) => options,
        Err(error) => return usage_error(error),
    };
    let Some(run_id) = options.run_id else {
        return usage_error("--run-id is required for resume".to_owned());
    };

    match Store::open(&options.store).and_then(|store| {
        let run = store.load_run(&run_id)?;
        let definitions = StepDefinition::sequence(run.total_steps);
        exec::execute(&store, &run_id, &definitions, None).map_err(|error| match error {
            ExecutionError::Store(error) => error,
            ExecutionError::SimulatedCrash { .. } => {
                StoreError::RunNotFound("unexpected simulated crash".to_owned())
            }
        })?;
        println!("run_id={run_id}");
        println!("status=succeeded");
        Ok(())
    }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => command_error(error),
    }
}

fn status_command(arguments: Vec<String>) -> ExitCode {
    let options = match parse_options(arguments, false, false) {
        Ok(options) => options,
        Err(error) => return usage_error(error),
    };
    let Some(run_id) = options.run_id else {
        return usage_error("--run-id is required for status".to_owned());
    };

    match Store::open(&options.store).and_then(|store| {
        let run = store.load_run(&run_id)?;
        println!("run_id={}", run.run_id);
        println!("status={}", run.status);
        println!("progress={}/{}", run.current_step, run.total_steps);
        Ok(())
    }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => command_error(error),
    }
}

fn audit_command(arguments: Vec<String>) -> ExitCode {
    let options = match parse_options(arguments, false, true) {
        Ok(options) => options,
        Err(error) => return usage_error(error),
    };
    let Some(run_id) = options.run_id else {
        return usage_error("--run-id is required for audit".to_owned());
    };

    let store = match Store::open(&options.store) {
        Ok(store) => store,
        Err(error) => return command_error(error),
    };

    if let Some(destination) = options.export.as_deref() {
        return match audit::export(&store, &run_id, destination) {
            Ok(()) => {
                println!("exported={}", destination.display());
                ExitCode::SUCCESS
            }
            Err(error) => command_error(error),
        };
    }

    match store.load_events(&run_id) {
        Ok(events) => {
            for event in events {
                let step = event
                    .step_index
                    .map(|index| format!(" step={index}"))
                    .unwrap_or_default();
                let payload = if event.payload.is_empty() {
                    String::new()
                } else {
                    format!(" payload={}", event.payload)
                };
                println!("{} {}{}{}", event.sequence, event.event_type, step, payload);
            }
            ExitCode::SUCCESS
        }
        Err(error) => command_error(error),
    }
}

#[derive(Debug)]
struct Options {
    store: PathBuf,
    run_id: Option<String>,
    steps: usize,
    crash_after: Option<usize>,
    export: Option<PathBuf>,
}

fn parse_options(
    arguments: Vec<String>,
    allow_run_options: bool,
    allow_export: bool,
) -> Result<Options, String> {
    let mut options = Options {
        store: PathBuf::from(".agentrt/state.db"),
        run_id: None,
        steps: 4,
        crash_after: None,
        export: None,
    };
    let mut index = 0;

    while index < arguments.len() {
        let argument = &arguments[index];
        match argument.as_str() {
            "--store" => {
                index += 1;
                options.store = PathBuf::from(required_value(&arguments, index, argument)?);
            }
            "--run-id" => {
                index += 1;
                options.run_id = Some(required_value(&arguments, index, argument)?.to_owned());
            }
            "--steps" if allow_run_options => {
                index += 1;
                options.steps =
                    parse_positive(required_value(&arguments, index, argument)?, argument)?;
            }
            "--crash-after" if allow_run_options => {
                index += 1;
                options.crash_after = Some(parse_positive(
                    required_value(&arguments, index, argument)?,
                    argument,
                )?);
            }
            "--export" if allow_export => {
                index += 1;
                options.export = Some(PathBuf::from(required_value(&arguments, index, argument)?));
            }
            "--help" | "-h" => return Err(RUN_USAGE.to_owned()),
            other => return Err(format!("unknown option `{other}`\n\n{RUN_USAGE}")),
        }
        index += 1;
    }

    if options.steps == 0 {
        return Err("--steps must be greater than zero".to_owned());
    }
    Ok(options)
}

fn required_value<'a>(
    arguments: &'a [String],
    index: usize,
    option: &str,
) -> Result<&'a str, String> {
    arguments
        .get(index)
        .map(String::as_str)
        .filter(|value| !value.starts_with('-'))
        .ok_or_else(|| format!("{option} requires a value"))
}

fn parse_positive(value: &str, option: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("{option} expects a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{option} expects a positive integer"));
    }
    Ok(parsed)
}

fn usage_error(error: String) -> ExitCode {
    eprintln!("error: {error}");
    ExitCode::from(2)
}

fn command_error(error: impl std::fmt::Display) -> ExitCode {
    eprintln!("error: {error}");
    ExitCode::from(1)
}

fn print_unknown_command() -> ExitCode {
    eprintln!("error: `version` does not accept additional arguments");
    ExitCode::from(2)
}

#[cfg(test)]
mod tests {
    #[test]
    fn package_version_is_available_at_compile_time() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }
}
