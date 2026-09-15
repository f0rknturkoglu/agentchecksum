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

    let mut value: serde_json::Value = serde_json::from_slice(&lock.to_bytes().unwrap()).unwrap();
    redact(&mut value);

    insta::assert_json_snapshot!(value);
}
