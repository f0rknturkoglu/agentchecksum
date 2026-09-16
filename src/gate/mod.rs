// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Behavior Gate: what a completed check concludes, and why.
//!
//! Three things decide a verdict and they are deliberately kept apart: the static
//! dependency comparison (Phase 2), the behavioral metric comparison (this module),
//! and whether either could honestly be made at all. The last one is why there is a
//! `Drift` status separate from `Regression` — a gate that called a changed test
//! suite a regression would be lying about what it measured.

pub mod baseline;
pub mod policy;
pub mod result;

use std::path::Path;

use crate::error::{Error, Result};

pub use baseline::{BASELINE_VERSION, BehaviorBaseline};
// The runner owns what a runner contract is; a baseline records it, and a comparison
// checks it. One definition, so capture, baseline and comparison cannot drift apart.
pub use crate::runner::RUNNER_CONTRACT;
pub use policy::{MetricComparison, PolicyFailure, PolicyNote, PolicyOutcome};
pub use result::{BehaviorHalf, CheckReport, DependencyHalf, MetricRow, MetricVerdict, ProbeRow};

/// The status of a completed check.
///
/// `Error` is a runtime outcome rather than a behavioral one, and the CLI produces it
/// without ever asking this module: a check that could not be evaluated must not
/// borrow the authority of one that was.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Pass,
    /// Comparable evidence showed behavior the policy does not accept.
    Drift,
    /// Comparable evidence showed unacceptable behavior.
    Regression,
    /// The check could not be evaluated.
    Error,
}

impl GateStatus {
    /// The stable lowercase token used in JSON and in the run artifact.
    pub fn as_str(self) -> &'static str {
        match self {
            GateStatus::Pass => "pass",
            GateStatus::Drift => "drift",
            GateStatus::Regression => "regression",
            GateStatus::Error => "error",
        }
    }

    /// The most serious of two statuses.
    ///
    /// Used to fold the static and behavioral halves together: a runtime error
    /// outranks a regression, which outranks drift, which outranks a pass.
    pub fn worst(self, other: GateStatus) -> GateStatus {
        self.max(other)
    }
}

/// Why a comparison could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftReason {
    /// No `.agentchecksum/baseline.json` exists.
    NoBaseline,
    /// The probes changed, so the baseline's scores describe a different test.
    ProbeSuiteChanged,
    /// The baseline was recorded by a different runner contract, so its scores came
    /// from different capture rules.
    RunnerContractChanged,
    /// Dependency state differs and no static policy failed it.
    DependencyDrift,
}

impl DriftReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DriftReason::NoBaseline => "no behavioral baseline exists",
            DriftReason::ProbeSuiteChanged => "behavior probe suite changed",
            DriftReason::RunnerContractChanged => {
                "the behavioral baseline was recorded by a different runner contract"
            }
            DriftReason::DependencyDrift => "dependency checksum changed",
        }
    }
}

/// The static half of a verdict: what the dependency comparison alone concludes.
///
/// It is deliberately independent of the behavioral half. `--diff-only` runs it
/// alone, and `--accept` consults it before writing anything, because accepting
/// new behavior while the dependency state itself has moved would attribute the
/// new scores to the wrong agent.
///
/// The default is to report, not to fail. A changed dependency is a fact; whether
/// it should block a merge is a policy decision, and a tool that decided it by
/// default would be unusable on the day someone deliberately changed a model.
pub fn dependency_status(
    report: &crate::diff::DiffReport,
    policy: &crate::config::PolicyConfig,
) -> GateStatus {
    if let Some(threshold) = policy.fail_on_risk
        // An unchanged dependency state has no risk to fail on, whatever the
        // threshold — including `none`, which asks for any risk at all. Without
        // this guard, the lowest threshold would fail the safest possible run.
        && report.changed
        && report.overall_risk >= threshold
    {
        return GateStatus::Regression;
    }
    if report.changed {
        return GateStatus::Drift;
    }
    GateStatus::Pass
}

/// The behavioral half of a verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct BehaviorOutcome {
    /// `false` when no baseline file exists at all.
    pub baseline_present: bool,
    pub probe_suite_digest: String,
    /// Whether the baseline describes the suite that just ran.
    pub suite_matches: bool,
    /// Whether the baseline was recorded under the runner contract in force now.
    pub runner_contract_matches: bool,
    /// Tools whose input schema moved since the baseline. `argument_validity` is
    /// measured against those schemas, so a report has to say when they changed
    /// instead of letting a schema edit look like a model regression.
    pub yardsticks_changed: Vec<String>,
    pub failures: Vec<PolicyFailure>,
    pub notes: Vec<PolicyNote>,
    /// A `max_drop` constraint could not be evaluated.
    pub relative_undecided: bool,
}

