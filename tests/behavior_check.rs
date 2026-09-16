// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end `check`: the real binary, a real model endpoint, a real baseline.
//!
//! These tests drive the CLI against `examples/openai_fixture_server`, which speaks the
//! OpenAI chat-completions contract over a real socket and records every request it
//! receives. What they check is the part no unit test can reach: the exit code, the
//! report that lands on stdout, and the promise that `check` never says PASS about a
//! check it could not evaluate.
//!
//! The fixture's answers are data, so a test states the behavior it needs — a text turn,
//! a tool call, an HTTP 500 — instead of writing Rust for it. Its request log is what
//! proves `--diff-only` never contacted a model. Every server listens on `127.0.0.1`
//! only; nothing here touches the network.

use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Output, Stdio};
use std::time::{Duration, Instant};

use agentchecksum::fingerprint::canonical;
use agentchecksum::lockfile::Lockfile;
use agentchecksum::manifest::{Dependency, DependencyKind, Digest, Facet};
use assert_cmd::Command;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// The fixture server
// ---------------------------------------------------------------------------

/// A fixture server this test started itself, killed when the test ends.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The fixture server, next to this test binary in the build directory.
///
/// `cargo test --test behavior_check` does not build example targets and does not
/// rebuild a stale one either, so the example this test drives is built once per test
/// process: a test that ran against a binary from an earlier edit would be evidence
/// about code nobody is looking at.
fn fixture() -> PathBuf {
    static BUILT: std::sync::LazyLock<PathBuf> =
        std::sync::LazyLock::new(|| example("openai_fixture_server"));
    BUILT.clone()
}

/// The MCP fixture server, for the one test that needs a real tool catalog.
fn mcp_fixture() -> PathBuf {
    static BUILT: std::sync::LazyLock<PathBuf> =
        std::sync::LazyLock::new(|| example("mcp_fixture_server"));
    BUILT.clone()
}

/// Build one example target in this test process's profile and return its path.
fn example(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary has a path");
    let profile = exe
        .parent()
        .and_then(Path::parent)
        .expect("the test binary lives in <target>/<profile>/deps")
        .to_path_buf();
    let path = profile.join("examples").join(name);

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut command = StdCommand::new(cargo);
    command
        .args(["build", "--example", name])
        .current_dir(env!("CARGO_MANIFEST_DIR"));
    if profile.file_name().is_some_and(|n| n == "release") {
        command.arg("--release");
    }
    let status = command.status().expect("cargo runs");
    assert!(status.success(), "building the `{name}` example failed");

    assert!(
        path.exists(),
        "the `{name}` example is not built at {}",
        path.display()
    );
    path
}

