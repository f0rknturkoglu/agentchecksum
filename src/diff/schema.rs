// SPDX-License-Identifier: MIT OR Apache-2.0

//! A deliberately bounded JSON Schema analyzer.
//!
//! This is not a schema theorem prover. It understands the constructs that decide
//! whether a tool contract still holds — `type`, `properties`, `required`, `enum`,
//! `description`, and the direction of `additionalProperties` when a bounded
//! analyzer can actually see it — and descends through `properties` and
//! single-schema `items`. Everything else (`allOf`, `$ref`, tuple-form `items`,
//! `contentSchema`, conditionals, `examples`, `title`, …) is deliberately not
//! interpreted.
//!
//! What keeps that safe is not the *emptiness* of the result but its
//! **completeness**: a comparison reports both the differences it could name and
//! whether anything differed that it could not. "Nothing I could name" is never
//! reported as "there is nothing there", and a mixed change — part understood,
//! part not — stays as loud as its unexplained part.

use serde_json::Value;

use crate::diff::model::{ChangeKind, DetailChange, child_path};
use crate::diff::risk::SchemaFact;

/// How deep the analyzer will descend. Pathological or adversarial nesting falls
/// back to the generic result instead of recursing without bound.
const MAX_DEPTH: usize = 8;

/// The keys interpreted at each node, in both directions: whatever is listed here
/// is handled below, and whatever is handled below is listed here.
///
/// The list is load-bearing. Every key *outside* it is compared verbatim, and any
/// difference in one is unclassified by construction — so a construct that gains
/// an interpretation without being added here would keep being reported as
/// unexplained (loud, but wrong), while a key added here without an
/// interpretation would become silent.
const INTERPRETED_KEYS: [&str; 7] = [
    "type",
    "description",
    "enum",
    "additionalProperties",
    "properties",
    "required",
    "items",
];

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

/// What comparing two schemas produced.
///
/// Two things, not one: the differences that could be named, and whether anything
/// differed that could not be. The second field is what keeps a *mixed* change
/// honest, and it is not derivable from the first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchemaAnalysis {
    /// The named differences, sorted for stable output.
    pub differences: Vec<SchemaDifference>,
    /// Whether the schemas differ somewhere this analyzer cannot name: a key it
    /// does not interpret, a construct it declines to order, or a subtree past its
    /// depth bound.
    ///
    /// The caller must apply the generic schema risk when this is set, **even when
    /// `differences` is not empty**. A schema that changed in one place the
    /// analyzer understands and in another place it does not is not a classified
    /// change; reporting only the understood part is precisely the under-reporting
    /// this flag exists to prevent.
    pub has_unclassified_change: bool,
}

impl SchemaAnalysis {
    /// Whether the pair produced nothing at all: nothing named, nothing flagged.
    ///
    /// A caller only reaches this when the recorded digests differed, so it means a
    /// payload disagrees with its own digest. That is a distinct case from "every
    /// difference was classified", which is why the caller cannot spell this as
    /// `differences.is_empty()`.
    pub fn found_nothing(&self) -> bool {
        self.differences.is_empty() && !self.has_unclassified_change
    }
}

