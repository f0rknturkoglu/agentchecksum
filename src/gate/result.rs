// SPDX-License-Identifier: MIT OR Apache-2.0

//! The completed verdict, as data.
//!
//! This is what `check` concluded, in one value that both renderers read and that a
//! CI job can parse. It carries no raw evidence: prompts, model output and tool
//! arguments stay in the machine-local run artifact, and a report is something people
//! paste into a pull request.

use serde::Serialize;

use crate::config::{PolicyConfig, RiskLevel};
use crate::diff::{DependencyChange, DiffReport};
use crate::gate::{BehaviorOutcome, DriftReason, GateStatus};
use crate::manifest::AgentChecksum;
use crate::probes::{Metric, MetricScore, ProbeOutcome};

/// The static half of the report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DependencyHalf {
    pub changed: bool,
    pub overall_risk: RiskLevel,
    pub changes: Vec<DependencyChange>,
}

impl DependencyHalf {
    pub fn of(report: &DiffReport) -> Self {
        Self {
            changed: report.changed,
            overall_risk: report.overall_risk,
            changes: report.changes.clone(),
        }
    }

    pub fn change_count(&self) -> usize {
        self.changes.len()
    }
}

/// How one metric stands.
///
/// `Warn` exists because "no policy failed" and "the behavior was good" are different
/// claims, and a table that renders 0% as PASS makes the second one by accident. A
/// metric nobody declared a threshold for is not gated — and it is not ignored either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricVerdict {
    /// Every declared constraint passed, and the measurement is not short of anything.
    Pass,
    /// A declared constraint failed. This is what fails the gate.
    Fail,
    /// Measured, and no constraint was declared for it: reported, not gated.
    Warn,
    /// No sample made the metric applicable, so there is nothing to compare.
    NotMeasured,
}

/// One line of the metrics table.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricRow {
    pub metric: Metric,
    /// What the baseline recorded, when there is a comparable baseline.
    pub baseline: Option<MetricScore>,
    /// This run's counts. `None` when nothing measured the metric.
    pub current: Option<MetricScore>,
    pub verdict: MetricVerdict,
    /// The policy constraints declared for this metric, in the order they are
    /// checked, so a report can say what the number was measured against.
    pub policy: Vec<String>,
}

/// Build the metrics table: one row per metric the run measured, or that the policy
/// names, in the report's fixed order.
///
/// A metric that was neither measured nor constrained is absent rather than shown as
/// zero: they would be indistinguishable in a table, and only one of them is a fact.
pub fn metric_rows(
    outcomes: &[ProbeOutcome],
    baseline: Option<&crate::gate::BehaviorBaseline>,
    policy: &PolicyConfig,
    comparable: bool,
    failure_metrics: &[Metric],
) -> Vec<MetricRow> {
    let current = crate::probes::aggregate(outcomes);
    let constrained: Vec<Metric> = policy
        .metrics
        .keys()
        .filter_map(|name| Metric::known(name))
        .collect();

    Metric::ALL
        .into_iter()
        .filter(|metric| current.contains_key(metric) || constrained.contains(metric))
        .map(|metric| {
            let now = current.get(&metric).copied();
            let then = baseline
                .filter(|_| comparable)
                .and_then(|baseline| baseline.metric(metric).copied());

            let constrained = constrained.contains(&metric);
            MetricRow {
                metric,
                baseline: then,
                current: now,
                verdict: match (now, constrained) {
                    (None, _) => MetricVerdict::NotMeasured,
                    (Some(_), _) if failure_metrics.contains(&metric) => MetricVerdict::Fail,
                    // Nothing was declared for it, so nothing gates it — and a score
                    // short of perfect is still worth the reader's attention.
                    (Some(score), false) if score.score() != Some(1.0) => MetricVerdict::Warn,
                    (Some(_), _) => MetricVerdict::Pass,
                },
                policy: policy
                    .metrics
                    .get(metric.as_str())
                    .map(describe_policy)
                    .unwrap_or_default(),
            }
        })
        .collect()
}

/// Render a policy in the words the report uses, in a fixed order.
fn describe_policy(policy: &crate::config::MetricPolicy) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(min) = policy.min {
        lines.push(format!("min {min:.4}"));
    }
    if let Some(max) = policy.max {
        lines.push(format!("max {max:.4}"));
    }
    if let Some(max_drop) = policy.max_drop {
        lines.push(format!("max_drop {max_drop:.4}"));
    }
    lines
}

/// The behavioral half of the report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BehaviorHalf {
    pub baseline_present: bool,
    pub suite_matches: bool,
    pub probe_suite_digest: String,
    pub probes_passed: u32,
    pub probes_total: u32,
    pub metrics: Vec<MetricRow>,
    /// Tools whose input schema moved since the baseline, so a schema edit cannot
    /// quietly look like a model regression.
    pub yardsticks_changed: Vec<String>,
    pub failures: Vec<String>,
    pub notes: Vec<String>,
    pub drift_reasons: Vec<String>,
    /// Per-probe pass rates, in probe order.
    pub probes: Vec<ProbeRow>,
}