/// Start the fixture for a project and wait for the port it bound.
///
/// `responses` are served in order, one per request; the last one repeats, so a
/// `repeat = N` capture against a single-response spec is N identical samples.
fn start(project: &Path, responses: Vec<Value>) -> (Server, u16) {
    let spec_path = project.join("spec.json");
    let spec = json!({
        "responses": responses,
        "record_file": project.join("requests.jsonl").display().to_string(),
    });
    std::fs::write(&spec_path, serde_json::to_vec_pretty(&spec).unwrap()).unwrap();

    // A fixture reads its spec once, at startup, so a port file left by an earlier
    // process must not be read as this one's.
    let port_file = project.join("port");
    let _ = std::fs::remove_file(&port_file);

    let child = StdCommand::new(fixture())
        .env("AC_FIXTURE_SPEC", &spec_path)
        .env("AC_FIXTURE_PORT_FILE", &port_file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the fixture server starts");
    let guard = Server(child);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(&port_file)
            && let Ok(port) = text.trim().parse::<u16>()
        {
            return (guard, port);
        }
        assert!(
            Instant::now() < deadline,
            "the fixture server never reported a listening port"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn text(content: &str) -> Value {
    json!({ "type": "text", "content": content })
}

/// A tool call the model emits. With no tool in the catalog, the name is recorded as an
/// invented call — which is exactly what a restraint or a forbidden-tool probe measures.
fn tool_call(name: &str) -> Value {
    tool_call_with(name, "{}")
}

/// A tool call with arguments, as the wire's JSON string.
fn tool_call_with(name: &str, arguments: &str) -> Value {
    json!({
        "type": "tool_calls",
        "calls": [{ "name": name, "arguments": arguments }],
    })
}

/// Every request the fixture received for a project, in order.
fn recorded(project: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(project.join("requests.jsonl")) else {
        return Vec::new();
    };
    text.lines()
        .map(|line| serde_json::from_str(line).expect("a recorded request is JSON"))
        .collect()
}

// ---------------------------------------------------------------------------
// The project under test
// ---------------------------------------------------------------------------

/// A project directory: a config, a prompt, a probe suite, and the endpoint it samples.
struct Project {
    dir: tempfile::TempDir,
    server: Option<Server>,
}

impl Project {
    /// Start the fixture, then write the config that points at it.
    ///
    /// `policy` is appended to the config verbatim, so a test states the thresholds it
    /// needs without rebuilding the whole file.
    fn new(responses: Vec<Value>, policy: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("prompts")).unwrap();
        std::fs::write(dir.path().join("prompts/system.md"), "Be concise.\n").unwrap();

        let (server, port) = start(dir.path(), responses);
        std::fs::write(dir.path().join("agentchecksum.toml"), config(port, policy)).unwrap();

        Self {
            dir,
            server: Some(server),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Write one probe file.
    fn probe(&self, file: &str, body: &str) {
        let probes = self.path().join("probes");
        std::fs::create_dir_all(&probes).unwrap();
        std::fs::write(probes.join(file), body).unwrap();
    }

    /// Write a JSON Schema the probes can point at.
    fn schema(&self, file: &str, schema: &str) {
        let schemas = self.path().join("schemas");
        std::fs::create_dir_all(&schemas).unwrap();
        std::fs::write(schemas.join(file), schema).unwrap();
    }

    fn set_prompt(&self, text: &str) {
        std::fs::write(self.path().join("prompts/system.md"), text).unwrap();
    }

    /// Stop the fixture. A later run that still needs a model then fails instead of
    /// quietly reusing a server the test thinks is gone.
    fn stop_fixture(&mut self) {
        self.server = None;
    }

    /// Every request the fixture received, in order.
    fn requests(&self) -> Vec<Value> {
        recorded(self.path())
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::cargo_bin("agentchecksum")
            .unwrap()
            .current_dir(self.path())
            .args(args)
            .output()
            .unwrap()
    }

    /// The committed dependency baseline, failing loudly if that step is what broke.
    fn snapshot(&self) {
        let output = self.run(&["snapshot"]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "snapshot failed: {}",
            stderr(&output)
        );
    }

    fn baseline_path(&self) -> PathBuf {
        self.path().join(".agentchecksum/baseline.json")
    }

    fn baseline(&self) -> Value {
        let text = std::fs::read_to_string(self.baseline_path()).expect("the baseline exists");
        serde_json::from_str(&text).expect("the baseline is JSON")
    }

    fn lock_bytes(&self) -> Vec<u8> {
        std::fs::read(self.path().join("agentchecksum.lock")).expect("the lockfile exists")
    }

    /// Append to this project's configuration.
    fn append_config(&self, extra: &str) {
        let path = self.path().join("agentchecksum.toml");
        let mut config = std::fs::read_to_string(&path).unwrap();
        config.push_str(extra);
        std::fs::write(&path, config).unwrap();
    }

    /// Declare one MCP server, so a probe resolves against a real tool catalog.
    ///
    /// A catalog is what tool identities are checked against, so a test about them
    /// needs one a real server declared rather than a fixture-shaped lockfile.
    fn declare_tools(&self, tools: &[(&str, Value)]) {
        let spec = self.path().join("mcp.json");
        let declared: Vec<Value> = tools
            .iter()
            .map(|(name, schema)| {
                json!({
                    "name": name,
                    "description": format!("The {name} tool."),
                    "input_schema": schema,
                })
            })
            .collect();
        std::fs::write(
            &spec,
            serde_json::to_vec_pretty(&json!({ "tools": declared })).unwrap(),
        )
        .unwrap();

        self.append_config(&format!(
            "\n[[mcp.servers]]\nname = \"local\"\ntransport = \"stdio\"\ncommand = \"{}\"\n\
             args = [\"--stdio\"]\nenv = {{ AC_FIXTURE_SPEC = \"{}\" }}\n",
            mcp_fixture().display(),
            spec.display(),
        ));
    }

    /// The first run artifact `check` left behind.
    fn artifact(&self) -> PathBuf {
        let mut artifacts = self.run_artifacts();
        assert_eq!(artifacts.len(), 1, "one run, one artifact: {artifacts:?}");
        artifacts.remove(0)
    }

    /// The run artifacts `check` left behind, addressed by their own contents.
    fn run_artifacts(&self) -> Vec<PathBuf> {
        let runs = self.path().join(".agentchecksum/runs");
        let mut paths: Vec<PathBuf> = std::fs::read_dir(runs)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                    .collect()
            })
            .unwrap_or_default();
        paths.sort();
        paths
    }
}

/// Write a single-trace file derived from the evidence a real run produced.
///
/// `mutate` changes exactly one fact, and everything else — the agent checksum, the
/// catalog digest, the runner, the probe name and its digest — is copied from evidence
/// captured in this project. That is what makes a refusal attributable: the fixture
/// cannot be blamed for a second mistake nobody made.
fn derived_trace(
    project: &Path,
    artifact: &Path,
    name: &str,
    mutate: impl FnOnce(&mut Value),
) -> PathBuf {
    let run: Value = serde_json::from_slice(&std::fs::read(artifact).unwrap()).unwrap();
    let mut trace = run["traces"][0].clone();
    mutate(&mut trace);

    let path = project.join(name);
    std::fs::write(&path, serde_json::to_vec_pretty(&trace).unwrap()).unwrap();
    path
}

/// The config every test starts from: one prompt, a model pointed at the fixture, and
/// whatever policy the test appends.
fn config(port: u16, policy: &str) -> String {
    format!(
        "version = 1\n\n[agent]\nname = \"behavior-check-test\"\n\n\
         [[prompts]]\npath = \"prompts/system.md\"\n\n\
         [model]\nprovider = \"openai-compatible\"\nid = \"fixture-model\"\n\
         endpoint = \"http://127.0.0.1:{port}\"\n\n{policy}"
    )
}

/// A restraint probe: the one expectation that needs no tool catalog to be meaningful.
const RESTRAINT_PROBE: &str = r#"
[[probe]]
name = "no-tools"
prompt = "Answer without calling any tool: what is the capital of Portugal?"
expect_no_tool = true
"#;

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn exit_code(output: &Output) -> Option<i32> {
    output.status.code()
}

// ---------------------------------------------------------------------------
// 1. The pass path, and the baseline that makes it a comparison
// ---------------------------------------------------------------------------

/// The load-bearing test of the whole command: a project that passes, a baseline
/// recorded from it, and a second run that compares against that baseline rather than
/// merely reporting its own scores.
#[test]
fn a_passing_check_records_a_baseline_and_then_passes_against_it() {
    let project = Project::new(vec![text("Lisbon.")], "");
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();

    let lock_before = project.lock_bytes();

    // First run: nothing to compare against, so the gate says DRIFT rather than
    // inventing a pass. `--accept` records it.
    let accepted = project.run(&["check", "--accept"]);
    assert_eq!(exit_code(&accepted), Some(0), "{}", stderr(&accepted));
    assert!(
        project.baseline_path().is_file(),
        "the baseline was written"
    );

    let baseline = project.baseline();
    assert_eq!(baseline["baseline_version"], 1);
    assert_eq!(baseline["metrics"]["tool_restraint"]["passed"], 1);
    assert_eq!(baseline["metrics"]["tool_restraint"]["total"], 1);
    assert_eq!(baseline["probes"]["no-tools"]["passed"], 1);
    assert!(
        baseline["probe_suite_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert!(baseline["runner_contract"].is_string());

    // Second run: the baseline exists and describes this suite.
    let compared = project.run(&["check"]);
    let report = stdout(&compared);
    assert_eq!(exit_code(&compared), Some(0), "{report}");
    assert!(
        report.contains("Behavioral probes: 1 / 1 passed"),
        "{report}"
    );
    assert!(report.contains("tool_restraint"), "{report}");
    assert!(report.contains("PASS"), "{report}");
    assert!(report.contains("Behavior Gate: PASS"), "{report}");
    assert!(!report.contains("DRIFT"), "{report}");

    // `check` reads the lockfile and never writes it.
    assert_eq!(project.lock_bytes(), lock_before);
}

/// A check that fails a threshold names the metric, the score it measured, and the
/// constraint it missed — and exits 1.
#[test]
fn a_regression_names_the_failing_metric_and_exits_one() {
    let project = Project::new(
        vec![text("Lisbon."), tool_call("delete_everything")],
        "[policy.metrics.tool_restraint]\nmin = 1.0\n",
    );
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();

    let accepted = project.run(&["check", "--accept"]);
    assert_eq!(exit_code(&accepted), Some(0), "{}", stderr(&accepted));
    let before = std::fs::read_to_string(project.baseline_path()).unwrap();

    // The same probe, a different answer: the model reaches for a tool when it was
    // asked not to. `--refresh` is what makes the fixture answer again rather than the
    // cache answering for it.
    let regressed = project.run(&["check", "--refresh"]);
    let report = stdout(&regressed);

    assert_eq!(exit_code(&regressed), Some(1), "{report}");
    assert!(report.contains("Behavior Gate: FAIL"), "{report}");
    assert!(report.contains("tool_restraint"), "{report}");
    assert!(report.contains("FAIL"), "{report}");
    assert!(
        report.contains("below the required minimum 1.0000"),
        "the report must say which constraint failed: {report}"
    );
    assert!(report.contains("Failing probes:"), "{report}");
    assert!(
        report.contains("no-tools  0 / 1 passed"),
        "the failing probe must be named: {report}"
    );

    // A run that failed its own policy must not replace the accepted baseline.
    assert_eq!(
        std::fs::read_to_string(project.baseline_path()).unwrap(),
        before
    );
}

/// `--accept` with a failing policy is refused, visibly: exit 1 (the gate failed), no
/// baseline written, and a diagnostic that says so.
#[test]
fn accept_refuses_a_run_that_did_not_pass() {
    let project = Project::new(
        vec![tool_call("delete_everything")],
        "[policy.metrics.tool_restraint]\nmin = 1.0\n",
    );
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();

    let refused = project.run(&["check", "--accept"]);

    assert_eq!(exit_code(&refused), Some(1), "{}", stdout(&refused));
    assert!(!project.baseline_path().exists(), "nothing was accepted");
    assert!(
        stderr(&refused).contains("did not write"),
        "the refusal must be visible: {}",
        stderr(&refused)
    );
}

// ---------------------------------------------------------------------------
// 2. Drift: what a missing or incomparable baseline means
// ---------------------------------------------------------------------------

/// A probe that fails is a measurement, not a verdict: without a policy naming that
/// metric, the gate reports the score and passes — and the baseline records exactly
/// what was measured, which is what makes the next comparison honest.
#[test]
fn a_probe_that_fails_an_unconstrained_metric_is_reported_rather_than_gated() {
    // Two samples: one restraint, one where the model reaches for a tool.
    let project = Project::new(
        vec![text("Lisbon."), tool_call("delete_everything")],
        "[probes]\nrepeat = 2\n",
    );
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();

    let reported = project.run(&["check", "--accept"]);
    let report = stdout(&reported);
    assert_eq!(exit_code(&reported), Some(0), "{report}");
    assert!(
        report.contains("Behavioral probes: 1 / 2 passed"),
        "{report}"
    );
    assert!(report.contains("Failing probes:"), "{report}");
    assert!(
        report.contains("no-tools  1 / 2 passed"),
        "the probe that failed must be named: {report}"
    );

    // The baseline keeps the counts, not a rounded "good enough".
    let baseline = project.baseline();
    assert_eq!(baseline["metrics"]["tool_restraint"]["passed"], 1);
    assert_eq!(baseline["metrics"]["tool_restraint"]["total"], 2);
    assert_eq!(baseline["probes"]["no-tools"]["passed"], 1);
    assert_eq!(baseline["probes"]["no-tools"]["total"], 2);

    // No policy constrains the metric, so the score does not fail the gate — and the
    // row says WARN rather than PASS, because "nothing failed" and "the behavior was
    // good" are different claims.
    let compared = project.run(&["check"]);
    let report = stdout(&compared);
    assert_eq!(exit_code(&compared), Some(0), "{report}");
    assert!(report.contains("Behavior Gate: PASS"), "{report}");
    assert!(report.contains("50% → 50%  WARN"), "{report}");
}

/// No baseline is drift, not a pass — and drift is a report unless policy asks for a
/// gate. The same project, two runs, two exit codes; the flag is the only difference.
#[test]
fn a_missing_baseline_is_drift_unless_fail_on_drift_asks_for_a_gate() {
    let project = Project::new(vec![text("Lisbon.")], "");
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();

    let reported = project.run(&["check"]);
    let report = stdout(&reported);
    assert_eq!(exit_code(&reported), Some(0), "{report}");
    assert!(report.contains("Behavior Gate: DRIFT"), "{report}");
    assert!(report.contains("no behavioral baseline exists"), "{report}");
    // Nothing to compare against, so no "before": an absent score is not 0%.
    assert!(report.contains("n/a → 100%"), "{report}");
    assert!(!report.contains("Behavior Gate: PASS"), "{report}");

    let gated = project.run(&["check", "--fail-on-drift"]);
    let report = stdout(&gated);
    assert_eq!(exit_code(&gated), Some(1), "{report}");
    assert!(report.contains("Behavior Gate: DRIFT"), "{report}");
}

/// A baseline that describes a different probe suite answers different questions, so
/// the run drifts instead of comparing two tests.
#[test]
fn a_changed_probe_suite_is_drift_rather_than_a_regression() {
    let project = Project::new(vec![text("Lisbon.")], "");
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();

    assert_eq!(exit_code(&project.run(&["check", "--accept"])), Some(0));
    let accepted = stdout(&project.run(&["check"]));
    assert!(accepted.contains("Behavior Gate: PASS"), "{accepted}");

    // The assertion changed: the recorded score is no longer about this probe.
    project.probe(
        "no-tools.toml",
        &RESTRAINT_PROBE.replace("Portugal", "Portugal, in one word"),
    );

    let changed = project.run(&["check"]);
    let report = stdout(&changed);
    assert_eq!(exit_code(&changed), Some(0), "{report}");
    assert!(report.contains("Behavior Gate: DRIFT"), "{report}");
    assert!(report.contains("behavior probe suite changed"), "{report}");
    // The baseline is not comparable, so it is not shown as this run's "before".
    assert!(report.contains("n/a → 100%"), "{report}");
}

// ---------------------------------------------------------------------------
// 3. `--accept`'s refusals
// ---------------------------------------------------------------------------

/// Accepting behavior while the agent itself has moved would attribute the scores to
/// the wrong revision, so `--accept` refuses — before contacting a model.
#[test]
fn accept_is_refused_while_the_dependency_state_has_moved() {
    let project = Project::new(vec![text("Lisbon.")], "");
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();
    project.set_prompt("Be terse.\n");

    let refused = project.run(&["check", "--accept"]);

    assert_eq!(exit_code(&refused), Some(2), "{}", stdout(&refused));
    assert!(
        stderr(&refused).contains("`--accept`"),
        "{}",
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("1 dependency changed"),
        "the refusal must say what moved: {}",
        stderr(&refused)
    );
    assert!(!project.baseline_path().exists(), "nothing was accepted");
    assert!(
        project.requests().is_empty(),
        "the refusal must cost nothing: {:?}",
        project.requests()
    );
}

// ---------------------------------------------------------------------------
// 4. Skipping the probes
// ---------------------------------------------------------------------------

/// `--diff-only` and `--no-probes` gate on the dependency half alone, and the fixture's
/// request log is what proves no model was contacted.
#[test]
fn diff_only_and_no_probes_never_contact_the_model() {
    let project = Project::new(vec![text("Lisbon.")], "");
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();

    for flag in ["--diff-only", "--no-probes"] {
        let output = project.run(&["check", flag]);
        let report = stdout(&output);

        assert_eq!(exit_code(&output), Some(0), "{report}");
        assert!(
            report.contains("No dependency changes detected."),
            "{report}"
        );
        assert!(report.contains("Behavioral probes: not run."), "{report}");
        assert!(report.contains("Behavior Gate: PASS"), "{report}");
        assert!(
            !report.contains("tool_restraint"),
            "no probe ran, so no metric can be reported: {report}"
        );
        assert!(
            project.requests().is_empty(),
            "{flag} must not contact the model: {:?}",
            project.requests()
        );
    }
}

/// `--from` selects the lockfile the static half compares against, exactly as it does
/// for `diff`.
#[test]
fn diff_only_compares_against_the_lockfile_from_names() {
    let project = Project::new(vec![text("Lisbon.")], "");
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();

    let base = project.path().join("base.lock");
    std::fs::write(&base, project.lock_bytes()).unwrap();

    // Move the committed state on, and commit that.
    project.set_prompt("Be terse.\n");
    project.snapshot();

    let committed = project.run(&["check", "--diff-only"]);
    assert_eq!(exit_code(&committed), Some(0), "{}", stdout(&committed));
    assert!(
        stdout(&committed).contains("No dependency changes detected."),
        "{}",
        stdout(&committed)
    );

    let against_base = project.run(&[
        "check",
        "--diff-only",
        "--fail-on-drift",
        "--from",
        base.to_str().unwrap(),
    ]);
    let report = stdout(&against_base);
    assert_eq!(exit_code(&against_base), Some(1), "{report}");
    assert!(report.contains("Behavior Gate: DRIFT"), "{report}");
    assert!(report.contains("dependency checksum changed"), "{report}");
}

/// `--fail-on-risk` overrides `[policy].fail_on_risk` rather than adding to it.
#[test]
fn fail_on_risk_overrides_the_configured_policy() {
    let project = Project::new(vec![text("Lisbon.")], "");
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();
    project.set_prompt("Be terse.\n");

    // A changed prompt is reported, not failed, by default.
    let silent = project.run(&["check", "--diff-only"]);
    assert_eq!(exit_code(&silent), Some(0), "{}", stdout(&silent));

    // With a threshold, the same change is a gate failure — and the report still says
    // what happened rather than only that something did.
    let gated = project.run(&["check", "--diff-only", "--fail-on-risk", "medium"]);
    let report = stdout(&gated);
    assert_eq!(exit_code(&gated), Some(1), "{report}");
    assert!(report.contains("Behavior Gate: FAIL"), "{report}");

    // A threshold the change does not reach is still not a failure.
    let below = project.run(&["check", "--diff-only", "--fail-on-risk", "critical"]);
    assert_eq!(exit_code(&below), Some(0), "{}", stdout(&below));
}

// ---------------------------------------------------------------------------
// 5. The output contracts
// ---------------------------------------------------------------------------

/// `--format json` writes the report alone to stdout, with the documented keys.
#[test]
fn json_output_carries_the_documented_keys() {
    let project = Project::new(vec![text("Lisbon.")], "");
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();

    let output = project.run(&["--format", "json", "check"]);
    assert_eq!(exit_code(&output), Some(0), "{}", stderr(&output));

    let report: Value = serde_json::from_slice(&output.stdout).expect("stdout is JSON alone");
    assert_eq!(report["status"], "drift");
    assert!(
        report["agent_checksum"]
            .as_str()
            .unwrap()
            .starts_with("ac1:")
    );
    assert!(
        report["baseline_checksum"]
            .as_str()
            .unwrap()
            .starts_with("ac1:")
    );
    assert_eq!(report["dependency"]["changed"], false);
    assert_eq!(report["dependency"]["overall_risk"], "none");
    assert_eq!(report["behavior"]["probes_passed"], 1);
    assert_eq!(report["behavior"]["probes_total"], 1);
    assert_eq!(report["behavior"]["baseline_present"], false);
    assert!(
        report["behavior"]["probe_suite_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );

    let metrics = report["behavior"]["metrics"].as_array().unwrap();
    let restraint = metrics
        .iter()
        .find(|row| row["metric"] == "tool_restraint")
        .expect("the measured metric is reported");
    assert_eq!(restraint["verdict"], "pass");
    assert_eq!(restraint["current"]["passed"], 1);
    assert_eq!(restraint["current"]["total"], 1);
    // Nothing to compare against, so no baseline score is claimed — expressed as an
    // explicit null rather than an absent key, so the shape never changes.
    assert!(restraint["baseline"].is_null(), "{restraint}");

    // The machine-readable report is the same verdict the exit code states.
    let accepted = project.run(&["--format", "json", "check", "--accept"]);
    let report: Value = serde_json::from_slice(&accepted.stdout).unwrap();
    assert_eq!(report["status"], "drift");
    assert!(project.baseline_path().is_file());
}

/// Concurrency is an implementation detail of capture: the report is the same bytes
/// whatever `--jobs` says, and so is the verdict.
#[test]
fn jobs_do_not_change_the_report() {
    let project = Project::new(
        vec![text(r#"{"answer":42}"#)],
        "[probes]\nrepeat = 2\n\n\
         [policy.metrics.structured_output_validity]\nmin = 0.9\n",
    );
    project.schema(
        "answer.json",
        r#"{"type":"object","properties":{"answer":{"type":"integer"}},"required":["answer"]}"#,
    );
    project.schema(
        "other.json",
        r#"{"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]}"#,
    );
    project.probe(
        "mixed.toml",
        r#"
[[probe]]
name = "restraint"
prompt = "Answer without calling any tool: what is the capital of Portugal?"
expect_no_tool = true

[[probe]]
name = "structured-ok"
prompt = "Answer with the required schema."
output_schema = "schemas/answer.json"

[[probe]]
name = "structured-bad"
prompt = "Answer with the other required schema."
output_schema = "schemas/other.json"
"#,
    );
    project.snapshot();

    let sequential = project.run(&["check", "--jobs", "1", "--refresh"]);
    let concurrent = project.run(&["check", "--jobs", "4", "--refresh"]);

    assert_eq!(exit_code(&sequential), Some(1), "{}", stdout(&sequential));
    assert_eq!(
        stdout(&sequential),
        stdout(&concurrent),
        "--jobs changed the report"
    );
    assert_eq!(exit_code(&concurrent), exit_code(&sequential));

    // The report is a real comparison of three probes, not a vacuous one.
    let report = stdout(&sequential);
    assert!(
        report.contains("Behavioral probes: 4 / 6 passed"),
        "{report}"
    );
    assert!(report.contains("structured_output_validity"), "{report}");
    assert!(report.contains("structured-bad  0 / 2 passed"), "{report}");
    // Two probes passed and one did not: the aggregate is a real comparison, and the
    // metric rows say which way each went.
    assert!(
        report.contains("tool_restraint") && report.contains("n/a → 100%"),
        "{report}"
    );
    assert!(report.contains("structured_output_validity"), "{report}");
    assert!(report.contains("n/a → 50%  FAIL"), "{report}");
}

/// A runtime failure is an error, not a report: exit 3, a diagnosis on stderr, and no
/// verdict anywhere on stdout.
#[test]
fn a_runtime_failure_exits_three_and_never_reports_a_verdict() {
    let project = Project::new(vec![json!({ "type": "status", "status": 500 })], "");
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();

    let output = project.run(&["check"]);

    assert_eq!(exit_code(&output), Some(3), "{}", stdout(&output));
    // stderr also carries the diagnostics (a discovery warning, here), so the error is
    // found rather than assumed to be the first line.
    assert!(stderr(&output).contains("Error:"), "{}", stderr(&output));
    assert!(stderr(&output).contains("500"), "{}", stderr(&output));
    let report = stdout(&output);
    assert!(
        !report.contains("PASS") && !report.contains("Behavior Gate"),
        "a check that could not be evaluated must not render a verdict: {report}"
    );
}

// ---------------------------------------------------------------------------
// 6. Recorded evidence
// ---------------------------------------------------------------------------

/// `--trace` evaluates evidence instead of capturing it, so it needs no model at all.
#[test]
fn trace_evaluates_recorded_evidence_without_a_model() {
    let mut project = Project::new(vec![text("Lisbon.")], "");
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();

    let captured = project.run(&["check"]);
    assert_eq!(exit_code(&captured), Some(0), "{}", stderr(&captured));

    let artifacts = project.run_artifacts();
    assert_eq!(artifacts.len(), 1, "one run, one artifact: {artifacts:?}");
    let artifact = artifacts[0].clone();

    project.stop_fixture();

    let replayed = project.run(&["check", "--trace", artifact.to_str().unwrap()]);
    let report = stdout(&replayed);
    assert_eq!(
        exit_code(&replayed),
        Some(0),
        "{report}\n{}",
        stderr(&replayed)
    );
    assert!(report.contains("tool_restraint"), "{report}");
    assert!(report.contains("100%"), "{report}");

    // A replay records nothing: the artifact belongs to the run that captured it.
    assert_eq!(
        project.run_artifacts().len(),
        1,
        "a replay must not write a second run artifact"
    );
    assert_eq!(
        project.requests().len(),
        1,
        "a replay must not contact the model: the evidence is the sample"
    );

    // Two ways a well-formed file stops being evidence, both exit 3 and neither a
    // behavior verdict: it describes another agent, or it holds no trace for a probe
    // the suite asserts. The second is checked separately from the first so a file
    // whose identity is fine is still refused when its contents do not line up.
    let elsewhere = derived_trace(project.path(), &artifact, "elsewhere.json", |trace| {
        trace["agent_checksum"] = json!("ac1:0000");
    });
    let mismatched = project.run(&["check", "--trace", elsewhere.to_str().unwrap()]);
    assert_eq!(exit_code(&mismatched), Some(3), "{}", stdout(&mismatched));
    assert!(
        stderr(&mismatched).contains("the current agent is"),
        "{}",
        stderr(&mismatched)
    );

    let stranger = derived_trace(project.path(), &artifact, "stranger.json", |trace| {
        trace["probe"] = json!("another-probe");
    });
    let unheld = project.run(&["check", "--trace", stranger.to_str().unwrap()]);
    assert_eq!(exit_code(&unheld), Some(3), "{}", stdout(&unheld));
    assert!(
        stderr(&unheld).contains("holds no trace for the probe `no-tools`"),
        "{}",
        stderr(&unheld)
    );
}

/// Evidence captured under another agent is refused: a passing run from before a
/// dependency change must never be scored as the behavior of the agent that change
/// produced. The probes are untouched in this scenario — which is exactly why the
/// binding check cannot rely on them.
#[test]
fn replay_evidence_from_another_agent_is_refused() {
    let project = Project::new(vec![text("Lisbon.")], "");
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();

    let captured = project.run(&["check"]);
    assert_eq!(exit_code(&captured), Some(0), "{}", stderr(&captured));
    let artifact = project.artifact();
    let requests = project.requests().len();

    // The agent moves: the prompt changes, so a new snapshot describes a different
    // agent, while the probe suite stays exactly as it was.
    project.set_prompt("Be terse.\n");
    project.snapshot();

    let replayed = project.run(&["check", "--trace", artifact.to_str().unwrap()]);
    let report = stdout(&replayed);

    assert_eq!(exit_code(&replayed), Some(3), "{report}");
    assert!(
        !report.contains("Behavior Gate"),
        "unusable evidence is an error, not a verdict: {report}"
    );
    let diagnostic = stderr(&replayed);
    assert!(diagnostic.contains("the current agent is"), "{diagnostic}");
    assert!(
        diagnostic.contains("captured under the agent"),
        "{diagnostic}"
    );
    assert_eq!(
        project.requests().len(),
        requests,
        "a refused replay must not contact the model"
    );
}

/// Evidence captured against another tool catalog is refused: the model was choosing
/// from a different set of tools, so its choices measure something else.
#[test]
fn replay_evidence_from_another_tool_catalog_is_refused() {
    let project = Project::new(vec![text("Lisbon.")], "");
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();

    let captured = project.run(&["check"]);
    assert_eq!(exit_code(&captured), Some(0), "{}", stderr(&captured));
    let artifact = project.artifact();

    let elsewhere = derived_trace(project.path(), &artifact, "other-catalog.json", |trace| {
        trace["captured_with"]["tool_catalog_digest"] = json!("sha256:0000");
    });
    let replayed = project.run(&["check", "--trace", elsewhere.to_str().unwrap()]);

    assert_eq!(exit_code(&replayed), Some(3), "{}", stdout(&replayed));
    let diagnostic = stderr(&replayed);
    assert!(diagnostic.contains("tool catalog"), "{diagnostic}");
    assert!(
        diagnostic.contains("different set of tools"),
        "{diagnostic}"
    );
}

/// A run artifact whose probe suite has changed is refused — the suite digest is
/// metadata a reader could discard, and keeping it is what makes this refusal possible.
#[test]
fn replay_evidence_whose_probe_suite_changed_is_refused() {
    let project = Project::new(vec![text("Lisbon.")], "");
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();

    let captured = project.run(&["check"]);
    assert_eq!(exit_code(&captured), Some(0), "{}", stderr(&captured));
    let artifact = project.artifact();

    // The assertion changes, so the recorded scores answer a question nobody is
    // asking any more. The agent checksum is untouched.
    project.probe(
        "no-tools.toml",
        &RESTRAINT_PROBE.replace(
            "what is the capital of Portugal?",
            "what is the capital of Portugal, and name one river?",
        ),
    );

    let replayed = project.run(&["check", "--trace", artifact.to_str().unwrap()]);
    assert_eq!(exit_code(&replayed), Some(3), "{}", stdout(&replayed));
    let diagnostic = stderr(&replayed);
    assert!(diagnostic.contains("probe suite"), "{diagnostic}");
}

/// A trace from a runner this build does not implement is refused rather than read
/// under this evaluator's assumptions.
#[test]
fn replay_evidence_from_another_runner_is_refused() {
    let project = Project::new(vec![text("Lisbon.")], "");
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();

    let captured = project.run(&["check"]);
    assert_eq!(exit_code(&captured), Some(0), "{}", stderr(&captured));
    let artifact = project.artifact();

    let newer = derived_trace(project.path(), &artifact, "newer-runner.json", |trace| {
        trace["captured_with"]["runner_version"] = json!(agentchecksum::runner::RUNNER_VERSION + 1);
    });
    let replayed = project.run(&["check", "--trace", newer.to_str().unwrap()]);
    assert_eq!(exit_code(&replayed), Some(3), "{}", stdout(&replayed));
    assert!(
        stderr(&replayed).contains("capture contract changed"),
        "{}",
        stderr(&replayed)
    );

    let other = derived_trace(project.path(), &artifact, "other-runner.json", |trace| {
        trace["captured_with"]["runner"] = json!("some-other-runner");
    });
    let replayed = project.run(&["check", "--trace", other.to_str().unwrap()]);
    assert_eq!(exit_code(&replayed), Some(3), "{}", stdout(&replayed));
    assert!(
        stderr(&replayed).contains("some-other-runner"),
        "{}",
        stderr(&replayed)
    );
}

/// A call that claims a canonical tool identity must be telling the truth about it.
///
/// The distinction is the whole point: an invented name is real behavior and is scored,
/// while a claimed identity that the catalog contradicts is a corrupted record and is
/// refused. Believing either half of it would award credit for a tool the model never
/// named.
#[test]
fn a_call_that_claims_a_tool_identity_the_catalog_contradicts_is_refused() {
    let project = Project::new(
        vec![tool_call_with(
            "search_repositories",
            r#"{"query":"postgres"}"#,
        )],
        "[policy.metrics.tool_selection]\nmin = 1.0\n",
    );
    project.declare_tools(&[
        (
            "search_repositories",
            json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"],
            }),
        ),
        (
            "delete_file",
            json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        ),
    ]);
    project.probe(
        "search.toml",
        r#"
[[probe]]
name = "repository-search"
prompt = "Find repositories about PostgreSQL vector search."
expect_tool = "search_repositories"
"#,
    );
    project.snapshot();

    let captured = project.run(&["check"]);
    assert_eq!(exit_code(&captured), Some(0), "{}", stderr(&captured));
    let artifact = project.artifact();

    // A well-formed capture with a real identity still replays.
    let honest = derived_trace(project.path(), &artifact, "honest.json", |_| {});
    let replayed = project.run(&["check", "--trace", honest.to_str().unwrap()]);
    assert_eq!(exit_code(&replayed), Some(0), "{}", stderr(&replayed));
    assert!(stdout(&replayed).contains("100%"), "{}", stdout(&replayed));

    // A known canonical id whose declared name is a different tool.
    let contradictory = derived_trace(project.path(), &artifact, "contradiction.json", |trace| {
        trace["samples"][0]["tool_calls"] = json!([{
            "name": "search_repositories",
            "tool_id": "tool:local.delete_file",
            "arguments": { "query": "postgres" },
        }]);
    });
    let replayed = project.run(&["check", "--trace", contradictory.to_str().unwrap()]);
    assert_eq!(exit_code(&replayed), Some(3), "{}", stdout(&replayed));
    let diagnostic = stderr(&replayed);
    assert!(
        diagnostic.contains("tool:local.delete_file"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("search_repositories"), "{diagnostic}");
    assert!(
        !stdout(&replayed).contains("PASS"),
        "a contradictory record earns no credit: {}",
        stdout(&replayed)
    );

    // A canonical id the catalog does not declare at all.
    let unknown = derived_trace(project.path(), &artifact, "unknown-id.json", |trace| {
        trace["samples"][0]["tool_calls"] = json!([{
            "name": "search_repositories",
            "tool_id": "tool:local.hallucinated",
            "arguments": { "query": "postgres" },
        }]);
    });
    let replayed = project.run(&["check", "--trace", unknown.to_str().unwrap()]);
    assert_eq!(exit_code(&replayed), Some(3), "{}", stdout(&replayed));
    assert!(
        stderr(&replayed).contains("does not declare"),
        "{}",
        stderr(&replayed)
    );
}

/// The same identity rule holds for evidence that came from the cache rather than from
/// `--trace`: a cache entry is a file too, and a call that claims a tool identity is
/// checked against the catalog wherever the sample came from.
#[test]
fn a_forged_call_in_a_cached_sample_is_refused() {
    let project = Project::new(
        vec![tool_call_with(
            "search_repositories",
            r#"{"query":"postgres"}"#,
        )],
        "",
    );
    project.declare_tools(&[
        (
            "search_repositories",
            json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"],
            }),
        ),
        (
            "delete_file",
            json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        ),
    ]);
    project.probe(
        "search.toml",
        r#"
[[probe]]
name = "repository-search"
prompt = "Find repositories about PostgreSQL vector search."
expect_tool = "search_repositories"
"#,
    );
    project.snapshot();

    let captured = project.run(&["check"]);
    assert_eq!(exit_code(&captured), Some(0), "{}", stderr(&captured));
    let requests = project.requests().len();

    // Rewrite the recorded sample: the cache stores what the model answered, and a
    // claim about which tool that was is checked rather than believed.
    let cache = project.path().join(".agentchecksum/cache");
    let mut rewritten = 0;
    for entry in std::fs::read_dir(&cache).unwrap() {
        let path = entry.unwrap().path();
        let mut value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["sample"]["tool_calls"] = json!([{
            "name": "search_repositories",
            "tool_id": "tool:local.delete_file",
            "arguments": { "query": "postgres" },
        }]);
        std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        rewritten += 1;
    }
    assert!(rewritten > 0, "the run must have cached its samples");

    let replayed = project.run(&["check"]);
    assert_eq!(exit_code(&replayed), Some(3), "{}", stdout(&replayed));
    assert!(
        stderr(&replayed).contains("tool:local.delete_file"),
        "{}",
        stderr(&replayed)
    );
    assert_eq!(
        project.requests().len(),
        requests,
        "the forged sample came from the cache, so no request was made"
    );
}

/// An invented tool name carries no identity claim, so it stays behavioral evidence and
/// is measured — a hallucination the gate can see, rather than an error that hides it.
#[test]
fn an_invented_tool_name_is_measured_rather_than_refused() {
    let project = Project::new(
        vec![tool_call_with(
            "search_repositories",
            r#"{"query":"postgres"}"#,
        )],
        "",
    );
    project.declare_tools(&[(
        "search_repositories",
        json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"],
        }),
    )]);
    project.probe(
        "search.toml",
        r#"
[[probe]]
name = "repository-search"
prompt = "Find repositories about PostgreSQL vector search."
expect_tool = "search_repositories"
"#,
    );
    project.snapshot();

    let captured = project.run(&["check"]);
    assert_eq!(exit_code(&captured), Some(0), "{}", stderr(&captured));
    let artifact = project.artifact();

    let invented = derived_trace(project.path(), &artifact, "invented.json", |trace| {
        trace["samples"][0]["tool_calls"] = json!([{
            "name": "hallucinated_tool",
            "arguments": { "query": "postgres" },
        }]);
    });
    let replayed = project.run(&["check", "--trace", invented.to_str().unwrap()]);
    let report = stdout(&replayed);

    // Scored, not refused: the run is a measurement of behavior nobody asked for.
    assert_eq!(
        exit_code(&replayed),
        Some(0),
        "{report}\n{}",
        stderr(&replayed)
    );
    assert!(report.contains("Behavior Gate"), "{report}");
    assert!(
        report.contains("tool_selection") && report.contains("0%"),
        "{report}"
    );
    assert!(
        report.contains("hallucinated_tool"),
        "the report must name what the model actually called: {report}"
    );
}

/// A baseline recorded under a different runner contract is not comparable: its scores
/// came from different capture rules, so a relative constraint must not be applied to
/// it — while an absolute one still is, because a threshold this run misses is a fact
/// about this run.
#[test]
fn a_baseline_from_another_runner_contract_is_not_comparable() {
    // Two samples: a restraint the policy measures against the baseline, then a tool
    // call that drops the score to zero.
    let project = Project::new(
        vec![text("Lisbon."), tool_call("delete_everything")],
        "[probes]\nrepeat = 1\n\n[policy.metrics.tool_restraint]\nmax_drop = 0.05\n",
    );
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();

    let accepted = project.run(&["check", "--accept"]);
    assert_eq!(exit_code(&accepted), Some(0), "{}", stderr(&accepted));

    // The contract that recorded this baseline is no longer the one in force.
    let mut baseline = project.baseline();
    assert_eq!(
        baseline["runner_contract"],
        json!(agentchecksum::runner::RUNNER_CONTRACT)
    );
    baseline["runner_contract"] = json!("some-other-runner-v1");
    std::fs::write(
        project.baseline_path(),
        serde_json::to_vec_pretty(&baseline).unwrap(),
    )
    .unwrap();

    let compared = project.run(&["check", "--refresh"]);
    let report = stdout(&compared);

    // The drop would fail `max_drop` if the baseline were comparable. It is not, so it
    // is drift: the behavior did not get worse, the comparison stopped being possible.
    assert_eq!(
        exit_code(&compared),
        Some(0),
        "{report}\n{}",
        stderr(&compared)
    );
    assert!(report.contains("Behavior Gate: DRIFT"), "{report}");
    assert!(
        report.contains("different runner contract"),
        "the report must say why the baseline was not used: {report}"
    );
    // The baseline's number is not shown as this run's "before": it was produced by
    // different capture rules, and printing it beside this run would invite a
    // subtraction the gate deliberately did not make.
    assert!(report.contains("n/a → 0%"), "{report}");
    assert!(!report.contains("below"), "{report}");

    // An absolute constraint is still applied: a floor this run misses is a failure of
    // this run, whatever the baseline can or cannot be compared with.
    let absolute = Project::new(vec![text("Lisbon."), tool_call("delete_everything")], "");
    absolute.probe("no-tools.toml", RESTRAINT_PROBE);
    absolute.snapshot();
    let accepted = absolute.run(&["check", "--accept"]);
    assert_eq!(exit_code(&accepted), Some(0), "{}", stderr(&accepted));

    let mut baseline = absolute.baseline();
    baseline["runner_contract"] = json!("some-other-runner-v1");
    std::fs::write(
        absolute.baseline_path(),
        serde_json::to_vec_pretty(&baseline).unwrap(),
    )
    .unwrap();
    absolute.append_config("\n[policy.metrics.tool_restraint]\nmin = 1.0\n");

    let gated = absolute.run(&["check", "--refresh"]);
    let report = stdout(&gated);
    assert_eq!(exit_code(&gated), Some(1), "{report}\n{}", stderr(&gated));
    assert!(report.contains("Behavior Gate: FAIL"), "{report}");
    assert!(report.contains("below the required minimum"), "{report}");
}

// ---------------------------------------------------------------------------
// 7. The exit-code matrix
// ---------------------------------------------------------------------------

/// Every rejected combination exits 2 — clap's own code for a usage problem, which the
/// command's own refusals reuse so a CI script does not have to know which caught it.
#[test]
fn rejected_flag_combinations_exit_two() {
    let project = Project::new(vec![text("Lisbon.")], "");
    project.probe("no-tools.toml", RESTRAINT_PROBE);
    project.snapshot();

    for args in [
        vec!["check", "--accept", "--diff-only"],
        vec!["check", "--accept", "--no-probes"],
        vec!["check", "--accept", "--from", "base.lock"],
        vec!["check", "--diff-only", "--probes-only"],
        vec!["check", "--probes-only", "--no-probes"],
        vec!["check", "--trace", "trace.json", "--refresh"],
        vec!["check", "--trace", "trace.json", "--jobs", "2"],
        vec!["check", "--trace", "trace.json", "--repeat", "2"],
        vec!["check", "--repeat", "0"],
        vec!["check", "--repeat", "101"],
        vec!["check", "--jobs", "0"],
        vec!["check", "--jobs", "33"],
        vec!["check", "--fail-on-risk", "sometimes"],
        vec!["inspect", "everything"],
    ] {
        let output = project.run(&args);
        assert_eq!(
            exit_code(&output),
            Some(2),
            "{args:?} should be rejected: {}",
            stderr(&output)
        );
        assert!(
            project.requests().is_empty(),
            "{args:?} must be rejected before anything runs"
        );
    }

    // `--accept --trace` is not a clap conflict — the command refuses it itself, with
    // the same exit code and a diagnosis that names both flags.
    let refused = project.run(&["check", "--accept", "--trace", "trace.json"]);
    assert_eq!(exit_code(&refused), Some(2), "{}", stdout(&refused));
    assert!(
        stderr(&refused).contains("`--accept` and `--trace` cannot be combined"),
        "{}",
        stderr(&refused)
    );
}

// ---------------------------------------------------------------------------
// 8. A real tool catalog
// ---------------------------------------------------------------------------

/// The whole chain, against a real catalog: an MCP server declares a tool, the probe
/// references it by name, the catalog goes on the wire, and the model's arguments are
/// validated against the declared schema — then the schema moves and the report says
/// the yardstick did.
#[test]
fn a_tool_reference_is_measured_against_the_discovered_catalog() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("prompts")).unwrap();
    std::fs::write(dir.path().join("prompts/system.md"), "Be concise.\n").unwrap();

    let spec = dir.path().join("mcp.json");
    let schema = json!({
        "type": "object",
        "properties": { "query": { "type": "string" } },
        "required": ["query"],
    });
    write_mcp_spec(&spec, "search_repositories", schema.clone());
    let (_server, port) = start(
        dir.path(),
        vec![tool_call_with(
            "search_repositories",
            r#"{"query":"postgres"}"#,
        )],
    );

    std::fs::write(
        dir.path().join("agentchecksum.toml"),
        format!(
            "version = 1\n\n[agent]\nname = \"catalog-test\"\n\n\
             [[prompts]]\npath = \"prompts/system.md\"\n\n\
             [model]\nprovider = \"openai-compatible\"\nid = \"fixture-model\"\n\
             endpoint = \"http://127.0.0.1:{port}\"\n\n\
             [[mcp.servers]]\nname = \"local\"\ntransport = \"stdio\"\ncommand = \"{}\"\n\
             args = [\"--stdio\"]\nenv = {{ AC_FIXTURE_SPEC = \"{}\" }}\n",
            mcp_fixture().display(),
            spec.display(),
        ),
    )
    .unwrap();

    std::fs::create_dir_all(dir.path().join("probes")).unwrap();
    std::fs::write(
        dir.path().join("probes/search.toml"),
        r#"
[[probe]]
name = "repository-search"
prompt = "Find repositories about PostgreSQL vector search."
expect_tool = "search_repositories"
expect_args = { query = { contains = "postgres" } }
"#,
    )
    .unwrap();

    let run = |args: &[&str]| {
        Command::cargo_bin("agentchecksum")
            .unwrap()
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap()
    };

    let snapshot = run(&["snapshot"]);
    assert_eq!(exit_code(&snapshot), Some(0), "{}", stderr(&snapshot));
    let lock: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("agentchecksum.lock")).unwrap(),
    )
    .unwrap();
    assert!(
        lock["dependencies"]["tool:local.search_repositories"].is_object(),
        "the declared tool must be in the committed contract: {lock}"
    );

    let checked = run(&["check"]);
    let report = stdout(&checked);
    assert_eq!(
        exit_code(&checked),
        Some(0),
        "{report}\n{}",
        stderr(&checked)
    );
    assert!(
        report.contains("Behavioral probes: 1 / 1 passed"),
        "{report}"
    );
    for metric in [
        "tool_selection",
        "argument_validity",
        "argument_expectation",
    ] {
        assert!(report.contains(metric), "{report}");
    }
    assert!(report.contains("n/a → 100%"), "{report}");

    // The catalog the model was shown is the catalog the probe resolved against, with
    // the declared schema rather than a summary of it.
    let logged = recorded(dir.path());
    assert_eq!(logged.len(), 1, "one sample, one request: {logged:#?}");
    let request = &logged[0];
    assert_eq!(
        request["body"]["tools"][0]["function"]["name"],
        "search_repositories"
    );
    assert_eq!(
        request["body"]["tools"][0]["function"]["parameters"]["required"][0],
        "query"
    );

    // Accept it, then move the schema the arguments were judged against.
    let accepted = run(&["check", "--accept"]);
    assert_eq!(exit_code(&accepted), Some(0), "{}", stderr(&accepted));
    let baseline: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join(".agentchecksum/baseline.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        baseline["yardsticks"]["tool_input_schema"]["tool:local.search_repositories"]
            .as_str()
            .unwrap()
            .len(),
        "sha256:".len() + 64
    );

    let mut moved = schema;
    moved["properties"]["per_page"] = json!({ "type": "integer" });
    write_mcp_spec(&spec, "search_repositories", moved);

    let after = run(&["check"]);
    let report = stdout(&after);
    assert_eq!(exit_code(&after), Some(0), "{report}\n{}", stderr(&after));
    assert!(
        report.contains("Yardsticks changed: tool:local.search_repositories"),
        "a moved schema must be visible rather than silent: {report}"
    );
    assert!(report.contains("dependency checksum changed"), "{report}");
}

