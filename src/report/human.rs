// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::config::RiskLevel;
use crate::diff::{ChangeKind, DetailChange, DiffReport, FacetChange};
use crate::discovery::Discovery;
use crate::lockfile::Lockfile;
use crate::manifest::{DependencyKind, Digest};

/// The `snapshot` summary, in the shape described by the spec.
pub fn snapshot(lock: &Lockfile, discovery: &Discovery) -> String {
    let mut out = String::from("Agent checksum generated.\n\nChecksum:\n");
    out.push_str(lock.agent_checksum.as_str());
    out.push_str("\n\nDependencies:\n");

    for kind in [
        DependencyKind::Model,
        DependencyKind::Prompt,
        DependencyKind::Tool,
        DependencyKind::McpServer,
    ] {
        let count = discovery
            .dependencies
            .iter()
            .filter(|dependency| dependency.kind == kind)
            .count();
        if count == 0 {
            continue;
        }
        let plural = if count == 1 { "" } else { "s" };
        match kind {
            DependencyKind::McpServer => out.push_str(&format!("{count} MCP server{plural}\n")),
            other => out.push_str(&format!("{count} {}{plural}\n", other.as_str())),
        }
    }

    for warning in &discovery.warnings {
        out.push_str(&format!("\nwarning: {warning}\n"));
    }

    out
}

/// Risk labels in human output are uppercase; the JSON contract keeps them
/// lowercase.
fn risk_label(risk: RiskLevel) -> &'static str {
    match risk {
        RiskLevel::None => "NONE",
        RiskLevel::Low => "LOW",
        RiskLevel::Medium => "MEDIUM",
        RiskLevel::High => "HIGH",
        RiskLevel::Critical => "CRITICAL",
    }
}

/// The dependency kind as a column header. `MCP SERVER` is two words in prose.
fn kind_label(kind: DependencyKind) -> String {
    match kind {
        DependencyKind::McpServer => "MCP SERVER".to_string(),
        other => other.as_str().to_uppercase(),
    }
}

/// The identity without its redundant kind prefix: `TOOL  github.search_repos`
/// rather than `TOOL  tool:github.search_repos`.
fn bare_id(id: &str, kind: DependencyKind) -> &str {
    let prefix = format!("{}:", kind.as_str());
    id.strip_prefix(&prefix).unwrap_or(id)
}

/// A digest shortened to its algorithm and eight hex characters.
///
/// A log line is not a verification surface: eight characters are enough to see
/// that a fingerprint moved and to match it against the lockfile by eye.
fn abbreviate(digest: &Digest) -> String {
    match digest.as_str().split_once(':') {
        Some((algorithm, hex)) => {
            let head: String = hex.chars().take(8).collect();
            format!("{algorithm}:{head}…")
        }
        None => digest.as_str().to_string(),
    }
}

/// Render a value for the terminal: strings bare, everything else as compact
/// JSON. `Q8_0 → Q4_K_M` reads better than `"Q8_0" → "Q4_K_M"`.
fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// One difference, as a line a human can act on.
///
/// The facet's name is already on the line above, so this never repeats it. The
/// wording lives here rather than in the analysis layer so the report stays a
/// machine contract and this text stays free to improve.
fn describe(detail: &DetailChange) -> String {
    // The classification is a property of the facet pair, not of a path within
    // it, so it reads best on its own.
    if detail.path == "classification" {
        let value = detail
            .after
            .as_ref()
            .map(value_text)
            .unwrap_or_else(|| "changed".to_string());
        return format!("classification: {value}");
    }

    match (detail.change, detail.before.as_ref(), detail.after.as_ref()) {
        (ChangeKind::Modified, Some(before), Some(after)) => {
            format!(
                "{}: {} → {}",
                detail.path,
                value_text(before),
                value_text(after)
            )
        }
        (ChangeKind::Added, _, Some(after)) => {
            format!("{} added: {}", detail.path, value_text(after))
        }
        (ChangeKind::Removed, Some(before), _) => {
            format!("{} removed: {}", detail.path, value_text(before))
        }
        // No values to show: the difference itself is the message.
        _ => format!("{} {}", detail.path, detail.change.as_str()),
    }
}

