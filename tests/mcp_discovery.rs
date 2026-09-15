// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end MCP discovery: the real client, a real server, both transports.
//!
//! These tests drive `agentchecksum` against `examples/mcp_fixture_server`, which
//! speaks MCP because it is an MCP server — stdio through the SDK's server
//! implementation, Streamable HTTP on raw sockets. What is checked here is the part
//! no unit test can reach: that a configuration, a real session, and the fingerprint
//! and diff layers agree end to end, and that the safety properties the discovery
//! phase promises (bounds, cleanup, secret non-leakage, order independence) hold
//! against a server that actually behaves like one.
//!
//! The fixture's catalog is data, so each test states the server it needs instead of
//! writing Rust for it. Every server listens on `127.0.0.1` only; nothing here
//! touches the network.

use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use serde_json::{Value, json};

/// A credential and nothing else. If it is ever readable outside the fixture's own
/// environment, one of these tests has proved a leak.
const SENTINEL: &str = "SUPER_SECRET_AGENTCHECKSUM_TEST_TOKEN_123";

/// The `AC_FIXTURE_SPEC` file, plus the config that points one server at it.
struct Project {
    dir: tempfile::TempDir,
    spec: PathBuf,
}

impl Project {
    /// A project with one stdio server named `local`, declaring `spec`.
    fn stdio(spec: Value, env: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = write_spec(dir.path(), "spec.json", spec);
        write_config(dir.path(), &config(&stdio_server("local", &path, env)));
        Self { dir, spec: path }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Re-declare the catalog, keeping the same baseline lockfile.
    fn redeclare(&self, spec: Value) {
        write_spec(self.path(), "spec.json", spec);
    }

    fn snapshot(&self) -> std::process::Output {
        run(self.path(), &["snapshot"])
    }

    /// The baseline, failing loudly if that step is what broke.
    fn committed_baseline(&self) {
        let output = self.snapshot();
        assert_eq!(
            output.status.code(),
            Some(0),
            "snapshot failed: {}",
            stderr(&output)
        );
    }

    fn lock(&self) -> Value {
        lock(self.path())
    }

    fn lock_bytes(&self) -> Vec<u8> {
        std::fs::read(self.path().join("agentchecksum.lock")).expect("the lockfile exists")
    }

    fn lock_exists(&self) -> bool {
        self.path().join("agentchecksum.lock").exists()
    }

    fn diff_report(&self) -> Value {
        diff_report(self.path())
    }
}

// ---------------------------------------------------------------------------
// Fixtures, config, and the binary under test
// ---------------------------------------------------------------------------

/// The fixture server, next to this test binary in the build directory.
///
/// `cargo test --test <name>` does not build example targets, and it does not
/// rebuild a stale one either, so the example this test drives is built once per
/// test process: a test that ran against a binary from an earlier edit would be
/// evidence about code nobody is looking at. Warm artifacts make the build a
/// fraction of a second.
fn fixture() -> PathBuf {
    static BUILT: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(|| {
        let exe = std::env::current_exe().expect("the test binary has a path");
        let profile = exe
            .parent()
            .and_then(Path::parent)
            .expect("the test binary lives in <target>/<profile>/deps")
            .to_path_buf();
        let path = profile.join("examples").join("mcp_fixture_server");

        build_example(&profile);

        assert!(
            path.exists(),
            "the MCP fixture server is not built at {}; build it with `cargo build --example \
             mcp_fixture_server`",
            path.display()
        );
        path
    });

    BUILT.clone()
}

/// Build the example this test drives, in the profile the test itself was built in.
fn build_example(profile: &Path) {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut command = StdCommand::new(cargo);
    command
        .args(["build", "--example", "mcp_fixture_server"])
        .current_dir(env!("CARGO_MANIFEST_DIR"));
    if profile.file_name().is_some_and(|name| name == "release") {
        command.arg("--release");
    }

    let status = command.status().expect("cargo runs");
    assert!(status.success(), "building the MCP fixture server failed");
}

fn write_spec(dir: &Path, name: &str, spec: Value) -> PathBuf {
    let path = dir.join(name);
    let text = serde_json::to_vec_pretty(&spec).expect("a spec serializes");
    std::fs::write(&path, text).expect("the spec is writable");
    path
}

fn write_config(dir: &Path, text: &str) {
    std::fs::write(dir.join("agentchecksum.toml"), text).expect("the config is writable");
}

fn config(servers: &str) -> String {
    format!("version = 1\n\n[agent]\nname = \"mcp-discovery-test\"\n\n{servers}")
}

/// One stdio server, pointed at a spec file by absolute path.
fn stdio_server(alias: &str, spec: &Path, env: &[(&str, &str)]) -> String {
    let mut entries = vec![format!(
        "AC_FIXTURE_SPEC = {}",
        toml_string(&spec.display().to_string())
    )];
    for (key, value) in env {
        entries.push(format!("{key} = {}", toml_string(value)));
    }
    format!(
        "[[mcp.servers]]\nname = {alias}\ntransport = \"stdio\"\ncommand = {command}\nargs = \
         [\"--stdio\"]\nenv = {{ {env} }}\n",
        alias = toml_string(alias),
        command = toml_string(&fixture().display().to_string()),
        env = entries.join(", ")
    )
}

/// One Streamable HTTP server, pointed at an origin the test already bound.
fn http_server(alias: &str, port: u16) -> String {
    format!(
        "[[mcp.servers]]\nname = {alias}\ntransport = \"streamable-http\"\nurl = {url}\n",
        alias = toml_string(alias),
        url = toml_string(&format!("http://127.0.0.1:{port}/mcp"))
    )
}

/// A TOML basic string. Temp-directory paths are ordinary, but a path is user input
/// and quoting it properly costs one line.
fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A tool declaration with every field the discovery contract reads.
fn tool(name: &str) -> Value {
    json!({
        "name": name,
        "description": "Search the index.",
        "input_schema": {
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        },
        "output_schema": {
            "type": "object",
            "properties": { "hits": { "type": "array" }, "total": { "type": "integer" } },
            "required": ["hits", "total"]
        },
        "annotations": { "readOnlyHint": true, "openWorldHint": false }
    })
}

// ---------------------------------------------------------------------------
// Running the binary
// ---------------------------------------------------------------------------

fn run(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::cargo_bin("agentchecksum")
        .expect("the binary is built")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("the binary runs")
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

fn lock(dir: &Path) -> Value {
    let text =
        std::fs::read_to_string(dir.join("agentchecksum.lock")).expect("the lockfile was written");
    serde_json::from_str(&text).expect("the lockfile is JSON")
}

fn diff_report(dir: &Path) -> Value {
    let output = run(dir, &["diff", "--format", "json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "diff failed: {}",
        stderr(&output)
    );
    serde_json::from_str(&stdout(&output)).expect("diff --format json writes JSON")
}

/// The one change the report holds for `id`.
fn one_change(report: &Value, id: &str) -> Value {
    let changes = report["changes"].as_array().expect("changes is an array");
    let matching: Vec<&Value> = changes.iter().filter(|change| change["id"] == id).collect();
    assert_eq!(
        matching.len(),
        1,
        "expected one change for `{id}`: {report}"
    );
    matching[0].clone()
}

/// The named facet inside a dependency change.
fn facet(change: &Value, name: &str) -> Value {
    let facets = change["facets"].as_array().expect("facets is an array");
    let matching: Vec<&Value> = facets
        .iter()
        .filter(|facet| facet["name"] == name)
        .collect();
    assert_eq!(matching.len(), 1, "expected facet `{name}`: {change}");
    matching[0].clone()
}

// ---------------------------------------------------------------------------
// 1. The vertical slice
// ---------------------------------------------------------------------------

#[test]
fn a_stdio_server_and_its_tool_become_the_dependencies_and_the_aggregate_verifies() {
    let project = Project::stdio(
        json!({
            "server_info": { "name": "fixture", "version": "1.0.0" },
            "tools": [tool("search")]
        }),
        &[],
    );

    let output = project.snapshot();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let lock = project.lock();

    let server = &lock["dependencies"]["mcp:local"];
    assert_eq!(server["kind"], "mcp");
    assert_eq!(server["source"], "local");
    let identity = &server["facets"]["identity"]["normalized"];
    // The negotiated era is the point of recording it: this server implements the
    // stateless revision and the session the client got was a stateless one.
    assert_eq!(identity["era"], "stateless");
    assert_eq!(identity["protocol_version"], "2026-07-28");
    assert_eq!(
        identity["server_info"],
        json!({ "name": "fixture", "version": "1.0.0" })
    );

    let declared = &lock["dependencies"]["tool:local.search"];
    assert_eq!(declared["kind"], "tool");
    assert_eq!(declared["source"], "local");
    let facets = declared["facets"].as_object().expect("facets is an object");
    for expected in [
        "description",
        "input_schema",
        "output_schema",
        "capabilities",
    ] {
        assert!(
            facets.contains_key(expected),
            "missing facet `{expected}`: {declared}"
        );
    }
    // The declared hints, folded into the effective tokens the contract records.
    assert_eq!(
        declared["facets"]["capabilities"]["normalized"],
        json!(["closed-world", "read-only"])
    );
    assert_eq!(
        declared["facets"]["input_schema"]["normalized"]["type"],
        "object"
    );
    assert!(declared["facets"]["description"]["shape"].is_string());

    // The baseline verifies against a fresh discovery of the same server: nothing
    // moves between the snapshot and the comparison.
    let output = run(project.path(), &["diff"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("No dependency changes detected."),
        "{}",
        stdout(&output)
    );
}

// ---------------------------------------------------------------------------
// 2. Diff, through a real second discovery
// ---------------------------------------------------------------------------

/// Snapshot `baseline`, re-declare the server as `changed`, and compare.
fn baseline_then(project: &Project, baseline: Value, changed: Value) -> Value {
    project.redeclare(json!({ "tools": baseline }));
    project.committed_baseline();
    project.redeclare(json!({ "tools": changed }));
    project.diff_report()
}

#[test]
fn a_tool_description_change_is_medium_and_classified_as_a_text_change() {
    let project = Project::stdio(json!({ "tools": [tool("search")] }), &[]);

    let mut rewritten = tool("search");
    rewritten["description"] = json!("Search the index, thoroughly.");
    let report = baseline_then(&project, json!([tool("search")]), json!([rewritten]));

    assert_eq!(report["overall_risk"], "medium");
    let change = one_change(&report, "tool:local.search");
    assert_eq!(change["change"], "modified");
    assert_eq!(change["risk"], "medium");

    let description = facet(&change, "description");
    assert_eq!(description["risk"], "medium");
    let details = description["details"]
        .as_array()
        .expect("details is an array");
    assert_eq!(details[0]["path"], "classification");
    assert_eq!(details[0]["after"], "text-changed");
}

#[test]
fn a_newly_required_input_property_is_critical() {
    let project = Project::stdio(json!({ "tools": [tool("search")] }), &[]);

    let mut narrowed = tool("search");
    narrowed["input_schema"] = json!({
        "type": "object",
        "properties": {
            "query": { "type": "string" },
            "limit": { "type": "integer" }
        },
        "required": ["query", "limit"]
    });
    let report = baseline_then(&project, json!([tool("search")]), json!([narrowed]));

    assert_eq!(report["overall_risk"], "critical");
    let change = one_change(&report, "tool:local.search");
    assert_eq!(change["risk"], "critical");
    let schema = facet(&change, "input_schema");
    assert_eq!(schema["risk"], "critical");
    assert_eq!(schema["details"][0]["path"], "required");
}

#[test]
fn an_output_property_that_stops_being_required_is_critical() {
    let project = Project::stdio(json!({ "tools": [tool("search")] }), &[]);

    let mut weakened = tool("search");
    weakened["output_schema"] = json!({
        "type": "object",
        "properties": { "hits": { "type": "array" }, "total": { "type": "integer" } },
        "required": ["hits"]
    });
    let report = baseline_then(&project, json!([tool("search")]), json!([weakened]));

    assert_eq!(report["overall_risk"], "critical");
    let change = one_change(&report, "tool:local.search");
    assert_eq!(change["risk"], "critical");
    let schema = facet(&change, "output_schema");
    assert_eq!(schema["risk"], "critical");
    assert_eq!(schema["details"][0]["path"], "required");
    assert_eq!(schema["details"][0]["change"], "removed");
    assert_eq!(schema["details"][0]["before"], "total");
}

#[test]
fn a_removed_tool_is_high() {
    let project = Project::stdio(json!({ "tools": [tool("search")] }), &[]);

    let report = baseline_then(&project, json!([tool("search")]), json!([]));

    let change = one_change(&report, "tool:local.search");
    assert_eq!(change["change"], "removed");
    assert_eq!(change["risk"], "high");
    assert_eq!(report["overall_risk"], "high");
}

#[test]
fn an_added_tool_is_high() {
    let project = Project::stdio(json!({ "tools": [tool("search")] }), &[]);

    let report = baseline_then(
        &project,
        json!([tool("search")]),
        json!([tool("search"), tool("delete")]),
    );

    let change = one_change(&report, "tool:local.delete");
    assert_eq!(change["change"], "added");
    assert_eq!(change["risk"], "high");
    assert_eq!(report["overall_risk"], "high");
}

/// A change the server itself made visible: the implementation identity is part of
/// the server's contract, and the tool contract is untouched.
#[test]
fn a_server_changing_its_own_identity_is_reported_without_touching_the_tools() {
    let project = Project::stdio(
        json!({
            "server_info": { "name": "fixture", "version": "1.0.0" },
            "tools": [tool("search")]
        }),
        &[],
    );

    project.redeclare(json!({
        "server_info": { "name": "fixture-renamed", "version": "1.0.0" },
        "tools": [tool("search")]
    }));
    project.committed_baseline();
    project.redeclare(json!({
        "server_info": { "name": "fixture-renamed", "version": "2.0.0" },
        "tools": [tool("search")]
    }));

    let report = project.diff_report();
    let change = one_change(&report, "mcp:local");
    assert_eq!(change["risk"], "medium");
    assert!(one_change_opt(&report, "tool:local.search").is_none());
    // The identity payload carries the implementation as one nested object, so the
    // change is reported at the object rather than inside it.
    let details = facet(&change, "identity")["details"]
        .as_array()
        .expect("details is an array")
        .clone();
    assert!(
        details.iter().any(|detail| detail["path"] == "server_info"),
        "{details:?}"
    );
}

/// The change for `id`, or `None` when the report does not mention it.
fn one_change_opt(report: &Value, id: &str) -> Option<Value> {
    report["changes"]
        .as_array()
        .expect("changes is an array")
        .iter()
        .find(|change| change["id"] == id)
        .cloned()
}

// ---------------------------------------------------------------------------
// 3. Order and pagination are not part of the fingerprint
// ---------------------------------------------------------------------------

#[test]
fn response_order_and_page_size_do_not_reach_the_lockfile() {
    // One server answers in a different order, one page at a time; the other
    // answers in its declared order in a single page.
    let forward = Project::stdio(
        json!({ "tools": [tool("alpha"), tool("beta"), tool("gamma")] }),
        &[],
    );
    forward.committed_baseline();

    let reversed = Project::stdio(
        json!({
            "page_size": 1,
            "tools": [tool("gamma"), tool("beta"), tool("alpha")]
        }),
        &[],
    );
    reversed.committed_baseline();

    assert_eq!(
        forward.lock_bytes(),
        reversed.lock_bytes(),
        "one catalog, two response orders and page sizes, two different lockfiles"
    );
    assert_eq!(
        forward.lock()["agent_checksum"],
        reversed.lock()["agent_checksum"]
    );
}

// ---------------------------------------------------------------------------
// 4. Pagination is followed to the end
// ---------------------------------------------------------------------------

#[test]
fn a_catalog_that_needs_several_pages_is_read_whole() {
    let names = ["first", "second", "third", "fourth", "fifth"];
    let project = Project::stdio(
        json!({
            "page_size": 2,
            "tools": names.map(tool).to_vec()
        }),
        &[],
    );

    let output = project.snapshot();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let lock = project.lock();
    let dependencies = lock["dependencies"]
        .as_object()
        .expect("dependencies is an object");
    for name in names {
        let id = format!("tool:local.{name}");
        assert!(
            dependencies.contains_key(&id),
            "missing `{id}`: {:?}",
            dependencies.keys()
        );
    }
    // The tool that only exists on the last page, named explicitly: a client that
    // stopped after the first page would still have passed the loop above for
    // `first`.
    assert!(dependencies.contains_key("tool:local.fifth"));
}

// ---------------------------------------------------------------------------
// 5. Duplicates are a refusal, not a choice
// ---------------------------------------------------------------------------

#[test]
fn a_duplicate_tool_name_is_refused_and_no_lockfile_is_written() {
    let project = Project::stdio(json!({ "tools": [tool("search"), tool("search")] }), &[]);

    let output = project.snapshot();

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("declared the tool `search` more than once"),
        "{}",
        stderr(&output)
    );
    assert!(
        !project.lock_exists(),
        "a refused discovery left a lockfile behind: {:?}",
        project.lock_bytes()
    );
}

#[test]
fn a_refused_discovery_leaves_an_existing_lockfile_untouched() {
    let project = Project::stdio(json!({ "tools": [tool("search"), tool("search")] }), &[]);
    // A lockfile this build can read, with content no discovery would produce.
    let existing = b"{\n  \"lock_version\": 1,\n  \"untouched\": true\n}\n";
    std::fs::write(project.path().join("agentchecksum.lock"), existing).expect("writable");

    let output = project.snapshot();

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert_eq!(project.lock_bytes(), existing, "the lockfile was rewritten");
}

// ---------------------------------------------------------------------------
// 6. Bounds are refusals too
// ---------------------------------------------------------------------------

#[test]
fn exceeding_the_tool_bound_is_refused_by_name_and_writes_nothing() {
    // `limits::MAX_TOOLS_PER_SERVER` is 10_000; one more than that is a catalog this
    // build will not describe.
    let catalog: Vec<Value> = (0..10_001)
        .map(|index| json!({ "name": format!("t{index}") }))
        .collect();
    let project = Project::stdio(json!({ "tools": catalog }), &[]);

    let output = project.snapshot();

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    let diagnostic = stderr(&output);
    assert!(
        diagnostic.contains("more than 10000 tools on one server"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("`local` (stdio)"), "{diagnostic}");
    assert!(
        !project.lock_exists(),
        "a refused discovery left a lockfile behind"
    );
}

// ---------------------------------------------------------------------------
// 7. A server that stops answering is a timeout, not a hang
// ---------------------------------------------------------------------------

/// Longer than `limits::PAGE_TIMEOUT`, so the client gives up first.
const BEYOND_THE_PAGE_TIMEOUT_MS: u64 = 30_500;

#[test]
fn a_server_that_hangs_past_the_page_timeout_is_named_with_the_stage() {
    let project = Project::stdio(
        json!({ "hang_ms": BEYOND_THE_PAGE_TIMEOUT_MS, "tools": [tool("search")] }),
        &[],
    );

    let output = project.snapshot();

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    let diagnostic = stderr(&output);
    assert!(
        diagnostic.contains("did not complete reading a page of tools within 30s"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("MCP server `local` (stdio)"),
        "{diagnostic}"
    );
    assert!(
        !project.lock_exists(),
        "a timed-out discovery left a lockfile behind"
    );
}

// ---------------------------------------------------------------------------
// 8. No orphaned servers
// ---------------------------------------------------------------------------

#[test]
fn the_server_process_is_gone_when_the_snapshot_returns() {
    let project = Project::stdio(json!({ "tools": [tool("search")] }), &[]);
    // The pid file lives beside the spec, named by the config's environment.
    let pid_file = project.path().join("fixture.pid");
    write_config(
        project.path(),
        &config(&stdio_server(
            "local",
            &project.spec,
            &[("AC_FIXTURE_PID_FILE", &pid_file.display().to_string())],
        )),
    );

    let output = project.snapshot();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("the server recorded its pid")
        .trim()
        .parse()
        .expect("the pid is a number");
    assert!(pid > 0, "the recorded pid is not a process id");

    // The session is closed before the command returns, and closing it is what
    // reaps the child; a short wait absorbs the exit without hiding a leak.
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_is_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "the fixture server (pid {pid}) outlived the snapshot"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether the operating system still knows this pid.
fn process_is_alive(pid: i32) -> bool {
    StdCommand::new("ps")
        .args(["-p", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("ps runs")
        .success()
}

// ---------------------------------------------------------------------------
// 9. The configured environment is not readable anywhere
// ---------------------------------------------------------------------------

/// Every surface a credential could reach: the artifacts, the two output streams,
/// and the machine-readable report.
fn assert_no_sentinel(project: &Project, output: &std::process::Output, json_report: Option<&str>) {
    for (surface, text) in [
        ("stdout", stdout(output)),
        ("stderr", stderr(output)),
        ("--format json", json_report.unwrap_or_default().to_string()),
    ] {
        assert!(
            !text.contains(SENTINEL),
            "the sentinel reached {surface}: {text}"
        );
    }
    if project.lock_exists() {
        let bytes = project.lock_bytes();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(SENTINEL),
            "the sentinel reached the lockfile: {text}"
        );
        assert!(
            !text.contains("TOKEN"),
            "the environment key reached the lockfile: {text}"
        );
    }
}

#[test]
fn the_configured_token_never_reaches_a_successful_run() {
    let project = Project::stdio(json!({ "tools": [tool("search")] }), &[("TOKEN", SENTINEL)]);

    let output = run(project.path(), &["snapshot", "--format", "json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_no_sentinel(&project, &output, Some(&stdout(&output)));

    let report = run(project.path(), &["diff", "--format", "json"]);
    assert_eq!(report.status.code(), Some(0), "{}", stderr(&report));
    assert_no_sentinel(&project, &report, Some(&stdout(&report)));
}

#[test]
fn the_configured_token_never_reaches_a_failed_discovery() {
    let project = Project::stdio(
        json!({ "tools": [tool("search"), tool("search")] }),
        &[("TOKEN", SENTINEL)],
    );

    let output = run(project.path(), &["snapshot", "--format", "json"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert_no_sentinel(&project, &output, Some(&stdout(&output)));
}

#[test]
fn the_configured_token_never_reaches_a_timeout() {
    let project = Project::stdio(
        json!({ "hang_ms": BEYOND_THE_PAGE_TIMEOUT_MS, "tools": [tool("search")] }),
        &[("TOKEN", SENTINEL)],
    );

    let output = run(project.path(), &["snapshot", "--format", "json"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert_no_sentinel(&project, &output, Some(&stdout(&output)));
}

// ---------------------------------------------------------------------------
// 10. A server's own stderr is not a diagnostic surface
// ---------------------------------------------------------------------------

/// The property: a server's log line is not a diagnostic of ours.
///
/// This is the one test that pins it. `tokio::process::Command`-level `stderr`
/// settings do not survive `TokioChildProcess::new`, whose builder re-applies
/// `Stdio::inherit()` as its default — so reverting the client to that constructor
/// turns the fixture's startup line back into a line on AgentChecksum's stderr, and
/// this test fails.
#[test]
fn a_servers_own_stderr_never_reaches_a_diagnostic() {
    let project = Project::stdio(
        json!({
            "stderr_secret": SENTINEL,
            "tools": [tool("search"), tool("search")]
        }),
        &[],
    );

    let output = project.snapshot();

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(
        !stderr(&output).contains(SENTINEL),
        "the server's stderr reached the diagnostic: {}",
        stderr(&output)
    );
    assert!(!stdout(&output).contains(SENTINEL));
}

// ---------------------------------------------------------------------------
// 11. Streamable HTTP
// ---------------------------------------------------------------------------

/// A fixture server this test started itself, killed when the test ends.
struct FixtureProcess(Child);

impl Drop for FixtureProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start the fixture in HTTP mode and wait for the port it bound.
fn start_http(project: &Project) -> (FixtureProcess, u16) {
    let port_file = project.path().join("port");
    let child = StdCommand::new(fixture())
        .arg("--http")
        .env("AC_FIXTURE_SPEC", &project.spec)
        .env("AC_FIXTURE_PORT_FILE", &port_file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the fixture server starts");
    let guard = FixtureProcess(child);

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

#[test]
fn a_streamable_http_server_is_discovered_the_same_as_a_stdio_one() {
    let declared = json!({
        "server_info": { "name": "fixture", "version": "1.0.0" },
        "tools": [tool("search")]
    });

    // The same server, discovered over the same protocol on the other transport.
    let over_stdio = Project::stdio(declared.clone(), &[]);
    over_stdio.committed_baseline();

    let over_http = Project::stdio(declared, &[]);
    let (fixture, port) = start_http(&over_http);
    write_config(over_http.path(), &config(&http_server("local", port)));

    let output = over_http.snapshot();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    drop(fixture);

    // Same dependency contract, same aggregate checksum: the transport is not part
    // of what a dependency is.
    assert_eq!(
        over_http.lock()["dependencies"],
        over_stdio.lock()["dependencies"]
    );
    assert_eq!(
        over_http.lock()["agent_checksum"],
        over_stdio.lock()["agent_checksum"]
    );

    // The endpoint is connection material, not identity: neither the origin nor its
    // ephemeral port is part of the contract that was discovered.
    let lock_text = String::from_utf8_lossy(&over_http.lock_bytes()).to_string();
    for fragment in ["127.0.0.1", &format!(":{port}")] {
        assert!(
            !lock_text.contains(fragment),
            "the endpoint reached the lockfile as `{fragment}`"
        );
    }
}

// ---------------------------------------------------------------------------
// 12. A server pinned to the session protocol
// ---------------------------------------------------------------------------

#[test]
fn a_server_whose_newest_revision_is_legacy_is_recorded_as_the_legacy_era() {
    let project = Project::stdio(
        json!({
            "legacy_only": true,
            "server_info": { "name": "fixture", "version": "1.0.0" },
            "tools": [tool("search")]
        }),
        &[],
    );

    let output = project.snapshot();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let identity =
        project.lock()["dependencies"]["mcp:local"]["facets"]["identity"]["normalized"].clone();
    assert_eq!(identity["era"], "legacy");
    let negotiated = identity["protocol_version"]
        .as_str()
        .expect("the negotiated version is a string")
        .to_string();
    assert!(
        negotiated.as_str() < "2026-07-28",
        "the negotiated version `{negotiated}` is not a pre-stateless revision"
    );
    assert_eq!(identity["supported_versions"], json!(["2025-11-25"]));
    // The tool contract is still discovered: falling back is a protocol decision,
    // not a discovery failure.
    assert!(
        project.lock()["dependencies"]["tool:local.search"]["facets"]["input_schema"].is_object()
    );
}
