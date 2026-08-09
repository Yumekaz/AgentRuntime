use std::path::PathBuf;
use std::process::ExitCode;

use crate::agent;
use crate::audit;
use crate::exec::{self, ExecutionError, ToolAction, idempotency_key};
use crate::gate;
use crate::model::{FakeProvider, Message, ModelRequest};
use crate::run::{StepDefinition, new_run_id};
use crate::sandbox::{Policy, SandboxError, Tool, ToolRouter};
use crate::store::{Store, StoreError, ToolStepSpec};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "Usage:\n  agentrt version\n  agentrt --version\n  agentrt --help";

const RUN_USAGE: &str = "Usage:\n  agentrt run [--store <path>] [--steps <count>] [--crash-after <count>]\n  agentrt resume --run-id <id> [--store <path>]\n  agentrt status --run-id <id> [--store <path>]\n  agentrt audit --run-id <id> [--store <path>]";

const TOOL_USAGE: &str = "Usage:\n  agentrt tool read --workspace <path> --path <relative-path> [--store <path>]\n  agentrt tool write --workspace <path> --path <relative-path> --contents <text> [--store <path>]\n  agentrt tool list --workspace <path> [--path <relative-path>] [--store <path>]\nOptions:\n  --read-only\n  --deny-tool <read_file|write_file|list_dir>\n  --max-write-bytes <bytes>\n  --pause-ms <milliseconds>";

const MODEL_USAGE: &str =
    "Usage:\n  agentrt model fake --store <path> --model <name> --prompt <text> --response <text>";

const GATE_USAGE: &str = "Usage:\n  agentrt gate exists --workspace <path> --path <relative-path>\n  agentrt gate contains --workspace <path> --path <relative-path> --text <expected>";

const AGENT_USAGE: &str = "Usage:\n  agentrt agent repo-fix --workspace <path> --path <relative-path> --find <text> --replace <text> [--store <path>]";

