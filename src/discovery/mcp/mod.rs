// SPDX-License-Identifier: MIT OR Apache-2.0

//! MCP dependency discovery: what a configured server declares right now.
//!
//! The shape of the work is deliberate, and it is the same shape the rest of the
//! crate uses:
//!
//! ```text
//! transport + protocol      (client.rs, async, the only I/O)
//!        ↓
//! wire types                (rmcp model)
//!        ↓
//! pure normalization        (normalize.rs)
//!        ↓
//! DiscoveredServer          (this module: plain data)
//!        ↓
//! Dependency                (the frozen fingerprint contract)
//! ```
//!
//! Discovery is read-only introspection. No tool is ever called: a fingerprint of
//! what a server declares is worth having on its own, and executing a tool to
//! obtain one is not something a lockfile step may do.
//!
//! Nothing here decides severity. Discovery reports what the server declared and
//! the risk policy decides what that means, exactly as it does for models and
//! prompts.

pub mod client;
pub mod limits;
pub mod normalize;

use std::collections::BTreeMap;

use serde_json::Value;

use crate::config::Config;
use crate::error::Result;
use crate::fingerprint::{canonical, normalize as text, schema};
use crate::manifest::{Dependency, DependencyKind, Digest, Facet};

/// Which protocol family a session used.
///
/// The two eras are not the same protocol with different version strings: one is a
/// stateless request model and the other negotiates a session. Recording which one
/// we actually got is what makes a server's migration visible as a dependency
/// change instead of an invisible shift in how its declarations are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Era {
    /// The stateless protocol: per-request metadata, `server/discover`, no session.
    Stateless,
    /// A session-based revision, reached through the SDK's compatibility path.
    Legacy,
}

impl Era {
    pub fn as_str(self) -> &'static str {
        match self {
            Era::Stateless => "stateless",
            Era::Legacy => "legacy",
        }
    }
}

/// A server implementation's declared identity.
///
/// Name and version only. The protocol also carries presentation fields — a title,
/// icons, a website — and they are excluded on purpose: they cannot change how a
/// tool behaves, so fingerprinting them would turn a cosmetic edit into a
/// dependency change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// What a server declared about itself.
///
/// Every optional field means "the server did not tell us", and is omitted from the
/// payload rather than filled with an empty default. Missing is not the same
/// statement as empty, and pretending otherwise would make a server that stopped
/// reporting something look identical to one that never reported it.
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub era: Era,
    /// The version this session actually negotiated.
    pub protocol_version: String,
    /// Versions the server said it implements, when it said so.
    pub supported_versions: Option<Vec<String>>,
    /// The implementation behind the server, when it described itself.
    pub server_info: Option<ServerInfo>,
    /// Normalized standard capabilities, when the server declared any.
    pub capabilities: Option<Value>,
}

/// One tool as its server declares it.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    /// Effective annotation tokens, sorted; see `normalize::tool_capabilities`.
    pub capabilities: Vec<String>,
}

/// A complete discovery result for one configured server.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredServer {
    pub alias: String,
    pub identity: Identity,
    /// Sorted by name, so a server's response order cannot influence anything.
    pub tools: Vec<DiscoveredTool>,
    /// Deterministic, non-secret diagnostics gathered while discovering.
    pub warnings: Vec<String>,
}

/// The dependency id of a discovered tool.
///
/// The alias is the namespace, and it cannot contain a dot (config validation
/// enforces that), so the first dot always separates the two parts. That is what
/// keeps `tool:<alias>.<name>` injective while ordinary tool names stay readable.
pub fn tool_id(alias: &str, name: &str) -> String {
    format!("tool:{alias}.{}", encode_tool_name(name))
}

/// The dependency id of a configured server.
pub fn server_id(alias: &str) -> String {
    format!("mcp:{alias}")
}

