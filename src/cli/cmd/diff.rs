// SPDX-License-Identifier: MIT OR Apache-2.0

//! The `diff` command: load the baseline, fingerprint current state, compare.
//!
//! Responsibilities stay separate from `snapshot`: this command never writes the
//! lockfile, and it never writes a temporary one either — the current state lives
//! in memory for the duration of the comparison.

use std::path::Path;

use crate::config::Config;
use crate::diff::{self, DiffReport};
use crate::discovery;
use crate::error::Result;
use crate::lockfile::Lockfile;

/// Compare the committed baseline against freshly discovered state.
///
/// `baseline_path` is the lockfile to compare against: the committed one, or the
/// one named by `--from` when CI is comparing against a base revision.
pub async fn run(root: &Path, config_path: &Path, baseline_path: &Path) -> Result<DiffReport> {
    // Baseline first, on purpose: a missing or self-inconsistent lockfile means
    // the comparison cannot happen at all, and the user should be told that
    // without the tool first contacting a model server.
    let baseline = diff::load_baseline(baseline_path)?;

    let config = Config::load(config_path)?;
    let discovery = discovery::run(&config, root).await?;

    // Discovery warnings describe how far the current state can be trusted — for
    // example that a provider exposes no content digest — so they belong on the
    // diagnostic channel rather than in the report's contract.
    for warning in &discovery.warnings {
        tracing::warn!("{warning}");
    }

    let current = Lockfile::from_dependencies(&discovery.dependencies)?;
    Ok(diff::diff(&baseline, &current))
}
