// SPDX-License-Identifier: MIT OR Apache-2.0

//! The wire boundary: the SDK's model goes in, plain data comes out.
//!
//! Everything in this module is pure, which is the point. The interesting rules —
//! which metadata belongs to the dependency contract and which is excluded, how a
//! declared hint becomes a capability token, what a bound rejects — can then be
//! tested without launching a server, and a transport bug cannot be mistaken for a
//! normalization bug.

use rmcp::model::{
    Implementation, ProtocolVersion, ServerCapabilities, Tool as McpTool, ToolAnnotations,
};
use serde_json::Value;

use super::limits;
use super::{DiscoveredTool, Era, Identity, ServerInfo};

/// Why a declaration could not become a dependency.
///
/// Carries the subject rather than the server alias: the alias is attached where
/// the server is known, which keeps this pure and testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub subject: String,
    pub reason: String,
}

impl Rejected {
    fn new(subject: &str, reason: impl Into<String>) -> Self {
        Self {
            subject: subject.to_string(),
            reason: reason.into(),
        }
    }
}

/// Which protocol family a negotiated version belongs to.
///
/// The boundary is the revision that introduced the stateless request model. A
/// version newer than this one is stateless too, so the comparison is "at least",
/// not "equal to".
pub fn era(version: &ProtocolVersion) -> Era {
    if version.as_str() >= ProtocolVersion::STANDARD_HEADERS.as_str() {
        Era::Stateless
    } else {
        Era::Legacy
    }
}

/// What a server declared about itself, reduced to the behavior-relevant part.
pub fn identity(
    protocol_version: &ProtocolVersion,
    capabilities: &ServerCapabilities,
    server_info: Option<&Implementation>,
    supported_versions: Option<&[ProtocolVersion]>,
) -> std::result::Result<(Identity, Vec<String>), Rejected> {
    let mut warnings = Vec::new();

    let server_info = match server_info {
        Some(info) => {
            check_bytes(
                "server implementation name",
                &info.name,
                limits::MAX_TEXT_BYTES,
            )?;
            check_bytes(
                "server implementation version",
                &info.version,
                limits::MAX_TEXT_BYTES,
            )?;
            // Title, icons, website, and description are presentation. They cannot
            // change how a tool behaves, so a cosmetic edit must not read as a
            // dependency change.
            Some(ServerInfo {
                name: info.name.clone(),
                version: info.version.clone(),
            })
        }
        // A server that does not describe itself is not a server that described
        // itself as empty.
        None => None,
    };

    let supported_versions = supported_versions.map(|versions| {
        let mut reported: Vec<String> = versions
            .iter()
            .map(|version| version.as_str().to_string())
            .collect();
        // A set, not a sequence: order and repetition carry no meaning here.
        reported.sort();
        reported.dedup();
        reported
    });

    let (capabilities, capability_warnings) = capability_payload(capabilities);
    warnings.extend(capability_warnings);

    Ok((
        Identity {
            era: era(protocol_version),
            protocol_version: protocol_version.as_str().to_string(),
            supported_versions,
            server_info,
            capabilities,
        },
        warnings,
    ))
}

