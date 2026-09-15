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

    assert!(
        output.status.success(),
        "exit was {:?}",
        output.status.code()
    );
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
fn snapshot_exits_3_and_leaves_a_newer_lockfile_intact() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    write_config(dir.path());

    let path = dir.path().join("agentchecksum.lock");
    let future = r#"{"lock_version": 2, "schema": "future"}"#;
    std::fs::write(&path, future).unwrap();

    let output = Command::cargo_bin("agentchecksum")
        .unwrap()
        .current_dir(dir.path())
        .arg("snapshot")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Upgrade agentchecksum"), "{stderr}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), future);
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

#[test]
fn an_unknown_config_key_names_the_offending_field_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    std::fs::write(
        dir.path().join("agentchecksum.toml"),
        "version = 1\n\n[agentt]\nname = \"typo\"\n",
    )
    .unwrap();

    let output = Command::cargo_bin("agentchecksum")
        .unwrap()
        .current_dir(dir.path())
        .arg("snapshot")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("caused by:"), "{stderr}");
    // The suggestion tells the user to fix the reported key, so the cause chain
    // has to name it: `unknown field \`agentt\`` is the whole point of the report.
    assert!(
        stderr.contains("agentt"),
        "the diagnostic must name the offending key: {stderr}"
    );
}

#[test]
fn init_json_output_is_valid_json_on_stdout() {
    let dir = tempfile::tempdir().unwrap();

    let output = Command::cargo_bin("agentchecksum")
        .unwrap()
        .current_dir(dir.path())
        .args(["--format", "json", "init"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output.status.code());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "ok");
    assert_eq!(value["config"], "agentchecksum.toml");
    assert_eq!(value["probes"], "probes");
}

/// A lockfile that exists and could be overwritten but cannot be read is the one
/// case where refusing is the only safe direction: we cannot tell which format we
/// would be destroying, so "unreadable" must not be treated as "not our format".
#[cfg(unix)]
#[test]
fn snapshot_refuses_a_newer_lockfile_it_cannot_read() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    write_config(dir.path());

    let path = dir.path().join("agentchecksum.lock");
    let future = r#"{"lock_version": 2, "schema": "future"}"#;
    std::fs::write(&path, future).unwrap();

    // Write-only: present, writable, and not readable.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o200)).unwrap();
    let read_was_denied = std::fs::read(&path).is_err();

    let output = Command::cargo_bin("agentchecksum")
        .unwrap()
        .current_dir(dir.path())
        .arg("snapshot")
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(3),
        "an unreadable lockfile must be refused rather than overwritten (this \
         interpreter read it despite the mode, so only the version probe applied: {})",
        !read_was_denied,
    );

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        future,
        "the refusal must leave the file untouched"
    );
}