impl BehaviorOutcome {
    /// Whether the baseline can honestly be compared with this run.
    ///
    /// Two things have to hold before a *relative* comparison means anything: the
    /// baseline must describe the same test (suite digest) and the same capture rules
    /// (runner contract). Absolute policy constraints do not depend on either and are
    /// applied regardless — a threshold the current run misses is a fact about the
    /// current run.
    pub fn comparable(&self) -> bool {
        self.baseline_present && self.suite_matches && self.runner_contract_matches
    }

    /// The behavioral contribution to the final status.
    pub fn status(&self) -> GateStatus {
        if !self.failures.is_empty() {
            return GateStatus::Regression;
        }
        if !self.comparable() || self.relative_undecided {
            return GateStatus::Drift;
        }
        GateStatus::Pass
    }

    /// The drift reasons, in a stable order.
    pub fn drift_reasons(&self) -> Vec<DriftReason> {
        let mut reasons = Vec::new();
        if !self.baseline_present {
            reasons.push(DriftReason::NoBaseline);
        } else {
            if !self.suite_matches {
                reasons.push(DriftReason::ProbeSuiteChanged);
            }
            if !self.runner_contract_matches {
                reasons.push(DriftReason::RunnerContractChanged);
            }
        }
        reasons
    }
}

/// Compare the digests a baseline recorded against the tool schemas of this run.
pub fn changed_yardsticks(
    baseline: Option<&BehaviorBaseline>,
    current: &std::collections::BTreeMap<String, crate::manifest::Digest>,
) -> Vec<String> {
    let Some(baseline) = baseline else {
        return Vec::new();
    };
    let recorded = baseline.tool_input_schemas();

    let mut changed: Vec<String> = current
        .iter()
        .filter(|(id, digest)| recorded.get(*id) != Some(*digest))
        .map(|(id, _)| id.clone())
        .collect();
    // A tool the baseline knew and this run does not is a static change the diff
    // already reports; only schema movement is annotated here.
    changed.sort();
    changed.dedup();
    changed
}

