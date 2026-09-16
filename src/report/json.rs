// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::Serialize;

use crate::cli::cmd::init::InitOutcome;
use crate::cli::cmd::inspect::InspectProbesOutcome;
use crate::config::RiskLevel;
use crate::diff::{DependencyChange, DiffReport};
use crate::discovery::Discovery;
use crate::error::{Error, Result};
use crate::gate::CheckReport;
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
    /// The starter probe this run wrote, or `null` when it left an existing one alone.
    starter_probe: Option<String>,
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
        starter_probe: outcome.starter.as_ref().map(|probe| {
            probe
                .strip_prefix(".")
                .unwrap_or(probe)
                .display()
                .to_string()
        }),
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

/// Machine-readable `check` report (design spec §12.3).
///
/// The report is already a typed value with a stable field order, so this serializes it
/// rather than restating its shape: a hand-written projection would be a second
/// definition of the contract, free to drift from the one the renderers read.
pub fn check(report: &CheckReport) -> Result<String> {
    let mut text = serde_json::to_string_pretty(report).map_err(|source| Error::Json { source })?;
    text.push('\n');
    Ok(text)
}

/// Machine-readable `inspect probes` report.
///
/// Typed because the field order is part of the contract: a debugging view that a
/// script parses should not reorder its keys because a map implementation changed.
#[derive(Serialize)]
struct InspectProbesJson<'a> {
    status: &'static str,
    probe_suite_digest: &'a str,
    probes: Vec<InspectedProbeJson<'a>>,
}

#[derive(Serialize)]
struct InspectedProbeJson<'a> {
    probe: &'a str,
    file: &'a str,
    repeat: u32,
    digest: &'a str,
    metrics: &'a [String],
    tools: Vec<InspectedToolJson<'a>>,
}

#[derive(Serialize)]
struct InspectedToolJson<'a> {
    id: &'a str,
    metrics: &'a [String],
}

pub fn inspect_probes(outcome: &InspectProbesOutcome) -> Result<String> {
    let json = InspectProbesJson {
        status: "ok",
        probe_suite_digest: &outcome.suite_digest,
        probes: outcome
            .probes
            .iter()
            .map(|probe| InspectedProbeJson {
                probe: &probe.probe,
                file: &probe.file,
                repeat: probe.repeat,
                digest: &probe.digest,
                metrics: &probe.metrics,
                tools: probe
                    .tools
                    .iter()
                    .map(|tool| InspectedToolJson {
                        id: &tool.id,
                        metrics: &tool.metrics,
                    })
                    .collect(),
            })
            .collect(),
    };

    let mut text = serde_json::to_string_pretty(&json).map_err(|source| Error::Json { source })?;
    text.push('\n');
    Ok(text)
}
