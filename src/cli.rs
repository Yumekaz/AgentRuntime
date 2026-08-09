use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "Usage:\n  agentrt version\n  agentrt --version\n  agentrt --help";

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
        Some(command) => {
            eprintln!("error: unknown command `{command}`");
            println!();
            println!("{USAGE}");
            ExitCode::from(2)
        }
    }
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
