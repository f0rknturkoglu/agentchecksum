# Phase 1 — Fingerprint Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `agentchecksum init` and `agentchecksum snapshot` discover Model and Prompt dependencies and write a
byte-deterministic `agentchecksum.lock`.

**Architecture:** A single crate with a thin binary over a library. Every domain decision is a pure function over
already-fetched data; all I/O (files, HTTP) lives in `discovery/` and is called only from command handlers. Digests are
SHA-256 over RFC 8785 canonical bytes, so ordering and formatting can never influence a checksum.

**Tech Stack:** Rust 1.98.1 (edition 2024), `clap` 4.6 (derive), `serde`/`serde_json`, `serde_json_canonicalizer` 0.3
(RFC 8785 JCS), `sha2` 0.11, `toml` 1.1, `reqwest` 0.13, `tokio` 1, `thiserror` 2, `tracing` 0.1. Dev: `assert_cmd`,
`insta`, `tempfile`, `wiremock`.

**Spec:** `docs/specs/2026-09-15-agentchecksum-design.md`

## Global Constraints

- Single crate `agentchecksum`, single binary `agentchecksum`. No workspace, no second crate.
- `edition = "2024"`, `rust-version = "1.98"`, toolchain pinned to `1.98.1` in `rust-toolchain.toml`.
- License `MIT OR Apache-2.0`. **Every** `.rs` file begins with `// SPDX-License-Identifier: MIT OR Apache-2.0`.
- No `unsafe`. No trait/dynamic dispatch unless a second real consumer exists. No macro magic.
- No `unwrap()`/`expect()`/`panic!()` on production paths.
- Digest format is `sha256:<64 lowercase hex>`; agent checksum format is `ac1:<64 lowercase hex>`.
- Canonicalization is RFC 8785 via `serde_json_canonicalizer`. Never hand-roll a JSON serializer.
- The agent checksum is a function of dependency inputs **only** — never of lockfile serialization.
- Nothing hashed or committed may contain a timestamp, an absolute path, a machine identifier, or a discovery order.
- Human output → stdout. All diagnostics go through `tracing` → stderr.
- Exit codes: `0` success, `1` gate failure (unused this phase), `2` usage error (clap), `3` runtime error.
- `serverInfo.name` is never used as an identity. Identity comes from config.
- Every dependency declared in `Cargo.toml` must be exercised by a task in this plan.

---

### Task 1: Bootstrap the crate, the hashing primitive, and CI

**Files:**
- Create: `Cargo.toml`
- Create: `src/lib.rs`
- Create: `src/main.rs`
- Create: `src/error.rs`
- Create: `src/fingerprint/mod.rs`
- Create: `src/fingerprint/digest.rs`
- Create: `.github/workflows/ci.yml`
- Test: `tests/cli_basics.rs`, plus an in-file `#[cfg(test)]` module in `digest.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `fingerprint::digest::hex(&[u8]) -> String`, `fingerprint::digest::sha256_hex(&[u8]) -> String`,
  `error::Error`, `error::Result<T>`.

- [ ] **Step 1: Write `Cargo.toml`**

Every dependency carries its justification from the spec's dependency table, and every one is exercised by a task below.

```toml
# SPDX-License-Identifier: MIT OR Apache-2.0
[package]
name = "agentchecksum"
version = "0.1.0"
edition = "2024"
rust-version = "1.98"
license = "MIT OR Apache-2.0"
description = "Language-agnostic dependency fingerprint and behavioral regression gate for AI agents"
repository = "https://github.com/f0rknturkoglu/agentchecksum"

[dependencies]
# CLI parsing; hand-rolling argument handling is unjustifiable.
clap = { version = "4.6", features = ["derive"] }
# Data model and lockfile.
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
# RFC 8785 JCS: a specification instead of a hand-rolled canonicalizer.
serde_json_canonicalizer = "0.3"
# SHA-256, matching Ollama's model-digest alphabet.
sha2 = "0.11"
# Structured errors with typed fields, for the diagnostic format in spec §14.
thiserror = "2.0"
# Config and probes: one configuration language.
toml = "1.1"
# Diagnostics on stderr, keeping stdout machine-clean.
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
# Ollama metadata discovery (spec §7.6). Default features already select rustls.
reqwest = { version = "0.13", features = ["json"] }
# Required by reqwest; the MCP client joins in a later phase.
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }

[dev-dependencies]
assert_cmd = "2.2"
insta = { version = "1.48", features = ["json"] }
tempfile = "3.27"
wiremock = "0.6"
# Listed explicitly so integration tests do not rely on an implicit edge.
serde_json = "1.0"

[profile.release]
lto = true
strip = true
codegen-units = 1
```

Note on `reqwest` TLS: the feature is named `rustls` (not `rustls-tls`), and it is already enabled through the default
`default-tls` feature. Defaults are kept for robustness; trimming to
`default-features = false, features = ["json", "rustls"]` is a later size optimization, not a Phase 1 concern.

- [ ] **Step 2: Write the failing test for the hashing primitive**

Append to `src/fingerprint/digest.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encodes_lowercase_zero_padded_bytes() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }

    #[test]
    fn hex_encodes_empty_input_as_empty_string() {
        assert_eq!(hex(&[]), "");
    }

    #[test]
    fn sha256_matches_the_published_vector_for_hello_world() {
        // Vector from the RustCrypto sha2 crate documentation.
        assert_eq!(
            sha256_hex(b"hello world"),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn sha256_of_empty_input_matches_the_published_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --lib fingerprint::digest`
Expected: FAIL — compile error, `cannot find function hex in this scope`.

- [ ] **Step 4: Write the minimal implementation**

`src/fingerprint/digest.rs` (above the test module):

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Byte-level digests. Hex encoding is written out rather than pulled in as a
//! dependency: it is nine lines, it cannot fail, and it keeps the crate's
//! dependency set limited to things that actually need a crate.

use sha2::{Digest as _, Sha256};

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Lowercase hex encoding, two characters per byte.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX_DIGITS[(byte >> 4) as usize] as char);
        out.push(HEX_DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// SHA-256 as lowercase hex, without an algorithm prefix.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}
```

`src/fingerprint/mod.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Normalization, canonicalization, and digests.

pub mod digest;
```

- [ ] **Step 5: Write `src/error.rs`**

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Structured errors. The CLI boundary renders `Display` plus the optional
//! `suggestion()` as the what-failed / how-to-fix diagnostic from spec §14.

use std::path::PathBuf;
use thiserror::Error;

/// The project rule `rs-result-type` mandates the defaulted-parameter form.
/// `Result<T>` still resolves to `std::result::Result<T, Error>`, so callers see
/// the interface the plan specifies.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to read `{path}`")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write `{path}`")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    /// An actionable next step, printed under a `Suggested action:` heading.
    pub fn suggestion(&self) -> Option<String> {
        match self {
            Error::Read { .. } => Some("Check that the path exists and is readable.".to_string()),
            Error::Write { .. } => Some("Check directory permissions.".to_string()),
        }
    }
}
```

- [ ] **Step 6: Write `src/lib.rs` and `src/main.rs`**

`src/lib.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AgentChecksum: a language-agnostic dependency fingerprint and behavioral
//! regression gate for AI agents.

pub mod error;
pub mod fingerprint;
```

`src/main.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::process::ExitCode;

use clap::Parser;

/// Subcommands arrive with the code that implements them; there are no stubs.
#[derive(Debug, Parser)]
#[command(
    name = "agentchecksum",
    version,
    about = "Language-agnostic dependency fingerprint and behavioral regression gate for AI agents"
)]
struct Cli {}

fn main() -> ExitCode {
    let _cli = Cli::parse();
    ExitCode::SUCCESS
}
```

- [ ] **Step 7: Write the CLI smoke test**

`tests/cli_basics.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

use assert_cmd::Command;

#[test]
fn version_flag_prints_the_crate_version_and_exits_zero() {
    let output = Command::cargo_bin("agentchecksum")
        .unwrap()
        .arg("--version")
        .output()
        .unwrap();

    assert!(output.status.success(), "expected exit 0");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "version output was {stdout:?}"
    );
}

#[test]
fn unknown_flag_exits_with_code_2() {
    let output = Command::cargo_bin("agentchecksum")
        .unwrap()
        .arg("--definitely-not-a-flag")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2), "usage errors exit 2");
}
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test --all-targets`
Expected: PASS.

- [ ] **Step 9: Write `.github/workflows/ci.yml`**

Rust is installed through the runner's preinstalled `rustup` rather than a third-party action, so the pinned
`rust-toolchain.toml` is what actually selects the compiler.

```yaml
# SPDX-License-Identifier: MIT OR Apache-2.0
name: CI

on:
  push:
    branches: [main]
  pull_request:

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
      - name: Install pinned toolchain
        run: rustup toolchain install 1.98.1 --component rustfmt --component clippy
      - name: Format
        run: cargo fmt --all --check
      - name: Lint
        run: cargo clippy --all-targets -- -D warnings
      - name: Test
        run: cargo test --all-targets
```

- [ ] **Step 10: Verify the full local gate**

Run: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets`
Expected: all three succeed with no warnings.

- [ ] **Step 11: Commit**

```bash
git add Cargo.toml Cargo.lock src tests .github
git commit -m "Bootstrap crate, hashing primitive, and CI"
```

---

### Task 2: Canonicalization — RFC 8785 and text normalization

**Files:**
- Create: `src/fingerprint/canonical.rs`
- Create: `src/fingerprint/normalize.rs`
- Modify: `src/fingerprint/mod.rs`
- Modify: `src/error.rs` (add `Json`)
- Test: in-file `#[cfg(test)]` modules

**Interfaces:**
- Consumes: `error::{Error, Result}`.
- Produces: `fingerprint::canonical::to_vec<T: Serialize>(&T) -> Result<Vec<u8>>`,
  `fingerprint::normalize::normalize_text(&str) -> String`, `fingerprint::normalize::shape_text(&str) -> String`.

- [ ] **Step 1: Write the failing tests**

Append to `src/fingerprint/canonical.rs`:

```rust
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
        let value = Nested { list: vec![1, 2, 3] };
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
```

Append to `src/fingerprint/normalize.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_and_lf_produce_the_same_normalized_text() {
        assert_eq!(normalize_text("a\r\nb\r\n"), normalize_text("a\nb\n"));
    }

    #[test]
    fn trailing_whitespace_on_a_line_is_insignificant() {
        assert_eq!(normalize_text("a   \nb\t\n"), normalize_text("a\nb\n"));
    }

    #[test]
    fn a_leading_byte_order_mark_is_stripped() {
        assert_eq!(normalize_text("\u{feff}a"), "a");
    }

    #[test]
    fn surrounding_blank_lines_are_insignificant() {
        assert_eq!(normalize_text("\n\n  \na\nb\n\n"), "a\nb");
    }

    #[test]
    fn a_formatting_only_change_keeps_the_same_shape_but_changes_the_content() {
        let v1 = "Summarize   the repository.\n\nBe concise.";
        let v2 = "Summarize the repository.\n\nBe concise.\n";
        assert_eq!(shape_text(v1), shape_text(v2));
        assert_ne!(normalize_text(v1), normalize_text(v2));
    }

    #[test]
    fn shape_collapses_interior_whitespace_while_content_keeps_it() {
        assert_eq!(shape_text("a\n\nb"), "a b");
        assert_eq!(normalize_text("a\n\nb"), "a\n\nb");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib fingerprint`
