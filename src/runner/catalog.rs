// SPDX-License-Identifier: MIT OR Apache-2.0

//! The tool contracts, read from the committed lockfile.
//!
//! Both sides of Phase 4 need the same list and must agree on it exactly: the model
//! has to be shown the catalog that was fingerprinted, and the evaluator has to
//! resolve the names it answers with back to dependency ids and validate the
//! arguments against the same schemas.
//!
//! Taking it from the lockfile rather than from live discovery is deliberate. The
//! lockfile is the committed contract, it is already verified for payload integrity,
//! and it makes `--trace` evaluation work with no network at all.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::fingerprint::canonical;
use crate::lockfile::Lockfile;
use crate::manifest::{DependencyKind, Digest};

/// One tool as the model will see it and as arguments are validated against it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolContract {
    /// The canonical dependency id, e.g. `tool:github.search_repositories`.
    pub id: String,
    /// The remote tool name, which is also the model-facing function name.
    pub name: String,
    pub description: Option<String>,
    /// The declared input contract, as recorded.
    pub input_schema: Value,
}

impl ToolContract {
    /// The fingerprint of the schema arguments are validated against.
    ///
    /// Recorded in a behavioral baseline so a report can say that the yardstick moved
    /// rather than implying the model changed.
    pub fn input_schema_digest(&self) -> Result<Digest> {
        Ok(Digest::sha256(&canonical::to_vec(&self.input_schema)?))
    }

    /// The declaration sent to an OpenAI-compatible endpoint.
    ///
    /// Only what that contract consumes: `name`, `description`, and the input schema.
    /// An output schema, AgentChecksum's capability tokens, and MCP metadata are not
    /// part of it — sending them would change what the model sees, and the catalog
    /// digest is supposed to describe exactly that.
    pub fn wire_tool(&self) -> Value {
        let mut function = serde_json::Map::new();
        function.insert("name".to_string(), Value::String(self.name.clone()));
        if let Some(description) = &self.description {
            function.insert(
                "description".to_string(),
                Value::String(description.clone()),
            );
        }
        function.insert("parameters".to_string(), self.input_schema.clone());

        serde_json::json!({ "type": "function", "function": Value::Object(function) })
    }
}

/// Every discovered tool, indexed both ways.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCatalog {
    tools: Vec<ToolContract>,
}

impl ToolCatalog {
    /// Read the tool contracts out of a lockfile.
    ///
    /// A tool without a recorded input schema cannot be validated or described, so it
    /// is an error rather than a silently partial catalog.
    pub fn from_lockfile(lockfile: &Lockfile) -> Result<Self> {
        let mut tools = Vec::new();

        for (id, dependency) in &lockfile.dependencies {
            if dependency.kind != DependencyKind::Tool {
                continue;
            }

            let Some(schema) = dependency
                .facets
                .get("input_schema")
                .and_then(|facet| facet.normalized.clone())
            else {
                return Err(Error::RunnerUnsupported {
                    what: "describe a tool without a recorded input schema".to_string(),
                    reason: format!("`{id}` has no `input_schema` payload in the lockfile"),
                });
            };

            tools.push(ToolContract {
                id: id.clone(),
                // The id is `tool:<alias>.<encoded name>`; the model-facing name is
                // the remote name the server declared, which is what the wire carries
                // and what the model answers with.
                name: remote_name(id, dependency.source.as_deref()),
                description: dependency
                    .facets
                    .get("description")
                    .and_then(|facet| facet.normalized.clone())
                    .and_then(|value| value.as_str().map(str::to_string)),
                input_schema: schema,
            });
        }

        // BTreeMap iteration already orders by id, so the vector is deterministic.
        Ok(Self { tools })
    }

    pub fn tools(&self) -> &[ToolContract] {
        &self.tools
    }

    pub fn by_id(&self, id: &str) -> Option<&ToolContract> {
        self.tools.iter().find(|tool| tool.id == id)
    }

    /// The tools whose remote name matches, in id order.
    ///
    /// More than one is possible: two servers may declare the same tool name, which
    /// static fingerprinting handles fine but a wire function name cannot.
    pub fn by_name(&self, name: &str) -> Vec<&ToolContract> {
        self.tools.iter().filter(|tool| tool.name == name).collect()
    }

