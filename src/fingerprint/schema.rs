// SPDX-License-Identifier: MIT OR Apache-2.0

//! Semantic normalization for JSON Schemas.
//!
//! Rules applied (spec §7.3):
//!   1. `required` array order is not meaningful.
//!   2. `enum` array order is not meaningful.
//!   3. A missing root `$schema` means the 2020-12 dialect.
//!
//! The traversal descends only into positions JSON Schema defines as schemas.
//! `const`, `default`, and `examples` carry arbitrary *data* which may legitimately
//! contain keys named `required` or `enum`; sorting inside those would silently
//! discard a real difference.

use serde_json::Value;

use crate::error::Result;
use crate::fingerprint::canonical;

const DEFAULT_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// Keywords whose value is a single subschema.
const SUBSCHEMA_SINGLE: &[&str] = &[
    "additionalItems",
    "additionalProperties",
    "contains",
    "else",
    "if",
    "items",
    "not",
    "propertyNames",
    "then",
    "unevaluatedItems",
    "unevaluatedProperties",
];

/// Keywords whose value is a map of names to subschemas.
const SUBSCHEMA_MAP: &[&str] = &[
    "$defs",
    "definitions",
    "dependencies",
    "dependentSchemas",
    "patternProperties",
    "properties",
];

/// Keywords whose value is a list of subschemas.
const SUBSCHEMA_LIST: &[&str] = &["allOf", "anyOf", "oneOf", "prefixItems"];

/// Canonical bytes for a JSON Schema document.
pub fn canonical_schema(schema: &Value) -> Result<Vec<u8>> {
    let mut value = schema.clone();
    if let Value::Object(map) = &mut value {
        map.entry("$schema")
            .or_insert_with(|| Value::String(DEFAULT_DIALECT.to_string()));
    }
    normalize_schema(&mut value);
    canonical::to_vec(&value)
}

fn normalize_schema(node: &mut Value) {
    let Value::Object(map) = node else {
        return;
    };

    if let Some(Value::Array(items)) = map.get_mut("required") {
        items.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    }
    if let Some(Value::Array(items)) = map.get_mut("enum") {
        items.sort_by_cached_key(|item| item.to_string());
    }

    for keyword in SUBSCHEMA_SINGLE {
        if let Some(child) = map.get_mut(*keyword) {
            match child {
                // Draft-07 allows `items` to be a list of subschemas.
                Value::Array(items) => items.iter_mut().for_each(normalize_schema),
                other => normalize_schema(other),
            }
        }
    }

    for keyword in SUBSCHEMA_MAP {
        if let Some(Value::Object(children)) = map.get_mut(*keyword) {
            for child in children.values_mut() {
                // Draft-07 `dependencies` may map a name to a string array
                // (property dependency) rather than to a schema.
                if child.is_object() {
                    normalize_schema(child);
                }
            }
        }
    }

    for keyword in SUBSCHEMA_LIST {
        if let Some(Value::Array(items)) = map.get_mut(*keyword) {
            items.iter_mut().for_each(normalize_schema);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn canon(v: serde_json::Value) -> String {
        String::from_utf8(canonical_schema(&v).unwrap()).unwrap()
    }

    #[test]
    fn required_order_is_not_a_change() {
        assert_eq!(
            canon(json!({"type":"object","required":["b","a"]})),
            canon(json!({"type":"object","required":["a","b"]}))
        );
    }

    #[test]
    fn enum_order_is_not_a_change() {
        assert_eq!(
            canon(json!({"enum":["b","a"]})),
            canon(json!({"enum":["a","b"]}))
        );
    }

    #[test]
    fn nested_subschema_required_order_is_not_a_change() {
        assert_eq!(
            canon(json!({"properties":{"x":{"required":["b","a"]}}})),
            canon(json!({"properties":{"x":{"required":["a","b"]}}}))
        );
    }

    #[test]
    fn subschemas_inside_combinators_are_normalized() {
        assert_eq!(
            canon(json!({"anyOf":[{"required":["b","a"]}]})),
            canon(json!({"anyOf":[{"required":["a","b"]}]}))
        );
    }

    #[test]
    fn subschemas_inside_a_list_form_items_keyword_are_normalized() {
        // Draft-07 allows `items` to be a list of subschemas.
        assert_eq!(
            canon(json!({"items":[{"required":["b","a"]}]})),
            canon(json!({"items":[{"required":["a","b"]}]}))
        );
    }

    #[test]
    fn an_absent_schema_keyword_means_the_2020_12_dialect() {
        assert_eq!(
            canon(json!({"type":"object"})),
            canon(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object"
            }))
        );
    }

    #[test]
    fn a_different_dialect_is_a_real_difference() {
        assert_ne!(
            canon(json!({"type":"object"})),
            canon(json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object"
            }))
        );
    }

    #[test]
    fn required_inside_const_is_data_and_must_not_be_sorted() {
        assert_ne!(
            canon(json!({"const":{"required":["b","a"]}})),
            canon(json!({"const":{"required":["a","b"]}}))
        );
    }

    #[test]
    fn enum_inside_default_is_data_and_must_not_be_sorted() {
        assert_ne!(
            canon(json!({"default":{"enum":[2,1]}})),
            canon(json!({"default":{"enum":[1,2]}}))
        );
    }

    #[test]
    fn enum_arrays_inside_examples_are_data_and_must_not_be_sorted() {
        assert_ne!(
            canon(json!({"examples":[{"enum":[2,1]}]})),
            canon(json!({"examples":[{"enum":[1,2]}]}))
        );
    }

    #[test]
    fn a_behavior_relevant_description_change_is_never_normalized_away() {
        assert_ne!(
            canon(json!({"description":"query is a natural-language phrase"})),
            canon(json!({"description":"query is a search expression"}))
        );
    }

    #[test]
    fn a_required_field_addition_changes_the_digest() {
        assert_ne!(
            canon(json!({"type":"object","required":["a"]})),
            canon(json!({"type":"object","required":["a","b"]}))
        );
    }

    #[test]
    fn a_root_schema_that_is_not_an_object_is_handled() {
        // JSON Schema permits `true` and `false` as whole schemas.
        assert_eq!(canon(json!(true)), "true");
    }
}
