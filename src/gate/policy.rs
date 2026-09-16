// SPDX-License-Identifier: MIT OR Apache-2.0

//! Metric policy: the thresholds a run has to satisfy.
//!
//! Every metric points the same way — 1.0 is good — which is what lets one small
//! vocabulary describe all six. A policy needs no notion of "inverted" metrics, and
//! a reader needs no table to know which direction a number should move.

use crate::config::MetricPolicy;
use crate::probes::{Metric, MetricScore};

/// A policy a run did not satisfy. This is a gate failure: valid evidence showed
/// behavior that the configured policy does not accept.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyFailure {
    pub metric: Metric,
    pub detail: String,
}

/// Something the policy could not decide, and why.
///
/// Notes are not failures. They explain a comparison that could not honestly be
/// made — a missing baseline, a changed probe suite — which makes a run *drift*
/// rather than regress.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyNote {
    pub metric: Metric,
    pub detail: String,
}

/// The outcome of applying one metric's policy.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PolicyOutcome {
    pub failures: Vec<PolicyFailure>,
    pub notes: Vec<PolicyNote>,
    /// Whether a `max_drop` constraint was declared and could not be evaluated,
    /// because there is no baseline or because the suite changed.
    pub relative_undecided: bool,
}

impl PolicyOutcome {
    fn fail(&mut self, metric: Metric, detail: impl Into<String>) {
        self.failures.push(PolicyFailure {
            metric,
            detail: detail.into(),
        });
    }

    fn note(&mut self, metric: Metric, detail: impl Into<String>) {
        self.notes.push(PolicyNote {
            metric,
            detail: detail.into(),
        });
    }
}

/// The scores one metric can be judged against.
pub struct MetricComparison<'a> {
    /// `None` when no sample made the metric applicable.
    pub current: Option<&'a MetricScore>,
    /// The baseline's score for the same metric, when a baseline exists.
    pub baseline: Option<&'a MetricScore>,
    /// Whether `baseline` was produced by the same test *and* the same capture rules.
    ///
    /// A score from a different probe suite answers different questions, and a score
    /// from a different runner contract was produced by different rules; a relative
    /// comparison against either would be a comparison of two measurements.
    pub baseline_comparable: bool,
    /// Whether a comparison against this baseline is meaningful:
    /// a baseline was found and it describes this suite.
    pub baseline_present: bool,
}