    /// The catalog as the model sees it, sorted by wire name.
    pub fn wire_tools(&self) -> Vec<Value> {
        let mut wire: Vec<Value> = self.tools.iter().map(ToolContract::wire_tool).collect();
        wire.sort_by_key(|tool| {
            tool["function"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        });
        wire
    }

    /// A deterministic digest of exactly what the model was shown.
    ///
    /// Two runs whose catalogs differ here were answered under different choices, so
    /// their traces are not comparable — which is why this travels in the trace and
    /// in the cache key.
    pub fn digest(&self) -> Result<Digest> {
        Ok(Digest::sha256(&canonical::to_vec(&Value::Array(
            self.wire_tools(),
        ))?))
    }

    /// The input-schema digests, keyed by dependency id.
    ///
    /// Stored in a behavioral baseline so a report can annotate "the yardstick moved"
    /// instead of letting a schema edit look like a model regression.
    pub fn input_schema_digests(&self) -> Result<BTreeMap<String, Digest>> {
        let mut digests = BTreeMap::new();
        for tool in &self.tools {
            digests.insert(tool.id.clone(), tool.input_schema_digest()?);
        }
        Ok(digests)
    }
}

/// The remote tool name carried by a dependency id.
///
/// `tool:<alias>.<encoded name>`: the alias is everything up to the first dot, and the
/// rest is the name as the server declared it. An id that does not look like that is
/// returned whole — a tool dependency this code cannot parse is still a tool the user
/// can reference by id.
fn remote_name(id: &str, _source: Option<&str>) -> String {
    let without_kind = id.strip_prefix("tool:").unwrap_or(id);
    match without_kind.split_once('.') {
        Some((_alias, name)) => name.to_string(),
        None => without_kind.to_string(),
    }
}

/// Test fixtures shared by this module's tests and the runner's other modules'.
///
/// The catalog a client is built from and the catalog a test asserts against have to
/// be the same thing, so the fixture lives once rather than being re-spelled in every
/// test module that needs a tool to exist.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::manifest::{Dependency, Facet};
    use serde_json::json;

    /// A tool dependency carrying a recorded input schema, named by its id.
    pub(crate) fn tool_dependency(id: &str, description: Option<&str>) -> Dependency {
        let mut facets = BTreeMap::new();
        facets.insert(
            "input_schema".to_string(),
            Facet {
                digest: Digest::sha256(b"schema"),
                shape: None,
                normalized: Some(json!({
                    "type": "object",
                    "properties": { "query": { "type": "string" } }
                })),
            },
        );
        if let Some(description) = description {
            facets.insert(
                "description".to_string(),
                Facet {
                    digest: Digest::sha256(description.as_bytes()),
                    shape: None,
                    normalized: Some(Value::String(description.to_string())),
                },
            );
        }
        Dependency {
            id: id.to_string(),
            kind: DependencyKind::Tool,
            facets,
            source: None,
        }
    }

    /// A catalog holding exactly the declared tools.
    pub(crate) fn catalog_with(tools: &[(&str, Option<&str>)]) -> ToolCatalog {
        let dependencies: Vec<Dependency> = tools
            .iter()
            .map(|(id, description)| tool_dependency(id, *description))
            .collect();
        let lockfile = Lockfile::from_dependencies(&dependencies).unwrap();
        ToolCatalog::from_lockfile(&lockfile).unwrap()
    }

    /// Two tools, one described and one not — enough to tell the two apart on the
    /// wire.
    pub(crate) fn catalog() -> ToolCatalog {
        catalog_with(&[
            ("tool:github.search_repositories", Some("Search.")),
            ("tool:files.read_file", None),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{catalog, catalog_with, tool_dependency};
    use super::*;
    use crate::manifest::Dependency;

    #[test]
    fn the_remote_name_is_recovered_from_the_dependency_id() {
        assert_eq!(
            remote_name("tool:github.search_repositories", None),
            "search_repositories"
        );
        assert_eq!(remote_name("tool:files.read_file", None), "read_file");
    }

    #[test]
    fn a_wire_tool_carries_only_what_the_contract_consumes() {
        let catalog = catalog();
        let tool = catalog.by_id("tool:github.search_repositories").unwrap();
        let wire = tool.wire_tool();

        assert_eq!(wire["type"], "function");
        assert_eq!(wire["function"]["name"], "search_repositories");
        assert_eq!(wire["function"]["description"], "Search.");
        assert_eq!(wire["function"]["parameters"]["type"], "object");
        // Not sent: an output schema, capability tokens, MCP metadata.
        assert!(wire["function"].get("output_schema").is_none());
        assert!(wire.as_object().unwrap().get("capabilities").is_none());
    }

    #[test]
    fn the_catalog_digest_is_stable_and_reflects_what_the_model_sees() {
        let first = catalog().digest().unwrap();
        assert_eq!(first, catalog().digest().unwrap());

        // A changed description is a different set of choices.
        let changed = Lockfile::from_dependencies(&[tool_dependency(
            "tool:github.search_repositories",
            Some("Search repositories, differently."),
        )])
        .unwrap();
        assert_ne!(
            first,
            ToolCatalog::from_lockfile(&changed)
                .unwrap()
                .digest()
                .unwrap()
        );
    }

    #[test]
    fn a_tool_without_a_recorded_schema_is_refused() {
        let dependency = Dependency {
            id: "tool:s.t".to_string(),
            kind: DependencyKind::Tool,
            facets: BTreeMap::new(),
            source: None,
        };
        let lockfile = Lockfile::from_dependencies(&[dependency]).unwrap();

        let error = ToolCatalog::from_lockfile(&lockfile).unwrap_err();
        assert!(
            matches!(error, Error::RunnerUnsupported { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn two_servers_may_declare_the_same_remote_name() {
        // Static fingerprinting is fine with it; only the runner cannot map the name
        // back, so the catalog reports both rather than picking one.
        let catalog = catalog_with(&[("tool:one.search", None), ("tool:two.search", None)]);

        assert_eq!(catalog.by_name("search").len(), 2);
    }
}
