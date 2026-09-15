// SPDX-License-Identifier: MIT OR Apache-2.0

//! A deliberately bounded JSON Schema analyzer.
//!
//! This is not a schema theorem prover. It understands the constructs that decide
//! whether a tool contract still holds — `type`, `properties`, `required`, `enum`
//! — and descends through `properties` and single-schema `items`. Everything else
//! (`allOf`, `$ref`, `additionalProperties`, tuple-form `items`, `contentSchema`,
//! conditional schemas, …) is deliberately not interpreted.
//!
//! That is safe because of how the caller uses an empty result: it means "I could
//! not name a difference", never "there is no difference". A pair of differing
//! digests that yields no named difference falls back to the generic
//! schema-change risk, so an uninterpreted construct is over-reported rather than
//! silently ignored.

use serde_json::Value;

use crate::diff::model::{ChangeKind, DetailChange, child_path};
use crate::diff::risk::SchemaFact;

/// How deep the analyzer will descend. Pathological or adversarial nesting falls
/// back to the generic result instead of recursing without bound.
const MAX_DEPTH: usize = 8;

/// One named structural difference, with the evidence for it.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaDifference {
    pub fact: SchemaFact,
    pub detail: DetailChange,
}

impl SchemaDifference {
    fn new(fact: SchemaFact, detail: DetailChange) -> Self {
        Self { fact, detail }
    }

    /// Secondary sort key, so details that share a path still order stably.
    fn fact_rank(&self) -> u8 {
        match self.fact {
            SchemaFact::RequiredAdded => 0,
            SchemaFact::RequiredRemoved => 1,
            SchemaFact::PropertyRemoved => 2,
            SchemaFact::OptionalPropertyAdded => 3,
            SchemaFact::TypeChanged => 4,
            SchemaFact::EnumNarrowed => 5,
            SchemaFact::EnumExpanded => 6,
            SchemaFact::DescriptionChanged => 7,
            SchemaFact::AdditionalPropertiesTightened => 8,
            SchemaFact::AdditionalPropertiesLoosened => 9,
            SchemaFact::Generic => 10,
        }
    }
}

/// Compare two schemas and report the differences we can name.
///
/// An empty result means "nothing I can name", which the caller must treat as a
/// generic difference when the digests did not match.
pub fn compare(baseline: &Value, current: &Value) -> Vec<SchemaDifference> {
    let mut differences = Vec::new();
    compare_node(baseline, current, "", 0, &mut differences);

    differences.sort_by(|a, b| {
        let key = |d: &SchemaDifference| {
            (
                d.detail.path.clone(),
                d.fact_rank(),
                value_key(d.detail.before.as_ref().or(d.detail.after.as_ref())),
            )
        };
        key(a).cmp(&key(b))
    });
    differences
}

/// A stable rendering of a detail value, used only for ordering.
fn value_key(value: Option<&Value>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

fn compare_node(
    baseline: &Value,
    current: &Value,
    path: &str,
    depth: usize,
    out: &mut Vec<SchemaDifference>,
) {
    if depth > MAX_DEPTH {
        return;
    }

    let (Some(baseline), Some(current)) = (baseline.as_object(), current.as_object()) else {
        // Not an object schema: outside what we claim to understand. The caller's
        // generic fallback covers it.
        return;
    };

    let empty = serde_json::Map::new();
    let baseline_properties = baseline
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let current_properties = current
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);

    let baseline_required = string_set(baseline.get("required"));
    let current_required = string_set(current.get("required"));

    // Properties that disappeared.
    for name in baseline_properties.keys() {
        if !current_properties.contains_key(name) {
            out.push(SchemaDifference::new(
                SchemaFact::PropertyRemoved,
                DetailChange::new(
                    child_path(path, &format!("properties.{name}")),
                    ChangeKind::Removed,
                ),
            ));
        }
    }

    // Properties that appeared, split into the two very different cases.
    for name in current_properties.keys() {
        if baseline_properties.contains_key(name) {
            continue;
        }
        if current_required.contains(name.as_str()) {
            out.push(SchemaDifference::new(
                SchemaFact::RequiredAdded,
                DetailChange::one_sided(
                    child_path(path, "required"),
                    ChangeKind::Added,
                    Value::String(name.clone()),
                ),
            ));
        } else {
            out.push(SchemaDifference::new(
                SchemaFact::OptionalPropertyAdded,
                DetailChange::new(
                    child_path(path, &format!("properties.{name}")),
                    ChangeKind::Added,
                ),
            ));
        }
    }

    // A previously optional property becoming required is the case that breaks
    // existing calls, so it gets its own decision.
    for name in current_required.difference(&baseline_required) {
        if baseline_properties.contains_key(name.as_str()) {
            out.push(SchemaDifference::new(
                SchemaFact::RequiredAdded,
                DetailChange::one_sided(
                    child_path(path, "required"),
                    ChangeKind::Added,
                    Value::String(name.clone()),
                ),
            ));
        }
    }

    // A requirement lifting is a loosening, not a break.
    for name in baseline_required.difference(&current_required) {
        if current_properties.contains_key(name.as_str()) {
            out.push(SchemaDifference::new(
                SchemaFact::RequiredRemoved,
                DetailChange::one_sided(
                    child_path(path, "required"),
                    ChangeKind::Removed,
                    Value::String(name.clone()),
                ),
            ));
        }
    }

    // `type`, `description`, `enum`, and `additionalProperties`, at this node and
    // — through the recursion below — inside every property and item.
    compare_type(baseline, current, path, out);
    compare_description(baseline, current, path, out);
    compare_enum(baseline, current, path, out);
    compare_additional_properties(baseline, current, path, out);

    for (name, baseline_child) in baseline_properties {
        let Some(current_child) = current_properties.get(name) else {
            continue;
        };
        let child = child_path(path, &format!("properties.{name}"));
        compare_node(baseline_child, current_child, &child, depth + 1, out);
    }

    // Single-schema `items`; tuple form is out of scope and falls back.
    if let (Some(baseline_items), Some(current_items)) = (
        baseline.get("items").filter(|v| v.is_object()),
        current.get("items").filter(|v| v.is_object()),
    ) {
        let child = child_path(path, "items");
        compare_node(baseline_items, current_items, &child, depth + 1, out);
    }
}

