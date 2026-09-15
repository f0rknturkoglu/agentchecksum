// SPDX-License-Identifier: MIT OR Apache-2.0

//! Semantic interpretation of facet differences, per dependency kind.
//!
//! An analyzer explains what it can and returns only the facets it claims. The
//! engine then applies the generic rules to every facet the analyzer did not
//! claim, so a facet nobody understands still cannot pass silently — that split
//! is what keeps the fail-safe invariant true as new kinds and facets appear.
//!
//! Nothing here reads the network, the clock, or the filesystem: a facet's
//! digest is the only input, plus its recorded `normalized` payload when one
//! exists.

use serde_json::Value;

use crate::config::RiskLevel;
use crate::diff::model::{ChangeKind, DetailChange, FacetChange, child_path, max_risk};
use crate::diff::risk::{self, McpFact, ModelFact, PromptFact, SchemaFact, SchemaSide, ToolFact};
use crate::diff::schema;
use crate::lockfile::LockedDependency;
use crate::manifest::{DependencyKind, Facet};

/// Facet changes this crate can explain for a modified dependency.
pub fn for_dependency(baseline: &LockedDependency, current: &LockedDependency) -> Vec<FacetChange> {
    match baseline.kind {
        DependencyKind::Prompt => prompt(baseline, current),
        DependencyKind::Model => model(baseline, current),
        DependencyKind::Tool => tool(baseline, current),
        DependencyKind::McpServer => mcp_server(baseline, current),
    }
}

/// The shared "a text facet changed with the shape unchanged" test.
///
/// Returns `None` when the pair cannot support the claim — a missing shape on
/// either side means formatting-only is unproven, and the caller must fall back
/// to the stronger rule.
fn formatting_only(baseline: Option<&Facet>, current: Option<&Facet>) -> Option<bool> {
    let (baseline_shape, current_shape) = (
        baseline.and_then(|facet| facet.shape.as_ref()),
        current.and_then(|facet| facet.shape.as_ref()),
    );
    match (baseline_shape, current_shape) {
        (Some(before), Some(after)) => Some(before == after),
        _ => None,
    }
}

/// The classification token recorded for text facets. It is a reserved detail
/// path so renderers and consumers can find it without parsing prose.
fn classification(token: &str) -> Vec<DetailChange> {
    vec![DetailChange::one_sided(
        "classification",
        ChangeKind::Modified,
        Value::String(token.to_string()),
    )]
}

/// A text facet that carries its `shape` *inside* itself, as a tool description
/// does. Prompts are different — their shape is a separate facet — and must not
/// use this helper.
fn text_facet(
    name: &str,
    baseline: Option<&Facet>,
    current: Option<&Facet>,
    formatting_only_fact: impl FnOnce() -> RiskLevel,
    changed_fact: impl FnOnce() -> RiskLevel,
) -> Option<FacetChange> {
    let (before, after) = (baseline?, current?);
    if before.digest == after.digest {
        return None;
    }

    Some(match formatting_only(baseline, current) {
        Some(true) => FacetChange::new(name, ChangeKind::Modified, formatting_only_fact())
            .with_details(classification("formatting-only")),
        _ => FacetChange::new(name, ChangeKind::Modified, changed_fact())
            .with_details(classification("text-changed")),
    })
}

// prompt (spec §7.5, §8.3) ------------------------------------------------

fn prompt(baseline: &LockedDependency, current: &LockedDependency) -> Vec<FacetChange> {
    // A prompt records its shape as a *separate* facet (design spec §7.5), unlike
    // a tool description, which carries `shape` inside its own facet. The prompt
    // rules are therefore a relation between two facets, and cannot go through
    // the per-facet helper.
    let (Some(before), Some(after)) = (
        baseline.facets.get("content"),
        current.facets.get("content"),
    ) else {
        return Vec::new();
    };
    if before.digest == after.digest {
        return Vec::new();
    }

    // Only a shape present on both sides can support the formatting-only reading.
    // A missing shape means the weaker claim is unproven, so the text-change rule
    // applies instead.
    let formatting_only = match (baseline.facets.get("shape"), current.facets.get("shape")) {
        (Some(before_shape), Some(after_shape)) => Some(before_shape.digest == after_shape.digest),
        _ => None,
    };

    let change = match formatting_only {
        Some(true) => FacetChange::new(
            "content",
            ChangeKind::Modified,
            risk::prompt(PromptFact::FormattingOnly),
        )
        .with_details(classification("formatting-only")),
        _ => FacetChange::new(
            "content",
            ChangeKind::Modified,
            risk::prompt(PromptFact::TextChanged),
        )
        .with_details(classification("text-changed")),
    };

    vec![change]
}

// model (spec §7.5, §8.3) -------------------------------------------------

fn model(baseline: &LockedDependency, current: &LockedDependency) -> Vec<FacetChange> {
    let mut changes = Vec::new();

    if let Some(change) = model_identity(baseline, current) {
        changes.push(change);
    }
    if let Some(change) = model_params(baseline, current) {
        changes.push(change);
    }
    // The template is digest-only, so "it changed" is the whole message and the
    // report must not pretend to have before/after text.
    if let Some(change) = changed_digest_facet(
        "template",
        baseline,
        current,
        risk::model(ModelFact::TemplateChanged),
    ) {
        changes.push(change);
    }
    if let Some(change) = model_capabilities(baseline, current) {
        changes.push(change);
    }

    changes
}