/// One tool, declared the way an MCP server declares it.
fn write_mcp_spec(path: &Path, name: &str, input_schema: Value) {
    let spec = json!({
        "tools": [{ "name": name, "description": "Search repositories.", "input_schema": input_schema }],
    });
    std::fs::write(path, serde_json::to_vec_pretty(&spec).unwrap()).unwrap();
}

// ---------------------------------------------------------------------------
// 9. `inspect probes`
// ---------------------------------------------------------------------------

/// `inspect probes` describes the configured suite: every probe, the tools it references
/// by canonical id, the metrics those references feed, the effective repeat and the
/// digest — in both formats.
#[test]
fn inspect_probes_describes_the_configured_suite() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("probes")).unwrap();

    // A committed catalog with two tools, written the way `snapshot` would write it:
    // the payload and the digest over it have to agree, because a lockfile is verified
    // before anything resolves against it.
    let lockfile = Lockfile::from_dependencies(&[
        file_dependency(
            "prompt:prompts/system.md",
            "prompt",
            "content",
            "Be concise.",
        ),
        tool_dependency(
            "tool:github.search_repositories",
            "Search repositories.",
            json!({ "type": "object", "properties": { "query": { "type": "string" } } }),
        ),
        tool_dependency(
            "tool:github.delete_file",
            "Delete a file.",
            json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        ),
    ])
    .unwrap();
    lockfile
        .write(&dir.path().join("agentchecksum.lock"))
        .unwrap();

    std::fs::write(
        dir.path().join("agentchecksum.toml"),
        "version = 1\n\n[agent]\nname = \"inspect-test\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("probes/suite.toml"),
        r#"
[[probe]]
name = "repository-search"
prompt = "Find repositories about PostgreSQL vector search."
repeat = 2
expect_tool = "search_repositories"
expect_args = { query = { contains = "postgres" } }
forbid_tools = ["delete_file"]
"#,
    )
    .unwrap();

    let run = |args: &[&str]| {
        Command::cargo_bin("agentchecksum")
            .unwrap()
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap()
    };

    let human = run(&["inspect", "probes"]);
    let report = stdout(&human);
    assert_eq!(exit_code(&human), Some(0), "{}", stderr(&human));
    assert!(report.contains("AgentChecksum inspect probes"), "{report}");
    assert!(report.contains("Suite:  1 probe"), "{report}");
    assert!(report.contains("repository-search"), "{report}");
    assert!(report.contains("probes/suite.toml"), "{report}");
    assert!(report.contains("repeat   2"), "{report}");
    assert!(
        report.contains("argument_expectation, forbidden_tool_usage, tool_restraint")
            || report.contains("tool_selection"),
        "{report}"
    );
    // Tools by canonical id, each with the metrics its assertion feeds.
    assert!(
        report.contains("tool:github.search_repositories"),
        "{report}"
    );
    assert!(
        report.contains("metrics: tool_selection, argument_expectation"),
        "{report}"
    );
    assert!(report.contains("tool:github.delete_file"), "{report}");
    assert!(report.contains("metrics: forbidden_tool_usage"), "{report}");
    assert!(report.contains("digest   sha256:"), "{report}");

    let json = run(&["--format", "json", "inspect", "probes"]);
    let report: Value = serde_json::from_slice(&json.stdout).expect("stdout is JSON alone");
    assert_eq!(report["status"], "ok");
    assert!(
        report["probe_suite_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );

    let probe = &report["probes"][0];
    assert_eq!(probe["probe"], "repository-search");
    assert_eq!(probe["file"], "probes/suite.toml");
    assert_eq!(probe["repeat"], 2);
    assert!(probe["digest"].as_str().unwrap().starts_with("sha256:"));

    let metrics: Vec<&str> = probe["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|metric| metric.as_str().unwrap())
        .collect();
    assert_eq!(
        metrics,
        vec![
            "tool_selection",
            "argument_validity",
            "argument_expectation",
            "forbidden_tool_usage"
        ]
    );

    let tools = probe["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2, "{tools:#?}");
    assert_eq!(tools[0]["id"], "tool:github.search_repositories");
    assert_eq!(
        tools[0]["metrics"],
        json!(["tool_selection", "argument_expectation"])
    );
    assert_eq!(tools[1]["id"], "tool:github.delete_file");
    assert_eq!(tools[1]["metrics"], json!(["forbidden_tool_usage"]));
}

/// `init` leaves a project that can actually run `check`: a valid starter probe, not an
/// empty directory the loader refuses.
#[test]
fn init_scaffolds_a_starter_probe() {
    let dir = tempfile::tempdir().unwrap();

    let output = Command::cargo_bin("agentchecksum")
        .unwrap()
        .current_dir(dir.path())
        .arg("init")
        .output()
        .unwrap();

    assert_eq!(exit_code(&output), Some(0), "{}", stderr(&output));
    let starter = dir.path().join("probes/no-tools.toml");
    assert!(starter.is_file(), "the starter probe was written");
    assert!(
        stdout(&output).contains("probes/no-tools.toml"),
        "{}",
        stdout(&output)
    );

    // The config the scaffold writes points at `probes`, so the suite loads once the
    // prompt it names exists.
    std::fs::create_dir_all(dir.path().join("prompts")).unwrap();
    std::fs::write(dir.path().join("prompts/system.md"), "Be concise.\n").unwrap();
    let lockfile = Lockfile::from_dependencies(&[file_dependency(
        "prompt:prompts/system.md",
        "prompt",
        "content",
        "Be concise.",
    )])
    .unwrap();
    lockfile
        .write(&dir.path().join("agentchecksum.lock"))
        .unwrap();

    let inspected = Command::cargo_bin("agentchecksum")
        .unwrap()
        .current_dir(dir.path())
        .args(["inspect", "probes"])
        .output()
        .unwrap();
    let report = stdout(&inspected);
    assert_eq!(exit_code(&inspected), Some(0), "{report}");
    assert!(report.contains("no-tools"), "{report}");
    assert!(report.contains("tool_restraint"), "{report}");

    // A second `init` refuses without --force, and `--force` does not overwrite a probe
    // somebody has edited.
    std::fs::write(&starter, "# edited by the user\n").unwrap();
    let forced = Command::cargo_bin("agentchecksum")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "--force"])
        .output()
        .unwrap();
    assert_eq!(exit_code(&forced), Some(0), "{}", stderr(&forced));
    assert_eq!(
        std::fs::read_to_string(&starter).unwrap(),
        "# edited by the user\n"
    );
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A tool dependency, with the recorded payload and the digest over it.
fn tool_dependency(id: &str, description: &str, schema: Value) -> Dependency {
    let mut facets = std::collections::BTreeMap::new();
    facets.insert("input_schema".to_string(), recorded_facet(schema));
    facets.insert(
        "description".to_string(),
        recorded_facet(json!(description)),
    );

    Dependency {
        id: id.to_string(),
        kind: DependencyKind::Tool,
        facets,
        source: Some("github".to_string()),
    }
}

/// A dependency whose facet records a payload, as external sources do.
fn file_dependency(id: &str, kind: &str, facet: &str, payload: &str) -> Dependency {
    let kind = match kind {
        "prompt" => DependencyKind::Prompt,
        other => panic!("unsupported fixture kind `{other}`"),
    };

    Dependency {
        id: id.to_string(),
        kind,
        facets: std::collections::BTreeMap::from([(
            facet.to_string(),
            Facet {
                digest: Digest::sha256(payload.as_bytes()),
                shape: None,
                normalized: None,
            },
        )]),
        source: None,
    }
}

/// A facet whose digest is taken over the payload recorded beside it, which is what the
/// lockfile's payload check recomputes.
fn recorded_facet(payload: Value) -> Facet {
    let digest = Digest::sha256(&canonical::to_vec(&payload).unwrap());
    Facet {
        digest,
        shape: None,
        normalized: Some(payload),
    }
}
