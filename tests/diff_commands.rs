// SPDX-License-Identifier: MIT OR Apache-2.0

//! The CLI-level contract of `agentchecksum diff`.
//!
//! These tests drive the real binary because two things can only be observed
//! there: the exit code, and the JSON that lands on stdout. The analysis itself is
//! covered by unit tests; what is checked here is the wiring and the promise that
//! machine-readable output stays parseable.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;

const CONFIG: &str = r#"
version = 1

[agent]
name = "diff-test"

[[prompts]]
path = "prompts/system.md"
"#;

/// The prompt text used as the baseline in every test below.
const BASELINE_PROMPT: &str = "Be concise. Always prefer tools.\n";

fn project(dir: &Path) {
    std::fs::create_dir_all(dir.join("prompts")).unwrap();
    std::fs::write(dir.join("prompts/system.md"), BASELINE_PROMPT).unwrap();
    std::fs::write(dir.join("agentchecksum.toml"), CONFIG).unwrap();
}

fn set_prompt(dir: &Path, text: &str) {
    std::fs::write(dir.join("prompts/system.md"), text).unwrap();
}

fn run(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::cargo_bin("agentchecksum")
        .unwrap()
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}

/// Write the baseline and fail loudly if that step is what broke.
fn committed_baseline(dir: &Path) -> PathBuf {
    let output = run(dir, &["snapshot"]);
    assert!(
        output.status.success(),
        "snapshot failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    dir.join("agentchecksum.lock")
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

#[test]
fn diff_without_changes_says_so_and_shows_both_checksums() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    committed_baseline(dir.path());

    let output = run(dir.path(), &["diff"]);
    let text = stdout(&output);

    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(text.contains("No dependency changes detected."), "{text}");
    assert!(text.contains("Baseline: ac1:"), "{text}");
    assert!(text.contains("Current:  ac1:"), "{text}");
    assert!(!text.contains("Overall behavioral risk"), "{text}");
}

#[test]
fn diff_reports_the_changed_prompt_and_still_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    committed_baseline(dir.path());
    set_prompt(dir.path(), "Be concise.\n");

    let output = run(dir.path(), &["diff"]);
    let text = stdout(&output);

    // Risky or not, `diff` reports: whether a change should block a merge is the
    // gate's decision, and the exit code has to keep meaning "the comparison ran".
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(text.contains("1 dependency changed."), "{text}");
    assert!(text.contains("PROMPT  prompts/system.md"), "{text}");
    assert!(
        text.contains("Overall behavioral risk: MEDIUM (heuristic)"),
        "{text}"
    );
    assert!(text.contains("    classification: text-changed"), "{text}");
}

#[test]
fn a_formatting_only_change_is_low_and_shows_the_facet_it_did_not_move() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    committed_baseline(dir.path());
    // Same words, different layout: the content digest moves, the shape does not.
    set_prompt(dir.path(), "Be concise.\n\nAlways prefer tools.\n");

    let text = stdout(&run(dir.path(), &["diff"]));

    assert!(
        text.contains("Overall behavioral risk: LOW (heuristic)"),
        "{text}"
    );
    assert!(
        text.contains("    classification: formatting-only"),
        "{text}"
    );
    assert!(text.contains("shape"), "{text}");
    assert!(text.contains("unchanged"), "{text}");
}

