// SPDX-License-Identifier: MIT OR Apache-2.0

//! The six behavioral metrics, as exact counts with a derived score.
//!
//! Every metric points the same way: **1.0 is good**. That is what lets one policy
//! vocabulary (`min`, `max`, `max_drop`) describe all six, and it is why the report
//! never shows a percentage that has no denominator behind it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A behavioral metric.
///
/// Which ones apply is decided by the expectations a probe declares — users do not
/// select metrics, because a metric with no assertion behind it would be a number
/// pretending to be evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    ToolSelection,
    ArgumentValidity,
    ArgumentExpectation,
    ForbiddenToolUsage,
    ToolRestraint,
    StructuredOutputValidity,
}

impl Metric {
    /// Every metric, in the order a report presents them.
    pub const ALL: [Metric; 6] = [
        Metric::ToolSelection,
        Metric::ArgumentValidity,
        Metric::ArgumentExpectation,
        Metric::ForbiddenToolUsage,
        Metric::ToolRestraint,
        Metric::StructuredOutputValidity,
    ];

    /// The name used in configuration, JSON, and the baseline.
    pub fn as_str(self) -> &'static str {
        match self {
            Metric::ToolSelection => "tool_selection",
            Metric::ArgumentValidity => "argument_validity",
            Metric::ArgumentExpectation => "argument_expectation",
            Metric::ForbiddenToolUsage => "forbidden_tool_usage",
            Metric::ToolRestraint => "tool_restraint",
            Metric::StructuredOutputValidity => "structured_output_validity",
        }
    }

    /// The metric with this name, or `None`.
    ///
    /// Policy configuration uses it to reject a typo rather than to carry an unused
    /// metric that silently never applies.
    pub fn known(name: &str) -> Option<Metric> {
        Metric::ALL
            .into_iter()
            .find(|metric| metric.as_str() == name)
    }
}

/// Exact counts, and the score they imply.
///
/// Stored as a fraction rather than a float so a report can be audited: `9 / 10`
/// survives an argument about rounding that `0.9` does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricScore {
    pub passed: u32,
    pub total: u32,
}

impl MetricScore {
    pub fn new(passed: u32, total: u32) -> Self {
        Self { passed, total }
    }

    /// The score, or `None` when no sample made the metric applicable.
    ///
    /// `None` is deliberately not zero: an inapplicable metric is absent from the
    /// report and from policy, whereas zero would be a claim about behavior that was
    /// never measured. It also prevents the opposite error — scoring a sample that
    /// called no tools as a perfect `argument_validity`.
    pub fn score(&self) -> Option<f64> {
        (self.total > 0).then(|| f64::from(self.passed) / f64::from(self.total))
    }

    /// Fold one sample result in.
    pub fn record(&mut self, passed: bool) {
        self.total += 1;
        if passed {
            self.passed += 1;
        }
    }
}

/// The metric results of one run, in metric order rather than insertion order.
pub type MetricScores = BTreeMap<Metric, MetricScore>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_metric_points_the_same_way() {
        // The single vocabulary a policy needs: a bigger score is a better score.
        assert_eq!(Metric::ALL.len(), 6);
        for metric in Metric::ALL {
            let perfect = MetricScore::new(3, 3).score().unwrap();
            let partial = MetricScore::new(1, 3).score().unwrap();
            let none = MetricScore::new(0, 3).score().unwrap();
            assert_eq!(perfect, 1.0, "{metric:?}");
            assert_eq!(none, 0.0, "{metric:?}");
            assert!(partial > none && partial < perfect, "{metric:?}");
        }
    }

    #[test]
    fn an_inapplicable_metric_has_no_score_and_never_a_percentage() {
        let absent = MetricScore::new(0, 0);
        assert_eq!(absent.score(), None);
    }

    #[test]
    fn metric_names_are_the_configuration_vocabulary() {
        for metric in Metric::ALL {
            assert_eq!(Metric::known(metric.as_str()), Some(metric));
        }
        // A typo is a typo, not an unused policy that quietly never applies.
        assert_eq!(Metric::known("tool_seletion"), None);
        assert_eq!(Metric::known(""), None);
    }

    #[test]
    fn scores_serialize_as_the_six_names() {
        let mut scores = MetricScores::new();
        scores.insert(Metric::ToolSelection, MetricScore::new(9, 10));
        scores.insert(Metric::ToolRestraint, MetricScore::new(1, 1));

        let json = serde_json::to_value(&scores).unwrap();
        assert_eq!(json["tool_selection"]["passed"], 9);
        assert_eq!(json["tool_selection"]["total"], 10);
        assert_eq!(json["tool_restraint"]["passed"], 1);
        // Exact counts travel with the score; a percentage alone would not be auditable.
        assert!(json["tool_selection"].get("score").is_none());
    }
}
