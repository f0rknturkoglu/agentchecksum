// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod args;
pub mod cmd;

use std::process::ExitCode;

use crate::config::Config;
use crate::error::Result;
use crate::report;

use args::{Cli, Command, InspectTarget, OutputFormat, RiskThreshold};

/// Exit code for a runtime failure, per the CLI contract.
const EXIT_RUNTIME_ERROR: u8 = 3;

/// Exit code for a rejected flag combination.
///
/// The same code clap uses, because "you asked for two things that cannot both
/// happen" is a usage problem whether clap caught it or this crate did — and a CI
/// script that inspects the exit code should not have to know which.
const EXIT_USAGE: u8 = 2;

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
            ExitCode::from(exit_code(&error))
        }
    }
}

/// A usage error is reported the same way an error is, but is not a runtime failure.
fn exit_code(error: &crate::error::Error) -> u8 {
    match error {
        crate::error::Error::InvalidUsage { .. } => EXIT_USAGE,
        _ => EXIT_RUNTIME_ERROR,
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
                    if let Some(starter) = &outcome.starter {
                        println!("Wrote {}", starter.display());
                    }
                    println!(
                        "Next: add your prompts, edit the example probe, then run `agentchecksum \
                         snapshot` and `agentchecksum check`."
                    );
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
        Command::Diff => {
            let baseline = cli.from.as_deref().unwrap_or(&cli.lock);
            let outcome = cmd::diff::run(&root, &cli.config, baseline).await?;
            match cli.format {
                OutputFormat::Human => print!("{}", report::human::diff(&outcome)),
                OutputFormat::Json => print!("{}", report::json::diff(&outcome)?),
            }
            // Exit 0 even when the overall risk is CRITICAL. `diff` reports what
            // changed; deciding whether that should block a merge belongs to the
            // CI gate, and conflating "found danger" with "failed to compare"
            // would make the exit code useless for telling the two apart.
            Ok(ExitCode::SUCCESS)
        }
        Command::Check {
            accept,
            diff_only,
            probes_only,
            no_probes,
            trace,
            refresh,
            repeat,
            jobs,
            fail_on_drift,
            fail_on_risk,
        } => {
            let options = cmd::check::Options {
                accept: *accept,
                diff_only: *diff_only,
                probes_only: *probes_only,
                no_probes: *no_probes,
                trace: trace.clone(),
                refresh: *refresh,
                repeat: *repeat,
                jobs: *jobs,
                fail_on_drift: *fail_on_drift,
                fail_on_risk: fail_on_risk.map(RiskThreshold::level),
                from_override: cli.from.is_some(),
            };
            let baseline = cli.from.as_deref().unwrap_or(&cli.lock);
            let report = cmd::check::run(&root, &cli.config, baseline, &options).await?;

            match cli.format {
                OutputFormat::Human => {
                    print!("{}", report::human::check(&report, options.fail_on_drift));
                }
                OutputFormat::Json => print!("{}", report::json::check(&report)?),
            }

            // The report states the verdict; the exit code is what a CI job acts on.
            // `--fail-on-drift` moves drift from 0 to 1 without changing the report,
            // which is why the flag is passed in rather than baked into the status.
            Ok(ExitCode::from(report.exit_code(options.fail_on_drift)))
        }
        Command::Inspect { target } => {
            match target {
                InspectTarget::Probes => {
                    let outcome = cmd::inspect::probes(&root, &cli.config, &cli.lock)?;
                    match cli.format {
                        OutputFormat::Human => {
                            print!("{}", report::human::inspect_probes(&outcome));
                        }
                        OutputFormat::Json => {
                            print!("{}", report::json::inspect_probes(&outcome)?);
                        }
                    }
                }
            }
            // Inspection reports what is configured; a suite that cannot be read is an
            // error above, and nothing here is a verdict to fail on.
            Ok(ExitCode::SUCCESS)
        }
    }
}