fn model_identity(baseline: &LockedDependency, current: &LockedDependency) -> Option<FacetChange> {
    let before = baseline.facets.get("identity")?;
    let after = current.facets.get("identity")?;
    if before.digest == after.digest {
        return None;
    }

    let (Some(before_payload), Some(after_payload)) = (
        before.normalized.as_ref().and_then(Value::as_object),
        after.normalized.as_ref().and_then(Value::as_object),
    ) else {
        // Identity changed but we cannot say which part. Identity is the
        // strongest model signal there is, so it keeps the base rule rather than
        // dropping to the generic facet floor.
        return Some(FacetChange::new(
            "identity",
            ChangeKind::Modified,
            risk::model(ModelFact::IdentityOtherChanged),
        ));
    };

    let mut details = Vec::new();
    let mut risks = Vec::new();
    for key in object_keys(before_payload, after_payload) {
        let (Some(before_value), Some(after_value)) =
            (before_payload.get(&key), after_payload.get(&key))
        else {
            // A subfield appearing or disappearing is still a change, and the
            // safe reading of an unrecognised one is the identity floor.
            details.push(DetailChange::new(key, ChangeKind::Modified));
            risks.push(risk::model(ModelFact::IdentityOtherChanged));
            continue;
        };
        if before_value == after_value {
            continue;
        }

        let fact = match key.as_str() {
            "digest" => ModelFact::ContentDigestChanged,
            "provider" => ModelFact::ProviderChanged,
            "family" => ModelFact::FamilyChanged,
            "parameter_size" => ModelFact::ParameterSizeChanged,
            "quantization_level" => ModelFact::QuantizationChanged,
            // The openai-compatible endpoint is identity because no immutable
            // digest exists for that provider.
            "endpoint" => ModelFact::EndpointChanged,
            _ => ModelFact::IdentityOtherChanged,
        };
        risks.push(risk::model(fact));
        details.push(DetailChange::swapped(
            key,
            before_value.clone(),
            after_value.clone(),
        ));
    }

    if details.is_empty() {
        // Digests differ but no named subfield does: the same fallback the
        // opaque case takes, so a difference never disappears.
        return Some(FacetChange::new(
            "identity",
            ChangeKind::Modified,
            risk::model(ModelFact::IdentityOtherChanged),
        ));
    }

    Some(FacetChange::new("identity", ChangeKind::Modified, max_risk(risks)).with_details(details))
}

fn model_params(baseline: &LockedDependency, current: &LockedDependency) -> Option<FacetChange> {
    let before = baseline.facets.get("params")?;
    let after = current.facets.get("params")?;
    if before.digest == after.digest {
        return None;
    }

    let details = match (before.normalized.as_ref(), after.normalized.as_ref()) {
        (Some(before), Some(after)) => json_diff("", before, after),
        _ => Vec::new(),
    };

    Some(
        FacetChange::new(
            "params",
            ChangeKind::Modified,
            risk::model(ModelFact::ParamsChanged),
        )
        .with_details(details),
    )
}

fn model_capabilities(
    baseline: &LockedDependency,
    current: &LockedDependency,
) -> Option<FacetChange> {
    let before = baseline.facets.get("capabilities")?;
    let after = current.facets.get("capabilities")?;
    if before.digest == after.digest {
        return None;
    }

    let (Some(before_names), Some(after_names)) = (
        before.normalized.as_ref().and_then(Value::as_array),
        after.normalized.as_ref().and_then(Value::as_array),
    ) else {
        return Some(FacetChange::new(
            "capabilities",
            ChangeKind::Modified,
            risk::model(ModelFact::CapabilitiesChanged),
        ));
    };

    let names = |values: &[Value]| -> Vec<String> {
        values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    };
    let before_names = names(before_names);
    let after_names = names(after_names);

    let mut details = Vec::new();
    let mut risks = Vec::new();

    for name in &before_names {
        if after_names.contains(name) {
            continue;
        }
        let fact = if name == "tools" {
            ModelFact::ToolsCapabilityRemoved
        } else {
            ModelFact::CapabilityRemoved
        };
        risks.push(risk::model(fact));
        details.push(DetailChange::new(name.clone(), ChangeKind::Removed));
    }
    for name in &after_names {
        if before_names.contains(name) {
            continue;
        }
        let fact = if name == "tools" {
            ModelFact::ToolsCapabilityAdded
        } else {
            ModelFact::CapabilityAdded
        };
        risks.push(risk::model(fact));
        details.push(DetailChange::new(name.clone(), ChangeKind::Added));
    }

    if details.is_empty() {
        return Some(FacetChange::new(
            "capabilities",
            ChangeKind::Modified,
            risk::model(ModelFact::CapabilitiesChanged),
        ));
    }

    Some(
        FacetChange::new("capabilities", ChangeKind::Modified, max_risk(risks))
            .with_details(details),
    )
}

// tool (spec §7.5, §8.3) --------------------------------------------------

fn tool(baseline: &LockedDependency, current: &LockedDependency) -> Vec<FacetChange> {
    let mut changes = Vec::new();

    // A description is model input, so it follows the same content/shape
    // reasoning as a prompt. Here the shape sits inside the description facet.
    if let Some(change) = text_facet(
        "description",
        baseline.facets.get("description"),
        current.facets.get("description"),
        || risk::tool(ToolFact::DescriptionFormattingOnly),
        || risk::tool(ToolFact::DescriptionChanged),
    ) {
        changes.push(change);
    }

    for (name, side) in [
        ("input_schema", SchemaSide::Input),
        ("output_schema", SchemaSide::Output),
    ] {
        if let Some(change) = schema_facet(name, side, baseline, current) {
            changes.push(change);
        }
    }

    if let Some(change) = set_facet(
        "capabilities",
        baseline,
        current,
        |_| risk::tool(ToolFact::CapabilityRemoved),
        |_| risk::tool(ToolFact::CapabilityAdded),
        || risk::unknown_facet(baseline.kind, "capabilities"),
    ) {
        changes.push(change);
    }

    changes
}

