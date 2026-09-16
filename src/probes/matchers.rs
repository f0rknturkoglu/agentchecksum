// SPDX-License-Identifier: MIT OR Apache-2.0

//! Argument matchers, and the JSON Pointer that selects the value they judge.
//!
//! Three operators, and deliberately no more: `equals` for an exact value,
//! `contains` for a substring or a member, `one_of` for a small set. Anything richer
//! — a regex, a comparison — would be a second language inside the probe file, and
//! the brief fixes the vocabulary at three.
//!
//! Paths are RFC 6901 JSON Pointers. The one affordance is a shorthand: a key with no
//! leading `/` is a single top-level property, so `query` and `/query` mean the same
//! thing. Borrowing `/` for both keeps the common case short without inventing a
//! second path syntax with different escaping rules — `a.b` is the literal key
//! `a.b`, never two levels, because a dotted path would need its own escaping story
//! beside RFC 6901's `~0`/`~1`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One operator, and the value it was written with.
///
/// Serialized as the probe file spells it — `{"contains": "postgres"}` — because the
/// same shape is what the probe digest hashes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Matcher {
    Equals(Value),
    Contains(Value),
    OneOf(Vec<Value>),
}

impl Matcher {
    /// The operator's name, as configuration and reports spell it.
    pub fn operator(&self) -> &'static str {
        match self {
            Matcher::Equals(_) => "equals",
            Matcher::Contains(_) => "contains",
            Matcher::OneOf(_) => "one_of",
        }
    }

    /// Whether `actual` satisfies this matcher.
    ///
    /// A type mismatch is a failure, never an error: the model chose to answer with a
    /// string where a number was expected, and that is behavior worth measuring.
    pub fn matches(&self, actual: &Value) -> bool {
        match self {
            Matcher::Equals(expected) => actual == expected,
            Matcher::Contains(needle) => contains(actual, needle),
            Matcher::OneOf(allowed) => allowed.iter().any(|candidate| candidate == actual),
        }
    }

    /// The matcher as one line of prose, for a report.
    pub fn describe(&self) -> String {
        // A matcher is a small literal by construction, and `describe` runs once per
        // check in a report rather than in a loop over samples.
        let rendered = |value: &Value| {
            serde_json::to_string(value).unwrap_or_else(|_| "<unprintable>".to_string())
        };
        match self {
            Matcher::Equals(value) => format!("equals {}", rendered(value)),
            Matcher::Contains(value) => format!("contains {}", rendered(value)),
            Matcher::OneOf(values) => format!("one_of {}", rendered(&Value::Array(values.clone()))),
        }
    }
}

/// The two shapes `contains` understands.
///
/// A string actual is searched as a substring; an array actual is searched for an
/// element deep-equal to the matcher. An object actual is not supported — "contains
/// this key" and "contains this value" are different questions, and answering the
/// wrong one silently would be worse than failing.
fn contains(actual: &Value, needle: &Value) -> bool {
    match actual {
        Value::String(haystack) => needle
            .as_str()
            .is_some_and(|needle| haystack.contains(needle)),
        Value::Array(elements) => elements.iter().any(|element| element == needle),
        _ => false,
    }
}

/// The canonical JSON Pointer for a probe's argument key.
///
/// `None` means the key is not a usable pointer, which is a probe validation error
/// rather than an expectation failure: a typo in a path would otherwise read as a
/// model regression.
pub fn canonical_pointer(key: &str) -> Option<String> {
    if key.starts_with('/') {
        tokens(key)?;
        Some(key.to_string())
    } else {
        // One top-level property. A `/` or `~` here would be the start of a deeper
        // path or an escape, neither of which the shorthand claims to express.
        if key.is_empty() || key.contains('/') || key.contains('~') {
            return None;
        }
        Some(format!("/{key}"))
    }
}

/// Resolve a canonical pointer against a document.
///
/// `None` is a missing path: an expectation failure, not an error. A pointer that
/// resolves to nothing is exactly the case `equals` cannot pass and should not crash.
pub fn resolve_pointer<'a>(document: &'a Value, pointer: &str) -> Option<&'a Value> {
    let mut current = document;
    for token in tokens(pointer)? {
        current = match current {
            Value::Object(members) => members.get(&token)?,
            Value::Array(elements) => elements.get(index(&token)?)?,
            _ => return None,
        };
    }
    Some(current)
}

/// RFC 6901 tokens, unescaped. `None` for a malformed pointer.
fn tokens(pointer: &str) -> Option<Vec<String>> {
    let rest = pointer.strip_prefix('/')?;
    let mut tokens = Vec::new();

    for raw in rest.split('/') {
        let mut token = String::with_capacity(raw.len());
        let mut characters = raw.chars();
        while let Some(character) = characters.next() {
            if character == '~' {
                match characters.next() {
                    Some('0') => token.push('~'),
                    Some('1') => token.push('/'),
                    // `~` and anything else is malformed; RFC 6901 defines no
                    // other escape, and guessing would resolve a different value.
                    _ => return None,
                }
            } else {
                token.push(character);
            }
        }
        tokens.push(token);
    }

    Some(tokens)
}

