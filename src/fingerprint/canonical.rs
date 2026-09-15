// SPDX-License-Identifier: MIT OR Apache-2.0

//! RFC 8785 JSON Canonicalization Scheme (JCS).
//!
//! Key ordering, whitespace, and number formatting are settled by the RFC rather
//! than by us, so two semantically identical documents can never produce two
//! different digests for those reasons.

use serde::Serialize;

use crate::error::{Error, Result};

/// Serialize to JCS bytes.
pub fn to_vec<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json_canonicalizer::to_vec(value).map_err(|source| Error::Json { source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn object_key_order_does_not_affect_canonical_bytes() {
        let a = json!({ "b": 1, "a": 2 });
        let b = json!({ "a": 2, "b": 1 });
        assert_eq!(to_vec(&a).unwrap(), to_vec(&b).unwrap());
        assert_eq!(
            String::from_utf8(to_vec(&a).unwrap()).unwrap(),
            r#"{"a":2,"b":1}"#
        );
    }

    #[test]
    fn nested_object_keys_are_sorted_recursively() {
        let value = json!({ "outer": { "z": 1, "a": { "y": 1, "b": 2 } } });
        assert_eq!(
            String::from_utf8(to_vec(&value).unwrap()).unwrap(),
            r#"{"outer":{"a":{"b":2,"y":1},"z":1}}"#
        );
    }

    #[test]
    fn whitespace_in_the_source_text_cannot_influence_the_digest() {
        let compact: serde_json::Value = serde_json::from_str(r#"{"a":[1,2,3]}"#).unwrap();
        let spaced: serde_json::Value =
            serde_json::from_str("{\n  \"a\": [ 1, 2,\n 3 ]\n}").unwrap();
        assert_eq!(to_vec(&compact).unwrap(), to_vec(&spaced).unwrap());
    }

    #[test]
    fn integer_and_equivalent_float_canonicalize_identically() {
        // RFC 8785 uses ECMAScript number serialization, where 1.0 renders as 1.
        // If this test fails, the canonicalizer deviates from the RFC and the
        // deviation must be handled explicitly rather than papered over.
        let as_int = json!({ "a": 1 });
        let as_float: serde_json::Value = serde_json::from_str(r#"{"a":1.0}"#).unwrap();
        assert_eq!(to_vec(&as_int).unwrap(), to_vec(&as_float).unwrap());
    }
}