Expected: FAIL — `cannot find function to_vec` / `normalize_text`.

- [ ] **Step 3: Add the `Json` error variant**

In `src/error.rs`, add to `enum Error`:

```rust
    #[error("failed to serialize a value to JSON")]
    Json {
        #[source]
        source: serde_json::Error,
    },
```

and extend `suggestion()`:

```rust
            Error::Json { .. } => Some(
                "This is a bug in agentchecksum; please report it with the input that triggered it."
                    .to_string(),
            ),
```

The variant is named `Json` rather than `Canonicalize` because it is also returned when writing the lockfile, which
is plain JSON rather than JCS.

- [ ] **Step 4: Write the minimal implementation**

`src/fingerprint/canonical.rs`:

```rust
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
```

`src/fingerprint/normalize.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Text normalization for prompts and tool descriptions.
//!
//! Two different normalizations are needed.
//!
//! `normalize_text` is what gets hashed. It removes the byte-level differences a
//! human would not call a change: a leading BOM, line endings, trailing
//! whitespace on each line, and surrounding blank lines. It deliberately keeps
//! interior whitespace runs.
//!
//! `shape_text` additionally collapses every whitespace run to a single space. It
//! exists only so that a formatting-only edit can be told apart from a semantic
//! one without asking a model.

/// Strip a BOM and normalize line endings.
fn unify(raw: &str) -> String {
    raw.strip_prefix('\u{feff}')
        .unwrap_or(raw)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// Normalize text for hashing: line endings unified, trailing whitespace per
/// line removed, leading and trailing blank lines removed.
pub fn normalize_text(raw: &str) -> String {
    let unified = unify(raw);
    let mut lines: Vec<&str> = unified.lines().map(str::trim_end).collect();
    while lines.first().is_some_and(|line| line.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Collapse every whitespace run to a single space. Used to detect that a
/// change touched only formatting.
pub fn shape_text(raw: &str) -> String {
    unify(raw).split_whitespace().collect::<Vec<_>>().join(" ")
}
```

`src/fingerprint/mod.rs` becomes:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Normalization, canonicalization, and digests.

pub mod canonical;
pub mod digest;
pub mod normalize;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib fingerprint`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src
git commit -m "Add RFC 8785 canonicalization and text normalization"
```

---

### Task 3: Schema-aware semantic normalization rules

**Files:**
- Create: `src/fingerprint/schema.rs`
- Modify: `src/fingerprint/mod.rs`
- Test: in-file `#[cfg(test)]` module

**Interfaces:**
- Consumes: `fingerprint::canonical::to_vec`.
- Produces: `fingerprint::schema::canonical_schema(&serde_json::Value) -> Result<Vec<u8>>`.

**Why this is not a naive recursive walk.** Sorting every `required`/`enum` array found anywhere would corrupt data:
`{"const": {"required": ["b","a"]}}` and `{"examples": [{"enum": [2,1]}]}` contain those keys as *data*, not as schema
keywords. The traversal therefore descends only through positions the JSON Schema specification defines as schemas, which
also means the traversal has no "am I in a schema" flag to get wrong: every node it visits *is* a schema.

- [ ] **Step 1: Write the failing tests**

Append to `src/fingerprint/schema.rs`:

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib fingerprint::schema`
Expected: FAIL — `cannot find function canonical_schema`.

- [ ] **Step 3: Write the minimal implementation**

`src/fingerprint/schema.rs`:

```rust
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
```

Add `pub mod schema;` to `src/fingerprint/mod.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib fingerprint::schema`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src
git commit -m "Add schema-aware semantic normalization rules"
```

---

### Task 4: Configuration parsing

**Files:**
- Create: `src/config.rs`
- Modify: `src/lib.rs`, `src/error.rs`
- Test: in-file `#[cfg(test)]` module

**Interfaces:**
- Consumes: `error::{Error, Result}`.
- Produces: `config::{Config, AgentConfig, ModelConfig, PromptConfig, McpConfig, McpServerConfig, Transport, ProbesConfig, PolicyConfig, MetricPolicy, RiskLevel}`,
  `Config::load(&Path) -> Result<Config>`, `Config::from_toml_at(&str, &Path) -> Result<Config>`,
  `Config::validate(&self) -> Result<()>`, `Config::root_for(&Path) -> PathBuf`.

**Design note — strictness and scope.** Config parsing is strict (`deny_unknown_fields`) so a typo like `[modell]` is an
error instead of silently ignored configuration. The structs cover the *whole* documented config surface, including
sections later phases consume, so the config in spec §5 parses today. That is typed config surface, not dead code.

- [ ] **Step 1: Write the failing tests**

Append to `src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config> {
        Config::from_toml_at(text, Path::new("agentchecksum.toml"))
    }

    const MINIMAL: &str = r#"
version = 1

[agent]
name = "research-agent"

[[prompts]]
path = "prompts/system.md"
"#;

    #[test]
    fn a_minimal_config_parses() {
        let config = parse(MINIMAL).unwrap();
        assert_eq!(config.version, 1);
        assert_eq!(config.agent.name, "research-agent");
        assert_eq!(config.prompts.len(), 1);
        assert_eq!(config.probes.path, "probes");
        assert_eq!(config.probes.repeat, None);
    }

    #[test]
    fn a_typo_in_a_table_name_is_an_error_not_a_silent_ignore() {
        let text = MINIMAL.replace("[agent]", "[agentt]");
        assert!(parse(&text).is_err(), "unknown fields must be rejected");
    }

    #[test]
    fn a_typo_in_a_field_name_is_an_error() {
        let text = MINIMAL.replace("name = ", "naem = ");
        assert!(parse(&text).is_err());
    }

    #[test]
    fn an_unsupported_version_is_an_error() {
        let text = MINIMAL.replace("version = 1", "version = 2");
        let err = parse(&text).unwrap_err();
        assert!(
            matches!(err, Error::ConfigVersion { found: 2, supported: 1 }),
            "{err:?}"
        );
    }

    #[test]
    fn a_missing_version_is_an_error() {
        assert!(parse(&MINIMAL.replace("version = 1", "")).is_err());
    }

    #[test]
    fn the_full_documented_config_surface_parses() {
        let text = format!(
            r#"{MINIMAL}
[model]
provider = "ollama"
id = "qwen3:8b"
endpoint = "http://localhost:11434"
params = {{ temperature = 0.0, seed = 42 }}

[probes]
path = "probes"
repeat = 3

[[mcp.servers]]
name = "github"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[policy]
fail_on_risk = "critical"

[policy.metrics.tool_selection]
min = 0.95
"#
        );
        let config = parse(&text).unwrap();
        let model = config.model.as_ref().unwrap();
        assert_eq!(model.id, "qwen3:8b");
        assert_eq!(model.params.len(), 2);
        assert_eq!(config.probes.repeat, Some(3));
        assert_eq!(config.mcp.servers[0].name, "github");
        assert_eq!(config.mcp.servers[0].transport, Transport::Stdio);
        assert_eq!(config.policy.fail_on_risk, Some(RiskLevel::Critical));
        assert_eq!(config.policy.metrics["tool_selection"].min, Some(0.95));
    }

    #[test]
    fn a_stdio_server_without_a_command_is_rejected() {
        let text = format!(
            r#"{MINIMAL}

[[mcp.servers]]
name = "github"
transport = "stdio"
"#
        );
        let err = parse(&text).unwrap_err();
        assert!(matches!(err, Error::ConfigInvalid { .. }), "{err:?}");
    }

    #[test]
    fn an_http_server_without_a_url_is_rejected() {
        let text = format!(
            r#"{MINIMAL}

[[mcp.servers]]
name = "remote"
transport = "streamable-http"
"#
        );
        let err = parse(&text).unwrap_err();
        assert!(matches!(err, Error::ConfigInvalid { .. }), "{err:?}");
    }

    #[test]
    fn duplicate_mcp_server_names_are_rejected() {
        let text = format!(
            r#"{MINIMAL}

[[mcp.servers]]
name = "github"
transport = "stdio"
command = "a"

[[mcp.servers]]
name = "github"
transport = "stdio"
command = "b"
"#
        );
        let err = parse(&text).unwrap_err();
        assert!(matches!(err, Error::ConfigInvalid { .. }), "{err:?}");
    }

    #[test]
    fn duplicate_prompt_paths_are_rejected_because_they_would_collide_in_the_lockfile() {
        let text = format!(
            r#"{MINIMAL}

[[prompts]]
path = "prompts/system.md"
"#
        );
        let err = parse(&text).unwrap_err();
        assert!(matches!(err, Error::DependencyCollision { .. }), "{err:?}");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib config`
Expected: FAIL — `cannot find type Config`.

- [ ] **Step 3: Add the config error variants**

In `src/error.rs`, add:

```rust
    #[error("invalid configuration in `{path}`")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("invalid configuration: {reason}")]
    ConfigInvalid { reason: String },

    #[error("unsupported config version {found}; this build supports version {supported}")]
    ConfigVersion { found: u32, supported: u32 },

    #[error("dependency id collision: `{id}` is declared more than once")]
    DependencyCollision { id: String },
```

and extend `suggestion()`:

```rust
            Error::ConfigParse { .. } => Some(
                "Fix the reported key. Unknown keys are rejected so a typo cannot be silently ignored."
                    .to_string(),
            ),
            Error::ConfigInvalid { .. } => None,
            Error::ConfigVersion { .. } => {
                Some("Upgrade agentchecksum, or set `version` to a supported value.".to_string())
            }
            Error::DependencyCollision { .. } => None,
```

- [ ] **Step 4: Write the implementation**

`src/config.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `agentchecksum.toml`: user-owned configuration.
//!
//! Parsing is strict. A silently ignored typo means AgentChecksum fingerprinted
//! something other than what the user believes they declared.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The config format version this build understands.
pub const SUPPORTED_CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub agent: AgentConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelConfig>,
    #[serde(default)]
    pub prompts: Vec<PromptConfig>,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub probes: ProbesConfig,
    #[serde(default)]
    pub policy: PolicyConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub provider: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub params: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptConfig {
    pub path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: Transport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    Stdio,
    StreamableHttp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbesConfig {
    #[serde(default = "default_probes_path")]
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<u32>,
}

impl Default for ProbesConfig {
    fn default() -> Self {
        Self {
            path: default_probes_path(),
            repeat: None,
        }
    }
}

fn default_probes_path() -> String {
    "probes".to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_on_risk: Option<RiskLevel>,
    #[serde(default)]
    pub metrics: BTreeMap<String, MetricPolicy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    None,
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_drop: Option<f64>,
}

impl Config {
    /// Read and validate a config file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_at(&text, path)
    }

    /// Parse and validate text. Parse errors are reported against `path` so the
    /// diagnostic can name the file without a separate code path.
    pub fn from_toml_at(text: &str, path: &Path) -> Result<Self> {
        let config: Self = toml::from_str(text).map_err(|source| Error::ConfigParse {
            path: path.to_path_buf(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Structural checks that serde cannot express.
    pub fn validate(&self) -> Result<()> {
        if self.version != SUPPORTED_CONFIG_VERSION {
            return Err(Error::ConfigVersion {
                found: self.version,
                supported: SUPPORTED_CONFIG_VERSION,
            });
        }

        let mut prompt_ids: BTreeSet<&str> = BTreeSet::new();
        for prompt in &self.prompts {
            if !prompt_ids.insert(prompt.path.as_str()) {
                return Err(Error::DependencyCollision {
                    id: format!("prompt:{}", prompt.path),
                });
            }
        }

        let mut server_names: BTreeSet<&str> = BTreeSet::new();
        for server in &self.mcp.servers {
            if !server_names.insert(server.name.as_str()) {
                return Err(Error::ConfigInvalid {
                    reason: format!("MCP server name `{}` is declared more than once", server.name),
                });
            }
            match server.transport {
                Transport::Stdio if server.command.is_none() => {
                    return Err(Error::ConfigInvalid {
                        reason: format!(
                            "MCP server `{}` uses the stdio transport but declares no `command`",
                            server.name
                        ),
                    });
                }
                Transport::StreamableHttp if server.url.is_none() => {
                    return Err(Error::ConfigInvalid {
                        reason: format!(
                            "MCP server `{}` uses the streamable-http transport but declares no `url`",
                            server.name
                        ),
                    });
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Directory that project-relative paths are resolved against.
    pub fn root_for(config_path: &Path) -> PathBuf {
        config_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    }
}
```

