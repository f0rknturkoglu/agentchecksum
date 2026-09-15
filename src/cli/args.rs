// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
}

#[derive(Debug, Parser)]
#[command(
    name = "agentchecksum",
    version,
    about = "Language-agnostic dependency fingerprint and behavioral regression gate for AI agents"
)]
pub struct Cli {
    /// Output format. JSON is written to stdout alone.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Human)]
    pub format: OutputFormat,

    /// Path to the configuration file.
    #[arg(long, global = true, default_value = "agentchecksum.toml")]
    pub config: PathBuf,

    /// Path to the generated lockfile.
    #[arg(long, global = true, default_value = "agentchecksum.lock")]
    pub lock: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// clap exits with code 2 on usage errors, which is the documented contract.
    pub fn parse_or_exit() -> Self {
        <Self as Parser>::parse()
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scaffold agentchecksum.toml and a probes directory.
    Init {
        /// Overwrite an existing configuration file.
        #[arg(long)]
        force: bool,
    },
    /// Discover dependencies and write agentchecksum.lock.
    Snapshot,
}
