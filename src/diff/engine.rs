// SPDX-License-Identifier: MIT OR Apache-2.0

//! The pure comparison: two dependency states in, one report out.
//!
//! No filesystem, no network, no clock, no terminal. Everything that could differ
//! between two runs of the same inputs has already happened by the time this is
//! called, which is what makes the result reproducible and cheap to test.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::RiskLevel;
use crate::diff::analyze;
use crate::diff::model::{ChangeKind, DependencyChange, DiffReport, FacetChange, max_risk};
use crate::diff::risk;
use crate::lockfile::{LockedDependency, Lockfile};
use crate::manifest::DependencyKind;

/// Compare a baseline against current state.
pub fn diff(baseline: &Lockfile, current: &Lockfile) -> DiffReport {
    // Spec §8.1 permits the aggregate to short-circuit the common case. It is a
    // saving, not the algorithm: the semantic comparison below is what actually
    // establishes a report, and its tests exercise it directly by giving two
    // states deliberately unrelated aggregates.
    if baseline.agent_checksum == current.agent_checksum {
        return unchanged(baseline, current);
    }

    let mut changes = Vec::new();

    // Union of identities, iterated in sorted order so the report never depends
    // on discovery order or on map iteration order.
    let mut ids: BTreeSet<&String> = baseline.dependencies.keys().collect();
    ids.extend(current.dependencies.keys());

    for id in ids {
        match (baseline.dependencies.get(id), current.dependencies.get(id)) {
            (None, Some(added)) => changes.push(whole_change(
                id,
                added.kind,
                ChangeKind::Added,
                risk::added(added.kind),
            )),
            (Some(removed), None) => changes.push(whole_change(
                id,
                removed.kind,
                ChangeKind::Removed,
                risk::removed(removed.kind),
            )),
            (Some(before), Some(after)) => {
                if before.kind != after.kind {
                    // The same identity now means a different kind of thing. The
                    // facet maps on either side were produced under different
                    // rules and are not comparable, so this stands on its own
                    // rather than as a facet difference.
                    changes.push(whole_change(
                        id,
                        after.kind,
                        ChangeKind::Modified,
                        risk::kind_mismatch(),
                    ));
                    continue;
                }

                let facets = facet_changes(before, after);
                // The facet list includes the facets that were compared and found
                // equal, so "the list is empty" is no longer the test for "this
                // dependency did not change".
                if !facets.iter().any(FacetChange::is_change) {
                    continue;
                }
                let risk_level = max_risk(facets.iter().map(|facet| facet.risk));
                changes.push(DependencyChange {
                    id: id.clone(),
                    kind: after.kind,
                    change: ChangeKind::Modified,
                    risk: risk_level,
                    facets,
                });
            }
            (None, None) => {}
        }
    }

    let overall_risk = max_risk(changes.iter().map(|change| change.risk));
    DiffReport {
        baseline_checksum: baseline.agent_checksum.clone(),
        current_checksum: current.agent_checksum.clone(),
        changed: !changes.is_empty(),
        overall_risk,
        changes,
    }
}

fn unchanged(baseline: &Lockfile, current: &Lockfile) -> DiffReport {
    DiffReport {
        baseline_checksum: baseline.agent_checksum.clone(),
        current_checksum: current.agent_checksum.clone(),
        changed: false,
        overall_risk: RiskLevel::None,
        changes: Vec::new(),
    }
}

fn whole_change(
    id: &str,
    kind: DependencyKind,
    change: ChangeKind,
    risk: RiskLevel,
) -> DependencyChange {
    DependencyChange {
        id: id.to_string(),
        kind,
        change,
        risk,
        // An added or removed dependency has nothing to compare against.
        facets: Vec::new(),
    }
}