/// Standard server capabilities, normalized.
///
/// Members are recorded by name with their declared flags, so an undeclared flag
/// and a declared `false` are the same statement. Extension space is reduced to its
/// identifiers: the payloads are arbitrary server-controlled data, and copying them
/// into a committed lockfile is how a capture of opaque values becomes a dependency
/// fingerprint.
fn capability_payload(capabilities: &ServerCapabilities) -> (Option<Value>, Vec<String>) {
    let mut payload = serde_json::Map::new();
    let mut warnings = Vec::new();

    if let Some(tools) = &capabilities.tools {
        payload.insert(
            "tools".to_string(),
            serde_json::json!({ "list_changed": tools.list_changed.unwrap_or(false) }),
        );
    }
    if let Some(prompts) = &capabilities.prompts {
        payload.insert(
            "prompts".to_string(),
            serde_json::json!({ "list_changed": prompts.list_changed.unwrap_or(false) }),
        );
    }
    if let Some(resources) = &capabilities.resources {
        payload.insert(
            "resources".to_string(),
            serde_json::json!({
                "subscribe": resources.subscribe.unwrap_or(false),
                "list_changed": resources.list_changed.unwrap_or(false),
            }),
        );
    }
    // Presence is the whole statement for these two.
    if capabilities.logging.is_some() {
        payload.insert("logging".to_string(), serde_json::json!({}));
    }
    if capabilities.completions.is_some() {
        payload.insert("completions".to_string(), serde_json::json!({}));
    }

    let mut excluded_extensions = 0;
    if let Some(extensions) = &capabilities.extensions {
        let identifiers = identifiers_of(extensions);
        if !identifiers.is_empty() {
            excluded_extensions += identifiers.len();
            payload.insert(
                "extensions".to_string(),
                Value::Array(identifiers.into_iter().map(Value::String).collect()),
            );
        }
    }
    if let Some(experimental) = &capabilities.experimental {
        let identifiers = identifiers_of(experimental);
        if !identifiers.is_empty() {
            excluded_extensions += identifiers.len();
            payload.insert(
                "experimental".to_string(),
                Value::Array(identifiers.into_iter().map(Value::String).collect()),
            );
        }
    }
    if excluded_extensions > 0 {
        // One warning for the category, however many extensions there are: a server
        // with fifty of them must not produce fifty lines.
        warnings.push(format!(
            "{excluded_extensions} extension or experimental capabilit{} declared; only \
             identifiers are fingerprinted, not their settings",
            if excluded_extensions == 1 {
                "y was"
            } else {
                "ies were"
            }
        ));
    }

    if payload.is_empty() {
        return (None, warnings);
    }
    (Some(Value::Object(payload)), warnings)
}

/// The keys of an extension capability map, sorted.
///
/// Generic over the value type on purpose: extension settings are opaque, and the
/// only thing read from them is that they exist.
fn identifiers_of<V>(map: &std::collections::BTreeMap<String, V>) -> Vec<String> {
    map.keys().cloned().collect()
}

/// One tool, validated against the bounds and reduced to what we fingerprint.
///
/// `title`, `icons`, and `_meta` stay behind: the first two are presentation, and
/// `_meta` is extension space an agent's behavior does not depend on. The
/// invocation contract is the name, the description, the schemas, and the effective
/// behavior hints.
pub fn tool(tool: &McpTool) -> std::result::Result<DiscoveredTool, Rejected> {
    let name = tool.name.as_ref();
    if name.trim().is_empty() {
        return Err(Rejected::new(
            "tool name",
            "the server declared an empty name",
        ));
    }
    check_bytes("tool name", name, limits::MAX_TOOL_NAME_BYTES)?;

    let description = tool.description.as_ref().map(|text| text.as_ref());
    if let Some(description) = description {
        check_bytes("tool description", description, limits::MAX_TEXT_BYTES)?;
    }

    let input_schema = schema_value("input_schema", tool.input_schema.as_ref())?;
    let output_schema = match tool.output_schema.as_ref() {
        Some(schema) => Some(schema_value("output_schema", schema.as_ref())?),
        None => None,
    };

    Ok(DiscoveredTool {
        name: name.to_string(),
        description: description.map(str::to_string),
        input_schema,
        output_schema,
        capabilities: tool_capabilities(tool.annotations.as_ref()),
        // Presence only. The value is opaque, server-controlled, and may be large,
        // short-lived, or sensitive, so it is never read, compared, or stored.
        opaque_metadata: tool.meta.is_some(),
    })
}

/// The server's instructions, validated against the same bound as any other text.
///
/// Returned as text because the caller turns them into a facet: hashes are what
/// reach the lockfile, and the prose does not.
pub fn instructions(instructions: Option<&str>) -> std::result::Result<Option<String>, Rejected> {
    let Some(instructions) = instructions else {
        return Ok(None);
    };
    check_bytes("server instructions", instructions, limits::MAX_TEXT_BYTES)?;
    Ok(Some(instructions.to_string()))
}

