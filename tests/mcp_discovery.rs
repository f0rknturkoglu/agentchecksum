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
fn snapshot_then(project: &Project, baseline: Value, changed: Value) -> Value {
    project.redeclare(baseline);
    project.committed_baseline();
    project.redeclare(changed);
    project.diff_report()
}

/// The same two steps for a project whose declared catalog is what changes.
fn baseline_then(project: &Project, baseline: Value, changed: Value) -> Value {
    snapshot_then(
        project,
        json!({ "tools": baseline }),
        json!({ "tools": changed }),
    )
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
fn credential_surfaces(
    project: &Project,
    output: &std::process::Output,
    json_report: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut surfaces = vec![
        ("stdout", stdout(output)),
        ("stderr", stderr(output)),
        ("--format json", json_report.unwrap_or_default().to_string()),
    ];
    if project.lock_exists() {
        surfaces.push((
            "the lockfile",
            String::from_utf8_lossy(&project.lock_bytes()).to_string(),
        ));
    }
    surfaces
}

/// Nothing a user can see repeats this value.
fn assert_secret_absent(
    project: &Project,
    output: &std::process::Output,
    json_report: Option<&str>,
    secret: &str,
) {
    for (surface, text) in credential_surfaces(project, output, json_report) {
        assert!(
            !text.contains(secret),
            "`{secret}` reached {surface}: {text}"
        );
    }
}

fn assert_no_sentinel(project: &Project, output: &std::process::Output, json_report: Option<&str>) {
    assert_secret_absent(project, output, json_report, SENTINEL);
    // The key is not part of the contract either: a lockfile naming the variable a
    // credential traveled in describes configuration the contract must not carry.
    if project.lock_exists() {
        let text = String::from_utf8_lossy(&project.lock_bytes()).to_string();
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

/// Shorter than the six-character threshold a length-based filter would have kept in
/// place. A credential is not less of a credential for being three characters long.
const SHORT_SENTINEL: &str = "x7p";

/// One short configured value, on the ways a run can end: a success, a discovery the
/// server's own declarations fail, a failure whose reason is the server's own text,
/// and a server that stops answering. The surfaces are checked whole — both streams,
/// the lockfile, and the JSON report — because redaction is a property of the run and
/// not of one message.
///
/// The third run is the one that can fail: it is the only one of the four where the
/// configured value is in the text the client is holding, so a filter that treated a
/// three-character value as too short to be a credential would publish it there. The
/// other three are sweeps of the surfaces that run produces.
#[test]
fn a_short_configured_token_never_reaches_any_surface() {
    let succeeded = Project::stdio(
        json!({ "tools": [tool("search")] }),
        &[("TOKEN", SHORT_SENTINEL)],
    );
    let output = run(succeeded.path(), &["snapshot", "--format", "json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_secret_absent(&succeeded, &output, Some(&stdout(&output)), SHORT_SENTINEL);

    let refused = Project::stdio(
        json!({ "tools": [tool("search"), tool("search")] }),
        &[("TOKEN", SHORT_SENTINEL)],
    );
    let output = run(refused.path(), &["snapshot", "--format", "json"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert_secret_absent(&refused, &output, Some(&stdout(&output)), SHORT_SENTINEL);

    let echoed = Project::stdio(
        json!({
            "discover": "refused",
            "discover_error_code": -32600,
            "discover_error_message": format!("failure for {SHORT_SENTINEL}"),
            "tools": [tool("search")]
        }),
        &[("TOKEN", SHORT_SENTINEL)],
    );
    let output = run(echoed.path(), &["snapshot", "--format", "json"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("[redacted]"),
        "the value was never treated as a secret at all: {}",
        stderr(&output)
    );
    assert_secret_absent(&echoed, &output, Some(&stdout(&output)), SHORT_SENTINEL);

    let stalled = Project::stdio(
        json!({ "hang_ms": BEYOND_THE_PAGE_TIMEOUT_MS, "tools": [tool("search")] }),
        &[("TOKEN", SHORT_SENTINEL)],
    );
    let output = run(stalled.path(), &["snapshot", "--format", "json"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert_secret_absent(&stalled, &output, Some(&stdout(&output)), SHORT_SENTINEL);
}

/// A failure reason built out of the server's own text is sanitized where it is
/// produced.
///
/// `-32600` fails closed, so the reason in the diagnostic is the peer's message
/// verbatim — the fixture is handed the configured value and echoes it back. The
/// `[redacted]` marker is half the assertion: it proves the text was carried and
/// sanitized rather than dropped, so a future change that stopped quoting the peer
/// would not pass this test by accident.
#[test]
fn a_server_echoed_value_never_reaches_a_failure_diagnostic() {
    let project = Project::stdio(
        json!({
            "discover": "refused",
            "discover_error_code": -32600,
            "discover_error_message": format!("failure for {SHORT_SENTINEL}"),
            "tools": [tool("search")]
        }),
        &[("TOKEN", SHORT_SENTINEL)],
    );

    let output = run(project.path(), &["snapshot", "--format", "json"]);

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    let diagnostic = stderr(&output);
    assert!(
        diagnostic.contains("connecting and negotiating"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("[redacted]"),
        "the peer's text was dropped rather than sanitized: {diagnostic}"
    );
    assert_secret_absent(&project, &output, Some(&stdout(&output)), SHORT_SENTINEL);
}

/// The same echoed value on the legacy fallback path, where the session does start
/// and the client writes its note about the optional discovery metadata from a
/// failure the server was on the other end of.
///
/// The note is a warning rather than a failure, which is the reason this is a test of
/// its own: a warning is the surface a sanitizing bug reaches a user through without
/// anything failing. The value the server was handed appears nowhere, and the note
/// still names the server it is about.
#[test]
fn a_server_echoed_value_never_reaches_the_supported_versions_warning() {
    let project = Project::stdio(
        json!({
            "discover": "refused",
            "discover_error_message": format!("failure for {SHORT_SENTINEL}"),
            "tools": [tool("search")]
        }),
        &[("TOKEN", SHORT_SENTINEL)],
    );

    let output = run(project.path(), &["snapshot", "--format", "json"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // The refusal is what moved the session to the legacy lifecycle, and the warning
    // below is the one the fallback path produces.
    assert_eq!(
        project.lock()["dependencies"]["mcp:local"]["facets"]["identity"]["normalized"]["era"],
        "legacy"
    );

    let report = stdout(&output);
    let warning = report
        .lines()
        .find(|line| line.contains("did not report its supported protocol versions"))
        .unwrap_or_else(|| panic!("the supported-versions warning never fired: {report}"));
    assert!(warning.contains("`mcp:local`"), "unattributed: {warning}");
    assert_secret_absent(&project, &output, Some(&report), SHORT_SENTINEL);
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
    // A fixture reads its spec once, at startup, so a test that changes the
    // declarations restarts one — and a port file left by the previous process must
    // not be read as this one's.
    let _ = std::fs::remove_file(&port_file);
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
// 12. A server whose newest revision is the session one
// ---------------------------------------------------------------------------

/// Answering `server/discover` with only the session revision is a failure, not a
/// downgrade — and this is the test that keeps `2025-11-25` out of the stateless
/// candidate list.
///
/// The fixture advertises only the session revision, so the stateless handshake has
/// nothing to negotiate: the peer answered the modern opener by naming a revision the
/// modern lifecycle does not speak, which is a rejected handshake rather than an
/// invitation to run the session one. Recording it as a legacy server would describe
/// an era that never happened — and, because a snapshot's whole purpose is to notice
/// when a dependency moves, it would re-fingerprint every such server the day someone
/// added `2025-11-25` to `preferred_versions()` as a compatibility improvement.
#[test]
fn a_discover_response_advertising_only_the_session_revision_fails_closed() {
    let project = Project::stdio(
        json!({
            "legacy_only": true,
            "server_info": { "name": "fixture", "version": "1.0.0" },
            "tools": [tool("search")]
        }),
        &[],
    );
    let log = start_log(&project);

    let output = project.snapshot();

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    let diagnostic = stderr(&output);
    // The diagnosis names both sides of the disagreement. `2025-11-25` is in it
    // because that is what the peer offered, and a rejection that did not say what it
    // rejected would be a code rather than a diagnostic.
    assert!(
        diagnostic.contains("no compatible protocol version"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("2026-07-28"), "{diagnostic}");
    assert!(diagnostic.contains("2025-11-25"), "{diagnostic}");
    assert!(
        diagnostic.contains("connecting and negotiating"),
        "{diagnostic}"
    );

    // Nothing was negotiated, so nothing is recorded: no lockfile, no era, and none of
    // the lifecycle a fallback would have run.
    assert!(
        !project.lock_exists(),
        "a failed negotiation wrote a lockfile: {:?}",
        project.lock_bytes()
    );
    assert!(
        !diagnostic.contains("legacy lifecycle"),
        "a rejected revision was retried as a legacy connection: {diagnostic}"
    );
    assert_eq!(
        attempts(&log),
        1,
        "the server was asked a second time for a revision it had refused"
    );

    // The revision is a candidate the client refused, never an era a server was given:
    // it appears in the reason, in the line that says what was rejected, and nowhere
    // else — not on stdout, and not in any file the run produced.
    for line in diagnostic
        .lines()
        .filter(|line| line.contains("2025-11-25"))
    {
        assert!(
            line.contains("no compatible protocol version"),
            "the refused revision reached a line that is not the rejection: {line}"
        );
    }
    assert_no_legacy_text("stdout", &stdout(&output));
    assert_no_legacy_files(&project);
}

// ---------------------------------------------------------------------------
// 13. The lifecycle policy: only the peer decides the era
// ---------------------------------------------------------------------------

/// Just past the ten-second cap the SDK's own `Auto` policy puts on a
/// `server/discover` probe, and far inside `limits::CONNECT_TIMEOUT`.
const JUST_OVER_THE_SDK_DISCOVER_CAP_MS: u64 = 12_000;

/// Past `limits::CONNECT_TIMEOUT`, so the client gives up before the server answers.
const BEYOND_THE_CONNECT_TIMEOUT_MS: u64 = 61_000;

/// Re-point the one configured stdio server at its spec with the fixture's start log
/// enabled, and return that log's path.
fn start_log(project: &Project) -> PathBuf {
    let path = project.path().join("fixture-attempts");
    write_config(
        project.path(),
        &config(&stdio_server(
            "local",
            &project.spec,
            &[("AC_FIXTURE_ATTEMPT_FILE", &path.display().to_string())],
        )),
    );
    path
}

/// How many times the fixture was started. A log that was never created is zero
/// starts, which is what a client that started nothing leaves behind.
fn attempts(log: &Path) -> usize {
    std::fs::read_to_string(log)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

/// No output stream, and no file, mentions the session era or its revision.
///
/// Checked as a whole surface rather than on the diagnostic alone: a legacy retry
/// that is *logged* rather than reported is still a retry, and a lockfile naming the
/// wrong era is the failure this policy exists to prevent.
fn assert_no_legacy_surface(project: &Project, output: &std::process::Output) {
    assert_no_legacy_text("stdout", &stdout(output));
    assert_no_legacy_text("stderr", &stderr(output));
    assert_no_legacy_files(project);
}

/// A stream that names no era this run did not negotiate.
///
/// `legacy` is looked for as the label it is — the `legacy lifecycle` stage in a
/// diagnostic, or `"legacy"` as a recorded value — never as a bare word. A transport
/// error can name an unrelated crate's `legacy` client, and a test that failed on
/// that would be reporting a false positive about a real connection failure.
fn assert_no_legacy_text(surface: &str, text: &str) {
    for needle in ["2025-11-25", "legacy lifecycle"] {
        assert!(
            !text.contains(needle),
            "`{needle}` reached {surface}: {text}"
        );
    }
}

/// And no artifact on disk that does.
fn assert_no_legacy_files(project: &Project) {
    for entry in std::fs::read_dir(project.path()).expect("the project directory is readable") {
        let path = entry.expect("the directory entry is readable").path();
        let bytes = std::fs::read(&path).expect("the artifact is readable");
        let text = String::from_utf8_lossy(&bytes);
        for needle in ["2025-11-25", "\"legacy\""] {
            assert!(
                !text.contains(needle),
                "`{needle}` reached {}: {text}",
                path.display()
            );
        }
    }
}

/// A port nothing is listening on: bound to learn a free number, then released.
fn unused_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a port is bindable");
    let port = listener
        .local_addr()
        .expect("a bound socket has an address")
        .port();
    drop(listener);
    port
}

/// A slow `server/discover` is latency, and latency must not choose the protocol era.
///
/// The SDK's `Auto` policy abandons the probe after ten seconds and initializes a
/// session instead, which would record this server as a legacy one — the same
/// declarations, a different dependency identity, decided by how busy the machine
/// was. The delay here is deliberately just over that cap.
#[test]
fn a_slow_stateless_handshake_still_negotiates_the_stateless_era() {
    let project = Project::stdio(
        json!({
            "discover": "delayed",
            "discover_delay_ms": JUST_OVER_THE_SDK_DISCOVER_CAP_MS,
            "tools": [tool("search")]
        }),
        &[],
    );
    let log = start_log(&project);

    let output = project.snapshot();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let identity =
        project.lock()["dependencies"]["mcp:local"]["facets"]["identity"]["normalized"].clone();
    assert_eq!(identity["era"], "stateless");
    assert_eq!(identity["protocol_version"], "2026-07-28");
    assert!(
        project.lock()["dependencies"]["tool:local.search"]["facets"]["input_schema"].is_object()
    );
    assert_eq!(
        attempts(&log),
        1,
        "the handshake was retried by starting the server a second time"
    );
}

/// A server that never answers is a timeout, not evidence about its age.
///
/// This is the expensive one: it waits out `limits::CONNECT_TIMEOUT` on purpose, so
/// it costs about a minute. What it buys is the rule that a slow server cannot be
/// discovered *as a legacy server*, which is what a timeout-triggered fallback
/// would do — against this fixture the session handshake would answer immediately
/// and the snapshot would succeed.
#[test]
fn a_handshake_that_never_answers_is_a_timeout_and_never_a_second_attempt() {
    let project = Project::stdio(
        json!({
            "discover": "delayed",
            "discover_delay_ms": BEYOND_THE_CONNECT_TIMEOUT_MS,
            "tools": [tool("search")]
        }),
        &[],
    );
    let log = start_log(&project);

    let output = project.snapshot();

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    let diagnostic = stderr(&output);
    assert!(diagnostic.contains("`local` (stdio)"), "{diagnostic}");
    assert!(
        diagnostic.contains("did not complete connecting and negotiating within 60s"),
        "{diagnostic}"
    );
    assert!(
        !project.lock_exists(),
        "a timed-out handshake left a lockfile behind"
    );
    assert_eq!(attempts(&log), 1, "the server was started a second time");
    assert_no_legacy_surface(&project, &output);
}

/// The fallback a peer's refusal can trigger: the one signal that is evidence of a
/// server that predates `server/discover`.
///
/// The fixture answers `server/discover` with the JSON-RPC error that means "I do not
/// implement that method", and then accepts the session handshake at `2025-11-25`.
///
/// The spec deliberately does not pin the fixture to the session revision
/// (`legacy_only`). The SDK's server refuses a stateless `server/discover` that
/// declares a revision it does not implement *before* the fixture's own `discover`
/// runs, so a pinned server would fail the handshake with `-32022` instead of
/// refusing the method — and the client would be right not to fall back. That is the
/// fail-closed test above; this is the other path.
#[test]
fn a_server_that_refuses_discover_is_discovered_through_the_session_lifecycle() {
    let project = Project::stdio(
        json!({
            "discover": "refused",
            "server_info": { "name": "fixture", "version": "1.0.0" },
            "tools": [tool("search")]
        }),
        &[],
    );
    let log = start_log(&project);

    let output = project.snapshot();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let identity =
        project.lock()["dependencies"]["mcp:local"]["facets"]["identity"]["normalized"].clone();
    assert_eq!(identity["era"], "legacy");
    assert_eq!(identity["protocol_version"], "2025-11-25");
    assert_eq!(
        identity["server_info"],
        json!({ "name": "fixture", "version": "1.0.0" })
    );
    assert!(
        project.lock()["dependencies"]["tool:local.search"]["facets"]["input_schema"].is_object(),
        "the session lifecycle discovered no tool contract"
    );

    // The fallback is the one path that costs a second process: the transport the
    // first attempt consumed cannot be reused, so the peer is started twice and the
    // second session is the one that was introspected.
    assert_eq!(
        attempts(&log),
        2,
        "the legacy fallback opened its session without starting the server again"
    );
}

/// A failure that is not `METHOD_NOT_FOUND` is not evidence about a peer's age.
///
/// `-32600` is a peer saying the request was wrong, and `-32603` is a peer saying it
/// is unwell. Both are transient or local statements: a client that read either as a
/// handshake answer would open a session against a server that never claimed to
/// predate `server/discover`, and would then record an era chosen by an error the peer
/// did not mean that way.
#[test]
fn an_invalid_request_is_not_evidence_of_a_legacy_peer() {
    let project = Project::stdio(
        json!({
            "discover": "refused",
            "discover_error_code": -32600,
            "tools": [tool("search")]
        }),
        &[],
    );
    let log = start_log(&project);

    let output = project.snapshot();

    assert_discover_failure_fails_closed(&project, &output, &log, "-32600");
}

#[test]
fn an_internal_error_is_not_evidence_of_a_legacy_peer() {
    let project = Project::stdio(
        json!({
            "discover": "refused",
            "discover_error_code": -32603,
            "tools": [tool("search")]
        }),
        &[],
    );
    let log = start_log(&project);

    let output = project.snapshot();

    assert_discover_failure_fails_closed(&project, &output, &log, "-32603");
}

/// A `server/discover` failure that is not `METHOD_NOT_FOUND`: refused, attributed,
/// with no session, no artifact, no second process, and no mention of the lifecycle a
/// fallback would have run.
fn assert_discover_failure_fails_closed(
    project: &Project,
    output: &std::process::Output,
    log: &Path,
    code: &str,
) {
    assert_eq!(output.status.code(), Some(3), "{}", stderr(output));
    let diagnostic = stderr(output);
    assert!(diagnostic.contains("`local` (stdio)"), "{diagnostic}");
    assert!(
        diagnostic.contains("connecting and negotiating"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains(code),
        "the code the peer answered with is not in the diagnostic: {diagnostic}"
    );
    assert!(
        !diagnostic.contains("legacy lifecycle"),
        "the failure was retried as a legacy connection: {diagnostic}"
    );
    assert!(
        !project.lock_exists(),
        "a failed handshake left a lockfile behind: {:?}",
        project.lock_bytes()
    );
    assert_eq!(
        attempts(log),
        1,
        "a failure that is not method-not-found was retried against a second process"
    );
    assert_no_legacy_surface(project, output);
}

/// An unreachable server is unreachable, not old.
///
/// The second attempt is what a transport-triggered fallback would open; the stage
/// in the diagnostic is what tells the two apart, because the legacy attempt reports
/// a different one.
#[test]
fn a_transport_failure_is_not_retried_as_a_legacy_connection() {
    let project = Project::stdio(json!({ "tools": [tool("search")] }), &[]);
    let port = unused_port();
    write_config(project.path(), &config(&http_server("local", port)));
    let log = project.path().join("fixture-attempts");

    let output = project.snapshot();

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    let diagnostic = stderr(&output);
    assert!(
        diagnostic.contains("`local` (streamable-http)"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("connecting and negotiating"),
        "{diagnostic}"
    );
    assert!(
        !diagnostic.contains("legacy lifecycle"),
        "the failure was retried as a legacy connection: {diagnostic}"
    );
    assert!(
        !project.lock_exists(),
        "an unreachable server left a lockfile behind"
    );
    assert_eq!(
        attempts(&log),
        0,
        "a server was started for a dead endpoint"
    );
    assert_no_legacy_surface(&project, &output);
}

// ---------------------------------------------------------------------------
// 14. Server instructions: a digest, a shape, and no prose
// ---------------------------------------------------------------------------

/// Two sentences, so a reflow has somewhere to happen.
const INSTRUCTIONS: &str = "Prefer the read-only tools.\nSearch the index before answering.";

#[test]
fn instructions_are_a_digest_and_a_shape_and_carry_no_text() {
    let project = Project::stdio(
        json!({ "instructions": INSTRUCTIONS, "tools": [tool("search")] }),
        &[],
    );
    project.committed_baseline();

    let facet = project.lock()["dependencies"]["mcp:local"]["facets"]["instructions"].clone();
    assert!(
        facet["digest"]
            .as_str()
            .is_some_and(|digest| digest.starts_with("sha256:")),
        "the instructions facet has no content digest: {facet}"
    );
    assert!(
        facet["shape"]
            .as_str()
            .is_some_and(|shape| shape.starts_with("sha256:")),
        "the instructions facet has no shape digest: {facet}"
    );
    assert!(
        facet.get("normalized").is_none(),
        "the instructions were recorded as a payload: {facet}"
    );

    // A server that declares none has no instructions facet: missing is a different
    // statement from empty, and Phase 2 needs to see the difference.
    let silent = Project::stdio(json!({ "tools": [tool("search")] }), &[]);
    silent.committed_baseline();
    assert!(
        silent.lock()["dependencies"]["mcp:local"]["facets"]
            .get("instructions")
            .is_none(),
        "a server without instructions grew an instructions facet"
    );
}

#[test]
fn a_reflowed_instruction_is_low_and_classified_as_formatting_only() {
    let project = Project::stdio(
        json!({ "instructions": INSTRUCTIONS, "tools": [tool("search")] }),
        &[],
    );

    // The same words, on one line.
    let reflowed = INSTRUCTIONS.replace('\n', " ");
    let report = snapshot_then(
        &project,
        json!({ "instructions": INSTRUCTIONS, "tools": [tool("search")] }),
        json!({ "instructions": reflowed, "tools": [tool("search")] }),
    );

    assert_eq!(report["overall_risk"], "low");
    let change = one_change(&report, "mcp:local");
    assert_eq!(change["change"], "modified");
    let instructions = facet(&change, "instructions");
    assert_eq!(instructions["risk"], "low");
    assert_eq!(instructions["details"][0]["path"], "classification");
    assert_eq!(instructions["details"][0]["after"], "formatting-only");
    assert!(
        one_change_opt(&report, "tool:local.search").is_none(),
        "the tool contract moved with the instructions"
    );
}

#[test]
fn a_rewritten_instruction_is_medium_and_classified_as_a_text_change() {
    let project = Project::stdio(
        json!({ "instructions": INSTRUCTIONS, "tools": [tool("search")] }),
        &[],
    );

    let report = snapshot_then(
        &project,
        json!({ "instructions": INSTRUCTIONS, "tools": [tool("search")] }),
        json!({
            "instructions": "Always answer from the index cache.",
            "tools": [tool("search")]
        }),
    );

    assert_eq!(report["overall_risk"], "medium");
    let instructions = facet(&one_change(&report, "mcp:local"), "instructions");
    assert_eq!(instructions["risk"], "medium");
    assert_eq!(instructions["details"][0]["path"], "classification");
    assert_eq!(instructions["details"][0]["after"], "text-changed");
    assert!(one_change_opt(&report, "tool:local.search").is_none());
}

/// Instructions appearing and disappearing are the facet rules the engine already
/// has for a payload it has nothing to compare against: added MEDIUM, removed HIGH.
#[test]
fn instructions_added_are_medium_and_instructions_removed_are_high() {
    let added = Project::stdio(json!({ "tools": [tool("search")] }), &[]);
    let report = snapshot_then(
        &added,
        json!({ "tools": [tool("search")] }),
        json!({ "instructions": INSTRUCTIONS, "tools": [tool("search")] }),
    );
    assert_eq!(report["overall_risk"], "medium");
    let instructions = facet(&one_change(&report, "mcp:local"), "instructions");
    assert_eq!(instructions["change"], "added");
    assert_eq!(instructions["risk"], "medium");

    let removed = Project::stdio(
        json!({ "instructions": INSTRUCTIONS, "tools": [tool("search")] }),
        &[],
    );
    let report = snapshot_then(
        &removed,
        json!({ "instructions": INSTRUCTIONS, "tools": [tool("search")] }),
        json!({ "tools": [tool("search")] }),
    );
    assert_eq!(report["overall_risk"], "high");
    let instructions = facet(&one_change(&report, "mcp:local"), "instructions");
    assert_eq!(instructions["change"], "removed");
    assert_eq!(instructions["risk"], "high");
}

#[test]
fn changing_only_the_instructions_moves_the_aggregate_checksum() {
    let project = Project::stdio(
        json!({ "instructions": INSTRUCTIONS, "tools": [tool("search")] }),
        &[],
    );
    project.committed_baseline();
    let before = project.lock();

    project.redeclare(json!({
        "instructions": "Search the index before answering, and prefer the read-only tools.",
        "tools": [tool("search")]
    }));
    project.committed_baseline();
    let after = project.lock();

    assert_ne!(
        before["agent_checksum"], after["agent_checksum"],
        "the instructions are not part of the aggregate"
    );
    assert_eq!(
        before["dependencies"]["tool:local.search"], after["dependencies"]["tool:local.search"],
        "the tool contract moved with the instructions"
    );
    assert_ne!(
        before["dependencies"]["mcp:local"]["facets"]["instructions"],
        after["dependencies"]["mcp:local"]["facets"]["instructions"]
    );
}

#[test]
fn the_instructions_text_never_reaches_the_lockfile() {
    let project = Project::stdio(
        json!({ "instructions": INSTRUCTIONS, "tools": [tool("search")] }),
        &[],
    );
    project.committed_baseline();

    let text = String::from_utf8_lossy(&project.lock_bytes()).to_string();
    for fragment in [
        INSTRUCTIONS,
        "Prefer the read-only tools.",
        "Search the index before answering.",
    ] {
        assert!(
            !text.contains(fragment),
            "the instructions reached the lockfile as `{fragment}`: {text}"
        );
    }
}

/// The other transport, end to end: the fixture's `server/discover` and its session
/// `initialize` both carry the same instructions, and the diff sees the change.
///
/// The fixture reads its spec once, at startup, so the declarations change under a
/// new process: this is the path a test would otherwise get wrong and read as "the
/// instructions never moved".
#[test]
fn an_instruction_change_is_observed_end_to_end_over_streamable_http() {
    let project = Project::stdio(
        json!({ "instructions": INSTRUCTIONS, "tools": [tool("search")] }),
        &[],
    );
    let (fixture, port) = start_http(&project);
    write_config(project.path(), &config(&http_server("local", port)));
    project.committed_baseline();
    drop(fixture);

    project.redeclare(json!({
        "instructions": "Always answer from the index cache.",
        "tools": [tool("search")]
    }));
    let (fixture, port) = start_http(&project);
    write_config(project.path(), &config(&http_server("local", port)));
    let report = project.diff_report();
    drop(fixture);

    assert_eq!(report["overall_risk"], "medium");
    let instructions = facet(&one_change(&report, "mcp:local"), "instructions");
    assert_eq!(instructions["risk"], "medium");
    assert_eq!(instructions["details"][0]["after"], "text-changed");
}

// ---------------------------------------------------------------------------
// 15. A tool the server itself declares destructive
// ---------------------------------------------------------------------------

/// A tool whose own declaration says it writes and destroys.
fn destructive_tool(name: &str) -> Value {
    let mut declared = tool(name);
    declared["annotations"] = json!({ "readOnlyHint": false, "destructiveHint": true });
    declared
}

/// A tool whose own declaration says it writes, and says nothing about destroying.
fn writing_tool(name: &str) -> Value {
    let mut declared = tool(name);
    declared["annotations"] = json!({ "readOnlyHint": false, "destructiveHint": false });
    declared
}

/// A new tool is HIGH. A new tool the server declares write-capable *and*
/// destructive is CRITICAL, and the escalation needs both tokens: this pins the
/// three declarations side by side so the escalation cannot quietly widen.
#[test]
fn an_added_destructive_tool_is_critical_and_a_writing_or_read_only_one_is_high() {
    let project = Project::stdio(json!({ "tools": [tool("search")] }), &[]);
    let report = baseline_then(
        &project,
        json!([tool("search")]),
        json!([tool("search"), destructive_tool("purge")]),
    );

    assert_eq!(report["overall_risk"], "critical");
    let change = one_change(&report, "tool:local.purge");
    assert_eq!(change["change"], "added");
    assert_eq!(change["risk"], "critical");
    // The escalation reads the recorded tokens, and they are the ones the
    // declaration implies. The report holds no facets for an added dependency, so
    // this is read from a snapshot of the changed declaration.
    project.committed_baseline();
    assert_eq!(
        project.lock()["dependencies"]["tool:local.purge"]["facets"]["capabilities"]["normalized"],
        json!(["destructive", "non-idempotent", "open-world", "write"])
    );

    // Writing but not destructive is an ordinary new surface.
    let project = Project::stdio(json!({ "tools": [tool("search")] }), &[]);
    let report = baseline_then(
        &project,
        json!([tool("search")]),
        json!([tool("search"), writing_tool("purge")]),
    );
    assert_eq!(one_change(&report, "tool:local.purge")["risk"], "high");
    assert_eq!(report["overall_risk"], "high");

    // And so is read-only, which is the same diff the existing added-tool test runs.
    let project = Project::stdio(json!({ "tools": [tool("search")] }), &[]);
    let report = baseline_then(
        &project,
        json!([tool("search")]),
        json!([tool("search"), tool("purge")]),
    );
    assert_eq!(one_change(&report, "tool:local.purge")["risk"], "high");
    assert_eq!(report["overall_risk"], "high");
}

// ---------------------------------------------------------------------------
// 16. Opaque `_meta`: presence is a warning, the value is not a fingerprint
// ---------------------------------------------------------------------------

/// The extension key and value a server attaches to a tool. Neither may be read.
const OPAQUE_KEY: &str = "com.example/private-note";
const OPAQUE_VALUE: &str = "SUPER_SECRET_OPAQUE_METADATA_VALUE";

fn with_meta(name: &str, value: &str) -> Value {
    let mut declared = tool(name);
    declared["meta"] = json!({ OPAQUE_KEY: value });
    declared
}

/// A project with two stdio servers, each with its own spec.
fn two_stdio_servers(first: Value, second: Value) -> Project {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let first_spec = write_spec(dir.path(), "first.json", first);
    let second_spec = write_spec(dir.path(), "second.json", second);
    write_config(
        dir.path(),
        &config(&format!(
            "{}\n{}",
            stdio_server("local", &first_spec, &[]),
            stdio_server("remote", &second_spec, &[])
        )),
    );
    Project {
        dir,
        spec: first_spec,
    }
}

/// `_meta` is a presence, aggregated per server, and never a value.
///
/// The fixture declares it on two tools of one server and one tool of another, so
/// this pins all four properties at once: one warning line per server however many
/// tools carry metadata, no key or value in that line, a lockfile identical to the
/// same declarations without `_meta`, and — because a server that rotates a metadata
/// value is not a server that changed its contract — no move in the checksum when
/// only the value does.
#[test]
fn opaque_tool_metadata_is_one_warning_per_server_and_never_a_fingerprint() {
    let decorated = two_stdio_servers(
        json!({ "tools": [with_meta("search", OPAQUE_VALUE), with_meta("fetch", OPAQUE_VALUE)] }),
        json!({ "tools": [with_meta("search", OPAQUE_VALUE)] }),
    );

    let output = decorated.snapshot();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let report = stdout(&output);
    let warnings: Vec<&str> = report
        .lines()
        .filter(|line| line.contains("declared opaque MCP metadata"))
        .collect();
    assert_eq!(warnings.len(), 2, "{report}");
    assert!(
        warnings
            .iter()
            .any(|line| line.contains("2 tools declared") && line.contains("`mcp:local`")),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|line| line.contains("1 tool declared") && line.contains("`mcp:remote`")),
        "{warnings:?}"
    );
    assert!(!report.contains(OPAQUE_KEY), "{report}");
    assert!(!report.contains("private-note"), "{report}");
    assert!(!report.contains(OPAQUE_VALUE), "{report}");

    // The same two servers, declaring the same tools without `_meta`.
    let plain = two_stdio_servers(
        json!({ "tools": [tool("search"), tool("fetch")] }),
        json!({ "tools": [tool("search")] }),
    );
    plain.committed_baseline();
    assert_eq!(
        plain.lock_bytes(),
        decorated.lock_bytes(),
        "`_meta` reached the lockfile"
    );
    assert_eq!(
        plain.lock()["agent_checksum"],
        decorated.lock()["agent_checksum"]
    );

    // A different value for the same key is the same contract.
    let rotated = two_stdio_servers(
        json!({ "tools": [with_meta("search", "rotated-value"), with_meta("fetch", "rotated-value")] }),
        json!({ "tools": [with_meta("search", "rotated-value")] }),
    );
    rotated.committed_baseline();
    assert_eq!(
        rotated.lock_bytes(),
        decorated.lock_bytes(),
        "a `_meta` value was fingerprinted"
    );
}