/// One probe's line in the report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProbeRow {
    pub probe: String,
    pub passed: u32,
    pub total: u32,
    /// The checks that failed, so a failing probe names what it expected.
    pub failures: Vec<String>,
}

impl BehaviorHalf {
    pub fn of(
        outcomes: &[ProbeOutcome],
        outcome: &BehaviorOutcome,
        baseline: Option<&crate::gate::BehaviorBaseline>,
        policy: &PolicyConfig,
    ) -> Self {
        let failure_metrics: Vec<Metric> = outcome
            .failures
            .iter()
            .map(|failure| failure.metric)
            .collect();
        let comparable = outcome.baseline_present && outcome.suite_matches;

        Self {
            baseline_present: outcome.baseline_present,
            suite_matches: outcome.suite_matches,
            probe_suite_digest: outcome.probe_suite_digest.clone(),
            probes_passed: outcomes.iter().map(|outcome| outcome.passed).sum(),
            probes_total: outcomes.iter().map(|outcome| outcome.total).sum(),
            metrics: metric_rows(outcomes, baseline, policy, comparable, &failure_metrics),
            yardsticks_changed: outcome.yardsticks_changed.clone(),
            failures: outcome
                .failures
                .iter()
                .map(|failure| format!("{}: {}", failure.metric.as_str(), failure.detail))
                .collect(),
            notes: outcome
                .notes
                .iter()
                .map(|note| format!("{}: {}", note.metric.as_str(), note.detail))
                .collect(),
            drift_reasons: outcome
                .drift_reasons()
                .iter()
                .map(|reason| reason.as_str().to_string())
                .collect(),
            probes: outcomes
                .iter()
                .map(|outcome| ProbeRow {
                    probe: outcome.probe.clone(),
                    passed: outcome.passed,
                    total: outcome.total,
                    failures: failing_checks(outcome),
                })
                .collect(),
        }
    }
}

/// The first failing check of each sample, deduplicated by wording: a probe that
/// failed the same way three times should say so once.
fn failing_checks(outcome: &ProbeOutcome) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for sample in &outcome.samples {
        for check in &sample.checks {
            if check.passed {
                continue;
            }
            let line = format!(
                "sample {}: {} {}",
                sample.sample,
                check.metric.as_str(),
                check.detail
            );
            if !seen
                .iter()
                .any(|existing| existing.ends_with(&check.detail))
            {
                seen.push(line);
            }
        }
    }
    seen
}

/// Everything `check` concluded.
///
/// Every field serializes, always: a consumer parses a fixed shape where a missing
/// answer is `null` or `[]`, never an absent key. `behavior` is `null` when probes
/// were not requested, which is deliberately distinguishable from a behavioral half
/// that ran and measured nothing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckReport {
    pub status: GateStatus,
    pub agent_checksum: AgentChecksum,
    pub baseline_checksum: Option<AgentChecksum>,
    pub dependency: DependencyHalf,
    /// Absent when probes were not requested (`--diff-only` or `--no-probes`).
    pub behavior: Option<BehaviorHalf>,
    /// A runtime failure that happened after the static half had already been
    /// computed, so the report can say the gate did not finish rather than implying
    /// it passed.
    pub error: Option<String>,
}

impl CheckReport {
    /// The process exit code.
    ///
    /// `drift` is a report unless `--fail-on-drift` asks for a gate, because
    /// "something changed and nobody has reviewed the consequence" is information,
    /// whereas "a measurement says behavior got worse" is a verdict.
    pub fn exit_code(&self, fail_on_drift: bool) -> u8 {
        match self.status {
            GateStatus::Pass => 0,
            GateStatus::Regression => 1,
            GateStatus::Drift if fail_on_drift => 1,
            GateStatus::Drift => 0,
            // `check` returns a runtime failure as an error rather than a report, so
            // this is a guard: a report that cannot be trusted must not exit 0.
            GateStatus::Error => 3,
        }
    }

    /// The status, folding the best-known worst case together with an incomplete run.
    ///
    /// A half-finished check is an error, not a pass, regardless of what the half
    /// that finished found.
    pub fn with_error(mut self, reason: impl Into<String>) -> Self {
        self.status = GateStatus::Error;
        self.error = Some(reason.into());
        self
    }
}