/// The effective behavior hints, as a sorted set of tokens.
///
/// The protocol defines these as hints with defaults, and an absent hint means its
/// default: `readOnlyHint` false, `destructiveHint` true, `idempotentHint` false,
/// `openWorldHint` true. Folding the defaults in is what keeps "declared the
/// default" from reading as a change, and `destructive`/`idempotent` are only
/// meaningful for a tool that writes, so a read-only tool does not carry them.
///
/// These are declarations, not guarantees: a server that says `read-only` may still
/// write. AgentChecksum reports that the hint says so, and claims nothing further.
pub fn tool_capabilities(annotations: Option<&ToolAnnotations>) -> Vec<String> {
    let read_only = annotations
        .and_then(|it| it.read_only_hint)
        .unwrap_or(false);
    let destructive = annotations
        .and_then(|it| it.destructive_hint)
        .unwrap_or(true);
    let idempotent = annotations
        .and_then(|it| it.idempotent_hint)
        .unwrap_or(false);
    let open_world = annotations
        .and_then(|it| it.open_world_hint)
        .unwrap_or(true);

    let mut tokens = vec![if read_only { "read-only" } else { "write" }.to_string()];
    if !read_only {
        tokens.push(
            if destructive {
                "destructive"
            } else {
                "non-destructive"
            }
            .to_string(),
        );
        tokens.push(
            if idempotent {
                "idempotent"
            } else {
                "non-idempotent"
            }
            .to_string(),
        );
    }
    tokens.push(
        if open_world {
            "open-world"
        } else {
            "closed-world"
        }
        .to_string(),
    );
    tokens.sort();
    tokens
}

fn schema_value(
    subject: &str,
    value: &serde_json::Map<String, Value>,
) -> std::result::Result<Value, Rejected> {
    // The protocol requires a schema *object* at the root, which the SDK's model
    // already guarantees: a server that sent a bare `true` or a list would not have
    // produced a `Tool` at all. What is left to enforce here are the bounds, and the
    // type is deliberately not rewritten — an output schema may describe an array or
    // a scalar, and forcing `type: object` would misdescribe the contract.
    let value = Value::Object(value.clone());

    let bytes = serde_json::to_vec(&value)
        .map_err(|error| Rejected::new(subject, format!("could not be read: {error}")))?;
    if bytes.len() > limits::MAX_SCHEMA_BYTES {
        return Err(Rejected::new(
            subject,
            format!(
                "{} bytes of schema exceeds the {} byte bound",
                bytes.len(),
                limits::MAX_SCHEMA_BYTES
            ),
        ));
    }

    let depth = depth_of(&value);
    if depth > limits::MAX_SCHEMA_DEPTH {
        return Err(Rejected::new(
            subject,
            format!(
                "schema nests {depth} levels deep, beyond the {} level bound",
                limits::MAX_SCHEMA_DEPTH
            ),
        ));
    }

    Ok(value)
}

/// How deeply a value nests. Bounded by what `serde_json` was willing to parse.
fn depth_of(value: &Value) -> usize {
    match value {
        Value::Object(map) => 1 + map.values().map(depth_of).max().unwrap_or(0),
        Value::Array(items) => 1 + items.iter().map(depth_of).max().unwrap_or(0),
        _ => 0,
    }
}