/// A tool name in the form a dependency id may carry.
///
/// Percent-encoding, with `%` itself escaped, so the mapping is injective: two
/// different names can never produce one id. A name that is already a plain
/// identifier — which the protocol's naming guidance asks for — comes through
/// unchanged.
fn encode_tool_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        match byte {
            b'%' => out.push_str("%25"),
            b'.' | b'-' | b'_' => out.push(byte as char),
            _ if byte.is_ascii_alphanumeric() => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Whether a tool name needed encoding to become a dependency id.
pub(super) fn name_was_encoded(name: &str) -> bool {
    encode_tool_name(name) != name
}

/// The dependencies one discovered server contributes: its own identity, then one
/// per tool.
pub fn dependencies(server: &DiscoveredServer) -> Result<Vec<Dependency>> {
    let mut dependencies = Vec::with_capacity(server.tools.len() + 1);
    dependencies.push(server_dependency(server)?);
    for tool in &server.tools {
        dependencies.push(tool_dependency(&server.alias, tool)?);
    }
    Ok(dependencies)
}

fn server_dependency(server: &DiscoveredServer) -> Result<Dependency> {
    let mut facets = BTreeMap::new();
    facets.insert("identity".to_string(), identity_facet(&server.identity)?);
    Ok(Dependency {
        id: server_id(&server.alias),
        kind: DependencyKind::McpServer,
        facets,
        // Provenance only: the alias is already the identity, and this field is
        // not hashed.
        source: Some(server.alias.clone()),
    })
}

fn tool_dependency(alias: &str, tool: &DiscoveredTool) -> Result<Dependency> {
    let mut facets = BTreeMap::new();

    // A description is model input, so it uses the same contract a prompt does:
    // content keeps the text a model would read, shape collapses whitespace so
    // Phase 2 can tell a reflow from a rewrite.
    if let Some(description) = &tool.description {
        facets.insert(
            "description".to_string(),
            Facet {
                digest: Digest::sha256(text::normalize_text(description).as_bytes()),
                shape: Some(Digest::sha256(text::shape_text(description).as_bytes())),
                normalized: None,
            },
        );
    }

    facets.insert(
        "input_schema".to_string(),
        schema_facet(&tool.input_schema)?,
    );
    if let Some(output_schema) = &tool.output_schema {
        facets.insert("output_schema".to_string(), schema_facet(output_schema)?);
    }
    if !tool.capabilities.is_empty() {
        facets.insert("capabilities".to_string(), set_facet(&tool.capabilities)?);
    }

    Ok(Dependency {
        id: tool_id(alias, &tool.name),
        kind: DependencyKind::Tool,
        facets,
        source: Some(alias.to_string()),
    })
}

/// A schema facet: the normalized schema is recorded, and the digest is taken over
/// exactly it, so the payload can be re-hashed from the lockfile.
fn schema_facet(schema_value: &Value) -> Result<Facet> {
    let payload = schema::normalized_schema(schema_value);
    Ok(Facet {
        digest: Digest::sha256(&canonical::to_vec(&payload)?),
        shape: None,
        normalized: Some(payload),
    })
}

/// A facet whose value is a set: recorded sorted, digested canonically.
fn set_facet(tokens: &[String]) -> Result<Facet> {
    let payload = Value::Array(tokens.iter().cloned().map(Value::String).collect());
    Ok(Facet {
        digest: Digest::sha256(&canonical::to_vec(&payload)?),
        shape: None,
        normalized: Some(payload),
    })
}

fn identity_facet(identity: &Identity) -> Result<Facet> {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "era".to_string(),
        Value::String(identity.era.as_str().to_string()),
    );
    payload.insert(
        "protocol_version".to_string(),
        Value::String(identity.protocol_version.clone()),
    );
    if let Some(versions) = &identity.supported_versions {
        payload.insert(
            "supported_versions".to_string(),
            Value::Array(versions.iter().cloned().map(Value::String).collect()),
        );
    }
    if let Some(info) = &identity.server_info {
        payload.insert(
            "server_info".to_string(),
            serde_json::json!({ "name": info.name, "version": info.version }),
        );
    }
    if let Some(capabilities) = &identity.capabilities {
        payload.insert("capabilities".to_string(), capabilities.clone());
    }

    let payload = Value::Object(payload);
    Ok(Facet {
        digest: Digest::sha256(&canonical::to_vec(&payload)?),
        shape: None,
        normalized: Some(payload),
    })
}