/// Compare two schemas.
pub fn compare(baseline: &Value, current: &Value) -> SchemaAnalysis {
    let mut analysis = SchemaAnalysis::default();
    compare_node(baseline, current, "", 0, &mut analysis);

    analysis.differences.sort_by(|a, b| {
        let key = |d: &SchemaDifference| {
            (
                d.detail.path.clone(),
                d.fact_rank(),
                value_key(d.detail.before.as_ref().or(d.detail.after.as_ref())),
            )
        };
        key(a).cmp(&key(b))
    });

    analysis
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
    analysis: &mut SchemaAnalysis,
) {
    // Equal subtrees cannot contain a difference. Returning here is also what makes
    // the depth rule below exact: a subtree is only "too deep" when it differs.
    if baseline == current {
        return;
    }

    // Past the bound, a difference is real but unnamed. Returning quietly here is
    // exactly the failure this analyzer must not have, since it would make a deep
    // change *quieter* than a shallow one.
    if depth > MAX_DEPTH {
        analysis.has_unclassified_change = true;
        return;
    }

    let (Some(baseline), Some(current)) = (baseline.as_object(), current.as_object()) else {
        // A boolean schema, or a malformed node: nothing here is interpreted, so the
        // difference is unclassified rather than silent.
        analysis.has_unclassified_change = true;
        return;
    };

    // Everything this analyzer does not interpret, compared verbatim. No
    // normalization is claimed for a key the analyzer does not model, so any
    // difference in one is unclassified by construction.
    for key in baseline.keys().chain(current.keys()) {
        if INTERPRETED_KEYS.contains(&key.as_str()) {
            continue;
        }
        if baseline.get(key) != current.get(key) {
            analysis.has_unclassified_change = true;
        }
    }

    let empty = serde_json::Map::new();
    let baseline_properties = baseline
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let current_properties = current
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);

    // A `properties` that is not an object on either side is a difference the
    // analyzer will not interpret, so it must be flagged rather than skipped by the
    // loop above (which treats `properties` as interpreted).
    let not_an_object = |value: Option<&Value>| value.is_some_and(|value| !value.is_object());
    if baseline.get("properties") != current.get("properties")
        && (not_an_object(baseline.get("properties")) || not_an_object(current.get("properties")))
    {
        analysis.has_unclassified_change = true;
    }

    let baseline_required = string_set(baseline.get("required"));
    let current_required = string_set(current.get("required"));

    // Properties that disappeared.
    for name in baseline_properties.keys() {
        if !current_properties.contains_key(name) {
            analysis.differences.push(SchemaDifference::new(
                SchemaFact::PropertyRemoved,
                DetailChange::new(
                    child_path(path, &format!("properties.{name}")),
                    ChangeKind::Removed,
                ),
            ));

            // A removed property takes its requirement with it. "A required output
            // disappeared" is not the same change as "an optional output
            // disappeared", and the removal must not erase the requirement: the
            // policy table decides from both facts.
            if baseline_required.contains(name.as_str()) {
                analysis.differences.push(SchemaDifference::new(
                    SchemaFact::RequiredRemoved,
                    DetailChange::one_sided(
                        child_path(path, "required"),
                        ChangeKind::Removed,
                        Value::String(name.clone()),
                    ),
                ));
            }
        }
    }

    // Properties that appeared, split into the two very different cases.
    for name in current_properties.keys() {
        if baseline_properties.contains_key(name) {
            continue;
        }
        if current_required.contains(name.as_str()) {
            analysis.differences.push(SchemaDifference::new(
                SchemaFact::RequiredAdded,
                DetailChange::one_sided(
                    child_path(path, "required"),
                    ChangeKind::Added,
                    Value::String(name.clone()),
                ),
            ));
        } else {
            analysis.differences.push(SchemaDifference::new(
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
            analysis.differences.push(SchemaDifference::new(
                SchemaFact::RequiredAdded,
                DetailChange::one_sided(
                    child_path(path, "required"),
                    ChangeKind::Added,
                    Value::String(name.clone()),
                ),
            ));
        }
    }

    // A requirement lifting is a loosening, not a break. A property that
    // disappeared entirely is handled above, so this covers only a requirement
    // dropped while the property itself stayed.
    for name in baseline_required.difference(&current_required) {
        if current_properties.contains_key(name.as_str()) {
            analysis.differences.push(SchemaDifference::new(
                SchemaFact::RequiredRemoved,
                DetailChange::one_sided(
                    child_path(path, "required"),
                    ChangeKind::Removed,
                    Value::String(name.clone()),
                ),
            ));
        }
    }

    // Each keyword comparison reports whether it classified the difference it saw.
    // One that did not leaves this node with something unexplained, and that
    // verdict has to survive whatever else was classified here.
    let mut classified = compare_type(baseline, current, path, &mut analysis.differences);
    classified &= compare_description(baseline, current, path, &mut analysis.differences);
    classified &= compare_enum(baseline, current, path, &mut analysis.differences);
    classified &= compare_additional_properties(baseline, current, path, &mut analysis.differences);
    classified &= compare_items(baseline, current, path, depth, analysis);
    if !classified {
        analysis.has_unclassified_change = true;
    }

    for (name, baseline_child) in baseline_properties {
        let Some(current_child) = current_properties.get(name) else {
            continue;
        };
        let child = child_path(path, &format!("properties.{name}"));
        compare_node(baseline_child, current_child, &child, depth + 1, analysis);
    }
}

/// Returns whether the difference, if any, was classified.
fn compare_type(
    baseline: &serde_json::Map<String, Value>,
    current: &serde_json::Map<String, Value>,
    path: &str,
    out: &mut Vec<SchemaDifference>,
) -> bool {
    let before = baseline.get("type");
    let after = current.get("type");
    if before == after {
        return true;
    }

    match (before, after) {
        (Some(before), Some(after)) => {
            out.push(SchemaDifference::new(
                SchemaFact::TypeChanged,
                DetailChange::swapped(child_path(path, "type"), before.clone(), after.clone()),
            ));
            true
        }
        // A `type` appearing or disappearing changes what the schema accepts, and
        // this analyzer will not claim to know in which direction.
        _ => false,
    }
}

/// A `description` at this node, anywhere in the schema.
///
/// Descriptions are the guidance a model reads before it calls a tool, so a
/// change here is behavior-relevant — the same class as a tool description
/// change, one level down. This is why it is compared rather than treated as
/// documentation noise.
///
/// Every difference in this key is classified: the key *is* the interpretation, so
/// this always reports success. It returns a verdict anyway to keep the caller's
/// aggregation uniform, where a `false` means "this node has something it could not
/// explain".
fn compare_description(
    baseline: &serde_json::Map<String, Value>,
    current: &serde_json::Map<String, Value>,
    path: &str,
    out: &mut Vec<SchemaDifference>,
) -> bool {
    let before = baseline.get("description");
    let after = current.get("description");
    if before == after {
        return true;
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
        // Equal sides returned above, so this is unreachable.
        (None, None) => return true,
    };

    out.push(SchemaDifference::new(
        SchemaFact::DescriptionChanged,
        detail,
    ));
    true
}

/// How `additionalProperties` treats properties the schema does not declare.
///
/// Only two of these forms can be ordered against each other by a bounded
/// analyzer: "anything is accepted" and "nothing is". A schema-valued form needs
/// schema reasoning to rank — even the empty schema, whose *unconstrained* meaning
/// is what makes it equal to `true`, is not comparable against a non-empty one
/// here without claiming more than this analyzer knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// `true`, an absent key, or `{}`: anything is accepted.
    Anything,
    /// `false`: undeclared properties are refused.
    Nothing,
    /// A non-empty schema.
    Schema,
    /// A form JSON Schema does not define in this position.
    Unsupported,
}