/// Fold the static and behavioral halves into one status, with the config's failure
/// policy applied.
/// `--fail-on-drift` is deliberately not an input here: it changes the exit code,
/// not the status. A report must still say that this was drift rather than a
/// regression, because the two call for different responses.
pub fn combined_status(
    static_status: GateStatus,
    behavior: Option<GateStatus>,
    dependency_drift: bool,
) -> GateStatus {
    let mut status = static_status;
    if let Some(behavior) = behavior {
        status = status.worst(behavior);
    }
    // Dependency drift that no policy failed is still drift.
    if dependency_drift && status == GateStatus::Pass {
        status = GateStatus::Drift;
    }
    status
}

/// The drift reasons a report should show, in a stable order.
pub fn drift_reasons(behavior: Option<&BehaviorHalf>, dependency_drift: bool) -> Vec<DriftReason> {
    let mut reasons: Vec<DriftReason> = behavior
        .map(|behavior| {
            let mut reasons = Vec::new();
            if !behavior.baseline_present {
                reasons.push(DriftReason::NoBaseline);
            } else if !behavior.suite_matches {
                reasons.push(DriftReason::ProbeSuiteChanged);
            }
            reasons
        })
        .unwrap_or_default();

    if dependency_drift {
        reasons.push(DriftReason::DependencyDrift);
    }
    reasons
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probes::{Check, MetricScores, SampleOutcome};

    fn outcome(probe: &str, passed: u32, total: u32, metric: Metric) -> ProbeOutcome {
        let samples = (0..total)
            .map(|index| SampleOutcome {
                sample: index,
                passed: index < passed,
                checks: if index < passed {
                    Vec::new()
                } else {
                    vec![Check {
                        metric,
                        passed: false,
                        detail: "the expected tool was not called".to_string(),
                    }]
                },
            })
            .collect();
        ProbeOutcome {
            probe: probe.to_string(),
            probe_digest: "sha256:aa".to_string(),
            samples,
            passed,
            total,
            metrics: MetricScores::from([(metric, MetricScore::new(passed, total))]),
        }
    }

    fn report(status: GateStatus) -> CheckReport {
        CheckReport {
            status,
            agent_checksum: crate::manifest::agent_checksum(&[]).unwrap(),
            baseline_checksum: None,
            dependency: DependencyHalf {
                changed: false,
                overall_risk: RiskLevel::None,
                changes: Vec::new(),
            },
            behavior: None,
            error: None,
        }
    }

    #[test]
    fn a_pass_exits_zero_and_a_regression_exits_one() {
        assert_eq!(report(GateStatus::Pass).exit_code(false), 0);
        assert_eq!(report(GateStatus::Regression).exit_code(false), 1);
        assert_eq!(report(GateStatus::Regression).exit_code(true), 1);
    }

    #[test]
    fn drift_is_a_report_unless_it_was_asked_to_be_a_gate() {
        // The same status, two contracts, and the report never changes shape.
        assert_eq!(report(GateStatus::Drift).exit_code(false), 0);
        assert_eq!(report(GateStatus::Drift).exit_code(true), 1);
    }

    #[test]
    fn a_half_finished_check_is_never_a_pass() {
        let halted = report(GateStatus::Pass).with_error("the model endpoint was unreachable");

        assert_eq!(halted.status, GateStatus::Error);
        assert_eq!(halted.exit_code(false), 3);
        assert!(halted.error.is_some());
    }

    #[test]
    fn an_unmeasured_metric_is_absent_rather_than_zero() {
        // A probe that calls no tool leaves argument_validity with no denominator;
        // showing it as 0% would be a claim about behavior nobody measured.
        let rows = metric_rows(
            &[outcome("restraint", 3, 3, Metric::ToolRestraint)],
            None,
            &PolicyConfig::default(),
            false,
            &[],
        );

        let names: Vec<&str> = rows.iter().map(|row| row.metric.as_str()).collect();
        assert_eq!(names, vec!["tool_restraint"]);
        assert_eq!(rows[0].verdict, MetricVerdict::Pass);
    }

    #[test]
    fn a_constrained_metric_is_shown_even_when_nothing_measured_it() {
        // A `min` on a metric nothing measured is a failure waiting to be explained,
        // so the row has to be in the table.
        let policy = PolicyConfig {
            fail_on_risk: None,
            metrics: std::collections::BTreeMap::from([(
                "argument_validity".to_string(),
                crate::config::MetricPolicy {
                    min: Some(0.9),
                    max: None,
                    max_drop: None,
                },
            )]),
        };
        let rows = metric_rows(
            &[outcome("restraint", 3, 3, Metric::ToolRestraint)],
            None,
            &policy,
            false,
            &[],
        );

        let argument = rows
            .iter()
            .find(|row| row.metric == Metric::ArgumentValidity)
            .expect("the constrained metric must be reported");
        assert_eq!(argument.verdict, MetricVerdict::NotMeasured);
        assert!(argument.current.is_none());
        assert_eq!(argument.policy, vec!["min 0.9000".to_string()]);
    }

    #[test]
    fn a_metric_no_policy_declared_is_reported_without_claiming_a_pass() {
        // Three of five samples called a tool nobody expected. No threshold was
        // declared for tool_selection, so nothing gates it — but "PASS" would be the
        // report answering a question nobody asked.
        let rows = metric_rows(
            &[outcome("search", 3, 5, Metric::ToolSelection)],
            None,
            &PolicyConfig::default(),
            false,
            &[],
        );

        assert_eq!(rows[0].verdict, MetricVerdict::Warn);
        assert_eq!(rows[0].current, Some(MetricScore::new(3, 5)));

        // With a threshold declared and satisfied, it is a pass again.
        let policy = PolicyConfig {
            fail_on_risk: None,
            metrics: std::collections::BTreeMap::from([(
                "tool_selection".to_string(),
                crate::config::MetricPolicy {
                    min: Some(0.5),
                    max: None,
                    max_drop: None,
                },
            )]),
        };
        let gated = metric_rows(
            &[outcome("search", 3, 5, Metric::ToolSelection)],
            None,
            &policy,
            false,
            &[],
        );
        assert_eq!(gated[0].verdict, MetricVerdict::Pass);
    }

    #[test]
    fn a_metric_a_policy_failed_is_marked_failed() {
        let rows = metric_rows(
            &[outcome("search", 7, 10, Metric::ArgumentValidity)],
            None,
            &PolicyConfig::default(),
            false,
            &[Metric::ArgumentValidity],
        );

        assert_eq!(rows[0].verdict, MetricVerdict::Fail);
        assert_eq!(rows[0].current, Some(MetricScore::new(7, 10)));
    }

    #[test]
    fn a_baseline_is_only_shown_when_it_is_comparable() {
        let baseline = crate::gate::BehaviorBaseline::new(
            crate::manifest::agent_checksum(&[]).unwrap(),
            crate::manifest::Digest::sha256(b"suite"),
            MetricScores::from([(Metric::ToolRestraint, MetricScore::new(3, 3))]),
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
        );
        let outcomes = [outcome("restraint", 2, 3, Metric::ToolRestraint)];

        let comparable = metric_rows(
            &outcomes,
            Some(&baseline),
            &PolicyConfig::default(),
            true,
            &[],
        );
        assert_eq!(comparable[0].baseline, Some(MetricScore::new(3, 3)));

        // A score from a different suite answers a different question, so it is not
        // shown as this run's "before".
        let incomparable = metric_rows(
            &outcomes,
            Some(&baseline),
            &PolicyConfig::default(),
            false,
            &[],
        );
        assert_eq!(incomparable[0].baseline, None);
    }

    #[test]
    fn a_repeated_failure_is_reported_once_per_wording() {
        let half = BehaviorHalf::of(
            &[outcome("search", 1, 3, Metric::ToolSelection)],
            &BehaviorOutcome {
                baseline_present: true,
                probe_suite_digest: "sha256:aa".to_string(),
                suite_matches: true,
                yardsticks_changed: Vec::new(),
                failures: Vec::new(),
                notes: Vec::new(),
                relative_undecided: false,
            },
            None,
            &PolicyConfig::default(),
        );

        assert_eq!(half.probes_passed, 1);
        assert_eq!(half.probes_total, 3);
        // Two failing samples, one wording.
        assert_eq!(half.probes[0].failures.len(), 1);
        assert!(half.probes[0].failures[0].starts_with("sample 1: tool_selection"));
    }

    #[test]
    fn a_half_finished_recording_is_the_probe_name_and_its_own_suite() {
        let half = BehaviorHalf::of(
            &[outcome("search", 3, 3, Metric::ToolSelection)],
            &BehaviorOutcome {
                baseline_present: false,
                probe_suite_digest: "sha256:bb".to_string(),
                suite_matches: false,
                yardsticks_changed: Vec::new(),
                failures: Vec::new(),
                notes: Vec::new(),
                relative_undecided: false,
            },
            None,
            &PolicyConfig::default(),
        );

        assert!(!half.baseline_present);
        assert_eq!(half.probe_suite_digest, "sha256:bb");
        assert_eq!(half.drift_reasons, vec!["no behavioral baseline exists"]);
    }

    #[test]
    fn dependency_drift_makes_a_clean_comparison_drift() {
        assert_eq!(
            combined_status(GateStatus::Pass, Some(GateStatus::Pass), true),
            GateStatus::Drift
        );
        assert_eq!(
            combined_status(GateStatus::Pass, Some(GateStatus::Pass), false),
            GateStatus::Pass
        );
        // A regression is a regression, whatever the dependencies did.
        assert_eq!(
            combined_status(GateStatus::Regression, Some(GateStatus::Pass), true),
            GateStatus::Regression
        );
    }
}
