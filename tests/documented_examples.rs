// SPDX-License-Identifier: MIT OR Apache-2.0

//! The TOML in the documentation is parsed by the implementation.
//!
//! A configuration example that stopped parsing is a documentation bug that nothing
//! else catches: it reads correctly, it is copy-pasted correctly, and it fails in the
//! user's editor. This test extracts every ```toml block from the user-facing docs and
//! puts it through the same parser the CLI uses.
//!
//! The docs contain three shapes, and each is checked as what it claims to be:
//!
//! - a complete configuration (`version = 1` …) must load and validate;
//! - a probe suite (`[[probe]]` …) must load against a catalog that declares the tools
//!   it references;
//! - a fragment — a `[model]` table, a `params = { … }` line, one `expect_args`
//!   entry — is wrapped in the smallest document that could contain it and must then
//!   load. A fragment that parses in no context is as broken as a config that does
//!   not parse at all.
//!
//! Unknown keys are rejected by this implementation, so a typo in an example fails
//! here rather than in someone's project.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agentchecksum::config::{Config, ProbesConfig};
use agentchecksum::fingerprint::canonical;
use agentchecksum::lockfile::Lockfile;
use agentchecksum::manifest::{Dependency, DependencyKind, Digest, Facet};
use agentchecksum::probes;
use agentchecksum::runner::ToolCatalog;

/// Every file whose TOML a reader is invited to copy.
const DOCUMENTS: [&str; 5] = [
    "README.md",
    "docs/getting-started.md",
    "docs/configuration.md",
    "docs/probes.md",
    "docs/security.md",
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every ```toml fenced block, as written in the file.
fn toml_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;

    for line in text.lines() {
        match (&mut current, line.trim_end()) {
            (None, "```toml") => current = Some(String::new()),
            (None, _) => {}
            (Some(block), "```") => {
                blocks.push(std::mem::take(block));
                current = None;
            }
            (Some(block), line) => {
                block.push_str(line);
                block.push('\n');
            }
        }
    }

    assert!(
        current.is_none(),
        "a fenced block was left open while extracting"
    );
    blocks
}

/// The quoted strings a probe fragment references as tools.
fn referenced_tools(block: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in block.lines() {
        let line = line.trim_start();
        let values = if let Some(rest) = line.strip_prefix("expect_tool") {
            rest.to_string()
        } else if let Some(rest) = line.strip_prefix("forbid_tools") {
            rest.to_string()
        } else {
            continue;
        };

        // `= "name"` and `= ["a", "b"]` are the two shapes the probe file allows.
        let mut rest = values.as_str();
        while let Some(start) = rest.find('"') {
            let after = &rest[start + 1..];
            match after.find('"') {
                Some(end) => {
                    names.push(after[..end].to_string());
                    rest = &after[end + 1..];
                }
                None => break,
            }
        }
    }
    names
}

/// The output-schema paths a probe fragment points at.
fn referenced_schemas(block: &str) -> Vec<String> {
    block
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("output_schema"))
        .filter_map(|rest| {
            let start = rest.find('"')? + 1;
            let end = rest[start..].find('"')? + start;
            Some(rest[start..end].to_string())
        })
        .collect()
}

/// A lockfile declaring one tool per name, with a schema permissive enough that any
/// argument validates — this test is about the probe files, not about the arguments.
fn catalog(names: &[String]) -> ToolCatalog {
    let mut dependencies: Vec<Dependency> = Vec::new();
    for name in names {
        let schema = serde_json::json!({ "type": "object" });
        let payload = canonical::to_vec(&schema).unwrap();
        dependencies.push(Dependency {
            id: format!("tool:doc.{name}"),
            kind: DependencyKind::Tool,
            facets: BTreeMap::from([
                ("input_schema".to_string(), recorded(&payload)),
                (
                    "description".to_string(),
                    recorded(&canonical::to_vec(&serde_json::json!("A tool.")).unwrap()),
                ),
            ]),
            source: Some("doc".to_string()),
        });
    }

    ToolCatalog::from_lockfile(&Lockfile::from_dependencies(&dependencies).unwrap()).unwrap()
}

fn recorded(payload: &[u8]) -> Facet {
    Facet {
        digest: Digest::sha256(payload),
        shape: None,
        normalized: Some(serde_json::from_slice(payload).unwrap()),
    }
}

/// Where a probe suite and its schemas are written for one block.
fn suite_directory(index: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("probes")).unwrap();
    let _ = index;
    dir
}

/// The `[probes]` section the loader is given: the directory the block was written to.
fn probes_config() -> ProbesConfig {
    ProbesConfig {
        path: "probes".to_string(),
        repeat: None,
    }
}

/// Load a probe block as the suite it claims to be.
fn load_as_suite(index: usize, block: &str) -> Result<(), String> {
    let dir = suite_directory(index);
    let tools = referenced_tools(block);
    for schema in referenced_schemas(block) {
        let path = dir.path().join(&schema);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, br#"{ "type": "object" }"#).unwrap();
    }
    std::fs::write(dir.path().join("probes/example.toml"), block).unwrap();

    probes::load_suite(dir.path(), &probes_config(), &catalog(&tools), None)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// The smallest documents a fragment could be part of, in the order they are tried.
fn fragment_wrappers(block: &str) -> Vec<String> {
    let header = "version = 1\n[agent]\nname = \"doc-example\"\n";
    vec![
        // A fragment that declares its own `[agent]` table cannot sit under this one.
        format!("version = 1\n{block}"),
        format!("{header}{block}"),
        format!(
            "{header}[model]\nprovider = \"ollama\"\nid = \"m\"\nendpoint = \"http://localhost:11434\"\n{block}"
        ),
        format!("[[probe]]\nname = \"doc-example\"\nprompt = \"An example.\"\n{block}"),
        // `expect_args` in full, including the expectation it depends on.
        format!(
            "[[probe]]\nname = \"doc-example\"\nprompt = \"An example.\"\nexpect_tool = \"doc_example\"\n{block}"
        ),
    ]
}

#[test]
fn every_documented_toml_example_parses() {
    let mut checked = 0;
    let mut probe_suites = 0;

    for document in DOCUMENTS {
        let path = root().join(document);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{} is not readable: {error}", path.display()));

        for (index, block) in toml_blocks(&text).into_iter().enumerate() {
            if block.trim().is_empty() {
                continue;
            }
            checked += 1;

            // A probe suite, checked against a catalog that declares its tools.
            // A block that is a whole probe suite, checked against a catalog that
            // declares its tools. A block that is only a *fragment* of one falls
            // through to the fragment attempts below; only a block that parses
            // nowhere is a failure.
            if block.contains("[[probe]]") && load_as_suite(index, &block).is_ok() {
                probe_suites += 1;
                continue;
            }

            // A complete configuration.
            if block.contains("version = 1")
                && Config::from_toml_at(&block, Path::new(document)).is_ok()
            {
                continue;
            }

            // A fragment, in each document that could contain it.
            let parsed = fragment_wrappers(&block).into_iter().any(|document| {
                Config::from_toml_at(&document, Path::new("fragment.toml")).is_ok()
                    || load_as_suite(index, &document).is_ok()
            });

            assert!(
                parsed,
                "the TOML example in {document} (block {}) parses in no context:\n{block}",
                index + 1
            );
        }
    }

    assert!(checked >= 20, "only {checked} examples were found");
    assert!(probe_suites >= 1, "no probe suite example was loaded");
}