const EVAL_USAGE: &str = "Usage:\n  agentrt eval [--break]";

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
        Some("tool") => tool_command(args.collect()),
        Some("model") => model_command(args.collect()),
        Some("gate") => gate_command(args.collect()),
        Some("agent") => agent_command(args.collect()),
        Some("eval") => eval_command(args.collect()),
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
            Err(ExecutionError::Sandbox(error)) => {
                Err(StoreError::InvalidStatus(error.to_string()))
            }
            Err(ExecutionError::Model(error)) => Err(StoreError::InvalidStatus(error.to_string())),
            Err(ExecutionError::GateFailed(error)) => Err(StoreError::InvalidStatus(error)),
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
        exec::resume_run(&store, &run_id).map_err(|error| match error {
            ExecutionError::Store(error) => error,
            ExecutionError::SimulatedCrash { .. } => {
                StoreError::RunNotFound("unexpected simulated crash".to_owned())
            }
            ExecutionError::Sandbox(error) => StoreError::InvalidStatus(error.to_string()),
            ExecutionError::Model(error) => StoreError::InvalidStatus(error.to_string()),
            ExecutionError::GateFailed(error) => StoreError::InvalidStatus(error),
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

fn tool_command(arguments: Vec<String>) -> ExitCode {
    let Some(command) = arguments.first().map(String::as_str) else {
        return usage_error(TOOL_USAGE.to_owned());
    };
    if !matches!(command, "read" | "write" | "list") {
        return usage_error(format!("unknown tool command `{command}`\n\n{TOOL_USAGE}"));
    }

    let mut workspace = None;
    let mut path = None;
    let mut contents = None;
    let mut read_only = false;
    let mut denied_tool = None;
    let mut store_path = PathBuf::from(".agentrt/state.db");
    let mut pause_ms = 0;
    let mut max_write_bytes = None;
    let mut index = 1;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--workspace" => {
                index += 1;
                workspace = Some(PathBuf::from(match arguments.get(index) {
                    Some(value) => value,
                    None => return usage_error("--workspace requires a value".to_owned()),
                }));
            }
            "--path" => {
                index += 1;
                path = Some(match arguments.get(index) {
                    Some(value) => value.clone(),
                    None => return usage_error("--path requires a value".to_owned()),
                });
            }
            "--contents" => {
                index += 1;
                contents = Some(match arguments.get(index) {
                    Some(value) => value.clone(),
                    None => return usage_error("--contents requires a value".to_owned()),
                });
            }
            "--store" => {
                index += 1;
                store_path = PathBuf::from(match arguments.get(index) {
                    Some(value) => value,
                    None => return usage_error("--store requires a value".to_owned()),
                });
            }
            "--pause-ms" => {
                index += 1;
                let Some(value) = arguments.get(index) else {
                    return usage_error("--pause-ms requires a value".to_owned());
                };
                pause_ms = match value.parse::<u64>() {
                    Ok(value) => value,
                    Err(_) => return usage_error("--pause-ms expects an integer".to_owned()),
                };
            }
            "--max-write-bytes" => {
                index += 1;
                let Some(value) = arguments.get(index) else {
                    return usage_error("--max-write-bytes requires a value".to_owned());
                };
                max_write_bytes = match value.parse::<usize>() {
                    Ok(value) if value > 0 => Some(value),
                    _ => {
                        return usage_error(
                            "--max-write-bytes expects a positive integer".to_owned(),
                        );
                    }
                };
            }
            "--read-only" => read_only = true,
            "--deny-tool" => {
                index += 1;
                let Some(value) = arguments.get(index) else {
                    return usage_error("--deny-tool requires a value".to_owned());
                };
                denied_tool = Tool::parse(value);
                if denied_tool.is_none() {
                    return usage_error(format!("unknown tool `{value}`\n\n{TOOL_USAGE}"));
                }
            }
            "--help" | "-h" => return usage_error(TOOL_USAGE.to_owned()),
            other => return usage_error(format!("unknown option `{other}`\n\n{TOOL_USAGE}")),
        }
        index += 1;
    }

    let Some(workspace) = workspace else {
        return usage_error("--workspace is required\n\n".to_owned() + TOOL_USAGE);
    };
    let policy = match if read_only {
        Policy::read_only(&workspace)
    } else {
        Policy::workspace(&workspace)
    } {
        Ok(policy) => {
            let policy = match max_write_bytes {
                Some(limit) => policy.with_max_write_bytes(limit),
                None => policy,
            };
            match denied_tool {
                Some(tool) => policy.deny_tool(tool),
                None => policy,
            }
        }
        Err(error) => return sandbox_error(error),
    };
    let router = ToolRouter::new(policy);
    let relative_path = path.as_deref().unwrap_or(".");

    let action = match command {
        "read" => ToolAction::ReadFile(relative_path.to_owned()),
        "write" => {
            let Some(contents) = contents else {
                return usage_error("--contents is required for write".to_owned());
            };
            ToolAction::WriteFile {
                path: relative_path.to_owned(),
                contents,
            }
        }
        "list" => ToolAction::ListDir(relative_path.to_owned()),
        _ => unreachable!(),
    };

    let definitions = StepDefinition::sequence(1);
    let run_id = new_run_id();
    let store = match Store::open(&store_path) {
        Ok(store) => store,
        Err(error) => return command_error(error),
    };
    if let Err(error) = store.create_run(&run_id, &definitions) {
        return command_error(error);
    }
    let workspace_root = match std::fs::canonicalize(&workspace) {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(error) => return sandbox_error(error.into()),
    };
    let tool_name = match command {
        "read" => "read_file",
        "write" => "write_file",
        "list" => "list_dir",
        _ => unreachable!(),
    };
    let spec = ToolStepSpec {
        idempotency_key: idempotency_key(&run_id, 0),
        workspace_root,
        tool_name: tool_name.to_owned(),
        path: relative_path.to_owned(),
        contents: match &action {
            ToolAction::WriteFile { contents, .. } => Some(contents.clone()),
            _ => None,
        },
        read_only,
        denied_tool: denied_tool.map(|tool| tool.name().to_owned()),
    };
    if let Err(error) = store.configure_tool_step(&run_id, 0, &spec) {
        return command_error(error);
    }

    match exec::execute_tool_step_with_pause(
        &store,
        &run_id,
        &definitions[0],
        &router,
        &action,
        pause_ms,
    ) {
        Ok(output) => {
            println!("run_id={run_id}");
            println!("status=succeeded");
            if !output.is_empty() && command != "write" {
                println!("output={output}");
            }
            ExitCode::SUCCESS
        }
        Err(ExecutionError::Sandbox(error)) => {
            eprintln!("run_id={run_id}");
            sandbox_error(error)
        }
        Err(ExecutionError::Store(error)) => command_error(error),
        Err(ExecutionError::Model(error)) => command_error(error),
        Err(ExecutionError::GateFailed(error)) => command_error(error),
        Err(ExecutionError::SimulatedCrash { .. }) => {
            usage_error("unexpected simulated crash in tool step".to_owned())
        }
    }
}