fn admission(value: Option<&Value>) -> Admission {
    match value {
        None | Some(Value::Bool(true)) => Admission::Anything,
        Some(Value::Bool(false)) => Admission::Nothing,
        // `{}` constrains nothing, so it says exactly what `true` says.
        Some(Value::Object(schema)) if schema.is_empty() => Admission::Anything,
        Some(Value::Object(_)) => Admission::Schema,
        Some(_) => Admission::Unsupported,
    }
}

/// Returns whether the difference, if any, was classified.
fn compare_additional_properties(
    baseline: &serde_json::Map<String, Value>,
    current: &serde_json::Map<String, Value>,
    path: &str,
    out: &mut Vec<SchemaDifference>,
) -> bool {
    let before = baseline.get("additionalProperties");
    let after = current.get("additionalProperties");
    if before == after {
        return true;
    }

    let fact = match (admission(before), admission(after)) {
        (Admission::Anything, Admission::Nothing) => SchemaFact::AdditionalPropertiesTightened,
        (Admission::Nothing, Admission::Anything) => SchemaFact::AdditionalPropertiesLoosened,
        // An absent key, `true`, and `{}` are the same statement, so a difference
        // between them is not a difference. Reporting a direction here would be a
        // false alarm on a change that cannot affect behavior.
        (Admission::Anything, Admission::Anything) => return true,
        // Ordering two schema-valued forms, or a form JSON Schema does not define,
        // needs reasoning this analyzer does not do. The caller's generic floor is
        // the honest answer, and it is never quieter than the rows above.
        _ => return false,
    };

    // An absent key means `true` in JSON Schema, so it is shown as such rather than
    // as nothing.
    out.push(SchemaDifference::new(
        fact,
        DetailChange::swapped(
            child_path(path, "additionalProperties"),
            before.cloned().unwrap_or(Value::Bool(true)),
            after.cloned().unwrap_or(Value::Bool(true)),
        ),
    ));
    true
}