fn schema_facet(
    name: &str,
    side: SchemaSide,
    baseline: &LockedDependency,
    current: &LockedDependency,
) -> Option<FacetChange> {
    let before = baseline.facets.get(name)?;
    let after = current.facets.get(name)?;
    if before.digest == after.digest {
        return None;
    }

    let comparison = match (before.normalized.as_ref(), after.normalized.as_ref()) {
        (Some(before_schema), Some(after_schema)) => schema::compare(before_schema, after_schema),
        // Without a payload there is nothing to interpret: the fingerprints moved
        // and this facet has nothing to explain them with.
        _ => {
            return Some(FacetChange::new(
                name,
                ChangeKind::Modified,
                risk::schema(side, SchemaFact::Generic),
            ));
        }
    };

    let schema::SchemaComparison::Changed {
        differences,
        has_unclassified_change,
    } = comparison
    else {
        // Equivalence is a conclusion, not a gap. The two payloads say the same
        // thing in the model this analyzer implements — `additionalProperties`
        // absent against `{}`, say — so the fingerprint moved in bytes only.
        // Applying the generic floor here would be a false alarm about behavior;
        // reporting nothing at all would hide that the facet was examined. The
        // fresh digests come along as the evidence that the bytes did move.
        return Some(FacetChange::equivalent(
            name,
            before.digest.clone(),
            after.digest.clone(),
        ));
    };

    let mut risks = Vec::new();
    let mut details = Vec::new();
    for difference in differences {
        risks.push(risk::schema(side, difference.fact));
        details.push(difference.detail);
    }

    // The generic floor applies whenever any part of the difference is unclassified
    // — and it composes with the named facts rather than replacing them. A change
    // that is part understood and part not is not a classified change.
    if has_unclassified_change {
        risks.push(risk::schema(side, SchemaFact::Generic));
    }

    Some(FacetChange::new(name, ChangeKind::Modified, max_risk(risks)).with_details(details))
}

// mcp (spec §9, §8.3) -----------------------------------------------------

fn mcp_server(baseline: &LockedDependency, current: &LockedDependency) -> Vec<FacetChange> {
    let mut changes = Vec::new();

    let Some(before) = baseline.facets.get("identity") else {
        return changes;
    };
    let Some(after) = current.facets.get("identity") else {
        return changes;
    };
    if before.digest == after.digest {
        return changes;
    }

    let (Some(before_payload), Some(after_payload)) = (
        before.normalized.as_ref().and_then(Value::as_object),
        after.normalized.as_ref().and_then(Value::as_object),
    ) else {
        changes.push(FacetChange::new(
            "identity",
            ChangeKind::Modified,
            risk::mcp(McpFact::IdentityOtherChanged),
        ));
        return changes;
    };

    let mut details = Vec::new();
    let mut risks = Vec::new();
    for key in object_keys(before_payload, after_payload) {
        let (Some(before_value), Some(after_value)) =
            (before_payload.get(&key), after_payload.get(&key))
        else {
            details.push(DetailChange::new(key, ChangeKind::Modified));
            risks.push(risk::mcp(McpFact::IdentityOtherChanged));
            continue;
        };
        if before_value == after_value {
            continue;
        }

        let fact = match key.as_str() {
            // The negotiated protocol decides how tool declarations are read.
            "era" | "protocol_version" | "supported_versions" => McpFact::ProtocolChanged,
            "server_info" => McpFact::ServerInfoChanged,
            _ => McpFact::IdentityOtherChanged,
        };
        risks.push(risk::mcp(fact));
        details.push(DetailChange::swapped(
            key,
            before_value.clone(),
            after_value.clone(),
        ));
    }

    if details.is_empty() {
        changes.push(FacetChange::new(
            "identity",
            ChangeKind::Modified,
            risk::mcp(McpFact::IdentityOtherChanged),
        ));
        return changes;
    }

    changes.push(
        FacetChange::new("identity", ChangeKind::Modified, max_risk(risks)).with_details(details),
    );
    changes
}

// ------------------------------------------------------------------ helpers

/// A facet whose only signal is its digest.
fn changed_digest_facet(
    name: &str,
    baseline: &LockedDependency,
    current: &LockedDependency,
    risk: RiskLevel,
) -> Option<FacetChange> {
    let before = baseline.facets.get(name)?;
    let after = current.facets.get(name)?;
    if before.digest == after.digest {
        return None;
    }
    Some(FacetChange::new(name, ChangeKind::Modified, risk))
}

/// A facet holding a set of names, where membership is the semantics.
fn set_facet(
    name: &str,
    baseline: &LockedDependency,
    current: &LockedDependency,
    removed: impl Fn(&str) -> RiskLevel,
    added: impl Fn(&str) -> RiskLevel,
    opaque: impl Fn() -> RiskLevel,
) -> Option<FacetChange> {
    let before = baseline.facets.get(name)?;
    let after = current.facets.get(name)?;
    if before.digest == after.digest {
        return None;
    }

    let (Some(before_values), Some(after_values)) = (
        before.normalized.as_ref().and_then(Value::as_array),
        after.normalized.as_ref().and_then(Value::as_array),
    ) else {
        return Some(FacetChange::new(name, ChangeKind::Modified, opaque()));
    };

    let names = |values: &[Value]| -> Vec<String> {
        values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    };
    let before_names = names(before_values);
    let after_names = names(after_values);

    let mut details = Vec::new();
    let mut risks = Vec::new();
    for value in &before_names {
        if !after_names.contains(value) {
            details.push(DetailChange::new(value.clone(), ChangeKind::Removed));
            risks.push(removed(value));
        }
    }
    for value in &after_names {
        if !before_names.contains(value) {
            details.push(DetailChange::new(value.clone(), ChangeKind::Added));
            risks.push(added(value));
        }
    }

    if details.is_empty() {
        return Some(FacetChange::new(name, ChangeKind::Modified, opaque()));
    }

    Some(FacetChange::new(name, ChangeKind::Modified, max_risk(risks)).with_details(details))
}

/// Union of two objects' keys, sorted. `serde_json::Map` is a `BTreeMap`, so
/// collecting and sorting keeps the output order independent of input order.
fn object_keys(
    before: &serde_json::Map<String, Value>,
    after: &serde_json::Map<String, Value>,
) -> Vec<String> {
    let mut keys: Vec<String> = before.keys().chain(after.keys()).cloned().collect();
    keys.sort();
    keys.dedup();
    keys
}

/// Recursive difference of two recorded payloads.
///
/// Objects are walked key by key so the report can name `configured.temperature`
/// rather than dumping a whole blob; everything else is reported as a single
/// swap. A general JSON Patch implementation is not needed for either job.
fn json_diff(path: &str, before: &Value, after: &Value) -> Vec<DetailChange> {
    let mut details = Vec::new();
    collect_json_diff(path, before, after, &mut details);
    details
}

