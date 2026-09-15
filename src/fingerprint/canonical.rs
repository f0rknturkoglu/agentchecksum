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

    /// Field declaration order is fixed by the type, so this value reaches the
    /// serializer as `z, a` no matter how `serde_json` backs its maps. A
    /// `json!({...})` literal cannot test key sorting: without the
    /// `preserve_order` feature a serde_json map is a BTreeMap, so the literal is
    /// already sorted and `to_vec(&a) == to_vec(&b)` degenerates to `f(x) == f(x)`.
    #[derive(serde::Serialize)]
    struct OutOfOrder {
        z: u8,
        a: u8,
    }

    #[derive(serde::Serialize)]
    struct Inner {
        y: u8,
        b: u8,
    }

    #[derive(serde::Serialize)]
    struct Outer {
        z: u8,
        a: Inner,
    }

    #[derive(serde::Serialize)]
    struct Nested {
        list: Vec<u8>,
    }

    #[test]
    fn object_keys_are_sorted_into_canonical_order() {
        let value = OutOfOrder { z: 1, a: 2 };
        assert_eq!(
            String::from_utf8(to_vec(&value).unwrap()).unwrap(),
            r#"{"a":2,"z":1}"#
        );
    }

    #[test]
    fn nested_object_keys_are_sorted_recursively() {
        let value = Outer {
            z: 1,
            a: Inner { y: 1, b: 2 },
        };
        assert_eq!(
            String::from_utf8(to_vec(&value).unwrap()).unwrap(),
            r#"{"a":{"b":2,"y":1},"z":1}"#
        );
    }

    #[test]
    fn canonical_output_carries_no_insignificant_whitespace() {
        let value = Nested {
            list: vec![1, 2, 3],
        };
        assert_eq!(
            String::from_utf8(to_vec(&value).unwrap()).unwrap(),
            r#"{"list":[1,2,3]}"#
        );
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