Add `pub mod config;` to `src/lib.rs`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib config`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src
git commit -m "Add strict configuration parsing and validation"
```

---

### Task 5: Dependency model and the agent checksum

**Files:**
- Create: `src/manifest/mod.rs`
- Modify: `src/lib.rs`
- Test: in-file `#[cfg(test)]` module

**Interfaces:**
- Consumes: `fingerprint::canonical::to_vec`, `fingerprint::digest::sha256_hex`.
- Produces: `manifest::{Digest, AgentChecksum, DependencyKind, Facet, Dependency}`,
  `manifest::dep_digest(&BTreeMap<String, Facet>) -> Result<Digest>`,
  `manifest::agent_checksum(&[Dependency]) -> Result<AgentChecksum>`.

`Facet` and `Dependency` derive `PartialEq` but not `Eq`: facets can carry a `serde_json::Value` payload, and equality is
all any consumer needs.

- [ ] **Step 1: Write the failing tests**

Append to `src/manifest/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn facet(seed: &str) -> Facet {
        Facet {
            digest: Digest::sha256(seed.as_bytes()),
            shape: None,
            normalized: None,
        }
    }

    fn dep(kind: DependencyKind, id: &str, facets: &[(&str, &str)]) -> Dependency {
        Dependency {
            id: id.to_string(),
            kind,
            facets: facets
                .iter()
                .map(|(name, seed)| ((*name).to_string(), facet(seed)))
                .collect(),
            source: None,
        }
    }

    #[test]
    fn digest_renders_with_an_algorithm_prefix() {
        let digest = Digest::sha256(b"");
        assert_eq!(
            digest.as_str(),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest.hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn the_agent_checksum_is_independent_of_dependency_order() {
        let a = dep(DependencyKind::Prompt, "prompt:a.md", &[("content", "a")]);
        let b = dep(DependencyKind::Model, "model:ollama/x", &[("identity", "b")]);
        let c = dep(DependencyKind::Tool, "tool:s.t", &[("description", "c")]);

        let forward = agent_checksum(&[a.clone(), b.clone(), c.clone()]).unwrap();
        let reversed = agent_checksum(&[c, b, a]).unwrap();
        assert_eq!(forward, reversed);
    }

    #[test]
    fn changing_one_facet_changes_the_agent_checksum() {
        let before =
            agent_checksum(&[dep(DependencyKind::Tool, "tool:s.t", &[("description", "v1")])])
                .unwrap();
        let after =
            agent_checksum(&[dep(DependencyKind::Tool, "tool:s.t", &[("description", "v2")])])
                .unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn renaming_a_facet_changes_the_dependency_digest() {
        let a = dep(DependencyKind::Tool, "tool:s.t", &[("description", "v1")]);
        let b = dep(DependencyKind::Tool, "tool:s.t", &[("input_schema", "v1")]);
        assert_ne!(a.digest().unwrap(), b.digest().unwrap());
    }

    #[test]
    fn metadata_outside_the_facets_does_not_affect_any_digest() {
        let original = dep(DependencyKind::Tool, "tool:s.t", &[("description", "v1")]);
        let mut renamed_source = original.clone();
        renamed_source.source = Some("some-other-server".to_string());

        assert_eq!(
            original.digest().unwrap(),
            renamed_source.digest().unwrap()
        );
        assert_eq!(
            agent_checksum(std::slice::from_ref(&original)).unwrap(),
            agent_checksum(std::slice::from_ref(&renamed_source)).unwrap()
        );
    }

    #[test]
    fn adding_a_dependency_changes_the_agent_checksum() {
        let one = dep(DependencyKind::Prompt, "prompt:a.md", &[("content", "a")]);
        let two = dep(DependencyKind::Prompt, "prompt:b.md", &[("content", "b")]);
        assert_ne!(
            agent_checksum(std::slice::from_ref(&one)).unwrap(),
            agent_checksum(&[one, two]).unwrap()
        );
    }

    #[test]
    fn the_agent_checksum_is_prefixed_with_its_format_version() {
        let checksum =
            agent_checksum(&[dep(DependencyKind::Prompt, "prompt:a.md", &[("content", "a")])])
                .unwrap();
        assert!(checksum.as_str().starts_with("ac1:"), "{checksum:?}");
        assert_eq!(checksum.as_str().len(), "ac1:".len() + 64);
    }

    #[test]
    fn an_empty_dependency_set_still_produces_a_stable_checksum() {
        assert_eq!(agent_checksum(&[]).unwrap(), agent_checksum(&[]).unwrap());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib manifest`
Expected: FAIL — `cannot find type Digest`.

- [ ] **Step 3: Write the implementation**

`src/manifest/mod.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The dependency model and the checksum aggregate.
//!
//! A dependency is identified by `(kind, id)` and carries named facets. Each
//! facet is digested on its own, which is what lets a diff say *a description
//! changed* rather than *something changed*.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::fingerprint::{canonical, digest as hashing};

/// Format version of the checksum aggregate, independent of `lock_version`.
pub const CHECKSUM_FORMAT: &str = "ac1";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digest(String);

impl Digest {
    /// `sha256:<hex>`
    pub fn sha256(bytes: &[u8]) -> Self {
        Self(format!("sha256:{}", hashing::sha256_hex(bytes)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The hex payload, without the algorithm prefix.
    pub fn hex(&self) -> &str {
        self.0.strip_prefix("sha256:").unwrap_or(&self.0)
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentChecksum(String);

impl AgentChecksum {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_digest(digest: &Digest) -> Self {
        Self(format!("{CHECKSUM_FORMAT}:{}", digest.hex()))
    }
}

impl std::fmt::Display for AgentChecksum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    Model,
    Prompt,
    Tool,
    McpServer,
}

impl DependencyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DependencyKind::Model => "model",
            DependencyKind::Prompt => "prompt",
            DependencyKind::Tool => "tool",
            DependencyKind::McpServer => "mcp",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Facet {
    pub digest: Digest,
    /// Whitespace-collapsed digest, present for text facets so a formatting-only
    /// edit can be told apart from a semantic one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<Digest>,
    /// Normalized payload, recorded only for external un-versioned sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dependency {
    /// `<kind>:<identity>`, e.g. `tool:github.search_repositories`.
    pub id: String,
    pub kind: DependencyKind,
    pub facets: BTreeMap<String, Facet>,
    /// Where this came from (server alias, provider). Metadata: never hashed,
    /// because moving a dependency between sources does not change how the
    /// agent behaves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl Dependency {
    /// Digest of the facet set, order-independent by construction.
    pub fn digest(&self) -> Result<Digest> {
        dep_digest(&self.facets)
    }
}

/// SHA-256 over the canonical form of `{ facet_name: facet_digest }`.
pub fn dep_digest(facets: &BTreeMap<String, Facet>) -> Result<Digest> {
    let payload: BTreeMap<&str, &str> = facets
        .iter()
        .map(|(name, facet)| (name.as_str(), facet.digest.as_str()))
        .collect();
    Ok(Digest::sha256(&canonical::to_vec(&payload)?))
}

/// SHA-256 over the canonical form of the `(kind, id, dep_digest)` list, after
/// sorting by `(kind, id)`. The sort is what makes the aggregate independent of
/// discovery order; nothing downstream depends on iteration order.
pub fn agent_checksum(dependencies: &[Dependency]) -> Result<AgentChecksum> {
    let mut sorted: Vec<&Dependency> = dependencies.iter().collect();
    sorted.sort_by(|a, b| (a.kind, a.id.as_str()).cmp(&(b.kind, b.id.as_str())));

    let mut entries: Vec<(DependencyKind, &str, Digest)> = Vec::with_capacity(sorted.len());
    for dependency in sorted {
        entries.push((dependency.kind, dependency.id.as_str(), dependency.digest()?));
    }

    let digest = Digest::sha256(&canonical::to_vec(&entries)?);
    Ok(AgentChecksum::from_digest(&digest))
}
```

Add `pub mod manifest;` to `src/lib.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib manifest`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src
git commit -m "Add dependency model and order-independent agent checksum"
```

---

### Task 6: Prompt discovery

**Files:**
- Create: `src/discovery/mod.rs`
- Create: `src/discovery/prompts.rs`
- Modify: `src/lib.rs`, `src/error.rs`
- Test: in-file `#[cfg(test)]` module

**Interfaces:**
- Consumes: `config::Config`, `manifest::{Dependency, Facet, Digest}`, `fingerprint::normalize`.
- Produces: `discovery::prompts::discover(&Config, &Path) -> Result<Vec<Dependency>>`,
  `discovery::prompts::normalize_rel_path(&str) -> Result<String>`.

- [ ] **Step 1: Write the failing tests**