#[test]
fn diff_json_is_the_documented_contract() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    committed_baseline(dir.path());
    set_prompt(dir.path(), "Be concise.\n\nAlways prefer tools.\n");

    let output = run(dir.path(), &["diff", "--format", "json"]);
    assert_eq!(output.status.code(), Some(0));
    let document: Value = serde_json::from_str(&stdout(&output)).unwrap();

    assert_eq!(document["status"], "ok");
    assert_eq!(document["changed"], true);
    assert_eq!(document["overall_risk"], "low");
    assert!(
        document["baseline_checksum"]
            .as_str()
            .unwrap()
            .starts_with("ac1:")
    );
    assert!(
        document["current_checksum"]
            .as_str()
            .unwrap()
            .starts_with("ac1:")
    );

    let changes = document["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1);
    let change = &changes[0];
    assert_eq!(change["id"], "prompt:prompts/system.md");
    assert_eq!(change["kind"], "prompt");
    assert_eq!(change["change"], "modified");
    assert_eq!(change["risk"], "low");

    let facets = change["facets"].as_array().unwrap();
    let content = facets
        .iter()
        .find(|facet| facet["name"] == "content")
        .unwrap();
    assert_eq!(content["change"], "modified");
    assert_eq!(content["risk"], "low");
    assert!(
        content["before_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert!(
        content["after_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(content["details"][0]["path"], "classification");
    assert_eq!(content["details"][0]["after"], "formatting-only");
    // Details never carry a stale `before`.
    assert!(content["details"][0].get("before").is_none());

    // A facet that was compared and found equal is still part of the report: it
    // is the evidence behind "the schema did not break, the description did".
    let shape = facets
        .iter()
        .find(|facet| facet["name"] == "shape")
        .unwrap();
    assert_eq!(shape["change"], "unchanged");
    assert_eq!(shape["risk"], "none");
    assert_eq!(shape["before_digest"], shape["after_digest"]);
    assert_eq!(shape["details"].as_array().unwrap().len(), 0);
}

#[test]
fn a_missing_baseline_exits_3_and_suggests_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());

    let output = run(dir.path(), &["diff"]);

    assert_eq!(output.status.code(), Some(3), "{}", stdout(&output));
    assert!(stdout(&output).is_empty(), "stdout must stay clean");
    let message = stderr(&output);
    assert!(message.contains("does not exist"), "{message}");
    assert!(message.contains("agentchecksum snapshot"), "{message}");
}

#[test]
fn a_hand_edited_lockfile_is_refused_rather_than_trusted() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let lock = committed_baseline(dir.path());

    // Hand-edit only the recorded aggregate, leaving the entries beside it: the
    // comparison would then be against a baseline that never existed.
    let mut document: Value =
        serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
    document["agent_checksum"] = Value::String(format!("ac1:{}", "0".repeat(64)));
    std::fs::write(&lock, serde_json::to_string_pretty(&document).unwrap()).unwrap();

    let output = run(dir.path(), &["diff"]);

    assert_eq!(output.status.code(), Some(3), "{}", stdout(&output));
    let message = stderr(&output);
    assert!(
        message.contains("does not describe its own contents"),
        "{message}"
    );
    assert!(message.contains("computed"), "{message}");
}

#[test]
fn diff_never_writes_the_lockfile() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let lock = committed_baseline(dir.path());
    let before = std::fs::read_to_string(&lock).unwrap();

    set_prompt(dir.path(), "A completely different instruction.\n");
    let output = run(dir.path(), &["diff"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        std::fs::read_to_string(&lock).unwrap(),
        before,
        "diff must leave the committed baseline alone"
    );
}

/// Replace fingerprints with a placeholder.
///
/// The golden pins the *shape* of the report — which columns exist, what is
/// named, what is admitted as unchanged — not the hash of one fixture prompt.
fn redact(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let tokens: Vec<String> = line
            .split(' ')
            .map(|token| match token.split_once(':') {
                Some((algorithm @ ("ac1" | "sha256"), _)) => format!("{algorithm}:…"),
                _ => token.to_string(),
            })
            .collect();
        out.push_str(&tokens.join(" "));
        out.push('\n');
    }
    out
}

#[test]
fn from_compares_against_a_supplied_lockfile() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let lock = committed_baseline(dir.path());

    // The base revision's lockfile, set aside the way CI would:
    // `git show HEAD:agentchecksum.lock > old.lock`.
    let old = dir.path().join("old.lock");
    std::fs::copy(&lock, &old).unwrap();

    set_prompt(dir.path(), "A different instruction.\n");
    committed_baseline(dir.path());

    let against_base = stdout(&run(dir.path(), &["diff", "--from", "old.lock"]));
    assert!(
        against_base.contains("1 dependency changed."),
        "{against_base}"
    );

    let against_head = stdout(&run(dir.path(), &["diff"]));
    assert!(
        against_head.contains("No dependency changes detected."),
        "{against_head}"
    );
}

/// The rendered report is a reviewed artifact: the demo's PR comment is this
/// layout, so a change to it should be a deliberate, visible diff.
#[test]
fn the_human_diff_report_is_a_stable_shape() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    committed_baseline(dir.path());
    set_prompt(dir.path(), "Be concise.\n\nAlways prefer tools.\n");

    let text = stdout(&run(dir.path(), &["diff"]));

    insta::assert_snapshot!(redact(&text));
}