/// Returns whether the difference, if any, was classified.
fn compare_enum(
    baseline: &serde_json::Map<String, Value>,
    current: &serde_json::Map<String, Value>,
    path: &str,
    out: &mut Vec<SchemaDifference>,
) -> bool {
    let raw_before = baseline.get("enum");
    let raw_after = current.get("enum");
    if raw_before == raw_after {
        return true;
    }

    let before = raw_before.and_then(Value::as_array);
    let after = raw_after.and_then(Value::as_array);

    match (before, after) {
        // An enum appearing restricts what was previously unrestricted.
        (None, Some(after)) => {
            out.push(SchemaDifference::new(
                SchemaFact::EnumNarrowed,
                DetailChange::one_sided(
                    child_path(path, "enum"),
                    ChangeKind::Added,
                    Value::Array(after.clone()),
                ),
            ));
            true
        }
        // Losing the enum widens the accepted set.
        (Some(before), None) => {
            out.push(SchemaDifference::new(
                SchemaFact::EnumExpanded,
                DetailChange::one_sided(
                    child_path(path, "enum"),
                    ChangeKind::Removed,
                    Value::Array(before.clone()),
                ),
            ));
            true
        }
        (Some(before), Some(after)) => {
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
            // Two arrays with the same members in a different order differ in bytes
            // but not in meaning: the fingerprint layer normalizes member order
            // (§7.3), so this is a difference classified as insignificant rather
            // than one left unexplained.
            true
        }
        // An `enum` that is not an array on either side is a real difference this
        // analyzer will not guess at.
        (None, None) => false,
    }
}

