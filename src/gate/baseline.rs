// SPDX-License-Identifier: MIT OR Apache-2.0

//! The committed behavioral baseline.
//!
//! A behavioral baseline is a *reviewed contract*, not a cache: it says "this is the
//! behavior we accepted", and it is written only by `check --accept`. It therefore
//! holds scores, counts, and the digests needed to tell whether it still describes
//! the same test — never raw model output, prompts, or tool arguments, which live in
//! machine-local artifacts instead.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::manifest::{AgentChecksum, Digest};
use crate::probes::{Metric, MetricScore, MetricScores};
use crate::runner::RUNNER_CONTRACT;

/// The baseline schema this build writes and understands.
pub const BASELINE_VERSION: u32 = 1;

/// Peeked before the full parse, like the lockfile, so a newer baseline is refused as
/// a version problem rather than reported as a parse error.
#[derive(Deserialize)]
struct VersionProbe {
    baseline_version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BehaviorBaseline {
    pub baseline_version: u32,
    /// The dependency state these scores describe.
    pub agent_checksum: AgentChecksum,
    /// The probe suite these scores describe. A different suite means the scores are
    /// answers to different questions.
    pub probe_suite_digest: Digest,
    pub runner_contract: String,
    pub metrics: MetricScores,
    /// Per-probe `passed`/`total`, keyed by probe name.
    pub probes: BTreeMap<String, MetricScore>,
    /// `argument_validity` is measured against the tool input schemas, so a baseline
    /// records their digests to make a moved yardstick visible. The schemas
    /// themselves stay in the lockfile.
    pub yardsticks: BTreeMap<String, BTreeMap<String, Digest>>,
}

impl BehaviorBaseline {
    pub fn new(
        agent_checksum: AgentChecksum,
        probe_suite_digest: Digest,
        metrics: MetricScores,
        probes: BTreeMap<String, MetricScore>,
        tool_input_schemas: BTreeMap<String, Digest>,
    ) -> Self {
        Self {
            baseline_version: BASELINE_VERSION,
            agent_checksum,
            probe_suite_digest,
            runner_contract: RUNNER_CONTRACT.to_string(),
            metrics,
            probes,
            yardsticks: BTreeMap::from([("tool_input_schema".to_string(), tool_input_schemas)]),
        }
    }

    /// Whether this baseline was recorded under the runner contract in force now.
    ///
    /// A baseline from another contract measures a different experiment: the scores
    /// were produced by different capture rules, so comparing them would be comparing
    /// two measurements rather than two states of one agent. It is not a regression —
    /// it is a comparison that cannot be made.
    pub fn runner_contract_matches(&self) -> bool {
        self.runner_contract == RUNNER_CONTRACT
    }

    pub fn metric(&self, metric: Metric) -> Option<&MetricScore> {
        self.metrics.get(&metric)
    }

    /// Which tool schemas this baseline was scored against.
    pub fn tool_input_schemas(&self) -> BTreeMap<String, Digest> {
        self.yardsticks
            .get("tool_input_schema")
            .cloned()
            .unwrap_or_default()
    }

    /// Deterministic bytes: the committed artifact must be reviewable and diffable.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut text =
            serde_json::to_string_pretty(self).map_err(|source| Error::Json { source })?;
        text.push('\n');
        Ok(text.into_bytes())
    }

    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_path_buf(),
            source,
        })?;

        if let Ok(probe) = serde_json::from_str::<VersionProbe>(&text)
            && probe.baseline_version > BASELINE_VERSION
        {
            return Err(Error::BehaviorBaselineVersion {
                path: path.to_path_buf(),
                found: probe.baseline_version,
                supported: BASELINE_VERSION,
            });
        }

        serde_json::from_str(&text).map_err(|source| Error::Json { source })
    }

    /// Read the baseline if it exists.
    ///
    /// Absence is not an error: a project that has never accepted a baseline still
    /// has a current behavior to report, it simply has nothing to compare against.
    pub fn read_optional(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        Self::read(path).map(Some)
    }

    /// Write atomically: a half-written baseline would be worse than none, because it
    /// would look like a contract.
    pub fn write(&self, path: &Path) -> Result<()> {
        crate::gate::write_atomically(path, &self.to_bytes()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline() -> BehaviorBaseline {
        BehaviorBaseline::new(
            sample_checksum(),
            Digest::sha256(b"suite"),
            MetricScores::from([
                (Metric::ToolSelection, MetricScore::new(9, 10)),
                (Metric::ToolRestraint, MetricScore::new(2, 2)),
            ]),
            BTreeMap::from([("repository-search".to_string(), MetricScore::new(3, 3))]),
            BTreeMap::from([("tool:github.search".to_string(), Digest::sha256(b"schema"))]),
        )
    }

    /// A real aggregate, so the test does not depend on a constructor the type
    /// deliberately does not expose.
    fn sample_checksum() -> AgentChecksum {
        crate::manifest::agent_checksum(&[crate::manifest::Dependency {
            id: "prompt:a.md".to_string(),
            kind: crate::manifest::DependencyKind::Prompt,
            facets: BTreeMap::from([(
                "content".to_string(),
                crate::manifest::Facet {
                    digest: Digest::sha256(b"a"),
                    shape: None,
                    normalized: None,
                },
            )]),
            source: None,
        }])
        .unwrap()
    }

    #[test]
    fn a_baseline_round_trips_and_serializes_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baseline.json");

        baseline().write(&path).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        baseline().write(&path).unwrap();

        assert_eq!(first, std::fs::read_to_string(&path).unwrap());
        assert!(first.ends_with("}\n"));
        assert_eq!(BehaviorBaseline::read(&path).unwrap(), baseline());
    }

    #[test]
    fn a_baseline_carries_no_raw_evidence() {
        // Scores and digests only: no prompt, no model output, no tool arguments.
        let text = String::from_utf8(baseline().to_bytes().unwrap()).unwrap();

        assert!(text.contains("tool_selection"));
        assert!(!text.contains("prompt"));
        assert!(!text.contains("arguments"));
        assert!(!text.contains("final_text"));
    }

    #[test]
    fn a_newer_baseline_is_refused_rather_than_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baseline.json");
        let mut value = serde_json::to_value(baseline()).unwrap();
        value["baseline_version"] = serde_json::json!(BASELINE_VERSION + 1);
        std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();

        let error = BehaviorBaseline::read(&path).unwrap_err();
        assert!(
            matches!(error, Error::BehaviorBaselineVersion { .. }),
            "{error:?}"
        );
        assert!(error.suggestion().is_some());
    }

    #[test]
    fn a_baseline_from_another_runner_contract_is_not_comparable() {
        let mut recorded = baseline();
        assert!(recorded.runner_contract_matches());

        recorded.runner_contract = "some-other-runner-v1".to_string();
        assert!(!recorded.runner_contract_matches());

        // The field is read back, not defaulted: a committed baseline written before
        // this check existed carries the contract it was really recorded under.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baseline.json");
        recorded.write(&path).unwrap();
        assert!(
            !BehaviorBaseline::read(&path)
                .unwrap()
                .runner_contract_matches()
        );
    }

    #[test]
    fn a_missing_baseline_is_absent_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            BehaviorBaseline::read_optional(&dir.path().join("none.json"))
                .unwrap()
                .is_none()
        );
    }
}
