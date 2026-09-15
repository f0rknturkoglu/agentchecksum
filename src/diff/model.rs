// SPDX-License-Identifier: MIT OR Apache-2.0

//! The typed result of comparing a baseline against current dependency state.
//!
//! The comparison produces this structure and nothing else: rendering happens
//! afterwards, so the report is reusable and the engine stays pure.

use serde::{Deserialize, Serialize};

use crate::config::RiskLevel;
use crate::manifest::{AgentChecksum, DependencyKind, Digest};

/// What happened to a dependency, a facet, or an aspect of one.
///
/// A report describes a *comparison*, not only a delta: `Unchanged` exists so a
/// modified dependency can list the facets it was compared on and found equal.
/// That is the evidence behind the product's sharpest sentence — the schema did
/// not break, the description did — and without it a reader cannot tell a facet
/// that was checked from a facet that was never looked at.
///
/// An unchanged facet always carries `RiskLevel::None` and no details. Details
/// themselves are never `Unchanged`: a detail exists because something differed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
    Unchanged,
}

impl ChangeKind {
    /// Lowercase token used in human output and in the JSON contract.
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Added => "added",
            ChangeKind::Removed => "removed",
            ChangeKind::Modified => "modified",
            ChangeKind::Unchanged => "unchanged",
        }
    }
}

/// One aspect of one facet that differs.
///
/// `path` is a dotted location whose vocabulary is documented per facet:
/// `quantization_level` for model identity, `configured.temperature` for params,
/// `properties.owner.type` or `required` for schemas. `before`/`after` carry the
/// values when the lockfile has a normalized payload to read them from, and are
/// `None` when only the digest is known — the difference is still reported, it is
/// simply not explained further.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetailChange {
    pub path: String,
    pub change: ChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<serde_json::Value>,
}

impl DetailChange {
    pub fn new(path: impl Into<String>, change: ChangeKind) -> Self {
        Self {
            path: path.into(),
            change,
            before: None,
            after: None,
        }
    }

    /// A value change: `before` and `after` are both known.
    pub fn swapped(
        path: impl Into<String>,
        before: serde_json::Value,
        after: serde_json::Value,
    ) -> Self {
        Self {
            path: path.into(),
            change: ChangeKind::Modified,
            before: Some(before),
            after: Some(after),
        }
    }

    /// An appearance or disappearance where only the new/old value is known.
    ///
    /// `Unchanged` is not a meaningful input here — a detail exists because
    /// something differed — but it degrades to an informational value rather
    /// than panicking on a caller mistake.
    pub fn one_sided(
        path: impl Into<String>,
        change: ChangeKind,
        value: serde_json::Value,
    ) -> Self {
        let (before, after) = match change {
            ChangeKind::Removed => (Some(value), None),
            ChangeKind::Added | ChangeKind::Modified | ChangeKind::Unchanged => (None, Some(value)),
        };
        Self {
            path: path.into(),
            change,
            before,
            after,
        }
    }
}

/// A facet of a modified dependency, with the evidence and the risk.
///
/// Every facet present on either side appears here, including the ones that did
/// not move: see `ChangeKind::Unchanged`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FacetChange {
    /// Facet name as it appears in the lockfile: `identity`, `content`,
    /// `input_schema`, …
    pub name: String,
    pub change: ChangeKind,
    pub risk: RiskLevel,
    /// The facet's digest on each side. `None` means the facet is absent there.
    /// These are the fingerprints that moved, so the report can be checked
    /// against the lockfile without re-reading both of them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_digest: Option<Digest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_digest: Option<Digest>,
    pub details: Vec<DetailChange>,
}

impl FacetChange {
    pub fn new(name: impl Into<String>, change: ChangeKind, risk: RiskLevel) -> Self {
        Self {
            name: name.into(),
            change,
            risk,
            before_digest: None,
            after_digest: None,
            details: Vec::new(),
        }
    }

    /// A facet that was compared and found equal: no risk, no details.
    pub fn unchanged(name: impl Into<String>, digest: Digest) -> Self {
        Self {
            name: name.into(),
            change: ChangeKind::Unchanged,
            risk: RiskLevel::None,
            before_digest: Some(digest.clone()),
            after_digest: Some(digest),
            details: Vec::new(),
        }
    }

