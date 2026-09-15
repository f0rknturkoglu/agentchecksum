// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::Serialize;

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