Append to `src/discovery/prompts.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, Config, PromptConfig};

    fn config_with(paths: &[&str]) -> Config {
        Config {
            version: 1,
            agent: AgentConfig {
                name: "test".to_string(),
            },
            model: None,
            prompts: paths
                .iter()
                .map(|path| PromptConfig {
                    path: (*path).to_string(),
                })
                .collect(),
            mcp: Default::default(),
            probes: Default::default(),
            policy: Default::default(),
        }
    }

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn content_digest(dependency: &Dependency) -> String {
        dependency.facets["content"].digest.as_str().to_string()
    }

    fn shape_digest(dependency: &Dependency) -> String {
        dependency.facets["shape"]
            .digest
            .as_str()
            .to_string()
    }

    #[test]
    fn a_leading_dot_slash_is_removed_from_the_id() {
        assert_eq!(normalize_rel_path("./prompts/a.md").unwrap(), "prompts/a.md");
    }

    #[test]
    fn an_absolute_path_is_rejected() {
        assert!(normalize_rel_path("/etc/passwd").is_err());
    }

    #[test]
    fn a_parent_directory_component_is_rejected() {
        assert!(normalize_rel_path("../outside.md").is_err());
        assert!(normalize_rel_path("prompts/../../outside.md").is_err());
    }

    #[test]
    fn a_backslash_is_rejected_because_the_id_must_mean_the_same_thing_on_every_platform() {
        // A backslash is a legal filename character on Unix and a path separator
        // on Windows, so an id containing one would not round-trip.
        assert!(normalize_rel_path("prompts\\a.md").is_err());
    }

    #[test]
    fn an_empty_path_is_rejected() {
        assert!(normalize_rel_path("").is_err());
        assert!(normalize_rel_path(".").is_err());
    }

    #[test]
    fn a_prompt_produces_content_and_shape_facets_and_a_stable_id() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "prompts/system.md", "Be concise.\n");

        let deps = discover(&config_with(&["prompts/system.md"]), dir.path()).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].id, "prompt:prompts/system.md");
        assert_eq!(deps[0].kind, DependencyKind::Prompt);
        assert!(deps[0].facets.contains_key("content"));
        assert!(deps[0].facets.contains_key("shape"));
        assert_eq!(deps[0].source, None);
    }

    #[test]
    fn line_endings_do_not_change_the_content_digest() {
        let crlf = tempfile::tempdir().unwrap();
        write(crlf.path(), "p.md", "a\r\nb\r\n");
        let lf = tempfile::tempdir().unwrap();
        write(lf.path(), "p.md", "a\nb\n");

        let a = discover(&config_with(&["p.md"]), crlf.path()).unwrap();
        let b = discover(&config_with(&["p.md"]), lf.path()).unwrap();

        assert_eq!(content_digest(&a[0]), content_digest(&b[0]));
    }

    #[test]
    fn a_formatting_only_edit_keeps_the_shape_digest() {
        let before = tempfile::tempdir().unwrap();
        write(before.path(), "p.md", "Summarize   this.\n");
        let after = tempfile::tempdir().unwrap();
        write(after.path(), "p.md", "Summarize this.\n");

        let a = discover(&config_with(&["p.md"]), before.path()).unwrap();
        let b = discover(&config_with(&["p.md"]), after.path()).unwrap();

        assert_ne!(content_digest(&a[0]), content_digest(&b[0]));
        assert_eq!(shape_digest(&a[0]), shape_digest(&b[0]));
    }

    #[test]
    fn a_semantic_edit_changes_both_digests() {
        let before = tempfile::tempdir().unwrap();
        write(before.path(), "p.md", "Be concise.\n");
        let after = tempfile::tempdir().unwrap();
        write(after.path(), "p.md", "Be thorough.\n");

        let a = discover(&config_with(&["p.md"]), before.path()).unwrap();
        let b = discover(&config_with(&["p.md"]), after.path()).unwrap();

        assert_ne!(content_digest(&a[0]), content_digest(&b[0]));
        assert_ne!(shape_digest(&a[0]), shape_digest(&b[0]));
    }

    #[test]
    fn a_missing_prompt_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = discover(&config_with(&["prompts/missing.md"]), dir.path()).unwrap_err();
        assert!(matches!(err, Error::Read { .. }), "{err:?}");
    }

    #[test]
    fn a_prompt_that_is_not_utf8_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.md"), [0xff, 0xfe, 0xfd]).unwrap();
        let err = discover(&config_with(&["p.md"]), dir.path()).unwrap_err();
        assert!(matches!(err, Error::PromptNotUtf8 { .. }), "{err:?}");
    }

    #[test]
    fn prompt_order_in_config_does_not_affect_the_discovered_set() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.md", "a");
        write(dir.path(), "b.md", "b");

        let mut forward = discover(&config_with(&["a.md", "b.md"]), dir.path()).unwrap();
        let mut reversed = discover(&config_with(&["b.md", "a.md"]), dir.path()).unwrap();
        forward.sort_by(|x, y| x.id.cmp(&y.id));
        reversed.sort_by(|x, y| x.id.cmp(&y.id));
        assert_eq!(forward, reversed);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib discovery`
Expected: FAIL — `cannot find function discover`.

- [ ] **Step 3: Add the path and encoding error variants**

In `src/error.rs`, add:

```rust
    #[error("prompt path `{path}` must be relative, must not contain `..`, and must not contain a backslash")]
    PromptPath { path: String },

    #[error("prompt `{path}` is not valid UTF-8")]
    PromptNotUtf8 { path: PathBuf },
```

and extend `suggestion()`:

```rust
            Error::PromptPath { .. } => Some(
                "Use a path relative to the config file, for example `prompts/system.md`.".to_string(),
            ),
            Error::PromptNotUtf8 { .. } => {
                Some("Re-save the file as UTF-8.".to_string())
            }
```

- [ ] **Step 4: Write the implementation**

`src/discovery/prompts.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Prompt discovery. Prompts are repo-local files, so only their digests are
//! recorded: git already versions the content, and a second copy in the
//! lockfile would be a second source of truth.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::fingerprint::normalize;
use crate::manifest::{Dependency, DependencyKind, Digest, Facet};

/// Normalize a config-declared path into a stable, project-relative,
/// forward-slash identity.
///
/// Absolute paths, `..`, and backslashes are rejected: an absolute path would
/// make the lockfile depend on the machine it was produced on, and a backslash is
/// a path separator on Windows but an ordinary filename character on Unix, so an
/// id containing one would not mean the same thing everywhere.
pub fn normalize_rel_path(path: &str) -> Result<String> {
    if path.contains('\\') {
        return Err(Error::PromptPath {
            path: path.to_string(),
        });
    }

    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err(Error::PromptPath {
            path: path.to_string(),
        });
    }

    let mut parts: Vec<&str> = Vec::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => {
                parts.push(part.to_str().ok_or_else(|| Error::PromptPath {
                    path: path.to_string(),
                })?);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Error::PromptPath {
                    path: path.to_string(),
                });
            }
        }
    }

    if parts.is_empty() {
        return Err(Error::PromptPath {
            path: path.to_string(),
        });
    }

    Ok(parts.join("/"))
}

/// Content and shape facets for a text file. `content` is what a human would
/// call the document; `shape` ignores interior whitespace so a formatting-only
/// edit can be recognized later without a model.
fn text_facets(bytes: &[u8], path: &Path) -> Result<BTreeMap<String, Facet>> {
    let raw = std::str::from_utf8(bytes).map_err(|_| Error::PromptNotUtf8 {
        path: path.to_path_buf(),
    })?;

    let mut facets = BTreeMap::new();
    facets.insert(
        "content".to_string(),
        Facet {
            digest: Digest::sha256(normalize::normalize_text(raw).as_bytes()),
            shape: None,
            normalized: None,
        },
    );
    facets.insert(
        "shape".to_string(),
        Facet {
            digest: Digest::sha256(normalize::shape_text(raw).as_bytes()),
            shape: None,
            normalized: None,
        },
    );
    Ok(facets)
}

/// Discover every declared prompt. `root` is the directory the config lives in.
pub fn discover(config: &Config, root: &Path) -> Result<Vec<Dependency>> {
    let mut dependencies = Vec::with_capacity(config.prompts.len());

    for prompt in &config.prompts {
        let id_path = normalize_rel_path(&prompt.path)?;
        let absolute = root.join(&id_path);
        let bytes = std::fs::read(&absolute).map_err(|source| Error::Read {
            path: absolute.clone(),
            source,
        })?;

        dependencies.push(Dependency {
            id: format!("prompt:{id_path}"),
            kind: DependencyKind::Prompt,
            facets: text_facets(&bytes, &absolute)?,
            source: None,
        });
    }

    Ok(dependencies)
}
```

`src/discovery/mod.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Dependency discovery. All I/O lives here; everything downstream is pure.

pub mod prompts;

use crate::manifest::Dependency;

/// The result of a discovery pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Discovery {
    pub dependencies: Vec<Dependency>,
    /// Non-fatal observations that must be visible to the user rather than
    /// silently swallowed (for example: a provider that exposes no model digest).
    pub warnings: Vec<String>,
}
```

Add `pub mod discovery;` to `src/lib.rs`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib discovery`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src
git commit -m "Add prompt discovery with project-relative identities"
```

---

### Task 7: Model discovery

**Files:**
- Create: `src/discovery/model.rs`
- Modify: `src/discovery/mod.rs`, `src/error.rs`
- Test: in-file `#[cfg(test)]` module with `wiremock` fixtures

**Interfaces:**
- Consumes: `config::ModelConfig`, `manifest::{Dependency, Facet, Digest}`, `fingerprint::{canonical, normalize}`.
- Produces: `discovery::model::OllamaMetadata`,
  `discovery::model::parse_ollama_parameters(&str) -> BTreeMap<String, Vec<String>>`,
  `discovery::model::dependency(&ModelConfig, Option<&OllamaMetadata>) -> Result<(Dependency, Vec<String>)>`,
  `async discovery::model::fetch(&reqwest::Client, &ModelConfig) -> Result<Option<OllamaMetadata>>`.

The split is deliberate: parsing is a pure function over a fetched struct, so fingerprint logic is tested without HTTP,
and HTTP is tested against a `wiremock` server separately. No mocking framework and no trait are introduced for this.

- [ ] **Step 1: Write the failing tests**