/// Discover every configured MCP server.
///
/// Servers are visited in alias order, so the same configuration produces the same
/// first failure and the same warnings however the TOML arrays happen to be
/// ordered. Sequential on purpose: a configuration holds a handful of servers, and
/// a deterministic order is worth more than a shorter wall clock.
///
/// Fails on the first server that cannot be fully discovered. A partial catalog
/// would be a lockfile describing a server that does not exist.
pub async fn discover(config: &Config) -> Result<(Vec<Dependency>, Vec<String>)> {
    let mut configured: Vec<_> = config.mcp.servers.iter().collect();
    configured.sort_by(|left, right| left.name.cmp(&right.name));

    let mut discovered_dependencies = Vec::new();
    let mut warnings = Vec::new();

    for server in configured {
        let discovered = client::discover(server).await?;
        for warning in &discovered.warnings {
            warnings.push(format!("{warning} (`mcp:{}`)", discovered.alias));
        }
        discovered_dependencies.extend(dependencies(&discovered)?);
    }

    Ok((discovered_dependencies, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> DiscoveredTool {
        DiscoveredTool {
            name: name.to_string(),
            description: Some("Search repositories.".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } }
            }),
            output_schema: None,
            capabilities: vec![],
        }
    }

    fn server(alias: &str, tools: Vec<DiscoveredTool>) -> DiscoveredServer {
        DiscoveredServer {
            alias: alias.to_string(),
            identity: Identity {
                era: Era::Stateless,
                protocol_version: "2026-07-28".to_string(),
                supported_versions: Some(vec!["2025-11-25".to_string(), "2026-07-28".to_string()]),
                server_info: Some(ServerInfo {
                    name: "demo".to_string(),
                    version: "1.0.0".to_string(),
                }),
                capabilities: Some(serde_json::json!({ "tools": { "list_changed": true } })),
            },
            tools,
            warnings: vec![],
        }
    }

    #[test]
    fn a_server_and_its_tools_become_dependencies() {
        let dependencies =
            dependencies(&server("github", vec![tool("search_repositories")])).unwrap();

        assert_eq!(dependencies.len(), 2);
        assert_eq!(dependencies[0].id, "mcp:github");
        assert_eq!(dependencies[0].kind, DependencyKind::McpServer);
        assert_eq!(dependencies[0].source.as_deref(), Some("github"));
        // The kind token and the id prefix agree; Phase 2's MCP analyzer reads the
        // identity payload by these key names.
        let identity = dependencies[0].facets["identity"]
            .normalized
            .clone()
            .unwrap();
        assert_eq!(identity["era"], "stateless");
        assert_eq!(identity["protocol_version"], "2026-07-28");

        assert_eq!(dependencies[1].id, "tool:github.search_repositories");
        assert_eq!(dependencies[1].kind, DependencyKind::Tool);
        assert_eq!(dependencies[1].source.as_deref(), Some("github"));
    }

    #[test]
    fn a_description_records_content_and_shape_without_a_payload() {
        let dependencies = dependencies(&server("s", vec![tool("t")])).unwrap();
        let description = &dependencies[1].facets["description"];

        // No payload: a description's digest covers text the lockfile deliberately
        // does not store, exactly as a prompt's does.
        assert!(description.normalized.is_none());
        assert!(description.shape.is_some());
    }

    #[test]
    fn a_reflowed_description_moves_content_but_not_shape() {
        // This is what lets Phase 2 classify a whitespace-only edit as LOW without
        // asking a model: the two digests answer different questions, so they only
        // agree when the text really did not change.
        let reflowed = |text: &str| {
            let mut tool = tool("t");
            tool.description = Some(text.to_string());
            dependencies(&server("s", vec![tool])).unwrap()[1].facets["description"].clone()
        };

        let original = reflowed("Search repositories.\nUse a plain phrase.");
        let rewrapped = reflowed("Search repositories.\n\n   Use a plain phrase.");
        let rewritten = reflowed("Search repositories.\nUse a search expression.");

        assert_ne!(original.digest, rewrapped.digest, "content is faithful");
        assert_eq!(original.shape, rewrapped.shape, "shape ignores layout");
        assert_ne!(original.shape, rewritten.shape, "and sees a rewrite");
        assert_ne!(original.digest, rewritten.digest);
    }

    #[test]
    fn a_missing_description_produces_no_facet() {
        // Absent is not empty: inventing a description would make a tool that never
        // had one look like a tool whose description was emptied.
        let mut bare = tool("t");
        bare.description = None;
        let dependencies = dependencies(&server("s", vec![bare])).unwrap();

        assert!(!dependencies[1].facets.contains_key("description"));
    }

    #[test]
    fn a_missing_output_schema_produces_no_facet() {
        let dependencies = dependencies(&server("s", vec![tool("t")])).unwrap();
        assert!(!dependencies[1].facets.contains_key("output_schema"));
    }

    #[test]
    fn a_non_object_output_schema_root_is_kept() {
        // Modern MCP permits an output schema that describes an array or a scalar.
        // Forcing `type: object` would misdescribe the server's contract.
        let mut array_output = tool("t");
        array_output.output_schema = Some(serde_json::json!({
            "type": "array",
            "items": { "type": "string" }
        }));
        let dependencies = dependencies(&server("s", vec![array_output])).unwrap();
        let output = dependencies[1].facets["output_schema"]
            .normalized
            .clone()
            .unwrap();

        assert_eq!(output["type"], "array");
    }

    #[test]
    fn schema_facets_record_a_payload_their_digest_can_be_recomputed_from() {
        // The integrity check in `diff` re-hashes every recorded payload, so a
        // schema facet whose digest is taken over anything else would be refused at
        // load time.
        let dependencies = dependencies(&server("s", vec![tool("t")])).unwrap();
        for facet in dependencies[1].facets.values() {
            let Some(payload) = facet.normalized.as_ref() else {
                continue;
            };
            assert_eq!(
                facet.digest,
                Digest::sha256(&canonical::to_vec(payload).unwrap())
            );
        }
    }

    #[test]
    fn tool_ids_are_injective_across_alias_and_name() {
        // The pair (alias, name) is the identity, so two different pairs must never
        // produce one id. Every tool name here is a different name, including the
        // ones that needed encoding, and every alias is one the config grammar
        // admits.
        let names = [
            "search",
            "search.by.owner",
            "search%2Eby",
            "search%",
            "search_repositories",
            "",
            "SEARCH",
        ];

        for alias in ["github", "github-prod", "a_1"] {
            let mut ids: Vec<String> = names.iter().map(|name| tool_id(alias, name)).collect();
            let distinct = ids.len();
            ids.sort();
            ids.dedup();
            assert_eq!(ids.len(), distinct, "alias {alias}: {ids:?}");
        }
    }

    #[test]
    fn the_alias_grammar_is_what_makes_tool_ids_injective() {
        // `tool:<alias>.<name>` separates on the *first* dot, so it is injective only
        // while an alias cannot contain one. Config validation enforces that; this
        // pins why the rule exists, by showing the collision it prevents.
        assert_eq!(
            tool_id("github.search", "by.owner"),
            tool_id("github", "search.by.owner"),
            "which is exactly why an alias may not contain a dot"
        );
        assert!(!crate::config::is_valid_alias("github.search"));
    }

    #[test]
    fn ordinary_tool_names_stay_readable() {
        assert_eq!(
            tool_id("github", "search_repositories"),
            "tool:github.search_repositories"
        );
        assert_eq!(tool_id("a-b", "x-1_2"), "tool:a-b.x-1_2");
    }

    #[test]
    fn an_unusual_tool_name_is_encoded_rather_than_collapsed() {
        // Server-owned names can contain anything; two distinct names must stay two
        // distinct ids, and the encoding has to be reversible by construction.
        assert_eq!(tool_id("s", "a/b"), "tool:s.a%2Fb");
        assert_eq!(tool_id("s", "a b"), "tool:s.a%20b");
        assert_eq!(tool_id("s", "a%b"), "tool:s.a%25b");
        assert_eq!(tool_id("s", "ü"), "tool:s.%C3%BC");
        assert!(name_was_encoded("a/b"));
        assert!(!name_was_encoded("search_repositories"));
    }

    #[test]
    fn capability_tokens_are_recorded_as_a_sorted_set() {
        let mut with_capabilities = tool("t");
        with_capabilities.capabilities = vec![
            "open-world".to_string(),
            "write".to_string(),
            "destructive".to_string(),
        ];
        let dependencies = dependencies(&server("s", vec![with_capabilities])).unwrap();
        let capabilities = dependencies[1].facets["capabilities"]
            .normalized
            .clone()
            .unwrap();

        assert_eq!(
            capabilities,
            serde_json::json!(["open-world", "write", "destructive"])
        );
    }

    #[test]
    fn a_tool_without_annotations_has_no_capabilities_facet() {
        let dependencies = dependencies(&server("s", vec![tool("t")])).unwrap();
        assert!(!dependencies[1].facets.contains_key("capabilities"));
    }

    #[test]
    fn the_source_field_never_reaches_the_checksum() {
        // Provenance is metadata; identity is the id. If source were hashed, moving
        // a server to a differently-named alias with the same contract would look
        // like a change even though the ids already said so.
        let mut first = server("s", vec![tool("t")]);
        let mut second = first.clone();
        second.alias = "other".to_string();
        for dependency in &mut second.tools {
            dependency.name = "t".to_string();
        }

        let left = dependencies(&first).unwrap();
        first.warnings.clear();
        let right = dependencies(&server("other", vec![tool("t")])).unwrap();
        assert_ne!(left[0].id, right[0].id, "the alias is the identity");
        assert_eq!(
            left[0].facets, right[0].facets,
            "and nothing else about them differs"
        );
    }
}
