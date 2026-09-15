// SPDX-License-Identifier: MIT OR Apache-2.0

use std::process::ExitCode;

use clap::Parser;

/// Subcommands arrive with the code that implements them; there are no stubs.
#[derive(Debug, Parser)]
#[command(
    name = "agentchecksum",
    version,
    about = "Language-agnostic dependency fingerprint and behavioral regression gate for AI agents"
)]
struct Cli {}

fn main() -> ExitCode {
    let _cli = Cli::parse();
    ExitCode::SUCCESS
}