fn check_bytes(subject: &str, value: &str, limit: usize) -> std::result::Result<(), Rejected> {
    if value.len() > limit {
        return Err(Rejected::new(
            subject,
            format!("{} bytes exceeds the {limit} byte bound", value.len()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tool_with(name: &str, annotations: Option<ToolAnnotations>) -> McpTool {
        let schema: serde_json::Map<String, Value> =
            serde_json::from_value(serde_json::json!({ "type": "object" })).expect("object schema");
        let mut tool = McpTool::new(name.to_string(), "Do a thing.".to_string(), schema);
        tool.annotations = annotations;
        tool
    }

    /// Declared hints, with `None` meaning "not declared" rather than "false".
    fn annotations(
        read_only: Option<bool>,
        destructive: Option<bool>,
        idempotent: Option<bool>,
        open_world: Option<bool>,
    ) -> ToolAnnotations {
        let mut annotations = ToolAnnotations::new();
        annotations.title = Some("Display only".to_string());
        annotations.read_only_hint = read_only;
        annotations.destructive_hint = destructive;
        annotations.idempotent_hint = idempotent;
        annotations.open_world_hint = open_world;
        annotations
    }

    #[test]
    fn the_era_follows_the_negotiated_version() {
        assert_eq!(era(&ProtocolVersion::V_2026_07_28), Era::Stateless);
        assert_eq!(era(&ProtocolVersion::V_2025_11_25), Era::Legacy);
        assert_eq!(era(&ProtocolVersion::V_2024_11_05), Era::Legacy);
    }

    #[test]
    fn effective_hints_fold_in_the_protocol_defaults() {
        // The whole point: "declared the default" and "declared nothing" are the
        // same statement, so neither is drift.
        assert_eq!(
            tool_capabilities(None),
            tool_capabilities(Some(&annotations(None, None, None, None)))
        );
        assert_eq!(
            tool_capabilities(None),
            tool_capabilities(Some(&annotations(
                Some(false),
                Some(true),
                Some(false),
                Some(true)
            )))
        );

        assert_eq!(
            tool_capabilities(None),
            vec!["destructive", "non-idempotent", "open-world", "write"]
        );
    }

    #[test]
    fn a_read_only_tool_does_not_carry_the_write_only_hints() {
        // `destructiveHint` and `idempotentHint` are defined as meaningful only for
        // a tool that writes, so declaring them on a read-only tool must not create a
        // change.
        let read_only = tool_capabilities(Some(&annotations(Some(true), None, None, None)));
        assert_eq!(read_only, vec!["open-world", "read-only"]);
        assert_eq!(
            read_only,
            tool_capabilities(Some(&annotations(Some(true), Some(true), Some(true), None)))
        );
    }

    #[test]
    fn a_semantic_hint_change_is_visible() {
        let safe = tool_capabilities(Some(&annotations(Some(true), None, None, Some(false))));
        assert_eq!(safe, vec!["closed-world", "read-only"]);
        assert_ne!(safe, tool_capabilities(None));

        let non_destructive =
            tool_capabilities(Some(&annotations(Some(false), Some(false), None, None)));
        assert_ne!(non_destructive, tool_capabilities(None));
        assert!(non_destructive.contains(&"non-destructive".to_string()));
    }

    #[test]
    fn a_tool_name_that_is_only_whitespace_is_rejected() {
        // An identity that cannot be told apart from another identity is not an
        // identity.
        let error = tool(&tool_with("   ", None)).unwrap_err();
        assert_eq!(error.subject, "tool name");
    }

    #[test]
    fn an_oversized_name_or_description_is_rejected_rather_than_cut() {
        let long_name = "n".repeat(limits::MAX_TOOL_NAME_BYTES + 1);
        assert_eq!(
            tool(&tool_with(&long_name, None)).unwrap_err().subject,
            "tool name"
        );

        let mut oversized = tool_with("t", None);
        oversized.description = Some("d".repeat(limits::MAX_TEXT_BYTES + 1).into());
        assert_eq!(tool(&oversized).unwrap_err().subject, "tool description");
    }

    #[test]
    fn a_schema_beyond_the_bounds_is_rejected_rather_than_truncated() {
        let mut deep = serde_json::json!({ "type": "string" });
        for _ in 0..(limits::MAX_SCHEMA_DEPTH + 2) {
            deep = serde_json::json!({ "type": "object", "properties": { "next": deep } });
        }
        let mut deep_tool = tool_with("t", None);
        deep_tool.input_schema =
            std::sync::Arc::new(serde_json::from_value(deep).expect("object schema"));
        assert_eq!(tool(&deep_tool).unwrap_err().subject, "input_schema");

        let mut wide = serde_json::json!({ "type": "object" });
        wide["description"] = Value::String("x".repeat(limits::MAX_SCHEMA_BYTES));
        let mut wide_tool = tool_with("t", None);
        wide_tool.input_schema =
            std::sync::Arc::new(serde_json::from_value(wide).expect("object schema"));
        assert_eq!(tool(&wide_tool).unwrap_err().subject, "input_schema");
    }

    #[test]
    fn display_only_metadata_is_not_part_of_the_contract() {
        // A title, an icon, and a `_meta` blob are all server presentation. Two
        // tools that differ only there are the same dependency contract.
        let plain = tool(&tool_with("t", None)).unwrap();

        let mut wire = tool_with("t", None);
        wire.title = Some("A prettier name".to_string());
        wire.icons = Some(vec![]);
        wire.meta = Some(rmcp::model::MetaObject::default());
        let decorated = tool(&wire).unwrap();

        assert_eq!(plain.name, decorated.name);
        assert_eq!(plain.description, decorated.description);
        assert_eq!(plain.input_schema, decorated.input_schema);
        assert_eq!(plain.output_schema, decorated.output_schema);
        assert_eq!(plain.capabilities, decorated.capabilities);

        // Presence alone is tracked, so discovery can say once per server that part
        // of the declaration is deliberately outside the fingerprint.
        assert!(!plain.opaque_metadata);
        assert!(decorated.opaque_metadata);
    }

    #[test]
    fn an_opaque_metadata_value_is_never_kept() {
        // The value is server-controlled: it can be large, short-lived, or
        // sensitive. Only its presence survives, so a declaration cannot smuggle
        // arbitrary data into a lockfile through `_meta`.
        let mut wire = tool_with("t", None);
        let mut meta = rmcp::model::MetaObject::default();
        meta.insert(
            "io.example/secret".to_string(),
            Value::String("SUPER_SECRET_OPAQUE_VALUE".to_string()),
        );
        wire.meta = Some(meta);

        let normalized = tool(&wire).unwrap();
        assert!(normalized.opaque_metadata);
        let debugged = format!("{normalized:?}");
        assert!(
            !debugged.contains("SUPER_SECRET_OPAQUE_VALUE"),
            "{debugged}"
        );
        assert!(!debugged.contains("io.example/secret"), "{debugged}");
    }

    #[test]
    fn server_instructions_are_bounded_and_kept_as_text() {
        assert_eq!(instructions(None).unwrap(), None);
        assert_eq!(
            instructions(Some("Prefer read-only tools.")).unwrap(),
            Some("Prefer read-only tools.".to_string())
        );

        let oversized = "x".repeat(limits::MAX_TEXT_BYTES + 1);
        let rejected = instructions(Some(&oversized)).unwrap_err();
        assert_eq!(rejected.subject, "server instructions");
    }

    #[test]
    fn server_identity_keeps_only_what_behaves() {
        let mut implementation = Implementation::new("demo", "1.2.3");
        implementation.title = Some("Demo Server".to_string());
        implementation.description = Some("A demo".to_string());
        implementation.website_url = Some("https://example.com".to_string());
        let (identity, warnings) = identity(
            &ProtocolVersion::V_2026_07_28,
            &ServerCapabilities::default(),
            Some(&implementation),
            None,
        )
        .unwrap();

        assert!(warnings.is_empty());
        assert_eq!(identity.era, Era::Stateless);
        assert_eq!(identity.protocol_version, "2026-07-28");
        assert_eq!(
            identity.server_info,
            Some(ServerInfo {
                name: "demo".to_string(),
                version: "1.2.3".to_string()
            })
        );
        // Nothing was declared, so nothing is claimed.
        assert!(identity.supported_versions.is_none());
        assert!(identity.capabilities.is_none());
    }

    #[test]
    fn supported_versions_are_a_sorted_set() {
        let (identity, _) = identity(
            &ProtocolVersion::V_2026_07_28,
            &ServerCapabilities::default(),
            None,
            Some(&[
                ProtocolVersion::V_2026_07_28,
                ProtocolVersion::V_2025_11_25,
                ProtocolVersion::V_2026_07_28,
            ]),
        )
        .unwrap();

        assert_eq!(
            identity.supported_versions,
            Some(vec!["2025-11-25".to_string(), "2026-07-28".to_string()])
        );
    }

    #[test]
    fn a_missing_supported_version_list_is_omitted_not_empty() {
        let (identity, _) = identity(
            &ProtocolVersion::V_2025_11_25,
            &ServerCapabilities::default(),
            None,
            None,
        )
        .unwrap();

        assert!(identity.supported_versions.is_none());
        assert_eq!(identity.era, Era::Legacy);
    }
}
