// SPDX-License-Identifier: MIT OR Apache-2.0

//! Semantic dependency diff and deterministic risk classification.
//!
//! This layer answers one question: *what changed between the committed baseline
//! and the current dependency state, and what kind of risk does that change
//! represent?* It does not answer whether the agent's behavior regressed — that
//! is the probe layer's job, later. A change can be HIGH risk and harmless, and a
//! LOW-risk change can still break an agent.
//!
//! The shape of the work is deliberate: loading does I/O and validation, the
//! engine is pure, and rendering is separate from both.

pub mod analyze;
pub mod engine;
pub mod model;
pub mod risk;
pub mod schema;

use std::path::Path;

use crate::error::{Error, Result};
use crate::lockfile::Lockfile;
use crate::manifest::{Dependency, agent_checksum};

pub use engine::diff;
pub use model::{ChangeKind, DependencyChange, DetailChange, DiffReport, FacetChange, max_risk};

/// Read the committed baseline, failing before any discovery happens.
///
/// Ordering matters: a missing or inconsistent baseline means the comparison
/// cannot happen at all, and `diff` should say so rather than first contacting a
/// model server.
pub fn load_baseline(path: &Path) -> Result<Lockfile> {
    if !path.exists() {
        return Err(Error::BaselineMissing {
            path: path.to_path_buf(),
        });
    }

    let lockfile = Lockfile::read(path)?;
    verify_baseline_checksum(path, &lockfile)?;
    Ok(lockfile)
}

/// Recompute a lockfile's aggregate from its own dependency entries and compare.
///
/// The lockfile is generated state, but it is also committed and therefore
/// hand-editable. If the recorded aggregate does not describe the entries beside
/// it, every later comparison is against a baseline that never existed, so this
/// is a hard error rather than a warning.
///
/// Verification is exact here because the aggregate is defined over the entry ids
/// and facets, both of which the lockfile stores. The per-facet digests
/// themselves are *not* verified: a facet's digest derivation is not recorded in
/// the lockfile, and the text facets (prompt content and shape, model template)
/// digest normalized text rather than JSON, so a generic "hash the payload and
/// compare" check would raise false alarms. That limitation is deliberate.
pub fn verify_baseline_checksum(path: &Path, lockfile: &Lockfile) -> Result<()> {
    let dependencies: Vec<Dependency> = lockfile
        .dependencies
        .iter()
        .map(|(id, locked)| Dependency {
            id: id.clone(),
            kind: locked.kind,
            facets: locked.facets.clone(),
            // Not hashed, but carried through so the reconstruction is faithful.
            source: locked.source.clone(),
        })
        .collect();

    let computed = agent_checksum(&dependencies)?;
    if computed != lockfile.agent_checksum {
        return Err(Error::BaselineChecksumMismatch {
            path: path.to_path_buf(),
            recorded: lockfile.agent_checksum.as_str().to_string(),
            computed: computed.as_str().to_string(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockfile::Lockfile;
    use crate::manifest::{Dependency, DependencyKind, Digest, Facet};
    use std::collections::BTreeMap;

    fn dependency(id: &str, seed: &str) -> Dependency {
        let mut facets = BTreeMap::new();
        facets.insert(
            "content".to_string(),
            Facet {
                digest: Digest::sha256(seed.as_bytes()),
                shape: None,
                normalized: None,
            },
        );
        Dependency {
            id: id.to_string(),
            kind: DependencyKind::Prompt,
            facets,
            source: None,
        }
    }

    fn written_lock(dir: &Path) -> std::path::PathBuf {
        let lock = Lockfile::from_dependencies(&[dependency("prompt:a.md", "content")]).unwrap();
        let path = dir.join("agentchecksum.lock");
        lock.write(&path).unwrap();
        path
    }

    #[test]
    fn a_missing_baseline_is_an_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentchecksum.lock");

        let error = load_baseline(&path).unwrap_err();
        assert!(matches!(error, Error::BaselineMissing { .. }), "{error:?}");
        assert!(error.suggestion().is_some());
    }

    #[test]
    fn a_consistent_baseline_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = written_lock(dir.path());

        let lockfile = load_baseline(&path).unwrap();
        assert_eq!(lockfile.dependencies.len(), 1);
    }

    #[test]
    fn a_hand_edited_aggregate_is_refused_rather_than_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let path = written_lock(dir.path());

        // Simulate a hand edit: change only the recorded aggregate. Parsing goes
        // through the public API, which is exactly how a real edit would arrive.
        let text = std::fs::read_to_string(&path).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        value["agent_checksum"] = serde_json::Value::String(format!("ac1:{}", "0".repeat(64)));
        std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();

        let error = load_baseline(&path).unwrap_err();
        assert!(
            matches!(error, Error::BaselineChecksumMismatch { .. }),
            "{error:?}"
        );
        // The message must be actionable and must not require the user to guess.
        assert!(error.suggestion().is_some());
        assert!(error.to_string().contains("checksum"), "{error}");
    }

    #[test]
    fn verification_accepts_a_lockfile_it_just_built() {
        let dir = tempfile::tempdir().unwrap();
        let path = written_lock(dir.path());
        let lockfile = Lockfile::read(&path).unwrap();

        assert!(verify_baseline_checksum(&path, &lockfile).is_ok());
    }
}