fn compare_type(
    baseline: &serde_json::Map<String, Value>,
    current: &serde_json::Map<String, Value>,
    path: &str,
    out: &mut Vec<SchemaDifference>,
) {
    let (Some(before), Some(after)) = (baseline.get("type"), current.get("type")) else {
        return;
    };
    if before == after {
        return;
    }
    out.push(SchemaDifference::new(
        SchemaFact::TypeChanged,
        DetailChange::swapped(child_path(path, "type"), before.clone(), after.clone()),
    ));
}

/// A `description` at this node, anywhere in the schema.
///
/// Descriptions are the guidance a model reads before it calls a tool, so a
/// change here is behavior-relevant — the same class as a tool description
/// change, one level down. This is why it is compared rather than treated as
/// documentation noise.
fn compare_description(
    baseline: &serde_json::Map<String, Value>,
    current: &serde_json::Map<String, Value>,
    path: &str,
    out: &mut Vec<SchemaDifference>,
) {
    let before = baseline.get("description");
    let after = current.get("description");
    if before == after {
        return;
    }

    let location = child_path(path, "description");
    let detail = match (before, after) {
        (Some(before), Some(after)) => {
            DetailChange::swapped(location, before.clone(), after.clone())
        }
        (Some(before), None) => {
            DetailChange::one_sided(location, ChangeKind::Removed, before.clone())
        }
        (None, Some(after)) => DetailChange::one_sided(location, ChangeKind::Added, after.clone()),
        // Equal sides returned above, so at least one side carries the key.
        (None, None) => return,
    };

    out.push(SchemaDifference::new(
        SchemaFact::DescriptionChanged,
        detail,
    ));
}

/// `additionalProperties` tightened or loosened.
///
/// Only the *direction* is classified, and only when it is unambiguous: `false`
/// and a subschema both restrict the property set, while `true` and an absent key
/// admit anything. Two restricting forms compared against each other — a subschema
/// against `false`, or two different subschemas — are left to the caller's generic
/// fallback, which is never quieter than these rows.
fn compare_additional_properties(
    baseline: &serde_json::Map<String, Value>,
    current: &serde_json::Map<String, Value>,
    path: &str,
    out: &mut Vec<SchemaDifference>,
) {
    let before = baseline.get("additionalProperties");
    let after = current.get("additionalProperties");
    if before == after {
        return;
    }

    let admits_anything =
        |value: Option<&Value>| !matches!(value, Some(Value::Bool(false)) | Some(Value::Object(_)));

    let fact = match (admits_anything(before), admits_anything(after)) {
        (true, false) => SchemaFact::AdditionalPropertiesTightened,
        (false, true) => SchemaFact::AdditionalPropertiesLoosened,
        _ => return,
    };

    // An absent key means `true` in JSON Schema, so it is shown as such rather
    // than as nothing.
    out.push(SchemaDifference::new(
        fact,
        DetailChange::swapped(
            child_path(path, "additionalProperties"),
            before.cloned().unwrap_or(Value::Bool(true)),
            after.cloned().unwrap_or(Value::Bool(true)),
        ),
    ));
}