    pub fn with_details(mut self, details: Vec<DetailChange>) -> Self {
        self.details = details;
        self
    }

    pub fn with_digests(mut self, before: Option<Digest>, after: Option<Digest>) -> Self {
        self.before_digest = before;
        self.after_digest = after;
        self
    }

    /// Whether this facet actually moved. Consumers that want only the delta
    /// filter on this rather than on the facet's presence in the list.
    pub fn is_change(&self) -> bool {
        self.change != ChangeKind::Unchanged
    }
}

/// One dependency that differs, with its facet-level explanation.
///
/// An added or removed dependency has no facets: there is nothing to compare
/// against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DependencyChange {
    pub id: String,
    pub kind: DependencyKind,
    pub change: ChangeKind,
    /// `max` of this dependency's facet risks, or the added/removed rule.
    pub risk: RiskLevel,
    pub facets: Vec<FacetChange>,
}

/// The complete comparison of two dependency states.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffReport {
    pub baseline_checksum: AgentChecksum,
    pub current_checksum: AgentChecksum,
    pub changed: bool,
    /// `max` of every dependency risk. No weighting, no score: one CRITICAL
    /// change keeps the report CRITICAL.
    pub overall_risk: RiskLevel,
    /// Sorted by dependency id; facet changes by facet name; details by path.
    pub changes: Vec<DependencyChange>,
}

/// Build a child path: `properties.owner`, `items`, or the bare key at the root.
///
/// Shared by the schema analyzer and the payload differ so both produce the same
/// path vocabulary.
pub(crate) fn child_path(parent: &str, segment: &str) -> String {
    if parent.is_empty() {
        segment.to_string()
    } else {
        format!("{parent}.{segment}")
    }
}

/// Risk of a change is the maximum of its parts, at every level.
pub fn max_risk(risks: impl IntoIterator<Item = RiskLevel>) -> RiskLevel {
    risks.into_iter().max().unwrap_or(RiskLevel::None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_kind_tokens_are_the_json_contract() {
        assert_eq!(ChangeKind::Added.as_str(), "added");
        assert_eq!(ChangeKind::Removed.as_str(), "removed");
        assert_eq!(ChangeKind::Modified.as_str(), "modified");
        assert_eq!(
            serde_json::to_string(&ChangeKind::Modified).unwrap(),
            "\"modified\""
        );
    }

    #[test]
    fn max_risk_takes_the_highest_not_an_average() {
        assert_eq!(
            max_risk([RiskLevel::Low, RiskLevel::Medium]),
            RiskLevel::Medium
        );
        assert_eq!(
            max_risk([RiskLevel::Medium, RiskLevel::Critical]),
            RiskLevel::Critical
        );
        // Two LOWs do not become MEDIUM, and an empty set is NONE.
        assert_eq!(max_risk([RiskLevel::Low, RiskLevel::Low]), RiskLevel::Low);
        assert_eq!(max_risk([]), RiskLevel::None);
    }

    #[test]
    fn swapped_details_carry_both_sides_and_a_one_sided_detail_carries_one() {
        let swapped = DetailChange::swapped(
            "quantization_level",
            serde_json::json!("Q8_0"),
            serde_json::json!("Q4_K_M"),
        );
        assert_eq!(swapped.before, Some(serde_json::json!("Q8_0")));
        assert_eq!(swapped.after, Some(serde_json::json!("Q4_K_M")));
        assert_eq!(swapped.change, ChangeKind::Modified);

        let added =
            DetailChange::one_sided("required", ChangeKind::Added, serde_json::json!("owner"));
        assert_eq!(added.before, None);
        assert_eq!(added.after, Some(serde_json::json!("owner")));

        let removed =
            DetailChange::one_sided("required", ChangeKind::Removed, serde_json::json!("owner"));
        assert_eq!(removed.before, Some(serde_json::json!("owner")));
        assert_eq!(removed.after, None);
    }
}
