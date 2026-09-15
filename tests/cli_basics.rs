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
