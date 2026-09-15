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
        matches!(
            err,
            agentchecksum::error::Error::LockVersion { found: 99, .. }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn unknown_fields_inside_a_known_lock_version_are_tolerated() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());

    let mut value: serde_json::Value =
        serde_json::from_slice(&lock_bytes(dir.path()).await).unwrap();
    let object = value.as_object_mut().unwrap();
    object.insert("future_top_level".to_string(), serde_json::json!("ignored"));
    object
        .get_mut("generator")
        .and_then(|value| value.as_object_mut())
        .unwrap()
        .insert("future_generator_field".to_string(), serde_json::json!(1));

    {
        let dependencies = object
            .get_mut("dependencies")
            .and_then(|value| value.as_object_mut())
            .unwrap();
        let (_, dependency) = dependencies.iter_mut().next().unwrap();
        let dependency = dependency.as_object_mut().unwrap();
        dependency.insert(
            "future_dependency_field".to_string(),
            serde_json::json!(true),
        );

        let facets = dependency
            .get_mut("facets")
            .and_then(|value| value.as_object_mut())
            .unwrap();
        for facet in facets.values_mut() {
            facet
                .as_object_mut()
                .unwrap()
                .insert("future_facet_field".to_string(), serde_json::json!("x"));
        }
    }

    // Guard against the injection silently doing nothing: all four unknown
    // fields must be in the bytes we are about to hand to `read`.
    let text = serde_json::to_string(&value).unwrap();
    for injected in [
        "future_top_level",
        "future_generator_field",
        "future_dependency_field",
        "future_facet_field",
    ] {
        assert!(
            text.contains(injected),
            "injection missing `{injected}`:\n{text}"
        );
    }

    let path = dir.path().join("agentchecksum.lock");
    std::fs::write(&path, text).unwrap();

    assert!(Lockfile::read(&path).is_ok());
}

#[tokio::test]
async fn the_lockfile_checksum_is_wired_from_the_dependency_inputs() {
    let dir = tempfile::tempdir().unwrap();
    write_project(dir.path());

    let config = Config::load(&dir.path().join("agentchecksum.toml")).unwrap();
    let discovery = discovery::run(&config, dir.path()).await.unwrap();

    // The lockfile's checksum must be the aggregate over the dependency
    // inputs, not something derived from the lockfile's own serialization.
    // Comparing two independently computed values is what pins that wiring.
    assert_eq!(
        Lockfile::from_dependencies(&discovery.dependencies)
            .unwrap()
            .agent_checksum
            .as_str(),
        agentchecksum::manifest::agent_checksum(&discovery.dependencies)
            .unwrap()
            .as_str()
    );
}