fn model_command(arguments: Vec<String>) -> ExitCode {
    if arguments.first().map(String::as_str) != Some("fake") {
        return usage_error(MODEL_USAGE.to_owned());
    }

    let mut store_path = PathBuf::from(".agentrt/state.db");
    let mut model = "fake-model".to_owned();
    let mut prompt = None;
    let mut response = "deterministic fake response".to_owned();
    let mut index = 1;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--store" | "--model" | "--prompt" | "--response" => {
                let option = arguments[index].clone();
                index += 1;
                let Some(value) = arguments.get(index) else {
                    return usage_error(format!("{option} requires a value\n\n{MODEL_USAGE}"));
                };
                match option.as_str() {
                    "--store" => store_path = PathBuf::from(value),
                    "--model" => model = value.clone(),
                    "--prompt" => prompt = Some(value.clone()),
                    "--response" => response = value.clone(),
                    _ => unreachable!(),
                }
            }
            "--help" | "-h" => return usage_error(MODEL_USAGE.to_owned()),
            other => return usage_error(format!("unknown option `{other}`\n\n{MODEL_USAGE}")),
        }
        index += 1;
    }
    let Some(prompt) = prompt else {
        return usage_error(format!("--prompt is required\n\n{MODEL_USAGE}"));
    };

    let run_id = new_run_id();
    let definitions = StepDefinition::sequence(1);
    let store = match Store::open(&store_path) {
        Ok(store) => store,
        Err(error) => return command_error(error),
    };
    if let Err(error) = store.create_run(&run_id, &definitions) {
        return command_error(error);
    }
    let request = ModelRequest {
        model,
        messages: vec![Message::user(prompt)],
        temperature: 0.0,
    };
    let provider = FakeProvider::new(response);
    match exec::execute_llm_step(&store, &run_id, &definitions[0], &provider, &request) {
        Ok(output) => {
            println!("run_id={run_id}");
            println!("status=succeeded");
            println!("output={output}");
            ExitCode::SUCCESS
        }
        Err(ExecutionError::Model(error)) => command_error(error),
        Err(ExecutionError::Store(error)) => command_error(error),
        Err(ExecutionError::Sandbox(error)) => command_error(error),
        Err(ExecutionError::GateFailed(error)) => command_error(error),
        Err(ExecutionError::SimulatedCrash { .. }) => {
            usage_error("unexpected simulated crash in model step".to_owned())
        }
    }
}