/// A facet line: the name, then the fingerprint evidence.
fn facet_line(facet: &FacetChange, width: usize) -> String {
    let evidence = match (&facet.before_digest, &facet.after_digest, facet.change) {
        (Some(_), Some(_), ChangeKind::Unchanged) => "unchanged".to_string(),
        (Some(before), Some(after), _) => {
            format!("{} → {}", abbreviate(before), abbreviate(after))
        }
        (None, Some(after), _) => format!("added  {}", abbreviate(after)),
        (Some(before), None, _) => format!("removed  {}", abbreviate(before)),
        (None, None, change) => change.as_str().to_string(),
    };

    format!("  {:<width$}  {}\n", facet.name, evidence)
}

/// The `diff` report.
///
/// The layout follows the design spec §8.2: which dependencies changed, each
/// facet with the fingerprint that moved — and the facets that were compared and
/// did not — then the overall verdict. Plain text carries every meaning on its
/// own: no colour, and no reliance on alignment to say what moved.
pub fn diff(report: &DiffReport) -> String {
    let mut out = String::from("AgentChecksum diff\n\n");
    out.push_str(&format!(
        "Baseline: {}\n",
        report.baseline_checksum.as_str()
    ));
    out.push_str(&format!(
        "Current:  {}\n\n",
        report.current_checksum.as_str()
    ));

    if !report.changed {
        out.push_str("No dependency changes detected.\n");
        return out;
    }

    let count = report.changes.len();
    let noun = if count == 1 {
        "dependency"
    } else {
        "dependencies"
    };
    out.push_str(&format!("{count} {noun} changed.\n"));

    // Two column widths, computed so the report is read column-wise instead of
    // guessed at line by line.
    let id_width = report
        .changes
        .iter()
        .map(|change| bare_id(&change.id, change.kind).chars().count())
        .max()
        .unwrap_or(0);
    let kind_width = report
        .changes
        .iter()
        .map(|change| kind_label(change.kind).chars().count())
        .max()
        .unwrap_or(0);

    for change in &report.changes {
        out.push_str(&format!(
            "\n{:<kind_width$}  {:<id_width$}  {}\n",
            kind_label(change.kind),
            bare_id(&change.id, change.kind),
            risk_label(change.risk),
        ));

        if change.facets.is_empty() {
            // Added or removed: there is nothing to compare against.
            out.push_str(&format!("  {}\n", change.change.as_str()));
            continue;
        }

        let facet_width = change
            .facets
            .iter()
            .map(|facet| facet.name.chars().count())
            .max()
            .unwrap_or(0);

        for facet in &change.facets {
            out.push_str(&facet_line(facet, facet_width));
            for detail in &facet.details {
                out.push_str(&format!("    {}\n", describe(detail)));
            }
        }
    }

    out.push_str(&format!(
        "\nOverall behavioral risk: {} (heuristic)\n",
        risk_label(report.overall_risk)
    ));

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    // `super::diff` is the renderer under test; the engine's comparison needs a
    // distinct name in this module.
    use crate::diff::diff as compare;
    use crate::manifest::{Dependency, Facet};
    use std::collections::BTreeMap;

    /// `<id>, <kind>, [<facet>, <seed>]` — one entry of a fixture lockfile.
    type Entry<'a> = (&'a str, DependencyKind, Vec<(&'a str, &'a str)>);

    /// Build a lockfile from `<id>, <kind>, [<facet>, <seed>]` triples. The seed
    /// stands for the facet's content, so equal seeds mean an equal fingerprint.
    fn lockfile(entries: Vec<Entry<'_>>) -> Lockfile {
        let dependencies = entries
            .into_iter()
            .map(|(id, kind, facets)| Dependency {
                id: id.to_string(),
                kind,
                facets: facets
                    .into_iter()
                    .map(|(name, seed)| {
                        (
                            name.to_string(),
                            Facet {
                                digest: Digest::sha256(seed.as_bytes()),
                                shape: None,
                                normalized: None,
                            },
                        )
                    })
                    .collect::<BTreeMap<_, _>>(),
                source: None,
            })
            .collect::<Vec<_>>();

        Lockfile::from_dependencies(&dependencies).unwrap()
    }

    /// The facet lines of the first dependency block: two-space indentation, as
    /// opposed to the four-space detail lines beneath them.
    fn facet_lines(text: &str) -> Vec<&str> {
        text.lines()
            .filter(|line| line.starts_with("  ") && !line.starts_with("    "))
            .collect()
    }

    /// The product's sharpest sentence, in the output that carries it: the
    /// description moved, the schema did not.
    #[test]
    fn a_description_change_shows_the_schema_it_did_not_change() {
        fn facets(description: &'static str) -> Vec<(&'static str, &'static str)> {
            vec![
                ("description", description),
                ("input_schema", "schema"),
                ("output_schema", "schema"),
            ]
        }
        let baseline = lockfile(vec![(
            "tool:demo-tools.search_repos",
            DependencyKind::Tool,
            facets("v1"),
        )]);
        let current = lockfile(vec![(
            "tool:demo-tools.search_repos",
            DependencyKind::Tool,
            facets("v2"),
        )]);

        let text = diff(&compare(&baseline, &current));

        assert!(
            text.contains("TOOL  demo-tools.search_repos  MEDIUM"),
            "{text}"
        );

        let lines = facet_lines(&text);
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].starts_with("  description "), "{text}");
        assert!(lines[0].contains(" → "), "{text}");
        assert!(
            lines[1].contains("input_schema") && lines[1].ends_with("unchanged"),
            "{text}"
        );
        assert!(
            lines[2].contains("output_schema") && lines[2].ends_with("unchanged"),
            "{text}"
        );
        assert!(
            text.contains("    classification: text-changed\n"),
            "{text}"
        );
        assert_eq!(
            text.lines().last(),
            Some("Overall behavioral risk: MEDIUM (heuristic)"),
            "{text}"
        );
    }

    #[test]
    fn an_unchanged_comparison_names_no_dependency() {
        let baseline = lockfile(vec![(
            "prompt:a.md",
            DependencyKind::Prompt,
            vec![("content", "same"), ("shape", "same")],
        )]);
        let current = lockfile(vec![(
            "prompt:a.md",
            DependencyKind::Prompt,
            vec![("content", "same"), ("shape", "same")],
        )]);

        let text = diff(&compare(&baseline, &current));
        assert!(text.contains("No dependency changes detected."), "{text}");
        assert!(!text.contains("Overall behavioral risk"), "{text}");
        assert!(facet_lines(&text).is_empty(), "{text}");
    }

    #[test]
    fn an_added_dependency_shows_its_state_instead_of_facet_lines() {
        let baseline = lockfile(vec![(
            "prompt:a.md",
            DependencyKind::Prompt,
            vec![("content", "a")],
        )]);
        let current = lockfile(vec![
            (
                "prompt:a.md",
                DependencyKind::Prompt,
                vec![("content", "a")],
            ),
            (
                "prompt:b.md",
                DependencyKind::Prompt,
                vec![("content", "b")],
            ),
        ]);

        let text = diff(&compare(&baseline, &current));
        assert!(text.contains("1 dependency changed."), "{text}");
        assert!(text.contains("PROMPT  b.md  MEDIUM"), "{text}");
        assert_eq!(facet_lines(&text), vec!["  added"], "{text}");
    }

    #[test]
    fn a_server_change_is_labelled_as_a_server_and_keeps_its_bare_identity() {
        let baseline = lockfile(vec![(
            "mcp:github",
            DependencyKind::McpServer,
            vec![("identity", "one-era")],
        )]);
        let current = lockfile(vec![(
            "mcp:github",
            DependencyKind::McpServer,
            vec![("identity", "another-era")],
        )]);

        let text = diff(&compare(&baseline, &current));
        assert!(text.contains("MCP SERVER  github"), "{text}");
        assert!(
            !text.contains("mcp:github"),
            "the kind column already says this: {text}"
        );
    }

    #[test]
    fn a_digest_is_shown_as_its_algorithm_and_eight_characters() {
        let short = abbreviate(&Digest::sha256(b"v1"));

        assert!(short.starts_with("sha256:"), "{short}");
        assert!(short.ends_with('…'), "{short}");
        assert_eq!(short.chars().count(), "sha256:".len() + 8 + 1, "{short}");
    }
}
