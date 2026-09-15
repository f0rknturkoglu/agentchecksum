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

    /// Compare against a different lockfile instead of the committed one.
    ///
    /// This is how CI compares a pull request against the base revision: the
    /// binary has no git integration, so the caller supplies the old lockfile
    /// (`git show HEAD:agentchecksum.lock > /tmp/old.lock`).
    #[arg(long, global = true)]
    pub from: Option<PathBuf>,

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
    /// Compare the committed baseline against current dependency state.
    ///
    /// Informational: it never writes the lockfile and never fails merely because
    /// a change is risky. Turning risk into a non-zero exit is the CI gate's job.
    Diff,
}