/// Apply one metric's policy.
///
/// All declared constraints must pass. None of them excuses another: a score can
/// clear `min` and still fail `max_drop`, and that is a failure.
pub fn evaluate(
    metric: Metric,
    policy: &MetricPolicy,
    comparison: &MetricComparison<'_>,
) -> PolicyOutcome {
    let mut outcome = PolicyOutcome::default();
    let current = comparison.current.and_then(MetricScore::score);

    if let Some(min) = policy.min {
        match current {
            Some(current) if current < min => outcome.fail(
                metric,
                format!("score {current:.4} is below the required minimum {min:.4}"),
            ),
            Some(_) => {}
            // A floor is a demand for evidence. Passing it because nothing was
            // measured would be the most flattering possible reading of an absent
            // result.
            None => outcome.fail(
                metric,
                "no sample made this metric applicable, so its minimum cannot be met",
            ),
        }
    }

    if let Some(max) = policy.max {
        // A ceiling needs no evidence to hold: nothing measured cannot exceed it.
        if let Some(current) = current
            && current > max
        {
            outcome.fail(
                metric,
                format!("score {current:.4} is above the allowed maximum {max:.4}"),
            );
        }
    }

    if let Some(max_drop) = policy.max_drop {
        let comparable = comparison.baseline_present && comparison.baseline_comparable;
        let pair = comparison
            .baseline
            .and_then(MetricScore::score)
            .zip(current);

        match pair {
            Some((baseline, current)) if comparable => {
                let drop = baseline - current;
                if drop > max_drop {
                    outcome.fail(
                        metric,
                        format!(
                            "score fell from {baseline:.4} to {current:.4}, a drop of {drop:.4} \
                             beyond the allowed {max_drop:.4}"
                        ),
                    );
                }
            }
            _ => {
                outcome.relative_undecided = true;
                // Which reason it is belongs to the report's drift reasons, which knows
                // about both the suite and the runner contract. Saying "the suite
                // changed" here would be guessing.
                outcome.note(
                    metric,
                    if comparison.baseline_present {
                        "the committed baseline is not comparable with this run, so a drop \
                         cannot be measured against it"
                    } else {
                        "no behavioral baseline exists, so a drop cannot be measured"
                    },
                );
            }
        }
    }

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(min: Option<f64>, max: Option<f64>, max_drop: Option<f64>) -> MetricPolicy {
        MetricPolicy { min, max, max_drop }
    }

    fn comparison<'a>(
        current: Option<&'a MetricScore>,
        baseline: Option<&'a MetricScore>,
        baseline_comparable: bool,
    ) -> MetricComparison<'a> {
        MetricComparison {
            current,
            baseline,
            baseline_comparable,
            baseline_present: baseline.is_some(),
        }
    }

    #[test]
    fn a_minimum_is_a_floor_on_the_current_score() {
        let passing = MetricScore::new(91, 100);
        let failing = MetricScore::new(89, 100);

        assert!(
            evaluate(
                Metric::ToolSelection,
                &policy(Some(0.90), None, None),
                &comparison(Some(&passing), None, false)
            )
            .failures
            .is_empty()
        );
        assert_eq!(
            evaluate(
                Metric::ToolSelection,
                &policy(Some(0.90), None, None),
                &comparison(Some(&failing), None, false)
            )
            .failures
            .len(),
            1
        );
    }

    #[test]
    fn a_floor_that_nothing_measured_is_not_a_pass() {
        // The alternative is a policy that reports 100% because no probe asserted it.
        let outcome = evaluate(
            Metric::ArgumentValidity,
            &policy(Some(0.9), None, None),
            &comparison(None, None, false),
        );

        assert_eq!(outcome.failures.len(), 1);
        assert!(outcome.failures[0].detail.contains("no sample"));
    }

    #[test]
    fn a_ceiling_holds_when_nothing_was_measured() {
        // A ceiling needs no evidence; a floor does.
        let outcome = evaluate(
            Metric::ForbiddenToolUsage,
            &policy(None, Some(1.0), None),
            &comparison(None, None, false),
        );

        assert!(outcome.failures.is_empty());
    }

    #[test]
    fn both_constraints_must_pass() {
        // 0.91 clears the floor and still fails the drop.
        let current = MetricScore::new(91, 100);
        let baseline = MetricScore::new(99, 100);
        let outcome = evaluate(
            Metric::ToolSelection,
            &policy(Some(0.90), None, Some(0.05)),
            &comparison(Some(&current), Some(&baseline), true),
        );

        assert_eq!(outcome.failures.len(), 1);
        assert!(outcome.failures[0].detail.contains("fell from"));
    }

    #[test]
    fn an_improvement_never_fails_a_drop_constraint() {
        let current = MetricScore::new(100, 100);
        let baseline = MetricScore::new(80, 100);

        assert!(
            evaluate(
                Metric::ToolSelection,
                &policy(None, None, Some(0.05)),
                &comparison(Some(&current), Some(&baseline), true),
            )
            .failures
            .is_empty()
        );
    }

    #[test]
    fn a_drop_cannot_be_measured_without_a_comparable_baseline() {
        let current = MetricScore::new(50, 100);
        let baseline = MetricScore::new(100, 100);

        // No baseline at all.
        let absent = evaluate(
            Metric::ToolSelection,
            &policy(None, None, Some(0.05)),
            &comparison(Some(&current), None, false),
        );
        assert!(absent.failures.is_empty());
        assert!(absent.relative_undecided);
        assert!(absent.notes[0].detail.contains("no behavioral baseline"));

        // A baseline from a different suite — or a different runner contract: either
        // way the scores answer different questions.
        let changed = evaluate(
            Metric::ToolSelection,
            &policy(None, None, Some(0.05)),
            &comparison(Some(&current), Some(&baseline), false),
        );
        assert!(changed.failures.is_empty());
        assert!(changed.relative_undecided);
        assert!(changed.notes[0].detail.contains("not comparable"));
    }

    #[test]
    fn an_empty_policy_decides_nothing() {
        let current = MetricScore::new(0, 10);
        let outcome = evaluate(
            Metric::ToolSelection,
            &policy(None, None, None),
            &comparison(Some(&current), None, false),
        );

        assert!(outcome.failures.is_empty());
        assert!(outcome.notes.is_empty());
        assert!(!outcome.relative_undecided);
    }
}