/// Write bytes to a path through a temporary file and a rename.
///
/// Cache, run and baseline artifacts are new files, so they get the safe write the
/// frozen lockfile writer has never had: a process interrupted mid-write must not
/// leave a half-written JSON document that parses as something.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(directory).map_err(|source| Error::Write {
        path: directory.to_path_buf(),
        source,
    })?;

    let temporary = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("json")
    ));

    std::fs::write(&temporary, bytes).map_err(|source| Error::Write {
        path: temporary.clone(),
        source,
    })?;
    std::fs::rename(&temporary, path).map_err(|source| Error::Write {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real aggregate, because `AgentChecksum` is derived from a dependency set
    /// and deliberately has no public constructor to assert one with.
    fn checksum(seed: &[u8]) -> crate::manifest::AgentChecksum {
        crate::manifest::agent_checksum(&[crate::manifest::Dependency {
            id: "prompt:a.md".to_string(),
            kind: crate::manifest::DependencyKind::Prompt,
            facets: std::collections::BTreeMap::from([(
                "content".to_string(),
                crate::manifest::Facet {
                    digest: crate::manifest::Digest::sha256(seed),
                    shape: None,
                    normalized: None,
                },
            )]),
            source: None,
        }])
        .unwrap()
    }

    fn outcome(failures: usize, baseline_present: bool, suite_matches: bool) -> BehaviorOutcome {
        BehaviorOutcome {
            baseline_present,
            probe_suite_digest: "sha256:aa".to_string(),
            suite_matches,
            runner_contract_matches: true,
            yardsticks_changed: Vec::new(),
            failures: (0..failures)
                .map(|index| PolicyFailure {
                    metric: crate::probes::Metric::ToolSelection,
                    detail: format!("failure {index}"),
                })
                .collect(),
            notes: Vec::new(),
            relative_undecided: false,
        }
    }

    #[test]
    fn a_failure_outranks_missing_comparability() {
        // A metric that failed on its own terms is a regression even when the suite
        // also changed: the absolute part of the policy still applied.
        assert_eq!(outcome(1, false, false).status(), GateStatus::Regression);
    }

    #[test]
    fn a_changed_suite_is_drift_rather_than_regression() {
        assert_eq!(outcome(0, true, false).status(), GateStatus::Drift);
        assert_eq!(
            outcome(0, true, false).drift_reasons(),
            vec![DriftReason::ProbeSuiteChanged]
        );
    }

    #[test]
    fn a_missing_baseline_is_drift_and_says_so() {
        let absent = outcome(0, false, false);
        assert_eq!(absent.status(), GateStatus::Drift);
        assert_eq!(absent.drift_reasons(), vec![DriftReason::NoBaseline]);
    }

    #[test]
    fn a_baseline_from_another_runner_contract_is_drift_not_regression() {
        // The scores were produced by different capture rules. That is not a
        // regression — nothing says the behavior got worse — it is a comparison that
        // cannot honestly be made.
        let mut incomparable = outcome(0, true, true);
        incomparable.runner_contract_matches = false;

        assert_eq!(incomparable.status(), GateStatus::Drift);
        assert_eq!(
            incomparable.drift_reasons(),
            vec![DriftReason::RunnerContractChanged]
        );

        // And an absolute policy failure still outranks it: a threshold this run
        // misses is a fact about this run.
        let mut failed = outcome(1, true, true);
        failed.runner_contract_matches = false;
        assert_eq!(failed.status(), GateStatus::Regression);
    }

    #[test]
    fn a_comparable_run_that_passed_is_a_pass() {
        assert_eq!(outcome(0, true, true).status(), GateStatus::Pass);
    }

    #[test]
    fn the_most_serious_status_wins() {
        assert_eq!(
            GateStatus::Drift.worst(GateStatus::Regression),
            GateStatus::Regression
        );
        assert_eq!(
            GateStatus::Error.worst(GateStatus::Regression),
            GateStatus::Error
        );
        assert_eq!(GateStatus::Pass.worst(GateStatus::Pass), GateStatus::Pass);
    }

    #[test]
    fn an_atomic_write_replaces_the_target_and_leaves_no_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact.json");

        write_atomically(&path, b"{\"first\":true}\n").unwrap();
        write_atomically(&path, b"{\"second\":true}\n").unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"second\":true}\n"
        );
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn the_static_half_reports_a_change_without_failing_it() {
        let report = crate::diff::DiffReport {
            baseline_checksum: checksum(b"before"),
            current_checksum: checksum(b"after"),
            changed: true,
            overall_risk: crate::config::RiskLevel::Critical,
            changes: Vec::new(),
        };
        let silent = crate::config::PolicyConfig::default();

        // Report, do not fail: the default must be usable on the day someone
        // deliberately changes a model.
        assert_eq!(dependency_status(&report, &silent), GateStatus::Drift);

        // `fail_on_risk` is what turns it into a gate.
        let gated = crate::config::PolicyConfig {
            fail_on_risk: Some(crate::config::RiskLevel::High),
            ..crate::config::PolicyConfig::default()
        };
        assert_eq!(dependency_status(&report, &gated), GateStatus::Regression);
    }

    #[test]
    fn the_lowest_risk_threshold_still_passes_an_unchanged_state() {
        // `fail_on_risk = "none"` asks to fail on *any* risk. It must not fail a run
        // where nothing changed: the lowest threshold would then reject the safest
        // possible state, which is the opposite of what it says.
        let unchanged = crate::diff::DiffReport {
            baseline_checksum: checksum(b"same"),
            current_checksum: checksum(b"same"),
            changed: false,
            overall_risk: crate::config::RiskLevel::None,
            changes: Vec::new(),
        };
        let strictest = crate::config::PolicyConfig {
            fail_on_risk: Some(crate::config::RiskLevel::None),
            ..crate::config::PolicyConfig::default()
        };

        assert_eq!(dependency_status(&unchanged, &strictest), GateStatus::Pass);

        // A change carrying no risk still fails it: the user asked for any risk at
        // all, and a change is the thing they were asking about.
        let changed = crate::diff::DiffReport {
            changed: true,
            overall_risk: crate::config::RiskLevel::None,
            ..unchanged
        };
        assert_eq!(
            dependency_status(&changed, &strictest),
            GateStatus::Regression
        );
    }

    #[test]
    fn the_static_half_passes_an_unchanged_dependency_state() {
        let report = crate::diff::DiffReport {
            baseline_checksum: checksum(b"same"),
            current_checksum: checksum(b"same"),
            changed: false,
            overall_risk: crate::config::RiskLevel::None,
            changes: Vec::new(),
        };
        let gated = crate::config::PolicyConfig {
            fail_on_risk: Some(crate::config::RiskLevel::Low),
            ..crate::config::PolicyConfig::default()
        };

        assert_eq!(dependency_status(&report, &gated), GateStatus::Pass);
    }
}
