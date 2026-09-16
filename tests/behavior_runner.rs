// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end capture: the real runner against a real HTTP endpoint.
//!
//! These tests drive `agentchecksum`'s runner against `examples/openai_fixture_server`,
//! which speaks the OpenAI chat-completions contract over a real socket and records
//! every request it receives. What they check is the part no unit test can reach: that
//! the bytes the runner puts on the wire are the bytes a probe is supposed to be about.
//! A request-body assertion read back from the fixture fails if the runner drops the
//! system prompt, the tool catalog, or the recorded defaults, and it fails for the
//! right reason rather than for a missing mock expectation.
//!
//! The cache tests are here for the same reason: a hit that skips the endpoint can
//! only be observed from the endpoint's side.
//!
//! Every server listens on `127.0.0.1` only; nothing here touches the network.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use agentchecksum::config::{Config, ModelConfig};
use agentchecksum::error::Error;
use agentchecksum::lockfile::Lockfile;
use agentchecksum::manifest::{Dependency, DependencyKind, Digest, Facet};
use agentchecksum::runner::{CaptureRequest, Runner, ToolCatalog, system_prompt};
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
/// `cargo test --test behavior_runner` does not build example targets and does not
/// rebuild a stale one either, so the example this test drives is built once per test
/// process: a test that ran against a binary from an earlier edit would be evidence
/// about code nobody is looking at. Warm artifacts make the build a fraction of a
/// second.
fn fixture() -> PathBuf {
    static BUILT: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(|| {
        let exe = std::env::current_exe().expect("the test binary has a path");
        let profile = exe
            .parent()
            .and_then(Path::parent)
            .expect("the test binary lives in <target>/<profile>/deps")
            .to_path_buf();
        let path = profile.join("examples").join("openai_fixture_server");

        build_example(&profile);

        assert!(
            path.exists(),
            "the OpenAI fixture server is not built at {}; build it with `cargo build --example \
             openai_fixture_server`",
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
        .args(["build", "--example", "openai_fixture_server"])
        .current_dir(env!("CARGO_MANIFEST_DIR"));
    if profile.file_name().is_some_and(|name| name == "release") {
        command.arg("--release");
    }

    let status = command.status().expect("cargo runs");
    assert!(
        status.success(),
        "building the OpenAI fixture server failed"
    );
}

/// Start the fixture for a project and wait for the port it bound.
///
/// `options` is merged into the spec's top level, so a test can state a delay without
/// a second constructor.
fn start(project: &Path, responses: Vec<Value>, options: Value) -> (Server, u16) {
    let spec_path = project.join("spec.json");
    let mut spec = json!({
        "responses": responses,
        "record_file": project.join("requests.jsonl").display().to_string(),
    });
    for (key, value) in options.as_object().into_iter().flatten() {
        spec[key] = value.clone();
    }
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

/// Every request the fixture received, in order.
fn requests(project: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(project.join("requests.jsonl")) else {
        return Vec::new();
    };
    text.lines()
        .map(|line| serde_json::from_str(line).expect("a recorded request is JSON"))
        .collect()
}

/// The one request the fixture received, failing if there was not exactly one.
fn one_request(project: &Path) -> Value {
    let recorded = requests(project);
    assert_eq!(recorded.len(), 1, "{recorded:#?}");
    recorded[0].clone()
}

fn text(content: &str) -> Value {
    json!({ "type": "text", "content": content })
}

// ---------------------------------------------------------------------------
// The project: a config, a prompt, a catalog
// ---------------------------------------------------------------------------

fn write_prompt(project: &Path, relative: &str, text: &str) {
    let path = project.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A config declaring two prompts, in an order that is deliberately not the order
/// their ids sort in.
fn config(project: &Path) -> Config {
    std::fs::write(
        project.join("agentchecksum.toml"),
        "version = 1\n\n[agent]\nname = \"behavior-runner-test\"\n\n\
         [[prompts]]\npath = \"prompts/second.md\"\n\n\
         [[prompts]]\npath = \"prompts/first.md\"\n",
    )
    .unwrap();
    Config::load(&project.join("agentchecksum.toml")).expect("the test config is valid")
}

fn model(port: u16, params: &[(&str, Value)]) -> ModelConfig {
    ModelConfig {
        provider: "openai-compatible".to_string(),
        id: "fixture-model".to_string(),
        // A bare origin: the runner has to derive `/v1/chat/completions` from it, and
        // the path the fixture records is what proves it did.
        endpoint: Some(format!("http://127.0.0.1:{port}")),
        params: params
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect(),
    }
}

/// A one-tool catalog whose description can be changed, so a test can move the
/// catalog digest without touching anything else.
fn catalog(description: &str) -> ToolCatalog {
    let mut facets = BTreeMap::new();
    facets.insert(
        "input_schema".to_string(),
        Facet {
            digest: Digest::sha256(b"input_schema"),
            shape: None,
            normalized: Some(json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            })),
        },
    );
    facets.insert(
        "description".to_string(),
        Facet {
            digest: Digest::sha256(description.as_bytes()),
            shape: None,
            normalized: Some(Value::String(description.to_string())),
        },
    );

    let lockfile = Lockfile::from_dependencies(&[Dependency {
        id: "tool:fixture.search_repositories".to_string(),
        kind: DependencyKind::Tool,
        facets,
        source: Some("fixture".to_string()),
    }])
    .unwrap();

    ToolCatalog::from_lockfile(&lockfile).unwrap()
}

fn capture_request<'a>(probe_digest: &'a str, prompt: &'a str) -> CaptureRequest<'a> {
    CaptureRequest {
        probe: "repository-search",
        probe_digest,
        agent_checksum: "ac1:fixture",
        prompt,
        repeat: 1,
    }
}

/// The final text of every sample, in index order.
fn texts(runner_output: &agentchecksum::runner::Trace) -> Vec<Option<String>> {
    runner_output
        .samples
        .iter()
        .map(|sample| sample.final_text.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// 1. What the runner sends
// ---------------------------------------------------------------------------

/// The load-bearing test of the capture half: the request that reaches the endpoint
/// carries the assembled system prompt (sorted by dependency id), the probe prompt,
/// the tool catalog with its input schemas, and the recorded defaults.
///
/// Delete the system prompt from the request and this fails; that is the point.
#[tokio::test]
async fn the_request_carries_the_assembled_system_prompt_and_the_tool_catalog() {
    let project = tempfile::tempdir().unwrap();
    write_prompt(project.path(), "prompts/first.md", "Be terse.\n");
    write_prompt(project.path(), "prompts/second.md", "Never guess.\n");
    let config = config(project.path());

    let (server, port) = start(project.path(), vec![text(r#"{"answer":42}"#)], json!({}));
    let system = system_prompt(&config, project.path()).unwrap();
    let runner = Runner::new(
        &model(port, &[]),
        catalog("Search the index."),
        system,
        project.path(),
    )
    .unwrap();

    let trace = runner
        .capture(&capture_request("sha256:probe-1", "Find repositories."))
        .await
        .unwrap();
    drop(server);

    let recorded = one_request(project.path());
    assert_eq!(recorded["method"], "POST");
    // The bare origin became the contract path, and nothing else was tried first.
    assert_eq!(recorded["path"], "/v1/chat/completions");

    let body = &recorded["body"];
    assert_eq!(body["model"], "fixture-model");
    assert_eq!(body["stream"], json!(false));
    assert_eq!(body["n"], json!(1));

    // The catalog the model may choose from, with the schema arguments are validated
    // against, and nothing else.
    let tool = &body["tools"][0];
    assert_eq!(tool["type"], "function");
    assert_eq!(tool["function"]["name"], "search_repositories");
    assert_eq!(tool["function"]["description"], "Search the index.");
    assert_eq!(
        tool["function"]["parameters"]["properties"]["query"]["type"],
        "string"
    );
    assert!(tool["function"].get("output_schema").is_none());

    // The system message: both configured prompts, in dependency-id order rather
    // than the order the config declared them, joined with a blank line.
    //
    // Each prompt keeps its own trailing newline, which is deliberate: the content
    // facet treats a trailing newline as significant, so trimming one here would send
    // text nobody fingerprinted. A file that ends in a line ending therefore
    // contributes that line ending *and* the separator.
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2, "{messages:#?}");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "Be terse.\n\n\nNever guess.\n");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "Find repositories.");

    // The defaults that were applied, recorded where the trace can be compared.
    assert_eq!(body["temperature"], json!(0.0));
    assert_eq!(body["seed"], json!(42));
    assert_eq!(
        trace.captured_with.effective_params["temperature"],
        json!(0.0)
    );
    assert_eq!(trace.captured_with.model_id, "fixture-model");

    // JSON final text is recorded as the text the model emitted, not parsed: parsing
    // it is the structured-output metric's job, and it must see the original bytes.
    assert_eq!(texts(&trace), vec![Some(r#"{"answer":42}"#.to_string())]);
}

/// A project with no configured prompts sends no system message — not an empty one,
/// which a backend could read as an instruction to say nothing.
#[tokio::test]
async fn a_project_without_prompts_sends_one_user_message() {
    let project = tempfile::tempdir().unwrap();
    let (server, port) = start(project.path(), vec![text("Ready.")], json!({}));
    let runner = Runner::new(
        &model(port, &[]),
        catalog("Search the index."),
        None,
        project.path(),
    )
    .unwrap();

    runner
        .capture(&capture_request("sha256:probe-1", "Find repositories."))
        .await
        .unwrap();
    drop(server);

    let messages = one_request(project.path())["body"]["messages"].clone();
    assert_eq!(messages.as_array().unwrap().len(), 1, "{messages:#?}");
    assert_eq!(messages[0]["role"], "user");
}

// ---------------------------------------------------------------------------
// 2. Tool calls are evidence, and nothing is executed
// ---------------------------------------------------------------------------

/// Every call shape a backend produces, in one response, and the proof that recording
/// them executes nothing.
///
/// **Name of the test that proves the runner never executes a tool:** this one. The
/// project configures no MCP server, so there is no tool to call even in principle,
/// and the fixture records every request it receives: the only one that exists is the
/// chat-completion the sample was captured with. A `tools/call` would have to arrive
/// somewhere, and the only thing this test runs is a chat-completions endpoint.
#[tokio::test]
async fn a_tool_turn_is_recorded_in_every_wire_shape_and_no_tool_is_executed() {
    let project = tempfile::tempdir().unwrap();
    let (server, port) = start(
        project.path(),
        vec![json!({
            "type": "tool_calls",
            "calls": [
                // The known tool, arguments as the wire's JSON string.
                { "name": "search_repositories", "arguments": "{\"query\":\"postgres\"}" },
                // The known tool again, arguments already parsed — and out of order,
                // which the trace must preserve.
                { "name": "search_repositories", "arguments": { "query": "sqlite" } },
                // A name the catalog does not declare.
                { "name": "delete_everything", "arguments": "{}" },
                // Arguments that are not JSON.
                { "name": "search_repositories", "arguments": "{not json" },
                // No `arguments` field at all.
                { "name": "search_repositories" }
            ]
        })],
        json!({}),
    );
    let runner = Runner::new(
        &model(port, &[]),
        catalog("Search the index."),
        None,
        project.path(),
    )
    .unwrap();

    let trace = runner
        .capture(&capture_request("sha256:probe-1", "Find repositories."))
        .await
        .unwrap();
    drop(server);

    let sample = &trace.samples[0];
    assert_eq!(sample.index, 0);
    assert_eq!(sample.final_text, None, "a tool turn carries null content");
    assert_eq!(sample.tool_calls.len(), 5);

    // Order is the model's, and every call is resolved against the catalog it was
    // shown.
    let called: Vec<&str> = sample
        .tool_calls
        .iter()
        .map(|call| call.name.as_str())
        .collect();
    assert_eq!(
        called,
        [
            "search_repositories",
            "search_repositories",
            "delete_everything",
            "search_repositories",
            "search_repositories"
        ]
    );

    assert_eq!(
        sample.tool_calls[0].tool_id.as_deref(),
        Some("tool:fixture.search_repositories")
    );
    assert_eq!(
        sample.tool_calls[0].arguments,
        Some(json!({ "query": "postgres" }))
    );
    assert_eq!(sample.tool_calls[0].arguments_parse_error, None);

    // Already-parsed arguments and string-encoded ones normalise to the same trace.
    assert_eq!(
        sample.tool_calls[1].arguments,
        Some(json!({ "query": "sqlite" }))
    );
    assert!(sample.tool_calls[1].arguments_are_parsed());

    // A hallucinated tool is behavior, recorded with no dependency id.
    assert_eq!(sample.tool_calls[2].name, "delete_everything");
    assert_eq!(sample.tool_calls[2].tool_id, None);

    // Malformed and absent arguments are recorded as parse failures, and the sample
    // is still evidence: the run did not fail.
    assert_eq!(sample.tool_calls[3].arguments, None);
    assert!(sample.tool_calls[3].arguments_parse_error.is_some());
    assert!(!sample.tool_calls[3].arguments_are_parsed());
    assert!(sample.tool_calls[4].arguments_parse_error.is_some());

    // Nothing else was ever contacted: one request, to the completion path only.
    let recorded = requests(project.path());
    assert_eq!(recorded.len(), 1, "{recorded:#?}");
    assert!(
        recorded
            .iter()
            .all(|entry| entry["path"] == "/v1/chat/completions"),
        "{recorded:#?}"
    );
}

// ---------------------------------------------------------------------------
// 3. The cache
// ---------------------------------------------------------------------------

/// Identical runs hit the cache: the second capture takes the same samples without
/// the endpoint being asked again.
#[tokio::test]
async fn identical_runs_hit_the_cache_and_the_endpoint_sees_one_set_of_requests() {
    let project = tempfile::tempdir().unwrap();
    let (server, port) = start(
        project.path(),
        vec![text("first sample"), text("second sample")],
        json!({}),
    );
    let runner = Runner::new(
        &model(port, &[]),
        catalog("Search the index."),
        None,
        project.path(),
    )
    .unwrap();

    let request = CaptureRequest {
        repeat: 2,
        ..capture_request("sha256:probe-1", "Find repositories.")
    };

    let first = runner.capture(&request).await.unwrap();
    let second = runner.capture(&request).await.unwrap();
    drop(server);

    assert_eq!(
        texts(&first),
        vec![
            Some("first sample".to_string()),
            Some("second sample".to_string())
        ],
        "one request per sample, in index order"
    );
    assert_eq!(first.samples, second.samples);
    // Two samples were captured and two requests reached the endpoint; the second
    // capture added none.
    assert_eq!(requests(project.path()).len(), 2);

    // One file per sample, addressed by its key and re-verifiable from its own
    // contents.
    let entries: Vec<PathBuf> = std::fs::read_dir(runner.cache().dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 2, "{entries:?}");
    let entry: Value =
        serde_json::from_str(&std::fs::read_to_string(&entries[0]).unwrap()).unwrap();
    assert_eq!(entry["cache_version"], 1);
    assert!(entry["inputs"]["sample_index"].as_u64().is_some());
    assert!(entry["key"].as_str().unwrap().starts_with("sha256:"));
}

/// Every input in the key moves it: a changed probe, a changed catalog, and changed
/// parameters each miss the cache and reach the endpoint again.
#[tokio::test]
async fn a_changed_probe_catalog_or_parameter_misses_the_cache() {
    let project = tempfile::tempdir().unwrap();
    let (server, port) = start(
        project.path(),
        vec![text("one"), text("two"), text("three"), text("four")],
        json!({}),
    );

    let runner = Runner::new(
        &model(port, &[]),
        catalog("Search the index."),
        None,
        project.path(),
    )
    .unwrap();

    let first = runner
        .capture(&capture_request("sha256:probe-1", "Find repositories."))
        .await
        .unwrap();
    let same = runner
        .capture(&capture_request("sha256:probe-1", "Find repositories."))
        .await
        .unwrap();
    assert_eq!(texts(&same), texts(&first));
    assert_eq!(requests(project.path()).len(), 1, "a hit makes no request");

    // A changed probe digest: different test, so the sample is not this experiment's.
    let changed_probe = runner
        .capture(&capture_request("sha256:probe-2", "Find repositories."))
        .await
        .unwrap();

    // A changed catalog: the model was offered different choices.
    let recatalogued = Runner::new(
        &model(port, &[]),
        catalog("Search the index, differently."),
        None,
        project.path(),
    )
    .unwrap();
    let changed_catalog = recatalogued
        .capture(&capture_request("sha256:probe-1", "Find repositories."))
        .await
        .unwrap();

    // Changed parameters: the same question asked of a different experiment.
    let tuned = Runner::new(
        &model(port, &[("temperature", json!(0.7))]),
        catalog("Search the index."),
        None,
        project.path(),
    )
    .unwrap();
    let changed_params = tuned
        .capture(&capture_request("sha256:probe-1", "Find repositories."))
        .await
        .unwrap();
    drop(server);

    // Each miss took the next answer the fixture had, in order, so no capture was
    // served from an entry another one wrote.
    assert_eq!(
        (
            texts(&first),
            texts(&changed_probe),
            texts(&changed_catalog),
            texts(&changed_params)
        ),
        (
            vec![Some("one".to_string())],
            vec![Some("two".to_string())],
            vec![Some("three".to_string())],
            vec![Some("four".to_string())]
        )
    );
    assert_eq!(requests(project.path()).len(), 4);
    assert_eq!(
        changed_params.captured_with.effective_params["temperature"],
        json!(0.7),
        "the configured parameter is what the key and the trace carry"
    );
}

/// `--refresh` ignores entries that exist and records the affected samples again.
#[tokio::test]
async fn refresh_bypasses_the_cache() {
    let project = tempfile::tempdir().unwrap();
    let (server, port) = start(
        project.path(),
        vec![text("captured first"), text("captured again")],
        json!({}),
    );

    let base = Runner::new(
        &model(port, &[]),
        catalog("Search the index."),
        None,
        project.path(),
    )
    .unwrap();
    let request = capture_request("sha256:probe-1", "Find repositories.");

    let first = base.capture(&request).await.unwrap();
    assert_eq!(texts(&first), vec![Some("captured first".to_string())]);

    let refreshed = base
        .clone()
        .refreshing(true)
        .capture(&request)
        .await
        .unwrap();
    drop(server);

    assert_eq!(
        texts(&refreshed),
        vec![Some("captured again".to_string())],
        "the entry existed and was ignored"
    );
    assert_eq!(requests(project.path()).len(), 2);
    // The refreshed sample replaced the entry rather than adding one: the key is the
    // same experiment.
    assert_eq!(std::fs::read_dir(base.cache().dir()).unwrap().count(), 1);
}

/// A damaged cache entry is an error, never a pass and never a silent recapture. The
/// request count is what proves it did not fall back to the endpoint.
#[tokio::test]
async fn a_corrupt_cache_entry_is_an_error_rather_than_a_recapture() {
    let project = tempfile::tempdir().unwrap();
    let (server, port) = start(project.path(), vec![text("one"), text("two")], json!({}));
    let runner = Runner::new(
        &model(port, &[]),
        catalog("Search the index."),
        None,
        project.path(),
    )
    .unwrap();
    let request = capture_request("sha256:probe-1", "Find repositories.");

    runner.capture(&request).await.unwrap();

    let entry = std::fs::read_dir(runner.cache().dir())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(&entry, "{ not a cache entry").unwrap();

    let error = runner.capture(&request).await.unwrap_err();
    drop(server);

    assert!(matches!(error, Error::TraceInvalid { .. }), "{error:?}");
    assert!(
        error.to_string().contains(&entry.display().to_string()),
        "the diagnostic does not name the file to delete: {error}"
    );
    assert_eq!(
        requests(project.path()).len(),
        1,
        "the runner recaptured instead of refusing unreadable evidence"
    );
}

// ---------------------------------------------------------------------------
// 4. The endpoint's own failures
// ---------------------------------------------------------------------------

/// A body that is not a chat completion is a broken endpoint, not a sample.
#[tokio::test]
async fn a_response_that_is_not_a_chat_completion_is_an_error() {
    let project = tempfile::tempdir().unwrap();
    let (server, port) = start(
        project.path(),
        vec![json!({ "type": "raw", "body": "<html>gateway</html>", "status": 200 })],
        json!({}),
    );
    let runner = Runner::new(
        &model(port, &[]),
        catalog("Search the index."),
        None,
        project.path(),
    )
    .unwrap();

    let error = runner
        .capture(&capture_request("sha256:probe-1", "Find repositories."))
        .await
        .unwrap_err();
    drop(server);

    assert!(matches!(error, Error::RunnerResponse { .. }), "{error:?}");
    assert!(error.suggestion().is_some());
}

/// An HTTP error fails the sample and is never retried: the request count is the
/// proof, and a retry would make `repeat = N` mean a different number of
/// observations.
#[tokio::test]
async fn an_http_error_fails_without_a_retry() {
    let project = tempfile::tempdir().unwrap();
    let (server, port) = start(
        project.path(),
        vec![json!({ "type": "status", "status": 500 })],
        json!({}),
    );
    let runner = Runner::new(
        &model(port, &[]),
        catalog("Search the index."),
        None,
        project.path(),
    )
    .unwrap();

    let error = runner
        .capture(&capture_request("sha256:probe-1", "Find repositories."))
        .await
        .unwrap_err();
    drop(server);

    assert!(matches!(error, Error::RunnerRequest { .. }), "{error:?}");
    assert!(error.to_string().contains("HTTP 500"), "{error}");
    assert_eq!(requests(project.path()).len(), 1);
}

/// An endpoint that answers too late fails the sample with the timeout named, and is
/// not tried again.
#[tokio::test]
async fn a_slow_endpoint_times_out_without_a_retry() {
    let project = tempfile::tempdir().unwrap();
    let (server, port) = start(
        project.path(),
        vec![text("too late")],
        json!({ "delay_ms": 5000 }),
    );
    let runner = Runner::new(
        &model(port, &[]),
        catalog("Search the index."),
        None,
        project.path(),
    )
    .unwrap()
    .with_timeout(Duration::from_millis(300));

    let error = runner
        .capture(&capture_request("sha256:probe-1", "Find repositories."))
        .await
        .unwrap_err();
    drop(server);

    assert!(matches!(error, Error::RunnerRequest { .. }), "{error:?}");
    assert!(
        error.to_string().contains("did not complete within"),
        "{error}"
    );
    assert_eq!(requests(project.path()).len(), 1);
}