/// Single-schema `items`, at this node.
///
/// Returns whether the difference, if any, was classified. Only the single-schema
/// form is descended into, so tuple-form `items` — and an `items` present on one
/// side only — are differences this analyzer does not name, and the verdict says so
/// rather than assuming the recursion covered them.
fn compare_items(
    baseline: &serde_json::Map<String, Value>,
    current: &serde_json::Map<String, Value>,
    path: &str,
    depth: usize,
    analysis: &mut SchemaAnalysis,
) -> bool {
    let before = baseline.get("items");
    let after = current.get("items");
    if before == after {
        return true;
    }

    let (Some(before_items), Some(after_items)) = (
        before.filter(|value| value.is_object()),
        after.filter(|value| value.is_object()),
    ) else {
        return false;
    };

    let child = child_path(path, "items");
    compare_node(before_items, after_items, &child, depth + 1, analysis);
    true
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
            .differences
            .into_iter()
            .map(|difference| difference.fact)
            .collect()
    }

    /// Whether the analyzer found something it could not name.
    fn unclassified(baseline: Value, current: Value) -> bool {
        compare(&baseline, &current).has_unclassified_change
    }

    /// Nest a schema inside `depth` object properties, for the depth-bound tests.
    fn nested(depth: usize, schema: Value) -> Value {
        let mut schema = schema;
        for _ in 0..depth {
            schema = json!({ "type": "object", "properties": { "next": schema } });
        }
        schema
    }

    fn paths(baseline: Value, current: Value) -> Vec<String> {
        compare(&baseline, &current)
            .differences
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

        let differences = compare(&baseline, &current).differences;
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

        let differences = compare(&baseline, &current).differences;
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

        let differences = compare(&baseline, &current).differences;
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

        let differences = compare(&baseline, &current).differences;
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

        let differences = compare(&baseline, &current).differences;
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

        let analysis = compare(&baseline, &current);
        assert!(analysis.differences.is_empty());
        assert!(
            analysis.has_unclassified_change,
            "nothing named is not the same as nothing there"
        );
    }

    #[test]
    fn a_boolean_schema_difference_is_unclassified_rather_than_silent() {
        // `true` accepts anything and `false` accepts nothing: the analyzer names
        // nothing for boolean schemas, so the difference must be flagged instead of
        // reported as no difference at all.
        let analysis = compare(&json!(true), &json!(false));
        assert!(analysis.differences.is_empty());
        assert!(analysis.has_unclassified_change);

        // Two malformed nodes are the same story.
        assert!(compare(&json!("a"), &json!("b")).has_unclassified_change);
    }

    #[test]
    fn nesting_beyond_the_bound_is_flagged_rather_than_descended_into() {
        // The bound exists so adversarial nesting cannot drive unbounded recursion,
        // and the only way to tell whether the walk stopped at it is to change the
        // node *past* it: a shallow mutation would pass with the bound removed.
        let baseline = nested(MAX_DEPTH + 4, json!({ "type": "string" }));
        let current = nested(MAX_DEPTH + 4, json!({ "type": "integer" }));

        let analysis = compare(&baseline, &current);
        assert!(
            analysis.differences.is_empty(),
            "the walk reported a node past its bound: {:?}",
            analysis.differences
        );
        assert!(analysis.has_unclassified_change);
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

        let differences = compare(&baseline, &current).differences;
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

        let differences = compare(&baseline, &current).differences;
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
        let differences = compare(&loose, &tight).differences;
        assert_eq!(differences[0].detail.before, Some(json!(true)));
        assert_eq!(differences[0].detail.after, Some(json!(false)));
        assert_eq!(
            differences[0].detail.path,
            "additionalProperties".to_string()
        );
    }

    #[test]
    fn two_schema_valued_forms_are_left_to_the_generic_fallback() {
        // Ranking two schema-valued forms needs reasoning this analyzer does not
        // do. The verdict is "unclassified", not "no change": the caller's generic
        // floor is the honest answer, and it is never quieter than the named rows.
        let closed = json!({ "type": "object", "additionalProperties": false });
        let filtered = json!({ "type": "object", "additionalProperties": { "type": "string" } });
        let open = json!({ "type": "object", "additionalProperties": {} });
        let other = json!({ "type": "object", "additionalProperties": { "type": "integer" } });

        for (baseline, current) in [
            (closed.clone(), filtered.clone()),
            (filtered.clone(), closed.clone()),
            (open.clone(), filtered.clone()),
            (filtered, other),
        ] {
            let analysis = compare(&baseline, &current);
            assert!(analysis.differences.is_empty(), "{baseline} -> {current}");
            assert!(analysis.has_unclassified_change, "{baseline} -> {current}");
        }
    }

    #[test]
    fn permissive_additional_properties_forms_are_the_same_statement() {
        // An absent key means `true`, and `{}` constrains nothing, so all three say
        // the same thing. Reporting a direction between them would be a false alarm
        // on a change that cannot affect behavior.
        let absent = json!({ "type": "object" });
        let empty = json!({ "type": "object", "additionalProperties": {} });
        let yes = json!({ "type": "object", "additionalProperties": true });

        for (baseline, current) in [
            (absent.clone(), empty.clone()),
            (empty.clone(), absent),
            (yes.clone(), empty.clone()),
            (empty, yes),
        ] {
            let analysis = compare(&baseline, &current);
            assert!(analysis.differences.is_empty(), "{baseline} -> {current}");
            assert!(!analysis.has_unclassified_change, "{baseline} -> {current}");
        }
    }

    #[test]
    fn an_empty_schema_still_tightens_against_a_closed_one() {
        let open = json!({ "type": "object", "additionalProperties": {} });
        let closed = json!({ "type": "object", "additionalProperties": false });

        assert_eq!(
            facts(open.clone(), closed.clone()),
            vec![SchemaFact::AdditionalPropertiesTightened]
        );
        assert_eq!(
            facts(closed, open),
            vec![SchemaFact::AdditionalPropertiesLoosened]
        );
    }

    #[test]
    fn a_removed_required_property_keeps_both_facts() {
        let baseline = input_schema(json!({ "id": { "type": "string" } }), json!(["id"]));
        let current = input_schema(json!({}), json!([]));

        let differences = compare(&baseline, &current).differences;
        let facts: Vec<SchemaFact> = differences.iter().map(|d| d.fact).collect();
        assert!(
            facts.contains(&SchemaFact::PropertyRemoved),
            "the property disappeared: {differences:?}"
        );
        assert!(
            facts.contains(&SchemaFact::RequiredRemoved),
            "and a requirement disappeared with it, which the removal must not erase: {differences:?}"
        );

        let requirement = differences
            .iter()
            .find(|difference| difference.fact == SchemaFact::RequiredRemoved)
            .unwrap();
        assert_eq!(requirement.detail.path, "required");
        assert_eq!(requirement.detail.before, Some(json!("id")));
    }

    #[test]
    fn an_optional_property_removal_does_not_claim_a_requirement_was_lost() {
        let baseline = input_schema(json!({ "note": { "type": "string" } }), json!([]));
        let current = input_schema(json!({}), json!([]));

        assert_eq!(facts(baseline, current), vec![SchemaFact::PropertyRemoved]);
    }

    #[test]
    fn a_named_change_beside_an_unnamed_one_is_not_reported_as_only_named() {
        // A property description is interpreted (MEDIUM); `examples` is not. The
        // change is both at once, and reporting only the interpreted half is the
        // under-reporting this pair exists to catch.
        let baseline = input_schema(
            json!({ "query": { "type": "string", "description": "A plain phrase." } }),
            json!(["query"]),
        );
        let mut current = input_schema(
            json!({ "query": { "type": "string", "description": "A search expression." } }),
            json!(["query"]),
        );
        current["examples"] = json!([{ "query": "postgres vector search" }]);

        assert_eq!(
            facts(baseline.clone(), current.clone()),
            vec![SchemaFact::DescriptionChanged]
        );
        assert!(
            unclassified(baseline, current),
            "the named change must not absorb the unnamed one"
        );
    }

    #[test]
    fn an_optional_property_addition_beside_an_unnamed_change_is_flagged() {
        let mut baseline = input_schema(json!({}), json!([]));
        baseline["title"] = json!("Search");
        let mut current = input_schema(json!({ "note": { "type": "string" } }), json!([]));
        current["title"] = json!("Repository search");

        assert_eq!(
            facts(baseline.clone(), current.clone()),
            vec![SchemaFact::OptionalPropertyAdded]
        );
        assert!(unclassified(baseline, current));
    }

    #[test]
    fn a_change_past_the_depth_bound_is_flagged_rather_than_dropped() {
        let baseline = nested(
            MAX_DEPTH + 2,
            json!({ "type": "string", "examples": ["a"] }),
        );
        let mut current = nested(
            MAX_DEPTH + 2,
            json!({ "type": "string", "examples": ["b"] }),
        );
        // A named change the walk does reach, next to one it cannot.
        current["description"] = json!("Root description.");

        assert_eq!(
            facts(baseline.clone(), current.clone()),
            vec![SchemaFact::DescriptionChanged]
        );
        assert!(unclassified(baseline, current));
    }
}