/// Facet-level comparison for one dependency whose identity and kind match.
///
/// The result is the full facet inventory of the dependency, sorted by name: the
/// facets that moved, and the facets that were compared and found equal. The
/// analyzers claim the facets they understand; everything else goes through the
/// generic rules. That order is deliberate — it is what guarantees that a digest
/// difference in a facet nobody has taught the engine about still becomes a
/// reported change instead of disappearing.
fn facet_changes(before: &LockedDependency, after: &LockedDependency) -> Vec<FacetChange> {
    let mut analyzed: BTreeMap<String, FacetChange> = analyze::for_dependency(before, after)
        .into_iter()
        .map(|facet| (facet.name.clone(), facet))
        .collect();

    // The union of both sides' facets, plus anything an analyzer named, so a
    // claimed facet can never be dropped from the report by a name mismatch.
    let mut names: BTreeSet<String> = before.facets.keys().cloned().collect();
    names.extend(after.facets.keys().cloned());
    names.extend(analyzed.keys().cloned());

    let mut changes = Vec::with_capacity(names.len());

    for name in names {
        let before_digest = before.facets.get(&name).map(|facet| facet.digest.clone());
        let after_digest = after.facets.get(&name).map(|facet| facet.digest.clone());

        if let Some(change) = analyzed.remove(&name) {
            changes.push(change.with_digests(before_digest, after_digest));
            continue;
        }

        // Nothing claimed this facet, so the generic rules decide from the
        // digests alone.
        let change = match (&before_digest, &after_digest) {
            (Some(before_digest), Some(after_digest)) if before_digest == after_digest => {
                FacetChange::unchanged(name, before_digest.clone())
            }
            (Some(_), Some(_)) => {
                let risk = risk::unknown_facet(before.kind, &name);
                FacetChange::new(name, ChangeKind::Modified, risk)
            }
            (None, Some(_)) => {
                let risk = risk::facet_added(before.kind, &name);
                FacetChange::new(name, ChangeKind::Added, risk)
            }
            (Some(_), None) => {
                let risk = risk::facet_removed(before.kind, &name);
                FacetChange::new(name, ChangeKind::Removed, risk)
            }
            (None, None) => continue,
        };

        changes.push(change.with_digests(before_digest, after_digest));
    }

    changes.sort_by(|left, right| left.name.cmp(&right.name));
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockfile::Generator;
    use crate::manifest::{AgentChecksum, Dependency, Digest, Facet, agent_checksum};
    use std::collections::BTreeMap;

    fn facet(seed: &str) -> Facet {
        Facet {
            digest: Digest::sha256(seed.as_bytes()),
            shape: None,
            normalized: None,
        }
    }

    fn facets(pairs: &[(&str, &str)]) -> BTreeMap<String, Facet> {
        pairs
            .iter()
            .map(|(name, seed)| ((*name).to_string(), facet(seed)))
            .collect()
    }

    fn dependency(kind: DependencyKind, id: &str, pairs: &[(&str, &str)]) -> Dependency {
        Dependency {
            id: id.to_string(),
            kind,
            facets: facets(pairs),
            source: None,
        }
    }

    /// A real aggregate, but for a dependency set unrelated to the lockfile it is
    /// attached to. Tests use this to skip the aggregate fast path and reach the
    /// semantic comparison on purpose.
    fn unrelated_checksum(seed: &str) -> AgentChecksum {
        agent_checksum(&[dependency(
            DependencyKind::Prompt,
            "prompt:seed.md",
            &[("content", seed)],
        )])
        .unwrap()
    }

    fn lockfile(dependencies: Vec<Dependency>) -> Lockfile {
        Lockfile::from_dependencies(&dependencies).unwrap()
    }

    fn lockfile_with(
        entries: Vec<(&str, DependencyKind, BTreeMap<String, Facet>)>,
        checksum_seed: &str,
    ) -> Lockfile {
        let mut dependencies = BTreeMap::new();
        for (id, kind, facets) in entries {
            dependencies.insert(
                id.to_string(),
                LockedDependency {
                    kind,
                    facets,
                    source: None,
                },
            );
        }
        Lockfile {
            lock_version: 1,
            generator: Generator {
                name: "agentchecksum".to_string(),
                version: "0.0.0-test".to_string(),
            },
            agent_checksum: unrelated_checksum(checksum_seed),
            dependencies,
        }
    }

    #[test]
    fn identical_state_reports_nothing_through_the_aggregate_fast_path() {
        let baseline = lockfile(vec![dependency(
            DependencyKind::Prompt,
            "prompt:a.md",
            &[("content", "c")],
        )]);
        let report = diff(&baseline, &baseline);

        assert!(!report.changed);
        assert_eq!(report.overall_risk, RiskLevel::None);
        assert!(report.changes.is_empty());
        assert_eq!(report.baseline_checksum, report.current_checksum);
    }

    #[test]
    fn a_semantically_identical_pair_reports_no_change_even_with_different_aggregates() {
        // This is the test that proves the semantic comparison is independently
        // correct rather than leaning on the aggregate: the fast path is skipped
        // because the two recorded checksums differ.
        let baseline = lockfile_with(
            vec![(
                "prompt:a.md",
                DependencyKind::Prompt,
                facets(&[("content", "same")]),
            )],
            "before",
        );
        let current = lockfile_with(
            vec![(
                "prompt:a.md",
                DependencyKind::Prompt,
                facets(&[("content", "same")]),
            )],
            "after",
        );

        let report = diff(&baseline, &current);
        assert!(!report.changed, "{report:?}");
        assert_eq!(report.overall_risk, RiskLevel::None);
    }

    #[test]
    fn an_added_dependency_uses_its_kind_rule() {
        let baseline = lockfile(vec![]);
        let current = lockfile(vec![dependency(
            DependencyKind::Tool,
            "tool:s.search",
            &[("description", "d")],
        )]);

        let report = diff(&baseline, &current);
        assert_eq!(report.changes.len(), 1);
        assert_eq!(report.changes[0].change, ChangeKind::Added);
        assert_eq!(report.changes[0].risk, RiskLevel::High);
        assert!(report.changes[0].facets.is_empty());
    }

    #[test]
    fn a_removed_model_is_critical() {
        let baseline = lockfile(vec![dependency(
            DependencyKind::Model,
            "model:ollama/m",
            &[("identity", "i")],
        )]);
        let current = lockfile(vec![]);

        let report = diff(&baseline, &current);
        assert_eq!(report.changes[0].change, ChangeKind::Removed);
        assert_eq!(report.changes[0].risk, RiskLevel::Critical);
        assert_eq!(report.overall_risk, RiskLevel::Critical);
    }

    #[test]
    fn a_modified_dependency_reports_the_facet_that_changed() {
        let baseline = lockfile(vec![dependency(
            DependencyKind::Prompt,
            "prompt:a.md",
            &[("content", "c1")],
        )]);
        let current = lockfile(vec![dependency(
            DependencyKind::Prompt,
            "prompt:a.md",
            &[("content", "c2")],
        )]);

        let report = diff(&baseline, &current);
        assert_eq!(report.changes.len(), 1);
        assert_eq!(report.changes[0].change, ChangeKind::Modified);
        assert_eq!(report.changes[0].facets.len(), 1);
        // With no shape facet, formatting-only cannot be claimed.
        assert_eq!(report.changes[0].facets[0].risk, RiskLevel::Medium);
    }

    #[test]
    fn a_facet_added_or_removed_is_reported_rather_than_ignored() {
        let baseline = lockfile_with(
            vec![(
                "model:ollama/m",
                DependencyKind::Model,
                facets(&[("identity", "i")]),
            )],
            "before",
        );
        let current = lockfile_with(
            vec![(
                "model:ollama/m",
                DependencyKind::Model,
                facets(&[("identity", "i"), ("template", "t")]),
            )],
            "after",
        );

        let report = diff(&baseline, &current);
        // Both facets are listed: the one that appeared, and the one that was
        // compared and did not move.
        assert_eq!(report.changes[0].facets.len(), 2);
        let template = report.changes[0]
            .facets
            .iter()
            .find(|facet| facet.name == "template")
            .unwrap();
        assert_eq!(template.change, ChangeKind::Added);
        assert_eq!(template.risk, RiskLevel::Medium);
        assert!(template.before_digest.is_none());
        assert!(template.after_digest.is_some());

        let identity = report.changes[0]
            .facets
            .iter()
            .find(|facet| facet.name == "identity")
            .unwrap();
        assert_eq!(identity.change, ChangeKind::Unchanged);
        assert_eq!(identity.risk, RiskLevel::None);
        assert_eq!(identity.before_digest, identity.after_digest);

        let reversed = diff(&current, &baseline);
        let template = reversed.changes[0]
            .facets
            .iter()
            .find(|facet| facet.name == "template")
            .unwrap();
        assert_eq!(template.change, ChangeKind::Removed);
        assert_eq!(template.risk, RiskLevel::High);
    }

    #[test]
    fn an_unknown_facet_change_is_never_silent() {
        // A facet from a future version: no analyzer knows it, and the change
        // still has to surface.
        let baseline = lockfile_with(
            vec![(
                "prompt:a.md",
                DependencyKind::Prompt,
                facets(&[("content", "c"), ("future_thing", "1")]),
            )],
            "before",
        );
        let current = lockfile_with(
            vec![(
                "prompt:a.md",
                DependencyKind::Prompt,
                facets(&[("content", "c"), ("future_thing", "2")]),
            )],
            "after",
        );

        let report = diff(&baseline, &current);
        assert_eq!(report.changes.len(), 1, "{report:?}");

        let unknown = report.changes[0]
            .facets
            .iter()
            .find(|facet| facet.name == "future_thing")
            .unwrap();
        assert!(unknown.is_change());
        assert!(unknown.risk >= RiskLevel::Medium);

        // The facet nobody changed is still listed, so a reader can tell it was
        // compared rather than skipped.
        let content = report.changes[0]
            .facets
            .iter()
            .find(|facet| facet.name == "content")
            .unwrap();
        assert_eq!(content.change, ChangeKind::Unchanged);
    }

    #[test]
    fn a_kind_mismatch_is_critical_and_stands_alone() {
        let baseline = lockfile_with(
            vec![(
                "weird:id",
                DependencyKind::Prompt,
                facets(&[("content", "c")]),
            )],
            "before",
        );
        let current = lockfile_with(
            vec![(
                "weird:id",
                DependencyKind::Model,
                facets(&[("identity", "i")]),
            )],
            "after",
        );

        let report = diff(&baseline, &current);
        assert_eq!(report.changes.len(), 1);
        assert_eq!(report.changes[0].change, ChangeKind::Modified);
        assert_eq!(report.changes[0].risk, RiskLevel::Critical);
        assert_eq!(report.overall_risk, RiskLevel::Critical);
        assert!(
            report.changes[0].facets.is_empty(),
            "facet maps from different kinds are not comparable"
        );
    }

    #[test]
    fn dependency_order_does_not_change_the_report() {
        let a = dependency(DependencyKind::Prompt, "prompt:a.md", &[("content", "c1")]);
        let b = dependency(
            DependencyKind::Model,
            "model:ollama/m",
            &[("identity", "i1")],
        );
        let c = dependency(DependencyKind::Tool, "tool:s.t", &[("description", "d1")]);

        let baseline = lockfile(vec![a.clone(), b.clone(), c.clone()]);
        let reversed_baseline = lockfile(vec![c, b, a]);
        let current = lockfile(vec![
            dependency(DependencyKind::Prompt, "prompt:a.md", &[("content", "c2")]),
            dependency(
                DependencyKind::Model,
                "model:ollama/m",
                &[("identity", "i2")],
            ),
            dependency(DependencyKind::Tool, "tool:s.t", &[("description", "d2")]),
        ]);

        let forward = diff(&baseline, &current);
        assert_eq!(forward, diff(&reversed_baseline, &current));

        let ids: Vec<&str> = forward.changes.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["model:ollama/m", "prompt:a.md", "tool:s.t"]);
    }

    #[test]
    fn facet_order_does_not_change_the_report() {
        let baseline = lockfile_with(
            vec![(
                "prompt:a.md",
                DependencyKind::Prompt,
                facets(&[("content", "c1"), ("shape", "s1")]),
            )],
            "before",
        );
        let current = lockfile_with(
            vec![(
                "prompt:a.md",
                DependencyKind::Prompt,
                facets(&[("shape", "s2"), ("content", "c2")]),
            )],
            "after",
        );

        let report = diff(&baseline, &current);
        let names: Vec<&str> = report.changes[0]
            .facets
            .iter()
            .map(|facet| facet.name.as_str())
            .collect();
        assert_eq!(names, vec!["content", "shape"]);
        assert_eq!(report, diff(&baseline, &current));
    }

    #[test]
    fn overall_risk_is_the_maximum_not_an_average() {
        let baseline = lockfile(vec![
            dependency(DependencyKind::Prompt, "prompt:a.md", &[("content", "c1")]),
            dependency(
                DependencyKind::Model,
                "model:ollama/m",
                &[("identity", "i1")],
            ),
        ]);
        let current = lockfile(vec![
            dependency(DependencyKind::Prompt, "prompt:a.md", &[("content", "c2")]),
            dependency(
                DependencyKind::Model,
                "model:ollama/m",
                &[("identity", "i2")],
            ),
        ]);

        let report = diff(&baseline, &current);
        assert_eq!(report.overall_risk, RiskLevel::High);
        assert_eq!(
            report.overall_risk,
            max_risk(report.changes.iter().map(|change| change.risk))
        );
    }
}