fn compare_enum(
    baseline: &serde_json::Map<String, Value>,
    current: &serde_json::Map<String, Value>,
    path: &str,
    out: &mut Vec<SchemaDifference>,
) {
    let before = baseline.get("enum").and_then(Value::as_array);
    let after = current.get("enum").and_then(Value::as_array);

    match (before, after) {
        // An enum appearing restricts what was previously unrestricted.
        (None, Some(after)) => out.push(SchemaDifference::new(
            SchemaFact::EnumNarrowed,
            DetailChange::one_sided(
                child_path(path, "enum"),
                ChangeKind::Added,
                Value::Array(after.clone()),
            ),
        )),
        // Losing the enum widens the accepted set.
        (Some(before), None) => out.push(SchemaDifference::new(
            SchemaFact::EnumExpanded,
            DetailChange::one_sided(
                child_path(path, "enum"),
                ChangeKind::Removed,
                Value::Array(before.clone()),
            ),
        )),
        (Some(before), Some(after)) => {
            let before_set: Vec<&Value> = before.iter().collect();
            let after_set: Vec<&Value> = after.iter().collect();

            let removed: Vec<Value> = before
                .iter()
                .filter(|value| !after.contains(value))
                .cloned()
                .collect();
            let added: Vec<Value> = after
                .iter()
                .filter(|value| !before.contains(value))
                .cloned()
                .collect();

            if !removed.is_empty() {
                out.push(SchemaDifference::new(
                    SchemaFact::EnumNarrowed,
                    DetailChange::one_sided(
                        child_path(path, "enum"),
                        ChangeKind::Removed,
                        Value::Array(removed),
                    ),
                ));
            }
            if !added.is_empty() {
                out.push(SchemaDifference::new(
                    SchemaFact::EnumExpanded,
                    DetailChange::one_sided(
                        child_path(path, "enum"),
                        ChangeKind::Added,
                        Value::Array(added),
                    ),
                ));
            }
            let _ = (before_set, after_set);
        }
        (None, None) => {}
    }
}

