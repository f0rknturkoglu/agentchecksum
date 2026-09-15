// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod args;
pub mod cmd;

use std::process::ExitCode;

use crate::config::Config;
use crate::error::Result;
use crate::report;

use args::{Cli, Command, OutputFormat};

/// Exit code for a runtime failure, per the CLI contract.
const EXIT_RUNTIME_ERROR: u8 = 3;

/// Parse, dispatch, and render. Returns the process exit code.
pub async fn main() -> ExitCode {
    let cli = Cli::parse_or_exit();

    match dispatch(&cli).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error}");

            // Walk the cause chain. `thiserror`'s `#[source]` carries the reason
            // ("unknown field `agentt`"), and without it the suggestion asks the
            // user to fix a key the message never names.
            let mut source = std::error::Error::source(&error);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }

            if let Some(suggestion) = error.suggestion() {
                eprintln!("\nSuggested action:\n  {suggestion}");
            }
            ExitCode::from(EXIT_RUNTIME_ERROR)
        }
    }
}

async fn dispatch(cli: &Cli) -> Result<ExitCode> {
    let root = Config::root_for(&cli.config);

    match &cli.command {
        Command::Init { force } => {
            let outcome = cmd::init::run(&root, &cli.config, *force)?;
            match cli.format {
                OutputFormat::Human => {
                    println!("Wrote {}", outcome.config.display());
                    println!("Next: add your prompts, then run `agentchecksum snapshot`.");
                }
                OutputFormat::Json => print!("{}", report::json::init(&outcome)?),
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Snapshot => {
            let (lock, discovery) = cmd::snapshot::run(&root, &cli.config, &cli.lock).await?;
            match cli.format {
                OutputFormat::Human => print!("{}", report::human::snapshot(&lock, &discovery)),
                OutputFormat::Json => print!("{}", report::json::snapshot(&lock, &discovery)?),
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}
