// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool references, resolved against the committed catalog.
//!
//! A probe may name a tool the way a human reads it (`search_repos`) or the way a
//! dependency id spells it (`tool:github.search_repositories`). Both are accepted,
//! and neither is guessed at: a bare name that two servers expose is an ambiguity the
//! user has to resolve, because the two tools are different tools with different
//! schemas, and picking one silently would evaluate arguments against the wrong
//! contract.

use serde_json::Value;

use crate::error::{Error, Result};
use crate::runner::catalog::{ToolCatalog, ToolContract};
use crate::runner::trace::ToolCall;

/// A tool a probe refers to, as the catalog describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTool {
    /// The canonical dependency id, e.g. `tool:github.search_repositories`.
    pub id: String,
    /// The remote name, which is also the model-facing function name.
    pub name: String,
    /// The declared input contract, for the automatic `argument_validity` metric.
    pub input_schema: Value,
}

impl ResolvedTool {
    /// Whether a recorded call is this tool.
    ///
    /// The canonical id decides. The name is the fallback for a call the runner could
    /// not resolve (`tool_id` is `None` by design when the model invents a name), so a
    /// hand-authored or fixture trace stays evaluable without weakening the rule that
    /// two servers' tools are told apart by id.
    pub fn is_called_by(&self, call: &ToolCall) -> bool {
        match &call.tool_id {
            Some(id) => id == &self.id,
            None => call.name == self.name,
        }
    }
}

impl From<&ToolContract> for ResolvedTool {
    fn from(tool: &ToolContract) -> Self {
        Self {
            id: tool.id.clone(),
            name: tool.name.clone(),
            input_schema: tool.input_schema.clone(),
        }
    }
}

/// Resolve one reference — the canonical id, or a bare remote name.
///
/// Zero matches and several matches are both errors, and the second one lists what it
/// found: "ambiguous" without the candidates leaves the reader to go and guess.
pub fn resolve_tool(catalog: &ToolCatalog, probe: &str, reference: &str) -> Result<ResolvedTool> {
    if let Some(tool) = catalog.by_id(reference) {
        return Ok(ResolvedTool::from(tool));
    }

    // No fuzzy matching, no case folding: a reference either names a tool or fails.
    let matches = catalog.by_name(reference);
    match matches.as_slice() {
        [tool] => Ok(ResolvedTool::from(*tool)),
        [] => Err(Error::ProbeToolUnknown {
            probe: probe.to_string(),
            reference: reference.to_string(),
        }),
        several => Err(Error::ProbeToolAmbiguous {
            probe: probe.to_string(),
            reference: reference.to_string(),
            matches: several.len(),
            candidates: several
                .iter()
                .map(|tool| tool.id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        }),
    }
}

/// Resolve every reference in a list, keeping the declared order.
pub fn resolve_tools(
    catalog: &ToolCatalog,
    probe: &str,
    references: &[String],
) -> Result<Vec<ResolvedTool>> {
    references
        .iter()
        .map(|reference| resolve_tool(catalog, probe, reference))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::lockfile::Lockfile;
    use crate::manifest::{Dependency, DependencyKind, Digest, Facet};

    fn dependency(id: &str) -> Dependency {
        let mut facets = BTreeMap::new();
        facets.insert(
            "input_schema".to_string(),
            Facet {
                digest: Digest::sha256(b"schema"),
                shape: None,
                normalized: Some(serde_json::json!({
                    "type": "object",
                    "properties": { "query": { "type": "string" } }
                })),
            },
        );
        Dependency {
            id: id.to_string(),
            kind: DependencyKind::Tool,
            facets,
            source: None,
        }
    }

    fn catalog() -> ToolCatalog {
        ToolCatalog::from_lockfile(
            &Lockfile::from_dependencies(&[
                dependency("tool:github.search_repositories"),
                dependency("tool:files.read_file"),
                // Two servers exposing the same remote name: fingerprinting is fine
                // with it, a bare reference is not.
                dependency("tool:one.search"),
                dependency("tool:two.search"),
            ])
            .unwrap(),
        )
        .unwrap()
    }

    fn call(tool_id: Option<&str>, name: &str) -> ToolCall {
        ToolCall {
            name: name.to_string(),
            tool_id: tool_id.map(str::to_string),
            arguments: Some(serde_json::json!({ "query": "x" })),
            arguments_parse_error: None,
        }
    }

    #[test]
    fn a_canonical_id_resolves_to_itself() {
        let resolved = resolve_tool(&catalog(), "p", "tool:github.search_repositories").unwrap();

        assert_eq!(resolved.id, "tool:github.search_repositories");
        assert_eq!(resolved.name, "search_repositories");
        assert_eq!(resolved.input_schema["type"], "object");
    }

    #[test]
    fn a_bare_name_resolves_when_exactly_one_tool_carries_it() {
        let resolved = resolve_tool(&catalog(), "p", "search_repositories").unwrap();

        assert_eq!(resolved.id, "tool:github.search_repositories");
        assert_eq!(resolved.name, "search_repositories");
    }

    #[test]
    fn an_unknown_reference_is_an_error_that_names_the_probe_and_the_reference() {
        let error = resolve_tool(&catalog(), "repository-search", "search_repoz").unwrap_err();

        match error {
            Error::ProbeToolUnknown { probe, reference } => {
                assert_eq!(probe, "repository-search");
                assert_eq!(reference, "search_repoz");
            }
            other => panic!("expected ProbeToolUnknown, got {other:?}"),
        }
    }

    #[test]
    fn an_ambiguous_reference_lists_every_candidate() {
        let error = resolve_tool(&catalog(), "p", "search").unwrap_err();

        match error {
            Error::ProbeToolAmbiguous {
                matches,
                candidates,
                ..
            } => {
                assert_eq!(matches, 2);
                assert_eq!(candidates, "tool:one.search, tool:two.search");
            }
            other => panic!("expected ProbeToolAmbiguous, got {other:?}"),
        }
        // The canonical id is never ambiguous, which is the way out.
        assert!(resolve_tool(&catalog(), "p", "tool:two.search").is_ok());
    }

    #[test]
    fn a_list_is_resolved_in_order_or_not_at_all() {
        let resolved = resolve_tools(
            &catalog(),
            "p",
            &["read_file".to_string(), "tool:one.search".to_string()],
        )
        .unwrap();
        assert_eq!(
            resolved
                .iter()
                .map(|tool| tool.id.as_str())
                .collect::<Vec<_>>(),
            vec!["tool:files.read_file", "tool:one.search"]
        );

        // One bad reference in the list fails the whole list: a probe either
        // resolves or is not usable, and half-resolved expectations would silently
        // measure less than the user wrote.
        assert!(
            resolve_tools(
                &catalog(),
                "p",
                &["read_file".to_string(), "nope".to_string()]
            )
            .is_err()
        );
    }

    #[test]
    fn a_call_is_matched_by_id_first_and_by_name_for_an_unresolved_one() {
        let tool = resolve_tool(&catalog(), "p", "search_repositories").unwrap();

        assert!(tool.is_called_by(&call(Some("tool:github.search_repositories"), "anything")));
        assert!(tool.is_called_by(&call(None, "search_repositories")));
        assert!(!tool.is_called_by(&call(None, "search")));
        assert!(!tool.is_called_by(&call(Some("tool:one.search"), "search_repositories")));
    }
}