fn gate_command(arguments: Vec<String>) -> ExitCode {
    let Some(kind) = arguments.first().map(String::as_str) else {
        return usage_error(GATE_USAGE.to_owned());
    };
    if !matches!(kind, "exists" | "contains") {
        return usage_error(format!("unknown gate `{kind}`\n\n{GATE_USAGE}"));
    }

    let mut workspace = None;
    let mut path = None;
    let mut text = None;
    let mut index = 1;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--workspace" | "--path" | "--text" => {
                let option = arguments[index].clone();
                index += 1;
                let Some(value) = arguments.get(index) else {
                    return usage_error(format!("{option} requires a value\n\n{GATE_USAGE}"));
                };
                match option.as_str() {
                    "--workspace" => workspace = Some(value.clone()),
                    "--path" => path = Some(value.clone()),
                    "--text" => text = Some(value.clone()),
                    _ => unreachable!(),
                }
            }
            "--help" | "-h" => return usage_error(GATE_USAGE.to_owned()),
            other => return usage_error(format!("unknown option `{other}`\n\n{GATE_USAGE}")),
        }
        index += 1;
    }

    let Some(workspace) = workspace else {
        return usage_error(format!("--workspace is required\n\n{GATE_USAGE}"));
    };
    let Some(path) = path else {
        return usage_error(format!("--path is required\n\n{GATE_USAGE}"));
    };
    let result = match kind {
        "exists" => gate::file_exists(&workspace, &path),
        "contains" => {
            let Some(text) = text else {
                return usage_error(format!("--text is required\n\n{GATE_USAGE}"));
            };
            gate::file_contains(&workspace, &path, &text)
        }
        _ => unreachable!(),
    };
    let passed = gate::evaluate_all(std::slice::from_ref(&result));
    println!("{result}");
    if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn agent_command(arguments: Vec<String>) -> ExitCode {
    if arguments.first().map(String::as_str) != Some("repo-fix") {
        return usage_error(AGENT_USAGE.to_owned());
    }

    let mut workspace = None;
    let mut path = None;
    let mut find = None;
    let mut replace = None;
    let mut store_path = PathBuf::from(".agentrt/state.db");
    let mut index = 1;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--workspace" | "--path" | "--find" | "--replace" | "--store" => {
                let option = arguments[index].clone();
                index += 1;
                let Some(value) = arguments.get(index) else {
                    return usage_error(format!("{option} requires a value\n\n{AGENT_USAGE}"));
                };
                match option.as_str() {
                    "--workspace" => workspace = Some(PathBuf::from(value)),
                    "--path" => path = Some(value.clone()),
                    "--find" => find = Some(value.clone()),
                    "--replace" => replace = Some(value.clone()),
                    "--store" => store_path = PathBuf::from(value),
                    _ => unreachable!(),
                }
            }
            "--help" | "-h" => return usage_error(AGENT_USAGE.to_owned()),
            other => return usage_error(format!("unknown option `{other}`\n\n{AGENT_USAGE}")),
        }
        index += 1;
    }

    let Some(workspace) = workspace else {
        return usage_error(format!("--workspace is required\n\n{AGENT_USAGE}"));
    };
    let Some(path) = path else {
        return usage_error(format!("--path is required\n\n{AGENT_USAGE}"));
    };
    let Some(find) = find else {
        return usage_error(format!("--find is required\n\n{AGENT_USAGE}"));
    };
    let Some(replace) = replace else {
        return usage_error(format!("--replace is required\n\n{AGENT_USAGE}"));
    };

    match agent::repo_fix(&store_path, &workspace, &path, &find, &replace) {
        Ok(result) => {
            println!("run_id={}", result.run_id);
            println!("status=succeeded");
            println!(
                "replacement_sha256={}",
                crate::audit::sha256_hex(result.output.as_bytes())
            );
            ExitCode::SUCCESS
        }
        Err(ExecutionError::Store(error)) => command_error(error),
        Err(ExecutionError::Sandbox(error)) => sandbox_error(error),
        Err(ExecutionError::Model(error)) => command_error(error),
        Err(ExecutionError::GateFailed(error)) => command_error(error),
        Err(ExecutionError::SimulatedCrash { .. }) => {
            usage_error("unexpected simulated crash in agent workflow".to_owned())
        }
    }
}

fn eval_command(arguments: Vec<String>) -> ExitCode {
    let mut break_regression = false;
    for argument in arguments {
        match argument.as_str() {
            "--break" => break_regression = true,
            "--help" | "-h" => return usage_error(EVAL_USAGE.to_owned()),
            other => return usage_error(format!("unknown option `{other}`\n\n{EVAL_USAGE}")),
        }
    }

    let report = crate::eval::run_suite(break_regression);
    print!("{report}");
    if report.succeeded() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
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

fn sandbox_error(error: SandboxError) -> ExitCode {
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