fn collect_json_diff(path: &str, before: &Value, after: &Value, out: &mut Vec<DetailChange>) {
    match (before, after) {
        (Value::Object(before), Value::Object(after)) => {
            for key in object_keys(before, after) {
                let child = child_path(path, &key);
                match (before.get(&key), after.get(&key)) {
                    (Some(before_value), Some(after_value)) => {
                        collect_json_diff(&child, before_value, after_value, out);
                    }
                    (Some(before_value), None) => out.push(DetailChange::one_sided(
                        child,
                        ChangeKind::Removed,
                        before_value.clone(),
                    )),
                    (None, Some(after_value)) => out.push(DetailChange::one_sided(
                        child,
                        ChangeKind::Added,
                        after_value.clone(),
                    )),
                    (None, None) => {}
                }
            }
        }
        _ if before == after => {}
        _ => {
            let path = if path.is_empty() { "value" } else { path };
            out.push(DetailChange::swapped(path, before.clone(), after.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::model::max_risk;
    use crate::manifest::{DependencyKind, Digest};

    fn facet(digest: &str, shape: Option<&str>, normalized: Option<Value>) -> Facet {
        Facet {
            digest: Digest::sha256(digest.as_bytes()),
            shape: shape.map(|shape| Digest::sha256(shape.as_bytes())),
            normalized,
        }
    }

    fn dependency(kind: DependencyKind, facets: Vec<(&str, Facet)>) -> LockedDependency {
        LockedDependency {
            kind,
            facets: facets
                .into_iter()
                .map(|(name, facet)| (name.to_string(), facet))
                .collect(),
            source: None,
        }
    }

    fn prompt_dependency(content: &str, shape: &str) -> LockedDependency {
        dependency(
            DependencyKind::Prompt,
            vec![
                ("content", facet(content, None, None)),
                ("shape", facet(shape, None, None)),
            ],
        )
    }

    fn model_dependency(identity: Value, extra: Vec<(&str, Facet)>) -> LockedDependency {
        let mut facets = vec![(
            "identity",
            facet(&identity.to_string(), None, Some(identity.clone())),
        )];
        facets.extend(extra);
        dependency(DependencyKind::Model, facets)
    }

    #[test]
    fn prompt_formatting_only_is_low_and_classified() {
        // Same shape digest, different content digest.
        let baseline = prompt_dependency("Be concise.\n\nUse tools.", "Be concise. Use tools.");
        let current = prompt_dependency("Be concise.\nUse tools.", "Be concise. Use tools.");

        let changes = prompt(&baseline, &current);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].name, "content");
        assert_eq!(changes[0].risk, RiskLevel::Low);
        assert_eq!(
            changes[0].details[0].after,
            Some(Value::String("formatting-only".to_string()))
        );
    }

    #[test]
    fn prompt_text_change_is_medium() {
        let baseline = prompt_dependency("Never modify files.", "Never modify files.");
        let current = prompt_dependency("You may modify files.", "You may modify files.");

        let changes = prompt(&baseline, &current);
        assert_eq!(changes[0].risk, RiskLevel::Medium);
        assert_eq!(
            changes[0].details[0].after,
            Some(Value::String("text-changed".to_string()))
        );
    }

    #[test]
    fn a_missing_shape_facet_cannot_claim_formatting_only() {
        // Without a shape to compare, the safe reading is a text change.
        let baseline = dependency(
            DependencyKind::Prompt,
            vec![("content", facet("a", None, None))],
        );
        let current = dependency(
            DependencyKind::Prompt,
            vec![("content", facet("b", None, None))],
        );

        let changes = prompt(&baseline, &current);
        assert_eq!(changes[0].risk, RiskLevel::Medium);
    }

    #[test]
    fn identical_prompts_produce_no_changes() {
        let baseline = prompt_dependency("same", "same");
        assert!(prompt(&baseline, &baseline).is_empty());
    }

    #[test]
    fn model_digest_change_is_critical_and_quantization_is_high() {
        let baseline = model_dependency(
            serde_json::json!({ "provider": "ollama", "id": "m", "digest": "sha256:aaa",
                                "quantization_level": "Q8_0" }),
            vec![],
        );
        let digest_changed = model_dependency(
            serde_json::json!({ "provider": "ollama", "id": "m", "digest": "sha256:bbb",
                                "quantization_level": "Q8_0" }),
            vec![],
        );
        let quantized = model_dependency(
            serde_json::json!({ "provider": "ollama", "id": "m", "digest": "sha256:aaa",
                                "quantization_level": "Q4_K_M" }),
            vec![],
        );

        let digest = model(&baseline, &digest_changed);
        assert_eq!(digest[0].risk, RiskLevel::Critical);
        assert_eq!(digest[0].details[0].path, "digest");

        let quantization = model(&baseline, &quantized);
        assert_eq!(quantization[0].risk, RiskLevel::High);
        assert_eq!(quantization[0].details[0].path, "quantization_level");
        assert_eq!(
            quantization[0].details[0].before,
            Some(Value::String("Q8_0".to_string()))
        );
        assert_eq!(
            quantization[0].details[0].after,
            Some(Value::String("Q4_K_M".to_string()))
        );
    }

    #[test]
    fn model_family_and_parameter_size_are_critical_but_an_unknown_field_is_high() {
        let baseline = model_dependency(
            serde_json::json!({ "family": "qwen3", "parameter_size": "8.0B", "mystery": 1 }),
            vec![],
        );
        let family = model_dependency(
            serde_json::json!({ "family": "llama", "parameter_size": "8.0B", "mystery": 1 }),
            vec![],
        );
        let size = model_dependency(
            serde_json::json!({ "family": "qwen3", "parameter_size": "3.0B", "mystery": 1 }),
            vec![],
        );
        let mystery = model_dependency(
            serde_json::json!({ "family": "qwen3", "parameter_size": "8.0B", "mystery": 2 }),
            vec![],
        );

        assert_eq!(model(&baseline, &family)[0].risk, RiskLevel::Critical);
        assert_eq!(model(&baseline, &size)[0].risk, RiskLevel::Critical);
        assert_eq!(model(&baseline, &mystery)[0].risk, RiskLevel::High);
    }

    #[test]
    fn an_endpoint_change_is_high_and_does_not_claim_the_weights_changed() {
        let baseline = model_dependency(
            serde_json::json!({ "provider": "openai-compatible", "id": "gpt",
                                "endpoint": "https://a.example/v1" }),
            vec![],
        );
        let current = model_dependency(
            serde_json::json!({ "provider": "openai-compatible", "id": "gpt",
                                "endpoint": "https://b.example/v1" }),
            vec![],
        );

        let changes = model(&baseline, &current);
        assert_eq!(changes[0].risk, RiskLevel::High);
        assert_eq!(changes[0].details[0].path, "endpoint");
    }

    #[test]
    fn opaque_identity_change_is_high_rather_than_nothing() {
        // No normalized payload: the digests differ and that must still be said.
        let baseline = dependency(
            DependencyKind::Model,
            vec![("identity", facet("aaa", None, None))],
        );
        let current = dependency(
            DependencyKind::Model,
            vec![("identity", facet("bbb", None, None))],
        );

        let changes = model(&baseline, &current);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].risk, RiskLevel::High);
    }

    #[test]
    fn params_changes_are_medium_with_key_level_detail() {
        let baseline = model_dependency(
            serde_json::json!({ "id": "m" }),
            vec![(
                "params",
                facet(
                    "p1",
                    None,
                    Some(serde_json::json!({ "configured": { "temperature": 0.0 } })),
                ),
            )],
        );
        let current = model_dependency(
            serde_json::json!({ "id": "m" }),
            vec![(
                "params",
                facet(
                    "p2",
                    None,
                    Some(serde_json::json!({ "configured": { "temperature": 0.7 } })),
                ),
            )],
        );

        let changes = model(&baseline, &current);
        assert_eq!(changes[0].name, "params");
        assert_eq!(changes[0].risk, RiskLevel::Medium);
        assert_eq!(changes[0].details[0].path, "configured.temperature");
        assert_eq!(changes[0].details[0].before, Some(serde_json::json!(0.0)));
        assert_eq!(changes[0].details[0].after, Some(serde_json::json!(0.7)));
    }

    #[test]
    fn template_change_is_high_without_fabricating_text() {
        let baseline = model_dependency(
            serde_json::json!({ "id": "m" }),
            vec![("template", facet("t1", None, None))],
        );
        let current = model_dependency(
            serde_json::json!({ "id": "m" }),
            vec![("template", facet("t2", None, None))],
        );

        let changes = model(&baseline, &current);
        assert_eq!(changes[0].name, "template");
        assert_eq!(changes[0].risk, RiskLevel::High);
        assert!(
            changes[0].details.is_empty(),
            "the lockfile has no template text to show"
        );
    }

    #[test]
    fn losing_the_tools_capability_is_critical() {
        let baseline = model_dependency(
            serde_json::json!({ "id": "m" }),
            vec![(
                "capabilities",
                facet("c1", None, Some(serde_json::json!(["completion", "tools"]))),
            )],
        );
        let current = model_dependency(
            serde_json::json!({ "id": "m" }),
            vec![(
                "capabilities",
                facet("c2", None, Some(serde_json::json!(["completion"]))),
            )],
        );

        let changes = model(&baseline, &current);
        assert_eq!(changes[0].risk, RiskLevel::Critical);
        assert_eq!(changes[0].details[0].path, "tools");
        assert_eq!(changes[0].details[0].change, ChangeKind::Removed);
    }

    #[test]
    fn gaining_the_tools_capability_is_high() {
        let baseline = model_dependency(
            serde_json::json!({ "id": "m" }),
            vec![(
                "capabilities",
                facet("c1", None, Some(serde_json::json!(["completion"]))),
            )],
        );
        let current = model_dependency(
            serde_json::json!({ "id": "m" }),
            vec![(
                "capabilities",
                facet("c2", None, Some(serde_json::json!(["completion", "tools"]))),
            )],
        );

        let changes = model(&baseline, &current);
        assert_eq!(changes[0].risk, RiskLevel::High);
    }

    #[test]
    fn other_capability_changes_follow_the_weaker_scale() {
        let baseline = model_dependency(
            serde_json::json!({ "id": "m" }),
            vec![(
                "capabilities",
                facet(
                    "c1",
                    None,
                    Some(serde_json::json!(["completion", "vision"])),
                ),
            )],
        );
        let removed = model_dependency(
            serde_json::json!({ "id": "m" }),
            vec![(
                "capabilities",
                facet("c2", None, Some(serde_json::json!(["completion"]))),
            )],
        );
        let added = model_dependency(
            serde_json::json!({ "id": "m" }),
            vec![(
                "capabilities",
                facet(
                    "c3",
                    None,
                    Some(serde_json::json!(["completion", "vision", "audio"])),
                ),
            )],
        );

        assert_eq!(model(&baseline, &removed)[0].risk, RiskLevel::High);
        assert_eq!(model(&baseline, &added)[0].risk, RiskLevel::Medium);
    }

    // --------------------------------------------------------------- tool

    fn tool_with(facets: Vec<(&str, Facet)>) -> LockedDependency {
        dependency(DependencyKind::Tool, facets)
    }

    fn schema_facet_value(schema: Value) -> Facet {
        facet(&schema.to_string(), None, Some(schema))
    }

    #[test]
    fn tool_description_change_is_medium_and_formatting_is_low() {
        let baseline = tool_with(vec![("description", facet("d1", Some("shape"), None))]);
        let reflowed = tool_with(vec![("description", facet("d2", Some("shape"), None))]);
        let rewritten = tool_with(vec![("description", facet("d3", Some("other"), None))]);

        assert_eq!(tool(&baseline, &reflowed)[0].risk, RiskLevel::Low);
        assert_eq!(tool(&baseline, &rewritten)[0].risk, RiskLevel::Medium);
    }

    #[test]
    fn a_required_input_property_addition_is_critical() {
        let baseline = tool_with(vec![(
            "input_schema",
            schema_facet_value(serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            })),
        )]);
        let current = tool_with(vec![(
            "input_schema",
            schema_facet_value(serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" }, "owner": { "type": "string" } },
                "required": ["query", "owner"]
            })),
        )]);

        let changes = tool(&baseline, &current);
        assert_eq!(changes[0].name, "input_schema");
        assert_eq!(changes[0].risk, RiskLevel::Critical);
        assert_eq!(changes[0].details[0].path, "required");
        assert_eq!(
            changes[0].details[0].after,
            Some(serde_json::json!("owner"))
        );
    }

    #[test]
    fn an_uninterpreted_schema_change_falls_back_to_the_generic_risk() {
        // `examples` is behaviour-relevant but is not one of the interpretations
        // the analyzer names, so this exercises the fallback rather than a gap.
        let baseline = tool_with(vec![(
            "input_schema",
            schema_facet_value(serde_json::json!({ "type": "object", "properties": {} })),
        )]);
        let current = tool_with(vec![(
            "input_schema",
            schema_facet_value(serde_json::json!({
                "type": "object", "properties": {},
                "examples": [{ "query": "postgres vector search" }]
            })),
        )]);

        let changes = tool(&baseline, &current);
        assert_eq!(changes[0].risk, RiskLevel::High);
        assert!(
            changes[0].details.is_empty(),
            "nothing to name, but the change is still reported"
        );
    }

    #[test]
    fn a_schema_facet_without_a_payload_is_generic() {
        let baseline = tool_with(vec![("input_schema", facet("s1", None, None))]);
        let current = tool_with(vec![("input_schema", facet("s2", None, None))]);

        assert_eq!(tool(&baseline, &current)[0].risk, RiskLevel::High);
    }

    #[test]
    fn output_schema_uses_the_output_side_of_the_table() {
        let baseline = tool_with(vec![(
            "output_schema",
            schema_facet_value(serde_json::json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            })),
        )]);
        let required_dropped = tool_with(vec![(
            "output_schema",
            schema_facet_value(serde_json::json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": []
            })),
        )]);

        // Critical for output, medium for input — the sides differ on purpose.
        assert_eq!(
            tool(&baseline, &required_dropped)[0].risk,
            RiskLevel::Critical
        );
    }

    #[test]
    fn tool_capability_changes_are_reported() {
        let baseline = tool_with(vec![(
            "capabilities",
            facet("cap1", None, Some(serde_json::json!(["read-only"]))),
        )]);
        let current = tool_with(vec![(
            "capabilities",
            facet("cap2", None, Some(serde_json::json!(["destructive"]))),
        )]);

        let changes = tool(&baseline, &current);
        assert_eq!(changes[0].risk, RiskLevel::High);
        assert_eq!(changes[0].details.len(), 2, "{:?}", changes[0].details);
    }

    // ---------------------------------------------------------------- mcp

    #[test]
    fn mcp_protocol_change_is_high_and_server_info_is_medium() {
        let baseline = dependency(
            DependencyKind::McpServer,
            vec![(
                "identity",
                facet(
                    "i1",
                    None,
                    Some(serde_json::json!({
                        "era": "modern",
                        "protocol_version": "2026-07-28",
                        "server_info": { "name": "s", "version": "1.0.0" }
                    })),
                ),
            )],
        );
        let protocol = dependency(
            DependencyKind::McpServer,
            vec![(
                "identity",
                facet(
                    "i2",
                    None,
                    Some(serde_json::json!({
                        "era": "legacy",
                        "protocol_version": "2025-11-25",
                        "server_info": { "name": "s", "version": "1.0.0" }
                    })),
                ),
            )],
        );
        let server_info = dependency(
            DependencyKind::McpServer,
            vec![(
                "identity",
                facet(
                    "i3",
                    None,
                    Some(serde_json::json!({
                        "era": "modern",
                        "protocol_version": "2026-07-28",
                        "server_info": { "name": "s", "version": "2.0.0" }
                    })),
                ),
            )],
        );

        assert_eq!(mcp_server(&baseline, &protocol)[0].risk, RiskLevel::High);
        assert_eq!(
            mcp_server(&baseline, &server_info)[0].risk,
            RiskLevel::Medium
        );
    }

    #[test]
    fn mcp_identity_without_a_payload_is_high() {
        let baseline = dependency(
            DependencyKind::McpServer,
            vec![("identity", facet("a", None, None))],
        );
        let current = dependency(
            DependencyKind::McpServer,
            vec![("identity", facet("b", None, None))],
        );

        let changes = mcp_server(&baseline, &current);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].risk, RiskLevel::High);
    }

    #[test]
    fn nested_payload_differences_are_named_by_path() {
        let details = json_diff(
            "",
            &serde_json::json!({ "configured": { "temperature": 0.0, "seed": 1 } }),
            &serde_json::json!({ "configured": { "temperature": 0.7, "seed": 1 } }),
        );
        assert_eq!(details.len(), 1, "{details:?}");
        assert_eq!(details[0].path, "configured.temperature");
    }

    #[test]
    fn max_risk_helper_is_used_by_analyzers() {
        assert_eq!(max_risk([RiskLevel::Low, RiskLevel::High]), RiskLevel::High);
    }

    // ------------------------------------------- schema fail-safe composition

    /// A schema nested inside `depth` object properties.
    fn nested(depth: usize, leaf: Value) -> Value {
        let mut schema = leaf;
        for _ in 0..depth {
            schema = serde_json::json!({
                "type": "object", "properties": { "next": schema }
            });
        }
        schema
    }

    fn tool_with_schema(name: &'static str, schema: Value) -> LockedDependency {
        tool_with(vec![(name, schema_facet_value(schema))])
    }

    #[test]
    fn a_named_change_beside_an_unnamed_one_keeps_the_generic_floor() {
        // A property description is MEDIUM; `examples` is unclassified and forces
        // HIGH. The bug this guards against reported MEDIUM, because only the part
        // the analyzer could name ever reached the risk calculation.
        let baseline = tool_with_schema(
            "input_schema",
            serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string", "description": "A plain phrase." } }
            }),
        );
        let current = tool_with_schema(
            "input_schema",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "A search expression." }
                },
                "examples": [{ "query": "postgres vector search" }]
            }),
        );

        let changes = tool(&baseline, &current);
        assert_eq!(changes[0].risk, RiskLevel::High);
        // The floor adds to the report rather than replacing what was named.
        assert_eq!(
            changes[0].details[0].path,
            "properties.query.description".to_string()
        );
    }

    #[test]
    fn an_optional_property_addition_beside_an_unnamed_change_keeps_the_floor() {
        // Optional property added is LOW on its own — the weakest row in the table,
        // and therefore the one a lost fallback hides most efficiently.
        let baseline = tool_with_schema(
            "input_schema",
            serde_json::json!({ "type": "object", "properties": {} }),
        );
        let current = tool_with_schema(
            "input_schema",
            serde_json::json!({
                "type": "object",
                "properties": { "note": { "type": "string" } },
                "title": "Not interpreted"
            }),
        );

        let changes = tool(&baseline, &current);
        assert_eq!(changes[0].risk, RiskLevel::High);
        assert_eq!(changes[0].details[0].path, "properties.note".to_string());
    }

    #[test]
    fn a_change_past_the_depth_bound_keeps_the_generic_floor() {
        // The leaf must genuinely differ: an identical deep subtree short-circuits,
        // and only a real difference past the bound is what the walk cannot reach.
        let baseline = tool_with_schema(
            "input_schema",
            nested(
                9,
                serde_json::json!({ "type": "string", "examples": ["a"] }),
            ),
        );
        let mut current_schema = nested(
            9,
            serde_json::json!({ "type": "string", "examples": ["b"] }),
        );
        current_schema["description"] = serde_json::json!("Root description.");

        let changes = tool(&baseline, &tool_with_schema("input_schema", current_schema));

        // The named root change would be MEDIUM on its own.
        assert_eq!(changes[0].risk, RiskLevel::High);
        assert_eq!(
            changes[0].details[0].path,
            "description".to_string(),
            "the named difference is still reported"
        );
    }

    #[test]
    fn a_named_only_change_keeps_its_policy_row() {
        // The other half of the invariant: when every difference is classified, the
        // generic floor must *not* appear. Otherwise the fix would just make
        // everything HIGH.
        let baseline = tool_with_schema(
            "input_schema",
            serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string", "description": "A." } }
            }),
        );
        let current = tool_with_schema(
            "input_schema",
            serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string", "description": "B." } }
            }),
        );

        assert_eq!(tool(&baseline, &current)[0].risk, RiskLevel::Medium);
    }

    #[test]
    fn an_unclassified_only_change_is_still_high() {
        // The behaviour that already worked, kept next to the new one so a fix that
        // dropped either path is visible.
        let baseline = tool_with_schema(
            "input_schema",
            serde_json::json!({ "type": "object", "properties": {} }),
        );
        let current = tool_with_schema(
            "input_schema",
            serde_json::json!({
                "type": "object", "properties": {},
                "examples": [{ "query": "postgres vector search" }]
            }),
        );

        let changes = tool(&baseline, &current);
        assert_eq!(changes[0].risk, RiskLevel::High);
        assert!(changes[0].details.is_empty());
    }

    #[test]
    fn a_required_output_property_removed_entirely_is_critical() {
        let baseline = tool_with_schema(
            "output_schema",
            serde_json::json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }),
        );
        let current = tool_with_schema(
            "output_schema",
            serde_json::json!({ "type": "object", "properties": {}, "required": [] }),
        );

        // Two facts, not one: the property is gone (HIGH) and a required output is
        // gone (CRITICAL). The removal must not erase the requirement.
        let changes = tool(&baseline, &current);
        assert_eq!(changes[0].risk, RiskLevel::Critical);
        assert_eq!(changes[0].details.len(), 2, "{:?}", changes[0].details);
        assert_eq!(changes[0].details[0].path, "properties.id".to_string());
        assert_eq!(changes[0].details[1].path, "required".to_string());
    }

    #[test]
    fn an_optional_output_property_removed_is_high() {
        let baseline = tool_with_schema(
            "output_schema",
            serde_json::json!({
                "type": "object",
                "properties": { "note": { "type": "string" } },
                "required": []
            }),
        );
        let current = tool_with_schema(
            "output_schema",
            serde_json::json!({ "type": "object", "properties": {}, "required": [] }),
        );

        let changes = tool(&baseline, &current);
        assert_eq!(changes[0].risk, RiskLevel::High);
        assert_eq!(changes[0].details.len(), 1, "{:?}", changes[0].details);
    }

    #[test]
    fn a_required_input_property_removed_entirely_follows_the_input_rows() {
        let baseline = tool_with_schema(
            "input_schema",
            serde_json::json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }),
        );
        let current = tool_with_schema(
            "input_schema",
            serde_json::json!({ "type": "object", "properties": {}, "required": [] }),
        );

        // max(PropertyRemoved: HIGH, RequiredRemoved: MEDIUM) on the input side.
        let changes = tool(&baseline, &current);
        assert_eq!(changes[0].risk, RiskLevel::High);
        assert_eq!(changes[0].details.len(), 2, "{:?}", changes[0].details);
    }

    /// The schema facet `tool()` reported for a pair of payloads, if any.
    fn schema_facet_of(name: &'static str, baseline: Value, current: Value) -> Option<FacetChange> {
        tool(
            &tool_with_schema(name, baseline),
            &tool_with_schema(name, current),
        )
        .into_iter()
        .find(|change| change.name == name)
    }

    #[test]
    fn permissive_additional_properties_forms_report_no_schema_change() {
        // An absent key, `true`, and `{}` all permit arbitrary additional properties,
        // so the analyzer concludes equivalence rather than "something I could not
        // name". The fingerprints differ, which is why the facet is still *claimed* —
        // and claimed as unchanged — instead of being left for the engine's
        // unclaimed-facet sweep, which would put the generic HIGH straight back.
        let absent = serde_json::json!({ "type": "object", "properties": {} });
        let empty = serde_json::json!({
            "type": "object", "properties": {}, "additionalProperties": {}
        });
        let yes = serde_json::json!({
            "type": "object", "properties": {}, "additionalProperties": true
        });

        for (baseline, current) in [
            (absent.clone(), empty.clone()),
            (empty.clone(), absent.clone()),
            (yes.clone(), empty.clone()),
            (empty, yes.clone()),
            (absent.clone(), yes.clone()),
            (yes, absent),
        ] {
            let facet = schema_facet_of("input_schema", baseline.clone(), current.clone())
                .unwrap_or_else(|| panic!("{baseline} -> {current}: not claimed at all"));
            assert_eq!(
                facet.change,
                ChangeKind::Unchanged,
                "{baseline} -> {current}"
            );
            assert_eq!(facet.risk, RiskLevel::None, "{baseline} -> {current}");
            assert!(!facet.is_change(), "{baseline} -> {current}");
            assert!(facet.details.is_empty(), "{baseline} -> {current}");
            // Both fingerprints stay in the report: the bytes did move, and hiding
            // that would be its own kind of lie.
            assert!(facet.before_digest.is_some() && facet.after_digest.is_some());
        }
    }

    #[test]
    fn schema_contract_changes_keep_their_policy_rows() {
        // The fix must not weaken a real contract change.
        let open = serde_json::json!({ "type": "object", "properties": {} });
        let closed = serde_json::json!({
            "type": "object", "properties": {}, "additionalProperties": false
        });

        let tightened = schema_facet_of("input_schema", open.clone(), closed.clone()).unwrap();
        assert_eq!(tightened.risk, RiskLevel::High);
        assert!(tightened.is_change());

        let loosened = schema_facet_of("input_schema", closed, open).unwrap();
        assert_eq!(loosened.risk, RiskLevel::Medium);
        assert!(loosened.is_change());
    }

    #[test]
    fn a_schema_valued_form_is_never_declared_equivalent() {
        // `{}` is permissive but `{"type": "string"}` is not something this analyzer
        // will order against it, so the honest verdict is "unaccounted for" — HIGH,
        // with no direction claimed.
        let open = serde_json::json!({ "type": "object", "properties": {} });
        let filtered = serde_json::json!({
            "type": "object", "properties": {},
            "additionalProperties": { "type": "string" }
        });

        for (baseline, current) in [
            (open.clone(), filtered.clone()),
            (filtered.clone(), open.clone()),
        ] {
            let facet = schema_facet_of("input_schema", baseline.clone(), current.clone())
                .unwrap_or_else(|| panic!("{baseline} -> {current}"));
            assert_eq!(facet.risk, RiskLevel::High, "{baseline} -> {current}");
            assert!(facet.is_change(), "{baseline} -> {current}");
            assert!(
                facet.details.is_empty(),
                "no direction is claimed: {:?}",
                facet.details
            );
        }
    }

    #[test]
    fn an_equivalent_form_does_not_hide_a_named_change() {
        // The equivalence is absorbed inside the same facet analysis, so it must
        // neither add risk nor subtract any: MEDIUM is exactly the description row.
        let baseline = tool_with_schema(
            "input_schema",
            serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string", "description": "A." } }
            }),
        );
        let current = tool_with_schema(
            "input_schema",
            serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string", "description": "B." } },
                "additionalProperties": {}
            }),
        );

        let changes = tool(&baseline, &current);
        assert_eq!(changes[0].risk, RiskLevel::Medium);
        assert_eq!(
            changes[0].details[0].path,
            "properties.query.description".to_string()
        );
    }

    #[test]
    fn an_equivalent_form_does_not_hide_an_unclassified_change() {
        // description (MEDIUM) + examples (unclassified) + absent → {} (equivalent).
        // The equivalent part must disappear without suppressing either of the other
        // two, so the facet is HIGH and still names the description.
        let baseline = tool_with_schema(
            "input_schema",
            serde_json::json!({
                "type": "object",
                "description": "A",
                "properties": {}
            }),
        );
        let current = tool_with_schema(
            "input_schema",
            serde_json::json!({
                "type": "object",
                "description": "B",
                "properties": {},
                "additionalProperties": {},
                "examples": [{}]
            }),
        );

        let changes = tool(&baseline, &current);
        assert_eq!(changes[0].risk, RiskLevel::High);
        assert_eq!(changes[0].details.len(), 1, "{:?}", changes[0].details);
        assert_eq!(changes[0].details[0].path, "description".to_string());
    }

    #[test]
    fn a_malformed_required_value_is_never_declared_equivalent() {
        // The set view of `required` maps both of these to the empty set, so without
        // an explicit guard this pair would read as agreement — silence, not
        // equivalence.
        let malformed = tool_with_schema(
            "input_schema",
            serde_json::json!({ "type": "object", "properties": {}, "required": "query" }),
        );
        let empty = tool_with_schema(
            "input_schema",
            serde_json::json!({ "type": "object", "properties": {}, "required": [] }),
        );

        let changes = tool(&malformed, &empty);
        assert_eq!(changes[0].risk, RiskLevel::High);
        assert!(changes[0].is_change());
    }

    #[test]
    fn a_permissive_form_change_that_hides_nothing_reports_no_facet_at_all() {
        // The end of the pipeline for this case: `tool()` sees a facet that was
        // claimed and found unchanged, so a dependency whose only difference is this
        // is not reported as changed by the engine.
        let changes = tool(
            &tool_with_schema(
                "input_schema",
                serde_json::json!({ "type": "object", "properties": {} }),
            ),
            &tool_with_schema(
                "input_schema",
                serde_json::json!({
                    "type": "object", "properties": {}, "additionalProperties": {}
                }),
            ),
        );

        assert!(
            changes.iter().all(|change| !change.is_change()),
            "{changes:?}"
        );
        assert!(changes.iter().all(|change| change.risk == RiskLevel::None));
    }
}
