// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::Serialize;

use crate::cli::cmd::init::InitOutcome;
use crate::config::RiskLevel;
use crate::diff::{DependencyChange, DiffReport};
use crate::discovery::Discovery;
use crate::error::{Error, Result};
use crate::lockfile::Lockfile;

/// Machine-readable snapshot summary. The shape is part of the CLI contract, so
/// it is a typed struct rather than an ad-hoc map: the field order stays stable
/// and there is no infallible-looking `unwrap` hidden inside a macro.
#[derive(Serialize)]
struct SnapshotReport<'a> {
    status: &'a str,
    lock_version: u32,
    agent_checksum: &'a str,
    dependency_count: usize,
    warnings: &'a [String],
}

pub fn snapshot(lock: &Lockfile, discovery: &Discovery) -> Result<String> {
    let report = SnapshotReport {
        status: "ok",
        lock_version: lock.lock_version,
        agent_checksum: lock.agent_checksum.as_str(),
        dependency_count: discovery.dependencies.len(),
        warnings: &discovery.warnings,
    };
    let mut text =
        serde_json::to_string_pretty(&report).map_err(|source| Error::Json { source })?;
    text.push('\n');
    Ok(text)
}

/// Machine-readable `init` summary. `--format json` promises stdout is JSON alone,
/// so `init` prints this instead of the two human prose lines.
#[derive(Serialize)]
struct InitReport {
    status: &'static str,
    config: String,
    probes: String,
}

pub fn init(outcome: &InitOutcome) -> Result<String> {
    // The probe directory is resolved against the config's directory, which is
    // `.` for a config named by a bare filename. Report it as the user would type
    // it (`probes`, not `./probes`).
    let probes = outcome.probes.strip_prefix(".").unwrap_or(&outcome.probes);

    let report = InitReport {
        status: "ok",
        config: outcome.config.display().to_string(),
        probes: probes.display().to_string(),
    };
    let mut text =
        serde_json::to_string_pretty(&report).map_err(|source| Error::Json { source })?;
    text.push('\n');
    Ok(text)
}

/// Machine-readable `diff` report (design spec §8.4).
///
/// Typed rather than assembled, so field order is stable, risk values serialise
/// lowercase, and nothing needs a human sentence to be parsed. The nested change
/// types come from the diff model and already define this shape.
#[derive(Serialize)]
struct DiffReportJson<'a> {
    status: &'static str,
    changed: bool,
    overall_risk: RiskLevel,
    baseline_checksum: &'a str,
    current_checksum: &'a str,
    changes: &'a [DependencyChange],
}

pub fn diff(report: &DiffReport) -> Result<String> {
    let json = DiffReportJson {
        status: "ok",
        changed: report.changed,
        overall_risk: report.overall_risk,
        baseline_checksum: report.baseline_checksum.as_str(),
        current_checksum: report.current_checksum.as_str(),
        changes: &report.changes,
    };
    let mut text = serde_json::to_string_pretty(&json).map_err(|source| Error::Json { source })?;
    text.push('\n');
    Ok(text)
}
