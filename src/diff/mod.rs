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
use crate::manifest::{Dependency, Digest, agent_checksum};

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
    // Before any payload is read for semantic comparison: a payload that disagrees
    // with its own digest must not be allowed to influence a risk decision, even
    // for the moment it takes to reject the file.
    verify_facet_payloads(path, &lockfile)?;
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

/// Recompute every recorded payload's digest and refuse a lockfile that disagrees.
///
/// A facet's digest is taken over the canonical form of the payload recorded beside
/// it, so the two can be checked against each other after the fact. Without this,
/// editing a payload while leaving the digest alone would let a hand-written file
/// steer the semantic diff — the analysis reads payloads, and Phase 2's decisions
/// (risk, details, equivalence) hang off what it finds there.
///
/// Facets without a payload are skipped: their digest covers something the lockfile
/// deliberately does not store (a prompt's text, a tool description), so there is
/// nothing to recompute from. That is a bounded check, not a general one.
pub fn verify_facet_payloads(path: &Path, lockfile: &Lockfile) -> Result<()> {
    for (id, dependency) in &lockfile.dependencies {
        for (name, facet) in &dependency.facets {
            let Some(payload) = facet.normalized.as_ref() else {
                continue;
            };

            let computed = Digest::sha256(&crate::fingerprint::canonical::to_vec(payload)?);
            if computed != facet.digest {
                return Err(Error::FacetPayloadMismatch {
                    path: path.to_path_buf(),
                    id: id.clone(),
                    facet: name.clone(),
                    recorded: facet.digest.as_str().to_string(),
                    computed: computed.as_str().to_string(),
                });
            }
        }
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
        assert!(verify_facet_payloads(&path, &lockfile).is_ok());
    }

    /// A dependency whose facet records a payload, the way external sources do.
    fn recorded_dependency(id: &str, payload: serde_json::Value) -> Dependency {
        let mut facets = BTreeMap::new();
        facets.insert(
            "identity".to_string(),
            Facet {
                digest: Digest::sha256(&crate::fingerprint::canonical::to_vec(&payload).unwrap()),
                shape: None,
                normalized: Some(payload),
            },
        );
        Dependency {
            id: id.to_string(),
            kind: DependencyKind::Model,
            facets,
            source: None,
        }
    }

    #[test]
    fn a_payload_that_contradicts_its_digest_is_refused() {
        // The digest is taken over the payload recorded beside it, so the two can be
        // checked against each other. Without this, editing a payload while leaving
        // its digest alone would steer the semantic diff: risk, details, and
        // equivalence are all read from payloads.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentchecksum.lock");

        let lockfile = Lockfile::from_dependencies(&[recorded_dependency(
            "model:ollama/m",
            serde_json::json!({
                "provider": "ollama",
                "id": "m"
            }),
        )])
        .unwrap();
        lockfile.write(&path).unwrap();
        assert!(verify_facet_payloads(&path, &lockfile).is_ok());

        let mut tampered = lockfile.clone();
        tampered
            .dependencies
            .get_mut("model:ollama/m")
            .unwrap()
            .facets
            .get_mut("identity")
            .unwrap()
            .normalized = Some(serde_json::json!({ "provider": "ollama", "id": "something-else" }));
        tampered.write(&path).unwrap();

        // The aggregate is computed from digests, so tampering with a payload leaves
        // it intact — which is exactly why this check has to exist separately.
        assert!(verify_baseline_checksum(&path, &tampered).is_ok());

        let error = load_baseline(&path).unwrap_err();
        assert!(
            matches!(error, Error::FacetPayloadMismatch { .. }),
            "{error:?}"
        );
        assert!(error.suggestion().is_some());
        assert!(error.to_string().contains("identity"), "{error}");
        assert!(error.to_string().contains("model:ollama/m"), "{error}");
    }

    #[test]
    fn a_facet_without_a_payload_is_not_checked() {
        // A prompt's digest covers text the lockfile deliberately does not store, so
        // there is nothing to recompute from. That is a bounded check, not a general
        // one, and it must not turn into a refusal of every prompt dependency.
        let dir = tempfile::tempdir().unwrap();
        let path = written_lock(dir.path());
        let lockfile = Lockfile::read(&path).unwrap();

        assert!(
            lockfile.dependencies["prompt:a.md"]
                .facets
                .values()
                .all(|facet| facet.normalized.is_none())
        );
        assert!(verify_facet_payloads(&path, &lockfile).is_ok());
    }
}
