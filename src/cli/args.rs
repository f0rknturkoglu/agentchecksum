// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::config::RiskLevel;
use crate::probes::{MAX_REPEAT, MIN_REPEAT};

/// How many probes `check` may have in flight at once.
///
/// A bound rather than an open loop: every probe is a set of model requests, and an
/// unbounded fan-out would turn `check` into a load generator against the endpoint
/// the user is measuring.
const MAX_JOBS: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
}

/// `--fail-on-risk <level>`, as the CLI spells it.
///
/// The same five levels the configuration accepts, so the flag and the config key
/// cannot drift apart; `none` is a level rather than the absence of one, because a
/// user who writes `--fail-on-risk none` means "fail on any change at all".
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RiskThreshold {
    None,
    Low,
    Medium,
    High,
    Critical,
}

impl RiskThreshold {
    /// The configuration level this flag overrides with.
    pub fn level(self) -> RiskLevel {
        match self {
            RiskThreshold::None => RiskLevel::None,
            RiskThreshold::Low => RiskLevel::Low,
            RiskThreshold::Medium => RiskLevel::Medium,
            RiskThreshold::High => RiskLevel::High,
            RiskThreshold::Critical => RiskLevel::Critical,
        }
    }
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
    /// Run the dependency diff and the probes, apply the policy, and report a verdict.
    ///
    /// This is the only command that fails a build, and the only one that writes the
    /// behavioral baseline — with `--accept`.
    Check {
        /// Record this run as the behavioral baseline.
        ///
        /// Refused while the dependency state itself has moved: scores captured under
        /// a changed agent would be attributed to the wrong one.
        #[arg(long, conflicts_with_all = ["diff_only", "no_probes", "from"])]
        accept: bool,

        /// Compare dependencies only: no probes, no model contact.
        #[arg(long, conflicts_with_all = ["probes_only", "accept"])]
        diff_only: bool,

        /// Run the probes only: the dependency half is reported but does not gate.
        #[arg(long, conflicts_with_all = ["diff_only", "no_probes"])]
        probes_only: bool,

        /// Skip the probes and gate on the dependency half, exactly as `--diff-only`.
        #[arg(long, conflicts_with_all = ["probes_only", "accept"])]
        no_probes: bool,

        /// Evaluate a recorded trace, or a run artifact, instead of calling a model.
        #[arg(
            long,
            value_name = "PATH",
            conflicts_with_all = ["refresh", "jobs", "repeat"]
        )]
        trace: Option<PathBuf>,

        /// Ignore the trace cache and sample every probe again.
        #[arg(long)]
        refresh: bool,

        /// Sample each probe this many times, overriding the probe and the config.
        #[arg(
            long,
            value_name = "N",
            value_parser = clap::value_parser!(u32).range(MIN_REPEAT as i64..=MAX_REPEAT as i64)
        )]
        repeat: Option<u32>,

        /// How many probes to capture concurrently. Does not change the report.
        #[arg(
            long,
            value_name = "N",
            value_parser = clap::value_parser!(u32).range(1..=MAX_JOBS as i64)
        )]
        jobs: Option<u32>,

        /// Fail when the dependency state drifted, even with no behavioral baseline.
        #[arg(long)]
        fail_on_drift: bool,

        /// Fail when the dependency risk reaches this level, overriding `[policy]`.
        #[arg(long, value_name = "LEVEL", value_enum)]
        fail_on_risk: Option<RiskThreshold>,
    },
    /// Debug view of what is configured: the parsed probe suite.
    Inspect {
        #[arg(value_enum)]
        target: InspectTarget,
    },
}

/// What `inspect` can be pointed at.
///
/// One variant today. `deps` and `mcp` describe live state and belong to a later
/// phase; `probes` describes the parsed suite, which needs no MCP session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InspectTarget {
    /// The parsed probe suite: what is configured, which tools each probe
    /// references, and the metrics each probe feeds.
    Probes,
}