Append to `src/discovery/model.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelConfig;

    fn ollama_config() -> ModelConfig {
        ModelConfig {
            provider: "ollama".to_string(),
            id: "qwen3:8b".to_string(),
            endpoint: Some("http://localhost:11434".to_string()),
            params: BTreeMap::new(),
        }
    }

    fn metadata() -> OllamaMetadata {
        OllamaMetadata {
            digest: Some("aa".repeat(32)),
            family: Some("qwen3".to_string()),
            parameter_size: Some("8.0B".to_string()),
            quantization_level: Some("Q8_0".to_string()),
            parameters: Some("temperature 0.7\nnum_ctx 2048\n".to_string()),
            template: Some("{{ .Prompt }}\n".to_string()),
            capabilities: vec!["completion".to_string(), "tools".to_string()],
        }
    }

    fn build(config: &ModelConfig, metadata: Option<&OllamaMetadata>) -> Dependency {
        dependency(config, metadata).unwrap().0
    }

    #[test]
    fn the_ollama_digest_is_normalized_to_the_sha256_prefixed_form() {
        let dependency = build(&ollama_config(), Some(&metadata()));
        let identity = dependency.facets["identity"].normalized.clone().unwrap();
        assert_eq!(
            identity["digest"],
            serde_json::Value::String(format!("sha256:{}", "aa".repeat(32)))
        );
    }

    #[test]
    fn the_dependency_id_names_the_provider_and_the_model() {
        let dependency = build(&ollama_config(), Some(&metadata()));
        assert_eq!(dependency.id, "model:ollama/qwen3:8b");
        assert_eq!(dependency.kind, DependencyKind::Model);
        assert_eq!(dependency.source.as_deref(), Some("ollama"));
    }

    #[test]
    fn changing_the_quantization_changes_the_identity_digest() {
        let baseline = build(&ollama_config(), Some(&metadata()));
        let mut quantized = metadata();
        quantized.quantization_level = Some("Q4_K_M".to_string());
        let after = build(&ollama_config(), Some(&quantized));
        assert_ne!(
            baseline.facets["identity"].digest,
            after.facets["identity"].digest
        );
    }

    #[test]
    fn changing_the_model_content_digest_changes_the_identity_digest() {
        let baseline = build(&ollama_config(), Some(&metadata()));
        let mut swapped = metadata();
        swapped.digest = Some("bb".repeat(32));
        let after = build(&ollama_config(), Some(&swapped));
        assert_ne!(
            baseline.facets["identity"].digest,
            after.facets["identity"].digest
        );
    }

    #[test]
    fn losing_the_tools_capability_changes_the_capabilities_digest() {
        let baseline = build(&ollama_config(), Some(&metadata()));
        let mut stripped = metadata();
        stripped.capabilities = vec!["completion".to_string()];
        let after = build(&ollama_config(), Some(&stripped));
        assert_ne!(
            baseline.facets["capabilities"].digest,
            after.facets["capabilities"].digest
        );
    }

    #[test]
    fn capability_order_is_insignificant() {
        let mut reordered = metadata();
        reordered.capabilities = vec!["tools".to_string(), "completion".to_string()];
        assert_eq!(
            build(&ollama_config(), Some(&metadata())),
            build(&ollama_config(), Some(&reordered))
        );
    }

    #[test]
    fn duplicated_capabilities_are_insignificant() {
        let mut duplicated = metadata();
        duplicated.capabilities = vec![
            "tools".to_string(),
            "completion".to_string(),
            "tools".to_string(),
        ];
        assert_eq!(
            build(&ollama_config(), Some(&metadata())),
            build(&ollama_config(), Some(&duplicated))
        );
    }

    #[test]
    fn parameter_line_order_is_insignificant() {
        let mut reordered = metadata();
        reordered.parameters = Some("num_ctx 2048\ntemperature 0.7\n".to_string());
        assert_eq!(
            build(&ollama_config(), Some(&metadata())),
            build(&ollama_config(), Some(&reordered))
        );
    }

    #[test]
    fn a_parameter_value_change_changes_the_params_digest() {
        let baseline = build(&ollama_config(), Some(&metadata()));
        let mut retuned = metadata();
        retuned.parameters = Some("temperature 0.0\nnum_ctx 2048\n".to_string());
        let after = build(&ollama_config(), Some(&retuned));
        assert_ne!(baseline.facets["params"].digest, after.facets["params"].digest);
    }

    #[test]
    fn a_chat_template_change_changes_the_template_digest() {
        let baseline = build(&ollama_config(), Some(&metadata()));
        let mut retemplated = metadata();
        retemplated.template = Some("<|im_start|>{{ .Prompt }}".to_string());
        let after = build(&ollama_config(), Some(&retemplated));
        assert_ne!(
            baseline.facets["template"].digest,
            after.facets["template"].digest
        );
    }

    #[test]
    fn a_template_difference_that_is_only_trailing_whitespace_is_insignificant() {
        let mut reflowed = metadata();
        reflowed.template = Some("{{ .Prompt }}   ".to_string());
        assert_eq!(
            build(&ollama_config(), Some(&metadata())),
            build(&ollama_config(), Some(&reflowed))
        );
    }

    #[test]
    fn an_absent_template_produces_no_template_facet() {
        let mut without = metadata();
        without.template = None;
        let dependency = build(&ollama_config(), Some(&without));
        assert!(!dependency.facets.contains_key("template"));
    }

    #[test]
    fn parameters_parse_into_an_order_insensitive_map() {
        let parsed = parse_ollama_parameters("temperature 0.7\nstop \"END\"\nstop \"STOP\"\n");
        assert_eq!(parsed["temperature"], vec!["0.7".to_string()]);
        assert_eq!(parsed["stop"], vec!["END".to_string(), "STOP".to_string()]);
    }

    #[test]
    fn an_openai_compatible_provider_records_identity_without_a_digest_and_warns() {
        let config = ModelConfig {
            provider: "openai-compatible".to_string(),
            id: "gpt-4o".to_string(),
            endpoint: Some("http://localhost:8000".to_string()),
            params: BTreeMap::new(),
        };
        let (dependency, warnings) = dependency(&config, None).unwrap();

        assert_eq!(dependency.id, "model:openai-compatible/gpt-4o");
        assert!(!dependency.facets.contains_key("template"));
        assert!(!dependency.facets.contains_key("params"));
        assert_eq!(warnings.len(), 1, "a missing digest must be surfaced");
        assert!(warnings[0].contains("digest unavailable"), "{warnings:?}");
    }

    #[test]
    fn an_unsupported_provider_is_an_error() {
        let mut config = ollama_config();
        config.provider = "anthropic".to_string();
        let err = dependency(&config, None).unwrap_err();
        assert!(matches!(err, Error::ModelProvider { .. }), "{err:?}");
    }

    #[test]
    fn an_ollama_model_that_the_server_does_not_report_is_an_error() {
        let err = dependency(&ollama_config(), None).unwrap_err();
        assert!(matches!(err, Error::ModelMissing { .. }), "{err:?}");
    }

    #[test]
    fn the_endpoint_is_never_part_of_the_identity() {
        let mut moved = ollama_config();
        moved.endpoint = Some("http://other-host:11434".to_string());
        assert_eq!(
            build(&ollama_config(), Some(&metadata())),
            build(&moved, Some(&metadata()))
        );
    }

    #[test]
    fn an_ollama_provider_without_an_endpoint_is_an_error() {
        let mut config = ollama_config();
        config.endpoint = None;
        let err = dependency(&config, None).unwrap_err();
        assert!(matches!(err, Error::ModelEndpointMissing { .. }), "{err:?}");
    }

    async fn metadata_for(server: &wiremock::MockServer, model_digest: &str) -> OllamaMetadata {
        // Built as a map so the optional `digest` is inserted rather than
        // mutated in place: `serde_json::Value` implements `Index` but not
        // `IndexMut`.
        let mut model = serde_json::Map::new();
        model.insert("name".to_string(), serde_json::json!("qwen3:8b"));
        model.insert("model".to_string(), serde_json::json!("qwen3:8b"));
        model.insert(
            "modified_at".to_string(),
            serde_json::json!("2025-10-03T23:34:03Z"),
        );
        model.insert("size".to_string(), serde_json::json!(9608350245u64));
        if !model_digest.is_empty() {
            model.insert(
                "digest".to_string(),
                serde_json::Value::String(model_digest.to_string()),
            );
        }
        model.insert(
            "details".to_string(),
            serde_json::json!({
                "format": "gguf",
                "family": "qwen3",
                "parameter_size": "8.0B",
                "quantization_level": "Q8_0"
            }),
        );
        let tags = serde_json::json!({ "models": [serde_json::Value::Object(model)] });

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/tags"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(tags))
            .mount(server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/show"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "parameters": "temperature 0.7\n",
                "template": "{{ .Prompt }}",
                "capabilities": ["completion", "tools"],
                "license": "Apache-2.0",
                "modified_at": "2025-08-14T15:49:43Z"
            })))
            .mount(server)
            .await;

        let config = ModelConfig {
            provider: "ollama".to_string(),
            id: "qwen3:8b".to_string(),
            endpoint: Some(server.uri()),
            params: BTreeMap::new(),
        };

        fetch(&reqwest::Client::new(), &config)
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn fetch_reads_tags_and_show_from_the_endpoint() {
        let server = wiremock::MockServer::start().await;
        let metadata = metadata_for(&server, &"cc".repeat(32)).await;

        assert_eq!(metadata.quantization_level.as_deref(), Some("Q8_0"));
        assert_eq!(metadata.family.as_deref(), Some("qwen3"));
        assert_eq!(metadata.capabilities, vec!["completion", "tools"]);
        assert_eq!(metadata.template.as_deref(), Some("{{ .Prompt }}"));
    }

    #[tokio::test]
    async fn timestamps_sizes_and_licenses_never_reach_the_dependency() {
        let first = wiremock::MockServer::start().await;
        let second = wiremock::MockServer::start().await;

        // Same model, but the second response carries a different modification
        // time, size, and license. None of those are behavior-relevant, so the
        // dependency must be identical byte for byte.
        let a = metadata_for(&first, &"dd".repeat(32)).await;
        let b = metadata_for(&second, &"dd".repeat(32)).await;

        let config_a = ModelConfig {
            provider: "ollama".to_string(),
            id: "qwen3:8b".to_string(),
            endpoint: Some(first.uri()),
            params: BTreeMap::new(),
        };
        let config_b = ModelConfig {
            endpoint: Some(second.uri()),
            ..config_a.clone()
        };

        assert_eq!(
            build(&config_a, Some(&a)),
            build(&config_b, Some(&b)),
            "non-behavioral provider metadata must not influence the fingerprint"
        );
    }

    #[tokio::test]
    async fn a_model_with_no_reported_digest_produces_a_warning() {
        let server = wiremock::MockServer::start().await;
        let metadata = metadata_for(&server, "").await;

        let (dependency, warnings) = dependency(&ollama_config(), Some(&metadata)).unwrap();
        let identity = dependency.facets["identity"].normalized.clone().unwrap();
        assert!(identity.get("digest").is_none(), "{identity}");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("no content digest"), "{warnings:?}");
    }

    #[tokio::test]
    async fn fetch_reports_a_server_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let config = ModelConfig {
            provider: "ollama".to_string(),
            id: "qwen3:8b".to_string(),
            endpoint: Some(server.uri()),
            params: BTreeMap::new(),
        };

        let err = fetch(&reqwest::Client::new(), &config).await.unwrap_err();
        assert!(matches!(err, Error::ModelStatus { status: 500, .. }), "{err:?}");
    }

    #[tokio::test]
    async fn fetch_reports_a_model_the_server_does_not_have() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/tags"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "models": [] })),
            )
            .mount(&server)
            .await;

        let config = ModelConfig {
            provider: "ollama".to_string(),
            id: "qwen3:8b".to_string(),
            endpoint: Some(server.uri()),
            params: BTreeMap::new(),
        };

        let err = fetch(&reqwest::Client::new(), &config).await.unwrap_err();
        assert!(matches!(err, Error::ModelMissing { .. }), "{err:?}");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib discovery::model`
Expected: FAIL — `cannot find type OllamaMetadata`.

- [ ] **Step 3: Add the model error variants**

In `src/error.rs`, add:

```rust
    #[error("model `{id}` was not found on the `{provider}` endpoint `{endpoint}`")]
    ModelMissing {
        provider: String,
        id: String,
        endpoint: String,
    },

    #[error("provider `{provider}` requires an `endpoint` to be configured")]
    ModelEndpointMissing { provider: String },

    #[error("failed to reach the `{provider}` endpoint `{endpoint}`")]
    ModelEndpoint {
        provider: String,
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("the `{provider}` endpoint `{endpoint}` returned HTTP {status}")]
    ModelStatus {
        provider: String,
        endpoint: String,
        status: u16,
    },

    #[error("unsupported model provider `{provider}`")]
    ModelProvider { provider: String },
```

and extend `suggestion()`:

```rust
            Error::ModelMissing { provider, id, .. } if provider == "ollama" => {
                Some(format!("Pull the model with `ollama pull {id}`, or correct `[model].id`."))
            }
            Error::ModelMissing { .. } => None,
            Error::ModelEndpointMissing { .. } => {
                Some("Add `endpoint = \"http://localhost:11434\"` to the `[model]` section.".to_string())
            }
            Error::ModelEndpoint { provider, .. } if provider == "ollama" => {
                Some("Check that the Ollama server is running (`ollama serve`).".to_string())
            }
            Error::ModelEndpoint { .. } => None,
            Error::ModelStatus { .. } => {
                Some("Run `ollama list` to confirm the model server is healthy.".to_string())
            }
            Error::ModelProvider { .. } => Some(
                "Providers supported in this version: `ollama`, `openai-compatible`.".to_string(),
            ),