/// An array index, in RFC 6901's grammar rather than Rust's integer parser.
///
/// Leading zeros and `+` are refused because `01` and `+1` name nothing in a JSON
/// array (Rust's parser accepts both), and `-` names the element after the last,
/// which never exists in a document being read.
fn index(token: &str) -> Option<usize> {
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if token.len() > 1 && token.starts_with('0') {
        return None;
    }
    token.parse::<usize>().ok()
}

/// An object of pointers to matchers, as a probe declares it.
pub type Matchers = std::collections::BTreeMap<String, Matcher>;

/// Every matcher in a declaration, as one readable phrase.
pub fn describe_all(matchers: &Matchers) -> String {
    if matchers.is_empty() {
        return "no matchers".to_string();
    }
    matchers
        .iter()
        .map(|(pointer, matcher)| format!("{pointer} {}", matcher.describe()))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// Whether a document satisfies every matcher.
///
/// A missing path fails the matcher it belongs to; the document is not modified and
/// nothing is defaulted, because a defaulted argument is not an argument the model
/// sent.
pub fn satisfies(document: &Value, matchers: &Matchers) -> bool {
    matchers.iter().all(|(pointer, matcher)| {
        resolve_pointer(document, pointer).is_some_and(|actual| matcher.matches(actual))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pointer(key: &str) -> String {
        canonical_pointer(key).unwrap_or_else(|| panic!("`{key}` should be a usable pointer"))
    }

    #[test]
    fn equals_is_structural_equality() {
        assert!(Matcher::Equals(json!(50)).matches(&json!(50)));
        assert!(Matcher::Equals(json!("postgres")).matches(&json!("postgres")));
        assert!(Matcher::Equals(json!({ "a": 1, "b": 2 })).matches(&json!({ "b": 2, "a": 1 })));
        assert!(Matcher::Equals(json!([1, 2])).matches(&json!([1, 2])));

        assert!(!Matcher::Equals(json!(50)).matches(&json!(51)));
        assert!(!Matcher::Equals(json!("50")).matches(&json!(50)));
        assert!(!Matcher::Equals(json!(true)).matches(&json!(1)));
        assert!(!Matcher::Equals(json!({ "a": 1 })).matches(&json!({ "a": 1, "b": 2 })));
    }

    #[test]
    fn contains_searches_a_string_actual_as_a_substring() {
        let matcher = Matcher::Contains(json!("postgres"));
        assert!(matcher.matches(&json!("postgres vector search")));
        assert!(matcher.matches(&json!("postgres")));
        assert!(!matcher.matches(&json!("mysql")));
        // Case matters: a case-insensitive search would be a different operator.
        assert!(!matcher.matches(&json!("Postgres")));
    }

    #[test]
    fn contains_searches_an_array_actual_for_a_deep_equal_element() {
        assert!(Matcher::Contains(json!("b")).matches(&json!(["a", "b"])));
        assert!(Matcher::Contains(json!({ "k": 1 })).matches(&json!([{ "k": 1 }, { "k": 2 }])));
        assert!(Matcher::Contains(json!([1, 2])).matches(&json!([[1, 2], [3]])));
        assert!(!Matcher::Contains(json!("c")).matches(&json!(["a", "b"])));
        assert!(!Matcher::Contains(json!({ "k": 1 })).matches(&json!([{ "k": 1, "j": 2 }])));
    }

    #[test]
    fn contains_refuses_the_shapes_it_does_not_define() {
        // An object actual has no single meaning: "contains this key" and "contains
        // this value" are different questions, so neither is answered.
        assert!(!Matcher::Contains(json!({ "k": 1 })).matches(&json!({ "k": 1 })));
        // A string actual with a non-string matcher is a type mismatch.
        assert!(!Matcher::Contains(json!(5)).matches(&json!("query=5")));
        assert!(!Matcher::Contains(json!(["a"])).matches(&json!(["a"])));
        // Numbers, booleans and null have no membership or substring notion here.
        assert!(!Matcher::Contains(json!(1)).matches(&json!(123)));
        assert!(!Matcher::Contains(json!(true)).matches(&json!(true)));
        assert!(!Matcher::Contains(json!("x")).matches(&json!(null)));
    }

    #[test]
    fn one_of_passes_when_any_element_deep_equals_the_actual() {
        let matcher = Matcher::OneOf(vec![json!(10), json!(50)]);
        assert!(matcher.matches(&json!(50)));
        assert!(matcher.matches(&json!(10)));
        assert!(!matcher.matches(&json!(20)));
        assert!(!matcher.matches(&json!("50")));

        let objects = Matcher::OneOf(vec![json!({ "query": "a" }), json!({ "query": "b" })]);
        assert!(objects.matches(&json!({ "query": "b" })));
        assert!(!objects.matches(&json!({ "query": "c" })));
    }

    #[test]
    fn a_top_level_key_is_the_same_pointer_as_its_slashed_form() {
        assert_eq!(pointer("query"), "/query");
        assert_eq!(pointer("/query"), "/query");
        assert_eq!(pointer("/a/b"), "/a/b");
        // A dot is an ordinary character in a JSON key, never a level separator.
        assert_eq!(pointer("a.b"), "/a.b");
        // The empty pointer is the whole document in RFC 6901, which is not what a
        // probe's argument key can mean.
        assert!(canonical_pointer("").is_none());
    }

    #[test]
    fn a_malformed_pointer_is_refused_rather_than_guessed_at() {
        // `foo/bar` is neither a top-level key nor an absolute pointer.
        assert!(canonical_pointer("foo/bar").is_none());
        // `~` is only an escape for `~0` and `~1`.
        assert!(canonical_pointer("/a~2b").is_none());
        assert!(canonical_pointer("/a~").is_none());
        assert!(canonical_pointer("a~0b").is_none());
    }

    #[test]
    fn a_pointer_resolves_objects_arrays_and_escapes() {
        let document = json!({
            "query": "postgres",
            "options": { "per_page": 50, "tags": ["a", "b"] },
            "results": [{ "name": "one" }],
            "a/b": { "~key": 7 },
            "": "empty key"
        });

        assert_eq!(
            resolve_pointer(&document, "/query"),
            Some(&json!("postgres"))
        );
        assert_eq!(
            resolve_pointer(&document, "/options/per_page"),
            Some(&json!(50))
        );
        assert_eq!(
            resolve_pointer(&document, "/options/tags/1"),
            Some(&json!("b"))
        );
        assert_eq!(
            resolve_pointer(&document, "/results/0/name"),
            Some(&json!("one"))
        );
        // `~1` is `/` and `~0` is `~`.
        assert_eq!(resolve_pointer(&document, "/a~1b/~0key"), Some(&json!(7)));
        // `/` addresses the member whose name is empty.
        assert_eq!(resolve_pointer(&document, "/"), Some(&json!("empty key")));
    }

    #[test]
    fn a_missing_path_resolves_to_nothing_without_an_error() {
        let document = json!({ "options": { "tags": ["a"] } });

        assert!(resolve_pointer(&document, "/absent").is_none());
        assert!(resolve_pointer(&document, "/options/absent").is_none());
        assert!(resolve_pointer(&document, "/options/tags/1").is_none());
        // RFC 6901's `-` names the element after the last, which never exists.
        assert!(resolve_pointer(&document, "/options/tags/-").is_none());
        // Leading zeros and a sign are not indices in the RFC's grammar.
        assert!(resolve_pointer(&document, "/options/tags/01").is_none());
        assert!(resolve_pointer(&document, "/options/tags/+0").is_none());
        // Walking into a scalar is a missing path, not a panic.
        assert!(resolve_pointer(&document, "/options/per_page/deeper").is_none());
        assert!(resolve_pointer(&json!(null), "/query").is_none());
        assert!(resolve_pointer(&json!([1]), "/0").is_some());
    }

    #[test]
    fn every_matcher_of_a_document_must_hold() {
        let matchers = Matchers::from([
            (pointer("query"), Matcher::Contains(json!("postgres"))),
            (pointer("/per_page"), Matcher::Equals(json!(50))),
        ]);

        assert!(satisfies(
            &json!({ "query": "postgres vector", "per_page": 50 }),
            &matchers
        ));
        assert!(!satisfies(
            &json!({ "query": "postgres vector", "per_page": 25 }),
            &matchers
        ));
        // A missing path fails the matcher that named it.
        assert!(!satisfies(
            &json!({ "query": "postgres vector" }),
            &matchers
        ));
        assert!(satisfies(&json!({ "anything": 1 }), &Matchers::new()));
    }

    #[test]
    fn a_matcher_serializes_as_the_probe_file_spells_it() {
        assert_eq!(
            serde_json::to_value(Matcher::Contains(json!(["postgres"]))).unwrap(),
            json!({ "contains": ["postgres"] })
        );
        assert_eq!(
            serde_json::to_value(Matcher::OneOf(vec![json!(1), json!(2)])).unwrap(),
            json!({ "one_of": [1, 2] })
        );
        assert_eq!(
            serde_json::to_value(Matcher::Equals(json!({ "a": 1 }))).unwrap(),
            json!({ "equals": { "a": 1 } })
        );
    }

    #[test]
    fn describing_a_matcher_set_is_stable_and_readable() {
        let matchers = Matchers::from([
            (pointer("/z"), Matcher::Equals(json!(1))),
            (pointer("a"), Matcher::Contains(json!("x"))),
        ]);

        assert_eq!(
            describe_all(&matchers),
            "/a contains \"x\" and /z equals 1",
            "BTreeMap order, so a report does not shuffle between runs"
        );
        assert_eq!(describe_all(&Matchers::new()), "no matchers");
        assert_eq!(Matcher::OneOf(vec![json!(1)]).describe(), "one_of [1]");
    }
}
