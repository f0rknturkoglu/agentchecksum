// SPDX-License-Identifier: MIT OR Apache-2.0

//! The `inspect` command: what is configured, without running any of it.
//!
//! `probes` answers the question a failed resolution raises — *what did I declare, and
//! what did it resolve to?* — for the suite the loader actually reads. Every tool
//! reference is reported by its canonical dependency id, because that is the spelling
//! that survives two servers exposing one remote name and the one a probe should use.

use std::path::Path;

use crate::config::Config;
use crate::diff;
use crate::error::Result;
use crate::probes::{self, Metric, ResolvedProbe};
use crate::runner::ToolCatalog;

/// One tool a probe references, and the metrics that reference feeds.
#[derive(Debug, Clone, PartialEq)]
pub struct InspectedTool {
    /// The canonical dependency id, e.g. `tool:github.search_repositories`.
    pub id: String,
    pub metrics: Vec<String>,
}

/// One probe, as the loader resolved it.
#[derive(Debug, Clone, PartialEq)]
pub struct InspectedProbe {
    pub probe: String,
    /// The file it was read from, as the user would name it.
    pub file: String,
    /// The effective repeat: the probe's own, or the configuration's, or 1.
    pub repeat: u32,
    pub digest: String,
    /// The metrics this probe feeds, in report order.
    pub metrics: Vec<String>,
    pub tools: Vec<InspectedTool>,
}

/// The parsed suite and what each probe resolved to.
#[derive(Debug, Clone, PartialEq)]
pub struct InspectProbesOutcome {
    pub suite_digest: String,
    pub probes: Vec<InspectedProbe>,
}

/// Read the probe suite and resolve it against the committed tool catalog.
///
/// The lockfile rather than live discovery, deliberately: this is the debugging view
/// for a suite that will not load or does not resolve, and it has to answer with the
/// catalog `snapshot` committed. A server being down must not change what the report
/// says the probes are, and nothing here should start an MCP session to find out.
pub fn probes(root: &Path, config_path: &Path, lock_path: &Path) -> Result<InspectProbesOutcome> {
    let config = Config::load(config_path)?;
    let lockfile = diff::load_baseline(lock_path)?;
    let catalog = ToolCatalog::from_lockfile(&lockfile)?;
    let suite = probes::load_suite(root, &config.probes, &catalog, None)?;

    Ok(InspectProbesOutcome {
        suite_digest: suite.digest.as_str().to_string(),
        probes: suite.probes.iter().map(inspect).collect(),
    })
}

/// One probe's tools, each with the metrics its assertion feeds.
///
/// `argument_validity` is absent here on purpose: it is validated against the schema of
/// whichever tool a sample calls, so it is not an assertion about a named tool. It is
/// still listed for the probe as a whole, because a sample that calls any tool is
/// measured by it.
fn inspect(probe: &ResolvedProbe) -> InspectedProbe {
    let mut tools = Vec::new();

    if let Some(expected) = &probe.expect_tool {
        let mut metrics = vec![Metric::ToolSelection];
        if !probe.expect_args.is_empty() {
            metrics.push(Metric::ArgumentExpectation);
        }
        tools.push(InspectedTool {
            id: expected.id.clone(),
            metrics: names(&metrics),
        });
    }

    for forbidden in &probe.forbid_tools {
        tools.push(InspectedTool {
            id: forbidden.id.clone(),
            metrics: names(&[Metric::ForbiddenToolUsage]),
        });
    }

    InspectedProbe {
        probe: probe.name.clone(),
        file: display_path(&probe.path),
        repeat: probe.repeat,
        digest: probe.digest.as_str().to_string(),
        metrics: names(&probe.metrics()),
        tools,
    }
}

fn names(metrics: &[Metric]) -> Vec<String> {
    metrics
        .iter()
        .map(|metric| metric.as_str().to_string())
        .collect()
}

/// A path as the user would type it: `probes/no-tools.toml`, not `./probes/no-tools.toml`.
///
/// The loader resolves the probe directory against the config's directory, which is `.`
/// for a config named by a bare filename.
fn display_path(path: &Path) -> String {
    path.strip_prefix(".").unwrap_or(path).display().to_string()
}