/// The `required` keyword as a set of names, ignoring anything that is not a
/// string (a malformed schema is out of scope, not a panic).
fn string_set(value: Option<&Value>) -> std::collections::BTreeSet<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::risk::SchemaFact;
    use serde_json::json;

    fn facts(baseline: Value, current: Value) -> Vec<SchemaFact> {
        compare(&baseline, &current)
            .into_iter()
            .map(|difference| difference.fact)
            .collect()
    }

    fn paths(baseline: Value, current: Value) -> Vec<String> {
        compare(&baseline, &current)
            .into_iter()
            .map(|difference| difference.detail.path)
            .collect()
    }

    fn input_schema(properties: Value, required: Value) -> Value {
        json!({ "type": "object", "properties": properties, "required": required })
    }

    #[test]
    fn adding_a_required_property_is_named() {
        let baseline = input_schema(json!({ "query": { "type": "string" } }), json!(["query"]));
        let current = input_schema(
            json!({ "query": { "type": "string" }, "owner": { "type": "string" } }),
            json!(["query", "owner"]),
        );

        let differences = compare(&baseline, &current);
        assert_eq!(differences.len(), 1, "{differences:?}");
        assert_eq!(differences[0].fact, SchemaFact::RequiredAdded);
        assert_eq!(differences[0].detail.path, "required");
        assert_eq!(differences[0].detail.after, Some(json!("owner")));
    }

    #[test]
    fn making_an_existing_property_required_is_named() {
        let baseline = input_schema(
            json!({ "query": { "type": "string" }, "owner": { "type": "string" } }),
            json!(["query"]),
        );
        let current = input_schema(
            json!({ "query": { "type": "string" }, "owner": { "type": "string" } }),
            json!(["query", "owner"]),
        );

        assert_eq!(facts(baseline, current), vec![SchemaFact::RequiredAdded]);
    }

    #[test]
    fn adding_an_optional_property_is_a_different_fact_from_adding_a_required_one() {
        let baseline = input_schema(json!({}), json!([]));
        let current = input_schema(json!({ "note": { "type": "string" } }), json!([]));

        let differences = compare(&baseline, &current);
        assert_eq!(differences.len(), 1, "{differences:?}");
        assert_eq!(differences[0].fact, SchemaFact::OptionalPropertyAdded);
        assert_eq!(differences[0].detail.path, "properties.note");
    }

    #[test]
    fn removing_a_property_is_named() {
        let baseline = input_schema(
            json!({ "query": { "type": "string" }, "legacy": { "type": "string" } }),
            json!(["query"]),
        );
        let current = input_schema(json!({ "query": { "type": "string" } }), json!(["query"]));

        assert_eq!(facts(baseline, current), vec![SchemaFact::PropertyRemoved]);
        assert_eq!(
            paths(
                input_schema(json!({ "legacy": {} }), json!([])),
                input_schema(json!({}), json!([]))
            ),
            vec!["properties.legacy".to_string()]
        );
    }

    #[test]
    fn removing_a_required_marker_is_a_loosening() {
        let baseline = input_schema(json!({ "query": { "type": "string" } }), json!(["query"]));
        let current = input_schema(json!({ "query": { "type": "string" } }), json!([]));

        assert_eq!(facts(baseline, current), vec![SchemaFact::RequiredRemoved]);
    }

    #[test]
    fn a_type_change_is_named_with_both_sides() {
        let baseline = input_schema(json!({ "per_page": { "type": "string" } }), json!([]));
        let current = input_schema(json!({ "per_page": { "type": "integer" } }), json!([]));

        let differences = compare(&baseline, &current);
        assert_eq!(differences.len(), 1, "{differences:?}");
        assert_eq!(differences[0].fact, SchemaFact::TypeChanged);
        assert_eq!(differences[0].detail.path, "properties.per_page.type");
        assert_eq!(differences[0].detail.before, Some(json!("string")));
        assert_eq!(differences[0].detail.after, Some(json!("integer")));
    }

    #[test]
    fn narrowing_and_expanding_an_enum_are_distinguished() {
        let baseline = input_schema(json!({ "sort": { "enum": ["asc", "desc"] } }), json!([]));
        let narrowed = input_schema(json!({ "sort": { "enum": ["asc"] } }), json!([]));
        let expanded = input_schema(
            json!({ "sort": { "enum": ["asc", "desc", "random"] } }),
            json!([]),
        );

        assert_eq!(
            facts(baseline.clone(), narrowed),
            vec![SchemaFact::EnumNarrowed]
        );
        assert_eq!(facts(baseline, expanded), vec![SchemaFact::EnumExpanded]);
    }

    #[test]
    fn an_enum_appearing_or_disappearing_is_a_constraint_change() {
        let with_enum = input_schema(json!({ "sort": { "enum": ["asc"] } }), json!([]));
        let without = input_schema(json!({ "sort": {} }), json!([]));

        assert_eq!(
            facts(without.clone(), with_enum.clone()),
            vec![SchemaFact::EnumNarrowed]
        );
        assert_eq!(facts(with_enum, without), vec![SchemaFact::EnumExpanded]);
    }

    #[test]
    fn nested_properties_are_reached_with_a_path() {
        let baseline = input_schema(
            json!({ "filter": { "type": "object", "properties": { "owner": { "type": "string" } },
                                "required": [] } }),
            json!([]),
        );
        let current = input_schema(
            json!({ "filter": { "type": "object", "properties": { "owner": { "type": "string" } },
                                "required": ["owner"] } }),
            json!([]),
        );

        let differences = compare(&baseline, &current);
        assert_eq!(differences.len(), 1, "{differences:?}");
        assert_eq!(differences[0].fact, SchemaFact::RequiredAdded);
        assert_eq!(differences[0].detail.path, "properties.filter.required");
        assert_eq!(differences[0].detail.after, Some(json!("owner")));
    }

    #[test]
    fn array_item_schemas_are_reached() {
        let baseline = input_schema(
            json!({ "ids": { "type": "array", "items": { "type": "integer" } } }),
            json!([]),
        );
        let current = input_schema(
            json!({ "ids": { "type": "array", "items": { "type": "string" } } }),
            json!([]),
        );

        let differences = compare(&baseline, &current);
        assert_eq!(differences.len(), 1, "{differences:?}");
        assert_eq!(differences[0].fact, SchemaFact::TypeChanged);
        assert_eq!(differences[0].detail.path, "properties.ids.items.type");
    }

    #[test]
    fn a_change_outside_the_analysed_subset_produces_no_named_difference() {
        // `examples` is behaviour-relevant (spec §7.4 forbids normalizing it away)
        // but is not one of the interpretations the diff layer claims to name. The
        // empty result is what makes the caller fall back to the generic risk, so
        // this guards the fail-safe path rather than a gap.
        let baseline = input_schema(json!({ "query": { "type": "string" } }), json!(["query"]));
        let mut current = baseline.clone();
        current["examples"] = json!([{ "query": "postgres vector search" }]);

        assert!(compare(&baseline, &current).is_empty());
    }

    #[test]
    fn non_object_schemas_produce_no_named_difference() {
        assert!(compare(&json!(true), &json!(false)).is_empty());
        assert!(compare(&json!("a"), &json!("b")).is_empty());
    }

    #[test]
    fn deep_nesting_stops_instead_of_recursing_without_bound() {
        let mut baseline = json!({ "type": "object", "properties": {} });
        let mut current = json!({ "type": "object", "properties": {} });
        for _ in 0..(MAX_DEPTH + 4) {
            baseline = json!({ "type": "object", "properties": { "next": baseline } });
            current = json!({ "type": "object", "properties": { "next": current } });
        }
        current["properties"]["next"]["properties"]["next"]["type"] = json!("string");

        // No panic, and whatever it reports is bounded; the caller's fallback
        // covers the rest.
        let _ = compare(&baseline, &current);
    }

    #[test]
    fn ordering_is_stable_across_runs() {
        let baseline = input_schema(
            json!({ "b": { "type": "string" }, "a": { "type": "string" },
                    "c": { "type": "string", "enum": ["x"] } }),
            json!(["b"]),
        );
        let current = input_schema(
            json!({ "b": { "type": "integer" }, "a": { "type": "string" },
                    "c": { "type": "string", "enum": ["y"] } }),
            json!([]),
        );

        let first = paths(baseline.clone(), current.clone());
        let second = paths(baseline, current);
        assert_eq!(first, second);
        // Replacing every enum member is reported as both a narrowing and an
        // expansion — the two facts are separate, and their risks are combined by
        // the caller's max, so neither direction is lost.
        assert_eq!(
            first,
            vec![
                "properties.b.type".to_string(),
                "properties.c.enum".to_string(),
                "properties.c.enum".to_string(),
                "required".to_string(),
            ]
        );
    }

    #[test]
    fn a_property_description_change_is_named_where_it_happened() {
        let baseline = input_schema(
            json!({ "query": { "type": "string", "description": "A plain phrase." } }),
            json!(["query"]),
        );
        let current = input_schema(
            json!({ "query": { "type": "string", "description": "A search expression." } }),
            json!(["query"]),
        );

        let differences = compare(&baseline, &current);
        assert_eq!(differences.len(), 1, "{differences:?}");
        assert_eq!(differences[0].fact, SchemaFact::DescriptionChanged);
        assert_eq!(differences[0].detail.path, "properties.query.description");
        assert_eq!(differences[0].detail.before, Some(json!("A plain phrase.")));
        assert_eq!(
            differences[0].detail.after,
            Some(json!("A search expression."))
        );
    }

    #[test]
    fn losing_a_description_is_named_too() {
        let baseline = input_schema(
            json!({ "query": { "type": "string", "description": "A plain phrase." } }),
            json!(["query"]),
        );
        let current = input_schema(json!({ "query": { "type": "string" } }), json!(["query"]));

        let differences = compare(&baseline, &current);
        assert_eq!(differences.len(), 1, "{differences:?}");
        assert_eq!(differences[0].fact, SchemaFact::DescriptionChanged);
        assert_eq!(differences[0].detail.change, ChangeKind::Removed);
    }

    #[test]
    fn additional_properties_direction_is_named() {
        let tight = json!({ "type": "object", "additionalProperties": false, "properties": {} });
        let loose = json!({ "type": "object", "properties": {} });

        assert_eq!(
            facts(loose.clone(), tight.clone()),
            vec![SchemaFact::AdditionalPropertiesTightened]
        );
        assert_eq!(
            facts(tight.clone(), loose.clone()),
            vec![SchemaFact::AdditionalPropertiesLoosened]
        );

        // An absent key means `true`, so it is shown as such rather than as
        // nothing.
        let differences = compare(&loose, &tight);
        assert_eq!(differences[0].detail.before, Some(json!(true)));
        assert_eq!(differences[0].detail.after, Some(json!(false)));
        assert_eq!(
            differences[0].detail.path,
            "additionalProperties".to_string()
        );
    }

    #[test]
    fn two_restricting_forms_are_left_to_the_generic_fallback() {
        // `false` and a subschema both restrict; ranking one against the other
        // would be a claim the analyzer cannot support, and the caller's generic
        // fallback is never quieter than these rows.
        let closed = json!({ "type": "object", "additionalProperties": false });
        let filtered = json!({ "type": "object", "additionalProperties": { "type": "string" } });

        assert!(compare(&closed, &filtered).is_empty());
    }
}