```

- [ ] **Step 4: Write the implementation**

`src/discovery/model.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Model dependency discovery.
//!
//! Behavior-relevant model state is not just the model name. Quantization, the
//! chat template, inference parameters, and the capability set all change how an
//! agent behaves while every source file stays byte-identical.
//!
//! Nothing here records `modified_at`, `size`, or `license`: they are not
//! behavior-relevant, and recording them would make the checksum depend on when
//! and where it was computed. `OllamaMetadata` simply has no fields for them, so
//! the omission is enforced by the type rather than by remembering to filter.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::ModelConfig;
use crate::error::{Error, Result};
use crate::fingerprint::{canonical, normalize};
use crate::manifest::{Dependency, DependencyKind, Digest, Facet};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OllamaMetadata {
    pub digest: Option<String>,
    pub family: Option<String>,
    pub parameter_size: Option<String>,
    pub quantization_level: Option<String>,
    /// Raw `parameters` text from `/api/show`.
    pub parameters: Option<String>,
    pub template: Option<String>,
    pub capabilities: Vec<String>,
}

#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagEntry>,
}

#[derive(Deserialize)]
struct TagEntry {
    name: String,
    model: Option<String>,
    digest: Option<String>,
    #[serde(default)]
    details: TagDetails,
}

#[derive(Default, Deserialize)]
struct TagDetails {
    family: Option<String>,
    parameter_size: Option<String>,
    quantization_level: Option<String>,
}

#[derive(Deserialize)]
struct ShowResponse {
    parameters: Option<String>,
    template: Option<String>,
    #[serde(default)]
    capabilities: Vec<String>,
}

#[derive(Serialize)]
struct ShowRequest<'a> {
    model: &'a str,
}

/// Parse Ollama's `parameters` text into a map.
///
/// The text is line-oriented (`temperature 0.7`), so the original ordering is an
/// artifact of how the Modelfile was written. Storing a map makes the digest
/// independent of that artifact while keeping repeated keys such as `stop`.
pub fn parse_ollama_parameters(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut parameters: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = match line.split_once(char::is_whitespace) {
            Some((key, value)) => (key, value.trim()),
            None => (line, ""),
        };
        parameters
            .entry(key.to_string())
            .or_default()
            .push(value.trim_matches('"').to_string());
    }
    parameters
}

/// Digest a JSON value without recording it.
fn facet(value: &serde_json::Value) -> Result<Facet> {
    Ok(Facet {
        digest: Digest::sha256(&canonical::to_vec(value)?),
        shape: None,
        normalized: None,
    })
}

/// Digest a JSON value and record it, because the source is external and
/// therefore not version-controlled inside this repository.
fn recorded_facet(value: &serde_json::Value) -> Result<Facet> {
    let mut facet = facet(value)?;
    facet.normalized = Some(value.clone());
    Ok(facet)
}

/// Pure mapping from config plus optional provider metadata to a dependency.
pub fn dependency(
    config: &ModelConfig,
    metadata: Option<&OllamaMetadata>,
) -> Result<(Dependency, Vec<String>)> {
    let mut warnings: Vec<String> = Vec::new();
    let id = format!("model:{}/{}", config.provider, config.id);

    let mut identity = serde_json::Map::new();
    identity.insert(
        "provider".to_string(),
        serde_json::Value::String(config.provider.clone()),
    );
    identity.insert(
        "id".to_string(),
        serde_json::Value::String(config.id.clone()),
    );

    let mut facets: BTreeMap<String, Facet> = BTreeMap::new();

    match config.provider.as_str() {
        "ollama" => {
            if config.endpoint.is_none() {
                return Err(Error::ModelEndpointMissing {
                    provider: config.provider.clone(),
                });
            }

            let metadata = metadata.ok_or_else(|| Error::ModelMissing {
                provider: config.provider.clone(),
                id: config.id.clone(),
                endpoint: config.endpoint.clone().unwrap_or_default(),
            })?;

            match &metadata.digest {
                Some(digest) => {
                    identity.insert(
                        "digest".to_string(),
                        serde_json::Value::String(format!("sha256:{digest}")),
                    );
                }
                None => warnings.push(format!(
                    "model `{}`: the endpoint reported no content digest; upstream model updates may go undetected",
                    config.id
                )),
            }

            for (key, value) in [
                ("family", &metadata.family),
                ("parameter_size", &metadata.parameter_size),
                ("quantization_level", &metadata.quantization_level),
            ] {
                if let Some(value) = value {
                    identity.insert(key.to_string(), serde_json::Value::String(value.clone()));
                }
            }

            if let Some(parameters) = &metadata.parameters {
                let parsed = parse_ollama_parameters(parameters);
                let value =
                    serde_json::to_value(&parsed).map_err(|source| Error::Json { source })?;
                facets.insert("params".to_string(), facet(&value)?);
            }

            if let Some(template) = &metadata.template {
                facets.insert(
                    "template".to_string(),
                    facet(&serde_json::Value::String(normalize::normalize_text(template)))?,
                );
            }

            let mut capabilities = metadata.capabilities.clone();
            capabilities.sort();
            capabilities.dedup();
            if !capabilities.is_empty() {
                let value = serde_json::to_value(&capabilities)
                    .map_err(|source| Error::Json { source })?;
                facets.insert("capabilities".to_string(), facet(&value)?);
            }
        }
        "openai-compatible" => {
            warnings.push(format!(
                "model `{}`: digest unavailable for the `openai-compatible` provider; upstream model updates may go undetected",
                config.id
            ));
        }
        other => {
            return Err(Error::ModelProvider {
                provider: other.to_string(),
            });
        }
    }

    let identity = serde_json::Value::Object(identity);
    facets.insert("identity".to_string(), recorded_facet(&identity)?);

    Ok((
        Dependency {
            id,
            kind: DependencyKind::Model,
            facets,
            source: Some(config.provider.clone()),
        },
        warnings,
    ))
}

/// Fetch metadata from an Ollama endpoint. Returns `Ok(None)` for providers that
/// expose no metadata endpoint.
pub async fn fetch(
    client: &reqwest::Client,
    config: &ModelConfig,
) -> Result<Option<OllamaMetadata>> {
    if config.provider != "ollama" {
        return Ok(None);
    }

    let endpoint = config
        .endpoint
        .clone()
        .ok_or_else(|| Error::ModelEndpointMissing {
            provider: config.provider.clone(),
        })?;
    let base = endpoint.trim_end_matches('/');

    let tags_response = client
        .get(format!("{base}/api/tags"))
        .send()
        .await
        .map_err(|source| Error::ModelEndpoint {
            provider: config.provider.clone(),
            endpoint: endpoint.clone(),
            source,
        })?;

    let status = tags_response.status();
    if !status.is_success() {
        return Err(Error::ModelStatus {
            provider: config.provider.clone(),
            endpoint: endpoint.clone(),
            status: status.as_u16(),
        });
    }

    let tags: TagsResponse = tags_response
        .json()
        .await
        .map_err(|source| Error::ModelEndpoint {
            provider: config.provider.clone(),
            endpoint: endpoint.clone(),
            source,
        })?;

    let entry = tags
        .models
        .into_iter()
        .find(|entry| entry.name == config.id || entry.model.as_deref() == Some(config.id.as_str()))
        .ok_or_else(|| Error::ModelMissing {
            provider: config.provider.clone(),
            id: config.id.clone(),
            endpoint: endpoint.clone(),
        })?;

    let show_response = client
        .post(format!("{base}/api/show"))
        .json(&ShowRequest { model: &config.id })
        .send()
        .await
        .map_err(|source| Error::ModelEndpoint {
            provider: config.provider.clone(),
            endpoint: endpoint.clone(),
            source,
        })?;

    let status = show_response.status();
    if !status.is_success() {
        return Err(Error::ModelStatus {
            provider: config.provider.clone(),
            endpoint: endpoint.clone(),
            status: status.as_u16(),
        });
    }

    let show: ShowResponse = show_response
        .json()
        .await
        .map_err(|source| Error::ModelEndpoint {
            provider: config.provider.clone(),
            endpoint: endpoint.clone(),
            source,
        })?;

    Ok(Some(OllamaMetadata {
        digest: entry.digest,
        family: entry.details.family,
        parameter_size: entry.details.parameter_size,
        quantization_level: entry.details.quantization_level,
        parameters: show.parameters,
        template: show.template,
        capabilities: show.capabilities,
    }))
}
```

Add `pub mod model;` to `src/discovery/mod.rs`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib discovery::model`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src
git commit -m "Add model discovery with quantization, template, and capability facets"
```

---

### Task 8: Lockfile, `init`, `snapshot`, and the CLI wiring

**Files:**
- Create: `src/lockfile.rs`
- Create: `src/report/mod.rs`, `src/report/human.rs`, `src/report/json.rs`
- Create: `src/cli/mod.rs`, `src/cli/args.rs`, `src/cli/cmd/mod.rs`, `src/cli/cmd/init.rs`, `src/cli/cmd/snapshot.rs`
- Modify: `src/lib.rs`, `src/main.rs`, `src/error.rs`, `src/discovery/mod.rs`
- Test: in-file `#[cfg(test)]` module plus `tests/snapshot_determinism.rs`, `tests/lockfile_contract.rs`, `tests/cli_commands.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: `lockfile::{Lockfile, Generator, LockedDependency}`, `Lockfile::from_dependencies(&[Dependency]) -> Result<Lockfile>`,
  `Lockfile::to_bytes(&self) -> Result<Vec<u8>>`, `Lockfile::write(&self, &Path) -> Result<()>`, `Lockfile::read(&Path) -> Result<Lockfile>`,
  `discovery::run(&Config, &Path) -> Result<Discovery>`,
  `cli::cmd::init::run(&Path, &Path, bool) -> Result<PathBuf>`,
  `cli::cmd::snapshot::run(&Path, &Path, &Path) -> Result<(Lockfile, Discovery)>`,
  `report::human::snapshot(&Lockfile, &Discovery) -> String`,
  `report::json::snapshot(&Lockfile, &Discovery) -> Result<String>`.

- [ ] **Step 1: Add the lockfile and composition error variants**

In `src/error.rs`, add:

```rust
    #[error("`{path}` already exists")]
    AlreadyExists { path: PathBuf },

    #[error("lockfile `{path}` is not valid JSON")]
    LockParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("lockfile `{path}` uses lock_version {found}, but this build supports {supported}")]
    LockVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
```

and extend `suggestion()`:

```rust
            Error::AlreadyExists { path } => {
                Some(format!("Re-run with `--force` to overwrite `{}`.", path.display()))
            }
            Error::LockParse { .. } => {
                Some("Regenerate it with `agentchecksum snapshot`, or restore it from git.".to_string())
            }
            Error::LockVersion { .. } => Some(
                "Upgrade agentchecksum. A newer lockfile is never silently reinterpreted.".to_string(),
            ),
```

