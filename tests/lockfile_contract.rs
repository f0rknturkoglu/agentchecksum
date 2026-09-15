// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeMap;

use agentchecksum::config::Config;
use agentchecksum::discovery;
use agentchecksum::lockfile::Lockfile;
use agentchecksum::manifest::{Dependency, DependencyKind, Digest, Facet};

/// A dependency with one facet, so a key-ordering pin needs no HTTP or config.
fn one_facet(id: &str, kind: DependencyKind) -> Dependency {
    let mut facets = BTreeMap::new();
    facets.insert(
        "content".to_string(),
        Facet {
            digest: Digest::sha256(id.as_bytes()),
            shape: None,
            normalized: None,
        },
    );
    Dependency {
        id: id.to_string(),
        kind,
        facets,
        source: None,
    }
}

#[test]
fn the_written_lockfile_lists_dependencies_in_id_order() {
    // The committed lockfile groups dependencies by id-prefixed key. Swapping the
    // map for an order-preserving container would keep every other test green
    // while rewriting every committed lockfile, so the rendered bytes are what
    // have to be pinned, not the container's internals.
    let lock = Lockfile::from_dependencies(&[
        one_facet("model:ollama/qwen3:8b", DependencyKind::Model),
        one_facet("prompt:b.md", DependencyKind::Prompt),
        one_facet("prompt:a.md", DependencyKind::Prompt),
    ])
    .unwrap();

    let text = String::from_utf8(lock.to_bytes().unwrap()).unwrap();
    let model = text.find("model:ollama/qwen3:8b").unwrap();
    let prompt_a = text.find("prompt:a.md").unwrap();
    let prompt_b = text.find("prompt:b.md").unwrap();

    assert!(
        model < prompt_a && prompt_a < prompt_b,
        "dependencies must be written in id order, got:\n{text}"
    );
}

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

    let mut value: serde_json::Value = serde_json::from_slice(&lock.to_bytes().unwrap()).unwrap();
    redact(&mut value);

    insta::assert_json_snapshot!(value);
}