- [ ] **Step 2: Write the failing determinism test**

`tests/snapshot_determinism.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use agentchecksum::config::Config;
use agentchecksum::discovery;
use agentchecksum::lockfile::Lockfile;

/// Writes an identical project into the given directory.
fn write_project(root: &Path) {
    std::fs::create_dir_all(root.join("prompts")).unwrap();
    std::fs::write(root.join("prompts/system.md"), "Be concise.\n").unwrap();
    std::fs::write(root.join("prompts/tools.md"), "Prefer read-only tools.\n").unwrap();
    std::fs::write(
        root.join("agentchecksum.toml"),
        r#"
version = 1

[agent]
name = "determinism-test"

[[prompts]]
path = "prompts/system.md"

[[prompts]]
path = "prompts/tools.md"
"#,
    )
    .unwrap();
}

async fn lock_for(root: &Path) -> Lockfile {
    let config = Config::load(&root.join("agentchecksum.toml")).unwrap();
    let discovery = discovery::run(&config, root).await.unwrap();
    Lockfile::from_dependencies(&discovery.dependencies).unwrap()
}

async fn lock_bytes(root: &Path) -> Vec<u8> {
    lock_for(root).await.to_bytes().unwrap()
}

#[tokio::test]
async fn the_same_project_in_two_directories_produces_byte_identical_lockfiles() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    write_project(first.path());
    write_project(second.path());

    assert_eq!(
        lock_bytes(first.path()).await,
        lock_bytes(second.path()).await
    );
}

#[tokio::test]
async fn repeated_runs_in_the_same_directory_produce_byte_identical_lockfiles() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());

    assert_eq!(lock_bytes(dir.path()).await, lock_bytes(dir.path()).await);
}

#[tokio::test]
async fn no_absolute_path_appears_in_the_lockfile() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());

    let text = String::from_utf8(lock_bytes(dir.path()).await).unwrap();
    let root = dir.path().to_string_lossy().to_string();
    assert!(
        !text.contains(&root),
        "lockfile leaked the absolute path:\n{text}"
    );
}

#[tokio::test]
async fn reformatting_the_lockfile_does_not_change_the_checksum() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());

    let path = dir.path().join("agentchecksum.lock");
    let original = lock_for(dir.path()).await;
    original.write(&path).unwrap();

    // Rewrite the same lock with a different layout, as an editor or a future
    // serializer version might. The checksum describes the dependencies, so it
    // must come back identical.
    let value: serde_json::Value = serde_json::from_slice(&original.to_bytes().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();

    assert_eq!(Lockfile::read(&path).unwrap(), original);
}

#[tokio::test]
async fn changing_a_prompt_changes_the_agent_checksum() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());
    let before = lock_bytes(dir.path()).await;

    std::fs::write(dir.path().join("prompts/system.md"), "Be thorough.\n").unwrap();

    assert_ne!(before, lock_bytes(dir.path()).await);
}

#[tokio::test]
async fn a_lockfile_with_a_newer_version_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());
    let future = String::from_utf8(lock_bytes(dir.path()).await)
        .unwrap()
        .replace("\"lock_version\": 1", "\"lock_version\": 99");
    let path = dir.path().join("agentchecksum.lock");
    std::fs::write(&path, future).unwrap();

    let err = Lockfile::read(&path).unwrap_err();
    assert!(
        matches!(err, agentchecksum::error::Error::LockVersion { found: 99, .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn unknown_fields_inside_a_known_lock_version_are_tolerated() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());
    let extended = String::from_utf8(lock_bytes(dir.path()).await)
        .unwrap()
        .replace(
            "\"lock_version\": 1",
            "\"lock_version\": 1,\n  \"future_field\": \"ignored\"",
        );
    let path = dir.path().join("agentchecksum.lock");
    std::fs::write(&path, extended).unwrap();

    assert!(Lockfile::read(&path).is_ok());
}
```

- [ ] **Step 3: Run the failing tests**

Run: `cargo test --test snapshot_determinism`
Expected: FAIL — `cannot find function run in module discovery`.

- [ ] **Step 4: Write the implementation**

Add to `src/discovery/mod.rs`:

```rust
use std::path::Path;

use crate::config::Config;
use crate::error::Result;

/// Run every configured discovery source.
pub async fn run(config: &Config, root: &Path) -> Result<Discovery> {
    let mut dependencies = prompts::discover(config, root)?;
    let mut warnings = Vec::new();

    if let Some(model) = &config.model {
        let client = reqwest::Client::new();
        let metadata = model::fetch(&client, model).await?;
        let (dependency, model_warnings) = model::dependency(model, metadata.as_ref())?;
        dependencies.push(dependency);
        warnings.extend(model_warnings);
    }

    Ok(Discovery {
        dependencies,
        warnings,
    })
}
```

`src/lockfile.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The generated dependency baseline.
//!
//! Read policy differs from config policy (spec §14.1). The lockfile tolerates
//! unknown fields inside a known `lock_version`, because an unknown field cannot
//! change the meaning of the digests we do understand. A newer `lock_version` is
//! refused outright: silently comparing an unknown format could report PASS for a
//! change we cannot see, and a wrong PASS is worse than a parse failure.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::manifest::{AgentChecksum, Dependency, DependencyKind, Facet, agent_checksum};

pub const SUPPORTED_LOCK_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lockfile {
    pub lock_version: u32,
    pub generator: Generator,
    pub agent_checksum: AgentChecksum,
    pub dependencies: BTreeMap<String, LockedDependency>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Generator {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockedDependency {
    pub kind: DependencyKind,
    pub facets: BTreeMap<String, Facet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl Lockfile {
    pub fn from_dependencies(dependencies: &[Dependency]) -> Result<Self> {
        let checksum = agent_checksum(dependencies)?;

        let mut locked = BTreeMap::new();
        for dependency in dependencies {
            locked.insert(
                dependency.id.clone(),
                LockedDependency {
                    kind: dependency.kind,
                    facets: dependency.facets.clone(),
                    source: dependency.source.clone(),
                },
            );
        }

        Ok(Self {
            lock_version: SUPPORTED_LOCK_VERSION,
            generator: Generator {
                name: "agentchecksum".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            agent_checksum: checksum,
            dependencies: locked,
        })
    }

    /// Deterministic serialization: sorted keys, two-space indent, one trailing
    /// newline. These bytes are never an input to the checksum.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut text =
            serde_json::to_string_pretty(self).map_err(|source| Error::Json { source })?;
        text.push('\n');
        Ok(text.into_bytes())
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        std::fs::write(path, self.to_bytes()?).map_err(|source| Error::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let lock: Self = serde_json::from_str(&text).map_err(|source| Error::LockParse {
            path: path.to_path_buf(),
            source,
        })?;
        if lock.lock_version > SUPPORTED_LOCK_VERSION {
            return Err(Error::LockVersion {
                path: path.to_path_buf(),
                found: lock.lock_version,
                supported: SUPPORTED_LOCK_VERSION,
            });
        }
        Ok(lock)
    }
}
```

`src/report/human.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::discovery::Discovery;
use crate::lockfile::Lockfile;
use crate::manifest::DependencyKind;

/// The `snapshot` summary, in the shape described by the spec.
pub fn snapshot(lock: &Lockfile, discovery: &Discovery) -> String {
    let mut out = String::from("Agent checksum generated.\n\nChecksum:\n");
    out.push_str(lock.agent_checksum.as_str());
    out.push_str("\n\nDependencies:\n");

    for kind in [
        DependencyKind::Model,
        DependencyKind::Prompt,
        DependencyKind::Tool,
        DependencyKind::McpServer,
    ] {
        let count = discovery
            .dependencies
            .iter()
            .filter(|dependency| dependency.kind == kind)
            .count();
        if count == 0 {
            continue;
        }
        let plural = if count == 1 { "" } else { "s" };
        match kind {
            DependencyKind::McpServer => out.push_str(&format!("{count} MCP server{plural}\n")),
            other => out.push_str(&format!("{count} {}{plural}\n", other.as_str())),
        }
    }

    for warning in &discovery.warnings {
        out.push_str(&format!("\nwarning: {warning}\n"));
    }

    out
}
```

`src/report/json.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::Serialize;

use crate::discovery::Discovery;
use crate::error::{Error, Result};
use crate::lockfile::Lockfile;

/// Machine-readable snapshot summary. The shape is part of the CLI contract, so
/// it is a typed struct rather than an ad-hoc map: the field order stays stable
/// and there is no infallible-looking `unwrap` hidden inside a macro.
#[derive(Serialize)]
struct SnapshotReport<'a> {
    status: &'a str,
    lock_version: u32,
    agent_checksum: &'a str,
    dependency_count: usize,
    warnings: &'a [String],
}

pub fn snapshot(lock: &Lockfile, discovery: &Discovery) -> Result<String> {
    let report = SnapshotReport {
        status: "ok",
        lock_version: lock.lock_version,
        agent_checksum: lock.agent_checksum.as_str(),
        dependency_count: discovery.dependencies.len(),
        warnings: &discovery.warnings,
    };
    let mut text =
        serde_json::to_string_pretty(&report).map_err(|source| Error::Json { source })?;
    text.push('\n');
    Ok(text)
}
```

`src/report/mod.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod human;
pub mod json;
```

`src/cli/cmd/init.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

const TEMPLATE: &str = r#"version = 1

[agent]
name = "my-agent"

# [model]
# provider = "ollama"
# id = "qwen3:8b"
# endpoint = "http://localhost:11434"
# params = { temperature = 0.0, seed = 42 }

[[prompts]]
path = "prompts/system.md"

# [[mcp.servers]]
# name = "github"
# transport = "stdio"
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-github"]

[probes]
path = "probes"
"#;

/// Scaffold the config and the probe directory. Returns the config path written.
pub fn run(root: &Path, config_path: &Path, force: bool) -> Result<PathBuf> {
    if config_path.exists() && !force {
        return Err(Error::AlreadyExists {
            path: config_path.to_path_buf(),
        });
    }

    std::fs::write(config_path, TEMPLATE).map_err(|source| Error::Write {
        path: config_path.to_path_buf(),
        source,
    })?;

    let probes = root.join("probes");
    std::fs::create_dir_all(&probes).map_err(|source| Error::Write {
        path: probes,
        source,
    })?;

    Ok(config_path.to_path_buf())
}
```

`src/cli/cmd/snapshot.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use crate::config::Config;
use crate::discovery::{self, Discovery};
use crate::error::Result;
use crate::lockfile::Lockfile;

/// Discover dependencies and write the lockfile.
pub async fn run(
    root: &Path,
    config_path: &Path,
    lock_path: &Path,
) -> Result<(Lockfile, Discovery)> {
    let config = Config::load(config_path)?;
    let discovery = discovery::run(&config, root).await?;
    let lock = Lockfile::from_dependencies(&discovery.dependencies)?;
    lock.write(lock_path)?;
    Ok((lock, discovery))
}
```

`src/cli/cmd/mod.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod init;
pub mod snapshot;
```

`src/cli/args.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
}

#[derive(Debug, Parser)]
#[command(
    name = "agentchecksum",
    version,
    about = "Language-agnostic dependency fingerprint and behavioral regression gate for AI agents"
)]
pub struct Cli {
    /// Output format. JSON is written to stdout alone.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Human)]
    pub format: OutputFormat,

    /// Path to the configuration file.
    #[arg(long, global = true, default_value = "agentchecksum.toml")]
    pub config: PathBuf,

    /// Path to the generated lockfile.
    #[arg(long, global = true, default_value = "agentchecksum.lock")]
    pub lock: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// clap exits with code 2 on usage errors, which is the documented contract.
    pub fn parse_or_exit() -> Self {
        <Self as Parser>::parse()
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scaffold agentchecksum.toml and a probes directory.
    Init {
        /// Overwrite an existing configuration file.
        #[arg(long)]
        force: bool,
    },
    /// Discover dependencies and write agentchecksum.lock.
    Snapshot,
}
```

`src/cli/mod.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod args;
pub mod cmd;

use std::process::ExitCode;

use crate::config::Config;
use crate::error::Result;
use crate::report;

use args::{Cli, Command, OutputFormat};

/// Exit code for a runtime failure, per the CLI contract.
const EXIT_RUNTIME_ERROR: u8 = 3;

/// Parse, dispatch, and render. Returns the process exit code.
pub async fn main() -> ExitCode {
    let cli = Cli::parse_or_exit();

    match dispatch(&cli).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error}");
            if let Some(suggestion) = error.suggestion() {
                eprintln!("\nSuggested action:\n  {suggestion}");
            }
            ExitCode::from(EXIT_RUNTIME_ERROR)
        }
    }
}

async fn dispatch(cli: &Cli) -> Result<ExitCode> {
    let root = Config::root_for(&cli.config);

    match &cli.command {
        Command::Init { force } => {
            let path = cmd::init::run(&root, &cli.config, *force)?;
            println!("Wrote {}", path.display());
            println!("Next: add your prompts, then run `agentchecksum snapshot`.");
            Ok(ExitCode::SUCCESS)
        }
        Command::Snapshot => {
            let (lock, discovery) = cmd::snapshot::run(&root, &cli.config, &cli.lock).await?;
            match cli.format {
                OutputFormat::Human => print!("{}", report::human::snapshot(&lock, &discovery)),
                OutputFormat::Json => print!("{}", report::json::snapshot(&lock, &discovery)?),
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}
```

`src/main.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    agentchecksum::cli::main().await
}
```

`src/lib.rs` becomes:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AgentChecksum: a language-agnostic dependency fingerprint and behavioral
//! regression gate for AI agents.

pub mod cli;
pub mod config;
pub mod discovery;
pub mod error;
pub mod fingerprint;
pub mod lockfile;
pub mod manifest;
pub mod report;
```

- [ ] **Step 5: Run the determinism tests to verify they pass**

Run: `cargo test --test snapshot_determinism`
Expected: PASS.

- [ ] **Step 6: Write the lockfile contract golden test**

The lockfile is committed and diffed by users and CI, so its on-disk shape is an observable contract. The golden records
the structure, with digests and the generator version redacted so it does not churn.

`tests/lockfile_contract.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

use agentchecksum::config::Config;
use agentchecksum::discovery;
use agentchecksum::lockfile::Lockfile;

/// Replace every digest with a placeholder so the snapshot documents the shape
/// rather than the input.
fn redact(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text)
            if text.starts_with("sha256:") || text.starts_with("ac1:") =>
        {
            *text = "<digest>".to_string();
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact),
        serde_json::Value::Object(map) => map.values_mut().for_each(redact),
        _ => {}
    }
}

#[tokio::test]
async fn the_lockfile_shape_is_a_stable_contract() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("prompts")).unwrap();
    std::fs::write(dir.path().join("prompts/system.md"), "Be concise.\n").unwrap();
    std::fs::write(
        dir.path().join("agentchecksum.toml"),
        r#"
version = 1

[agent]
name = "contract-test"

[[prompts]]
path = "prompts/system.md"
"#,
    )
    .unwrap();

    let config = Config::load(&dir.path().join("agentchecksum.toml")).unwrap();
    let discovery = discovery::run(&config, dir.path()).await.unwrap();
    let mut lock = Lockfile::from_dependencies(&discovery.dependencies).unwrap();
    lock.generator.version = "<agentchecksum-version>".to_string();

    let mut value: serde_json::Value =
        serde_json::from_slice(&lock.to_bytes().unwrap()).unwrap();
    redact(&mut value);

    insta::assert_json_snapshot!(value);
}
```

Create the golden with `INSTA_UPDATE=always cargo test --test lockfile_contract`, then **read the generated
`tests/snapshots/lockfile_contract__the_lockfile_shape_is_a_stable_contract.snap` and confirm it matches the lockfile
shape in spec §6** before committing it.

- [ ] **Step 7: Write the CLI integration tests**

`tests/cli_commands.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use assert_cmd::Command;

fn project(dir: &Path) {
    std::fs::create_dir_all(dir.join("prompts")).unwrap();
    std::fs::write(dir.join("prompts/system.md"), "Be concise.\n").unwrap();
}

fn write_config(dir: &Path) {
    std::fs::write(
        dir.join("agentchecksum.toml"),
        r#"
version = 1

[agent]
name = "cli-test"

[[prompts]]
path = "prompts/system.md"
"#,
    )
    .unwrap();
}

#[test]
fn init_writes_a_config_and_a_probes_directory() {
    let dir = tempfile::tempdir().unwrap();

    Command::cargo_bin("agentchecksum")
        .unwrap()
        .current_dir(dir.path())
        .arg("init")
        .assert()
        .success();

    assert!(dir.path().join("agentchecksum.toml").is_file());
    assert!(dir.path().join("probes").is_dir());
}

#[test]
fn init_refuses_to_overwrite_without_force() {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path());

    Command::cargo_bin("agentchecksum")
        .unwrap()
        .current_dir(dir.path())
        .arg("init")
        .assert()
        .code(3);
}

#[test]
fn snapshot_writes_a_lockfile_and_reports_the_checksum() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    write_config(dir.path());

    let output = Command::cargo_bin("agentchecksum")
        .unwrap()
        .current_dir(dir.path())
        .arg("snapshot")
        .output()
        .unwrap();

    assert!(output.status.success(), "exit was {:?}", output.status.code());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Agent checksum generated."), "{stdout}");
    assert!(stdout.contains("ac1:"), "{stdout}");
    assert!(stdout.contains("1 prompt"), "{stdout}");
    assert!(dir.path().join("agentchecksum.lock").is_file());
}

#[test]
fn snapshot_output_is_stable_across_invocations() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    write_config(dir.path());

    let run = || {
        let output = Command::cargo_bin("agentchecksum")
            .unwrap()
            .current_dir(dir.path())
            .arg("snapshot")
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    };

    assert_eq!(run(), run());
}

#[test]
fn snapshot_json_output_is_valid_json_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    write_config(dir.path());

    let output = Command::cargo_bin("agentchecksum")
        .unwrap()
        .current_dir(dir.path())
        .args(["--format", "json", "snapshot"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "ok");
    assert_eq!(value["dependency_count"], 1);
}

#[test]
fn a_missing_prompt_file_exits_with_code_3_and_suggests_a_fix() {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path());

    let output = Command::cargo_bin("agentchecksum")
        .unwrap()
        .current_dir(dir.path())
        .arg("snapshot")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Suggested action:"), "{stderr}");
}

#[test]
fn an_unknown_config_key_exits_with_code_3() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    std::fs::write(
        dir.path().join("agentchecksum.toml"),
        "version = 1\n\n[agentt]\nname = \"typo\"\n",
    )
    .unwrap();

    Command::cargo_bin("agentchecksum")
        .unwrap()
        .current_dir(dir.path())
        .arg("snapshot")
        .assert()
        .code(3);
}
```

- [ ] **Step 8: Run the whole suite**

Run: `cargo test --all-targets`
Expected: PASS.

- [ ] **Step 9: Verify behavior by hand, as a user would**

```bash
cd "$(mktemp -d)"
cargo run --quiet --manifest-path "$OLDPWD/Cargo.toml" -- init
mkdir -p prompts && printf 'You are a careful research agent.\n' > prompts/system.md
cargo run --quiet --manifest-path "$OLDPWD/Cargo.toml" -- snapshot
cargo run --quiet --manifest-path "$OLDPWD/Cargo.toml" -- snapshot
cat agentchecksum.lock
```

Expected: both runs print the same `ac1:` checksum; the lockfile contains a `prompt:prompts/system.md` entry with
`content` and `shape` facets, and contains no absolute path.

- [ ] **Step 10: Verify the checksum survives a lockfile reformat**

```bash
before=$(python3 -c "import json;print(json.load(open('agentchecksum.lock'))['agent_checksum'])")
python3 -c "
import json,pathlib
p=pathlib.Path('agentchecksum.lock')
p.write_text(json.dumps(json.loads(p.read_text()),indent=4)+'\n')
"
after=$(python3 -c "import json;print(json.load(open('agentchecksum.lock'))['agent_checksum'])")
test "$before" = "$after" && echo "checksum unaffected by reformatting: $before"
```

Expected: `checksum unaffected by reformatting: ac1:…`.

- [ ] **Step 11: Run the full local gate**

Run: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets`
Expected: clean.

- [ ] **Step 12: Commit**

```bash
git add -A
git commit -m "Add lockfile, init and snapshot commands, and CLI reports"
```

---

## Coverage Against the Spec

| Spec requirement | Task |
|---|---|
| §4 CLI `init` / `snapshot` with global `--format`, `--config`, `--lock` | 8 |
| §4.3 exit codes 0 / 2 / 3 | 1, 8 |
| §5 config schema and strictness | 4 |
| §6 lockfile shape, deterministic ordering, repo-local vs external asymmetry | 5, 6, 8 |
| §6.1 two serializations, one checksum | 8 (Steps 2, 10) |
| §6.2 unhashed metadata | 6 (digest-only prompts), 7 (type omits them; golden test) |
| §7.1 SHA-256, `sha256:` prefix, `ac1` prefix | 1, 5 |
| §7.2 RFC 8785 JCS | 2 |
| §7.3 semantic rules 1–3 | 2, 3 |
| §7.4 behavior-relevant keywords never normalized away | 3 |
| §7.5 facets for Model and Prompt | 6, 7 |
| §7.6 Ollama digest, quantization, template, capabilities; openai-compatible warning | 7 |
| §8.1 order-independent checksum, no Merkle | 5 |
| §13 module layout and dependency justification | 1, 6, 7, 8 |
| §14 error rendering with suggested action | 1, 4, 7, 8 |
| §14.1 config strict vs lockfile tolerant | 4, 8 |
| §16 tests 1–5, 10 (malformed config), 12 (CLI) | 1, 2, 3, 6, 7, 8 |
| §18 invariant 1 (same state → same checksum) | 8 |
| §18 invariant 2 (whitespace/key order insignificant) | 2, 3, 6, 7 |
| §18 invariant 3 (behavior-relevant content never insignificant) | 3, 7 |

Deliberately not covered by this plan: §8.2–8.3 diff and risk (Phase 2), §9 MCP (Phases 3–4), §10–11 probes (Phase 5),
§12 gate (Phase 6), §15 demo and Action (Phase 7), and §18 invariants 4–6 — those constrain `check`, which does not exist
yet. Each later phase gets its own plan against the same spec.
