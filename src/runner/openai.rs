// SPDX-License-Identifier: MIT OR Apache-2.0

//! The OpenAI-compatible chat-completions client.
//!
//! One request per sample, no retries, and every response shape that a
//! chat-completions endpoint is known to answer with is either normalised into a
//! [`Sample`] or refused as a response this build cannot read. The two are
//! deliberately different outcomes: a model that emits broken JSON arguments is
//! *behavior* — the whole reason the argument metrics exist — while a body that is
//! not a chat completion at all is a broken endpoint, and scoring it would be
//! scoring a misunderstanding.
//!
//! Nothing here executes a tool. The endpoint is asked what the model would like to
//! call; the answer is recorded and the sample ends there.
//!
//! Wire details this client is built around, because they differ between backends:
//!
//! - Tool-call `arguments` arrive as a JSON **string** from Ollama's
//!   OpenAI-compatible path and as an already-parsed **value** from backends that
//!   follow the spec's optional object form. Both normalise to a [`Value`];
//!   canonicalization applies to the parsed value, never to the raw string.
//! - `content` may be `null` (a pure tool-call turn) and `tool_calls` may be absent
//!   entirely (a pure text turn). Neither is an error.
//! - A tool name the model invented has no dependency id. That is recorded, not
//!   rejected: hallucination is a measurable behavior.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::config::ModelConfig;
use crate::error::{Error, Result};
use crate::runner::catalog::ToolCatalog;
use crate::runner::trace::{Sample, ToolCall};

/// The runner, as recorded in `captured_with.runner`. A trace written by a
/// different runner is not comparable with one written by this.
pub const RUNNER: &str = "openai-chat-completions";

/// The runner's own contract version, recorded beside the name and part of the
/// behavioral baseline's `runner_contract`.
pub const RUNNER_VERSION: u32 = 1;

/// The contract string a baseline records, so a comparison can be refused when the
/// capture semantics moved even though the digest algorithm did not.
pub const RUNNER_CONTRACT: &str = "openai-chat-completions-v1";

/// How long one sample may take.
///
/// One request per sample and **no retries of any kind**: a retry would make
/// `repeat = 2` mean a different number of observations on a flaky endpoint, and the
/// pass rate is the measurement.
pub const MODEL_SAMPLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Tool calls accepted from one response. Ollama models occasionally loop; a bound
/// fails the sample rather than recording a runaway transcript.
pub const MAX_TOOL_CALLS_PER_SAMPLE: usize = 128;

/// Response body bound. A response past it is an error, never a truncation: half a
/// completion parsed is an observation nobody made.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Final-text bound.
pub const MAX_FINAL_TEXT_BYTES: usize = 1024 * 1024;

/// Tool-argument bound, applied to the wire string and to the parsed value.
pub const MAX_TOOL_ARGUMENTS_BYTES: usize = 1024 * 1024;

/// The longest function name the chat-completions wire format carries.
const MAX_FUNCTION_NAME_BYTES: usize = 64;

/// The request fields AgentChecksum owns. Configuring one in `[model].params` is a
/// validation error rather than a silent merge.
const OWNED_FIELDS: [&str; 5] = ["model", "messages", "tools", "stream", "n"];

// ---------------------------------------------------------------------------
// Endpoint derivation
// ---------------------------------------------------------------------------

/// The chat-completions URL derived from a configured base.
///
/// The base is whatever the user configured for the provider — a bare Ollama origin
/// (`http://localhost:11434`) or a gateway path (`https://host/api/openai/v1`) — and
/// this is the one place that decides where the request goes. Two rules, and the
/// second is the reason this is a function rather than a `format!` at the call site:
///
/// - a base with no path gets `/v1/chat/completions` appended;
/// - a base whose path already ends in `/v1` gets only `/chat/completions` appended,
///   so a configured `…/v1` can never become `…/v1/v1/chat/completions`.
///
/// Anything else is refused as [`Error::RunnerUnsupported`] instead of guessed at.
/// A base that already names `/chat/completions` is refused too: appending to it
/// would produce a path no endpoint serves, and accepting it would mean this runner
/// silently supports two spellings of the same thing.
pub fn endpoint_for(provider: &str, base: &str) -> Result<String> {
    let undecidable = |reason: &str| Error::RunnerUnsupported {
        what: format!("derive a chat-completions endpoint for the `{provider}` provider"),
        reason: reason.to_string(),
    };

    let url = reqwest::Url::parse(base)
        .map_err(|_| undecidable("the configured `endpoint` is not an absolute URL"))?;

    match url.scheme() {
        "http" | "https" => {}
        _ => {
            return Err(undecidable(
                "only `http` and `https` endpoints are supported",
            ));
        }
    }
    if url.host_str().is_none() {
        return Err(undecidable("the configured `endpoint` names no host"));
    }
    // Rejected rather than stripped, for the same reason the rest of the tool
    // rejects them: a value this build cannot represent is never guessed at, and a
    // credential must not be carried by something that gets printed.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(undecidable(
            "the configured `endpoint` contains credentials, which are neither sent nor supported",
        ));
    }
    if url.query().is_some() {
        return Err(undecidable(
            "the configured `endpoint` carries a query string, which could select a different deployment behind the same host",
        ));
    }
    if url.fragment().is_some() {
        return Err(undecidable("the configured `endpoint` carries a fragment"));
    }

    let base = base.trim_end_matches('/');
    let path = url.path().trim_end_matches('/');

    if path.is_empty() {
        return Ok(format!("{base}/v1/chat/completions"));
    }
    if path.ends_with("/chat/completions") {
        return Err(undecidable(
            "the configured `endpoint` already names the completion path; configure the base it is reached through",
        ));
    }
    if path.ends_with("/v1") {
        return Ok(format!("{base}/chat/completions"));
    }

    Err(undecidable(
        "the path is not one this runner can derive from: use a bare origin such as `https://host`, or a base that ends in `/v1` such as `https://host/api/openai/v1`",
    ))
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// The parameters actually sent, with the recorded defaults filled in.
///
/// `temperature = 0.0` and `seed = 42` are applied only when the user did not
/// configure them, and they are recorded in the trace either way: a default nobody
/// can see is a difference nobody can explain between two runs. A backend that
/// rejects `seed` fails with a diagnostic — the runner never silently retries
/// without it, because that would make the sample a different experiment.
pub fn effective_params(configured: &BTreeMap<String, Value>) -> Result<BTreeMap<String, Value>> {
    for name in OWNED_FIELDS {
        if configured.contains_key(name) {
            return Err(Error::ConfigInvalid {
                reason: format!(
                    "`[model].params` sets `{name}`, which AgentChecksum owns: the request body is \
                     `{{ model, messages, tools, stream, n, …params }}`, and a configured `{name}` \
                     would either be ignored or change what the probe measured"
                ),
            });
        }
    }

    let mut params = configured.clone();
    params
        .entry("temperature".to_string())
        .or_insert_with(|| Value::from(0.0));
    params
        .entry("seed".to_string())
        .or_insert_with(|| Value::from(42));
    Ok(params)
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// A configured endpoint, the catalog it is shown, and the message prefix.
///
/// The system prompt is a property of the agent, not of the probe, so it is
/// assembled once here and prepended to every sample's user message.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    provider: String,
    endpoint: String,
    model_id: String,
    timeout: Duration,
    params: BTreeMap<String, Value>,
    tools: Vec<Value>,
    /// Wire function name to dependency id. Built at construction, when the
    /// catalog is known to be unambiguous.
    names: BTreeMap<String, String>,
    system: Option<String>,
}

impl Client {
    /// Build a client that may take `timeout` per sample.
    ///
    /// The timeout is a parameter rather than the constant alone so a test can drive
    /// a deliberately slow endpoint without waiting two minutes for it; the CLI
    /// always passes [`MODEL_SAMPLE_TIMEOUT`].
    pub fn new(
        model: &ModelConfig,
        catalog: &ToolCatalog,
        system: Option<String>,
        timeout: Duration,
    ) -> Result<Self> {
        let base = model
            .endpoint
            .as_deref()
            .ok_or_else(|| Error::ModelEndpointMissing {
                provider: model.provider.clone(),
            })?;

        let endpoint = endpoint_for(&model.provider, base)?;
        let params = effective_params(&model.params)?;
        let (tools, names) = wire_catalog(catalog)?;

        let http = reqwest::Client::builder()
            .build()
            .map_err(|source| Error::ModelClient { source })?;

        Ok(Self {
            http,
            provider: model.provider.clone(),
            endpoint,
            model_id: model.id.clone(),
            timeout,
            params,
            tools,
            names,
            system,
        })
    }

    /// The same client with the sample timeout [`MODEL_SAMPLE_TIMEOUT`] pins.
    pub fn with_default_timeout(
        model: &ModelConfig,
        catalog: &ToolCatalog,
        system: Option<String>,
    ) -> Result<Self> {
        Self::new(model, catalog, system, MODEL_SAMPLE_TIMEOUT)
    }

    /// The same client with a different per-sample timeout.
    ///
    /// The timeout is applied to each request rather than to the shared HTTP client,
    /// so changing it costs nothing and cannot leave a connection pool configured for
    /// a timeout nobody is using.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The absolute URL one sample is sent to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// The parameters that will actually be sent, defaults included.
    pub fn effective_params(&self) -> &BTreeMap<String, Value> {
        &self.params
    }

    /// The catalog exactly as the model sees it.
    pub fn tools(&self) -> &[Value] {
        &self.tools
    }

    /// The assembled system message, when the project configures prompts.
    pub fn system_prompt(&self) -> Option<&str> {
        self.system.as_deref()
    }

    /// The messages of one sample: the system message when there is one, then the
    /// probe prompt as the single user message.
    pub fn messages(&self, prompt: &str) -> Vec<Value> {
        let mut messages = Vec::with_capacity(2);
        if let Some(system) = &self.system {
            messages.push(serde_json::json!({ "role": "system", "content": system }));
        }
        messages.push(serde_json::json!({ "role": "user", "content": prompt }));
        messages
    }

    /// The body one sample is sent as. Public so a test can assert what the runner
    /// promises without an endpoint to receive it.
    pub fn request_body(&self, prompt: &str) -> Value {
        // The owned fields are inserted first and the configured parameters after,
        // which is safe only because `effective_params` refused any parameter that
        // would collide with one of them.
        let mut body = Map::new();
        body.insert("model".to_string(), Value::String(self.model_id.clone()));
        body.insert("messages".to_string(), Value::Array(self.messages(prompt)));
        body.insert("tools".to_string(), Value::Array(self.tools.clone()));
        body.insert("stream".to_string(), Value::Bool(false));
        body.insert("n".to_string(), Value::from(1));
        for (name, value) in &self.params {
            body.insert(name.clone(), value.clone());
        }
        Value::Object(body)
    }

    /// One sample: one request, and what the model answered with.
    pub async fn sample(&self, index: u32, prompt: &str) -> Result<Sample> {
        let response = self
            .http
            .post(&self.endpoint)
            .timeout(self.timeout)
            .json(&self.request_body(prompt))
            .send()
            .await
            .map_err(|source| self.request_error(source))?;

        let status = response.status();
        if !status.is_success() {
            return Err(Error::RunnerRequest {
                endpoint: self.endpoint.clone(),
                reason: format!("the endpoint answered HTTP {}", status.as_u16()),
            });
        }

        // Checked before reading, so a backend that declares an oversized body is
        // refused rather than buffered, and again after, because a chunked response
        // declares nothing.
        if let Some(length) = response.content_length()
            && length > MAX_RESPONSE_BYTES as u64
        {
            return Err(response_bound_error(format!(
                "the response declares {length} bytes, past the {MAX_RESPONSE_BYTES}-byte limit"
            )));
        }

        let body = response
            .bytes()
            .await
            .map_err(|source| self.request_error(source))?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(response_bound_error(format!(
                "the response is {} bytes, past the {MAX_RESPONSE_BYTES}-byte limit",
                body.len()
            )));
        }

        parse_sample(index, &body, &self.names)
    }

    /// A transport failure, described in the vocabulary of one sample.
    fn request_error(&self, source: reqwest::Error) -> Error {
        let reason = if source.is_timeout() {
            format!(
                "the request did not complete within {}s",
                self.timeout.as_secs()
            )
        } else if source.is_connect() {
            format!("the endpoint could not be reached: {source}")
        } else {
            source.to_string()
        };
        Error::RunnerRequest {
            endpoint: self.endpoint.clone(),
            reason,
        }
    }
}

/// The functions the model is shown, and the map back from a wire name.
///
/// Both refusals here are runner-level on purpose. Two servers may declare the same
/// tool name — static fingerprinting handles that fine and keeps working — but a wire
/// `tools` array cannot carry two functions with one name, so a probe cannot be
/// captured against it. A name the format cannot carry is refused too, and **never
/// sanitised**: a renamed tool would report an observation about a function the
/// server does not have.
fn wire_catalog(catalog: &ToolCatalog) -> Result<(Vec<Value>, BTreeMap<String, String>)> {
    let mut names: BTreeMap<String, String> = BTreeMap::new();

    for tool in catalog.tools() {
        if let Some(problem) = wire_name_problem(&tool.name) {
            return Err(Error::RunnerUnsupported {
                what: "send the tool catalog to the model".to_string(),
                reason: format!(
                    "`{}` declares the tool name `{}`, which the chat-completions function format \
                     cannot carry: {problem}. AgentChecksum never renames a tool to make it fit",
                    tool.id, tool.name
                ),
            });
        }

        if let Some(previous) = names.insert(tool.name.clone(), tool.id.clone()) {
            return Err(Error::RunnerUnsupported {
                what: "send the tool catalog to the model".to_string(),
                reason: format!(
                    "`{previous}` and `{}` both declare the tool name `{}`, and the chat-completions \
                     wire format carries one function per name; rename one of them at its server",
                    tool.id, tool.name
                ),
            });
        }
    }

    Ok((catalog.wire_tools(), names))
}

/// Why a name cannot be a wire function name, or `None`.
fn wire_name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("it is empty");
    }
    if name.len() > MAX_FUNCTION_NAME_BYTES {
        return Some("it is longer than 64 bytes");
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
    {
        return Some("it carries characters outside letters, digits, `_` and `-`");
    }
    None
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

/// Peeked first so a missing or malformed body reports what it is, rather than a
/// serde line number nobody can act on.
#[derive(Deserialize)]
struct Completion {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    content: Option<Value>,
    #[serde(default)]
    tool_calls: Vec<RawToolCall>,
}

#[derive(Deserialize)]
struct RawToolCall {
    #[serde(default)]
    function: Option<RawFunction>,
}

#[derive(Deserialize)]
struct RawFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<Value>,
}

/// Turn a response body into the sample it describes.
///
/// `names` resolves a wire name back to its dependency id; a name that is not in it
/// is recorded with no id, which is the documented treatment of a hallucinated tool.
fn parse_sample(index: u32, body: &[u8], names: &BTreeMap<String, String>) -> Result<Sample> {
    let completion: Completion =
        serde_json::from_slice(body).map_err(|source| Error::RunnerResponse {
            reason: format!("its body is not a chat completion: {source}"),
        })?;

    // `n: 1` was requested, so a sample is exactly one choice. More would mean the
    // request that produced it was not the request this client sent.
    let mut choices = completion.choices.into_iter();
    let choice = choices.next().ok_or_else(|| Error::RunnerResponse {
        reason: "it carries no choices".to_string(),
    })?;
    if choices.next().is_some() {
        return Err(Error::RunnerResponse {
            reason: "it carries more than one choice, and one sample is one completion".to_string(),
        });
    }

    let final_text = match choice.message.content {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => {
            if text.len() > MAX_FINAL_TEXT_BYTES {
                return Err(response_bound_error(format!(
                    "the final text is {} bytes, past the {MAX_FINAL_TEXT_BYTES}-byte limit",
                    text.len()
                )));
            }
            Some(text)
        }
        Some(other) => {
            return Err(Error::RunnerResponse {
                reason: format!(
                    "its `content` is {}, and this build reads a string or null",
                    kind_of(&other)
                ),
            });
        }
    };

    if choice.message.tool_calls.len() > MAX_TOOL_CALLS_PER_SAMPLE {
        return Err(response_bound_error(format!(
            "it carries {} tool calls, past the {MAX_TOOL_CALLS_PER_SAMPLE}-call limit",
            choice.message.tool_calls.len()
        )));
    }

    let mut tool_calls = Vec::with_capacity(choice.message.tool_calls.len());
    for raw in choice.message.tool_calls {
        tool_calls.push(read_tool_call(raw, names)?);
    }

    Ok(Sample {
        index,
        tool_calls,
        final_text,
    })
}

/// One emitted call, in the trace's shape.
fn read_tool_call(raw: RawToolCall, names: &BTreeMap<String, String>) -> Result<ToolCall> {
    let function = raw.function.ok_or_else(|| Error::RunnerResponse {
        reason: "a tool call carries no `function`".to_string(),
    })?;

    // An empty name is not a tool name, and recording it would put a call in the
    // trace that no metric could ever resolve. That is a structurally invalid
    // response, not a hallucination.
    let name = function
        .name
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Error::RunnerResponse {
            reason: "a tool call names no function".to_string(),
        })?;

    let (arguments, arguments_parse_error) = read_arguments(function.arguments)?;

    Ok(ToolCall {
        tool_id: names.get(&name).cloned(),
        name,
        arguments,
        arguments_parse_error,
    })
}

/// Normalise the two wire forms of `arguments`.
///
/// A failure to parse is **recorded, never fatal**: the model produced those bytes,
/// the argument metrics for this call fail accordingly, and the rest of the sample
/// is still evidence. Only an oversized value is an error, because it is a bound
/// rather than an observation.
#[allow(clippy::type_complexity)]
fn read_arguments(raw: Option<Value>) -> Result<(Option<Value>, Option<String>)> {
    match raw {
        Some(Value::String(text)) => {
            if text.len() > MAX_TOOL_ARGUMENTS_BYTES {
                return Err(overlong_arguments(format!(
                    "the arguments are {} bytes on the wire, past the \
                     {MAX_TOOL_ARGUMENTS_BYTES}-byte limit",
                    text.len()
                )));
            }
            match serde_json::from_str::<Value>(&text) {
                Ok(value) => {
                    check_argument_size(&value)?;
                    Ok((Some(value), None))
                }
                Err(source) => Ok((
                    None,
                    Some(format!("the arguments are not valid JSON: {source}")),
                )),
            }
        }
        Some(Value::Null) | None => Ok((
            None,
            Some("the tool call carried no `arguments` value".to_string()),
        )),
        Some(value @ (Value::Object(_) | Value::Array(_))) => {
            check_argument_size(&value)?;
            Ok((Some(value), None))
        }
        Some(other) => Ok((
            None,
            Some(format!(
                "the arguments are {}, and this build reads a JSON object, a JSON array, or a \
                 string carrying one",
                kind_of(&other)
            )),
        )),
    }
}

/// The canonical size of a parsed argument value is what the bound is about: two
/// spellings of the same value must not straddle it.
fn check_argument_size(value: &Value) -> Result<()> {
    let size = crate::fingerprint::canonical::to_vec(value)?.len();
    if size > MAX_TOOL_ARGUMENTS_BYTES {
        return Err(overlong_arguments(format!(
            "the arguments canonicalize to {size} bytes, past the \
             {MAX_TOOL_ARGUMENTS_BYTES}-byte limit"
        )));
    }
    Ok(())
}

/// The bound errors share one shape: the runner refuses rather than truncating, and
/// the refusal has to say which bound and by how much.
fn response_bound_error(reason: String) -> Error {
    Error::RunnerResponse { reason }
}

fn overlong_arguments(reason: String) -> Error {
    Error::RunnerResponse { reason }
}

/// What a JSON value is, for a diagnostic that says what arrived instead of only
/// what was expected.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::catalog::test_support;
    use serde_json::json;

    fn model(params: &[(&str, Value)]) -> ModelConfig {
        ModelConfig {
            provider: "openai-compatible".to_string(),
            id: "fixture-model".to_string(),
            endpoint: Some("http://127.0.0.1:9/v1".to_string()),
            params: params
                .iter()
                .map(|(name, value)| ((*name).to_string(), value.clone()))
                .collect(),
        }
    }

    fn client() -> Client {
        Client::with_default_timeout(
            &model(&[]),
            &test_support::catalog(),
            Some("Be terse.".into()),
        )
        .unwrap()
    }

    fn names() -> BTreeMap<String, String> {
        BTreeMap::from([(
            "search_repositories".to_string(),
            "tool:github.search_repositories".to_string(),
        )])
    }

    fn body(message: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({ "choices": [{ "message": message }] })).unwrap()
    }

    // -- endpoint derivation -------------------------------------------------

    #[test]
    fn the_endpoint_is_derived_from_every_shape_the_brief_pins() {
        for (base, expected) in [
            (
                "http://localhost:11434",
                "http://localhost:11434/v1/chat/completions",
            ),
            ("https://h", "https://h/v1/chat/completions"),
            ("https://h/v1", "https://h/v1/chat/completions"),
            ("https://h/v1/", "https://h/v1/chat/completions"),
            (
                "https://h/api/openai/v1",
                "https://h/api/openai/v1/chat/completions",
            ),
        ] {
            assert_eq!(
                endpoint_for("openai-compatible", base).unwrap(),
                expected,
                "base `{base}`"
            );
        }
    }

    #[test]
    fn no_base_shape_can_produce_a_doubled_v1() {
        // The bug this function exists to prevent, asserted directly: a gateway path
        // that already ends in `/v1` must not have another one appended.
        for base in [
            "https://h/v1",
            "https://h/api/openai/v1",
            "https://h/openai/v1/",
        ] {
            let derived = endpoint_for("openai-compatible", base).unwrap();
            assert!(!derived.contains("/v1/v1/"), "`{base}` derived `{derived}`");
        }
    }

    #[test]
    fn a_base_shape_this_runner_cannot_derive_from_is_refused() {
        for base in [
            "https://h/api/openai",
            "https://h/v1/chat/completions",
            "https://h/chat/completions",
            "ftp://h",
            "not a url",
            "https://user:secret@h/v1",
            "https://h/v1?tenant=x",
        ] {
            let error = endpoint_for("openai-compatible", base).unwrap_err();
            assert!(
                matches!(error, Error::RunnerUnsupported { .. }),
                "`{base}` produced {error:?}"
            );
            // A credential in a configured endpoint must not travel into a
            // diagnostic, whatever else is wrong with it.
            assert!(!error.to_string().contains("secret"), "{error:?}");
        }
    }

    // -- parameters ----------------------------------------------------------

    #[test]
    fn a_parameter_agentchecksum_owns_is_a_validation_error() {
        for field in ["model", "messages", "tools", "stream", "n"] {
            let configured = BTreeMap::from([(field.to_string(), json!("whatever"))]);
            let error = effective_params(&configured).unwrap_err();
            assert!(matches!(error, Error::ConfigInvalid { .. }), "{error:?}");
            assert!(
                error.to_string().contains(field),
                "the diagnostic does not name `{field}`: {error}"
            );
        }
    }

    #[test]
    fn the_recorded_defaults_apply_only_where_the_user_left_holes() {
        let defaults = effective_params(&BTreeMap::new()).unwrap();
        assert_eq!(defaults["temperature"], json!(0.0));
        assert_eq!(defaults["seed"], json!(42));

        let configured =
            effective_params(&BTreeMap::from([("temperature".to_string(), json!(0.7))])).unwrap();
        assert_eq!(configured["temperature"], json!(0.7));
        assert_eq!(
            configured["seed"],
            json!(42),
            "the other default still applies"
        );

        // Any other parameter is the user's, and is passed through untouched.
        let extra = effective_params(&BTreeMap::from([("top_p".to_string(), json!(0.9))])).unwrap();
        assert_eq!(extra["top_p"], json!(0.9));
        assert_eq!(extra.len(), 3);
    }

    // -- the request ---------------------------------------------------------

    #[test]
    fn the_request_body_carries_the_owned_fields_and_the_effective_parameters() {
        let body = client().request_body("Find repositories.");

        assert_eq!(body["model"], "fixture-model");
        assert_eq!(body["stream"], json!(false));
        assert_eq!(body["n"], json!(1));
        assert_eq!(body["temperature"], json!(0.0));
        assert_eq!(body["seed"], json!(42));
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "Be terse.");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "Find repositories.");
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
        assert_eq!(body["tools"][0]["type"], "function");
    }

    #[test]
    fn a_project_with_no_prompts_sends_no_system_message() {
        let client =
            Client::with_default_timeout(&model(&[]), &test_support::catalog(), None).unwrap();
        let body = client.request_body("Find repositories.");

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert_eq!(messages[0]["role"], "user");
    }

    // -- the catalog on the wire --------------------------------------------

    #[test]
    fn a_name_two_tools_share_cannot_be_sent() {
        let catalog = test_support::catalog_with(&[
            ("tool:one.search", Some("Search.")),
            ("tool:two.search", Some("Search too.")),
        ]);

        let error = Client::with_default_timeout(&model(&[]), &catalog, None).unwrap_err();
        assert!(
            matches!(error, Error::RunnerUnsupported { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("both declare"), "{error}");
    }

    #[test]
    fn a_name_the_wire_cannot_carry_is_refused_rather_than_renamed() {
        // A dot is outside the function-name grammar, and the failure names the id
        // and the name so a user can fix the server rather than guess.
        let catalog =
            test_support::catalog_with(&[("tool:s.search.repositories", Some("Search."))]);

        let error = Client::with_default_timeout(&model(&[]), &catalog, None).unwrap_err();
        assert!(
            matches!(error, Error::RunnerUnsupported { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("search.repositories"), "{error}");
        assert!(error.to_string().contains("never renames"), "{error}");
    }

    #[test]
    fn the_catalog_on_the_wire_is_the_one_the_digest_describes() {
        let catalog = test_support::catalog();
        let client = Client::with_default_timeout(&model(&[]), &catalog, None).unwrap();

        assert_eq!(client.tools(), catalog.wire_tools().as_slice());
    }

    // -- response parsing ----------------------------------------------------

    #[test]
    fn arguments_are_read_from_both_wire_forms() {
        let expect = json!({ "query": "postgres" });

        // The string form: Ollama's OpenAI-compatible path.
        let as_string = parse_sample(
            0,
            &body(json!({
                "tool_calls": [{ "function": { "name": "search_repositories",
                                               "arguments": "{\"query\":\"postgres\"}" } }]
            })),
            &names(),
        )
        .unwrap();
        // The object form: backends that already parse it.
        let as_object = parse_sample(
            0,
            &body(json!({
                "tool_calls": [{ "function": { "name": "search_repositories",
                                               "arguments": { "query": "postgres" } } }]
            })),
            &names(),
        )
        .unwrap();

        assert_eq!(as_string, as_object);
        assert_eq!(as_string.tool_calls[0].arguments, Some(expect));
        assert_eq!(as_string.tool_calls[0].arguments_parse_error, None);
        assert_eq!(
            as_string.tool_calls[0].tool_id.as_deref(),
            Some("tool:github.search_repositories")
        );
    }

    #[test]
    fn malformed_arguments_are_recorded_rather_than_failing_the_sample() {
        let sample = parse_sample(
            0,
            &body(json!({
                "tool_calls": [{ "function": { "name": "search_repositories",
                                               "arguments": "{\"query\":" } }]
            })),
            &names(),
        )
        .unwrap();

        let call = &sample.tool_calls[0];
        assert_eq!(call.arguments, None);
        assert!(call.arguments_parse_error.is_some(), "{call:?}");
        assert!(!call.arguments_are_parsed());
        // The name still resolved, so the selection metric can still say what the
        // model chose; only the argument metrics fail.
        assert_eq!(
            call.tool_id.as_deref(),
            Some("tool:github.search_repositories")
        );
    }

    #[test]
    fn a_tool_call_with_no_arguments_value_is_recorded_as_a_parse_failure() {
        let sample = parse_sample(
            0,
            &body(json!({
                "tool_calls": [{ "function": { "name": "search_repositories" } }]
            })),
            &names(),
        )
        .unwrap();

        assert!(sample.tool_calls[0].arguments_parse_error.is_some());
        assert!(!sample.tool_calls[0].arguments_are_parsed());
    }

    #[test]
    fn an_invented_tool_name_is_recorded_without_a_dependency_id() {
        let sample = parse_sample(
            0,
            &body(json!({
                "tool_calls": [{ "function": { "name": "delete_everything", "arguments": "{}" } }]
            })),
            &names(),
        )
        .unwrap();

        assert_eq!(sample.tool_calls[0].name, "delete_everything");
        assert_eq!(sample.tool_calls[0].tool_id, None);
        assert!(sample.tool_calls[0].arguments_are_parsed());
    }

    #[test]
    fn several_calls_keep_the_order_the_model_emitted_them_in() {
        let sample = parse_sample(
            0,
            &body(json!({
                "tool_calls": [
                    { "function": { "name": "read_file", "arguments": "{}" } },
                    { "function": { "name": "search_repositories", "arguments": "{}" } }
                ]
            })),
            &names(),
        )
        .unwrap();

        let ordered: Vec<&str> = sample
            .tool_calls
            .iter()
            .map(|call| call.name.as_str())
            .collect();
        assert_eq!(ordered, ["read_file", "search_repositories"]);
    }

    #[test]
    fn null_content_and_absent_tool_calls_are_read_as_an_empty_turn() {
        let text_turn = parse_sample(3, &body(json!({ "content": "Ready." })), &names()).unwrap();
        assert_eq!(text_turn.final_text.as_deref(), Some("Ready."));
        assert!(text_turn.tool_calls.is_empty());
        assert_eq!(text_turn.index, 3);

        // A pure tool-call turn: content is null.
        let call_turn = parse_sample(
            0,
            &body(json!({
                "content": null,
                "tool_calls": [{ "function": { "name": "read_file", "arguments": "{}" } }]
            })),
            &names(),
        )
        .unwrap();
        assert_eq!(call_turn.final_text, None);
        assert_eq!(call_turn.tool_calls.len(), 1);

        // An empty string is text, not absence. The difference is observable: a
        // structured-output probe scores an empty answer as a failure.
        let empty = parse_sample(0, &body(json!({ "content": "" })), &names()).unwrap();
        assert_eq!(empty.final_text.as_deref(), Some(""));
    }

    #[test]
    fn a_body_that_is_not_a_chat_completion_is_refused() {
        for invalid in [
            b"not json at all".to_vec(),
            b"{}".to_vec(),
            b"{\"choices\": []}".to_vec(),
            // `n: 1` was requested, so two choices did not come from this request.
            serde_json::to_vec(&json!({
                "choices": [
                    { "message": { "content": "a" } },
                    { "message": { "content": "b" } }
                ]
            }))
            .unwrap(),
            serde_json::to_vec(&json!({
                "choices": [{ "message": { "content": 7 } }]
            }))
            .unwrap(),
            serde_json::to_vec(&json!({
                "choices": [{ "message": { "content": null,
                    "tool_calls": [{ "function": { "name": "read_file", "arguments": "{}" } },
                                   { "id": "call_1" }] } }]
            }))
            .unwrap(),
            // A call with no name is not a hallucination; it is unreadable.
            serde_json::to_vec(&json!({
                "choices": [{ "message": { "content": null,
                    "tool_calls": [{ "function": { "arguments": "{}" } }] } }]
            }))
            .unwrap(),
        ] {
            let error = parse_sample(0, &invalid, &names()).unwrap_err();
            assert!(
                matches!(error, Error::RunnerResponse { .. }),
                "{} produced {error:?}",
                String::from_utf8_lossy(&invalid)
            );
        }
    }

    #[test]
    fn a_response_past_a_bound_is_refused_rather_than_truncated() {
        // Tool calls: one past the limit, and the diagnostic says which limit.
        let calls: Vec<Value> = (0..=MAX_TOOL_CALLS_PER_SAMPLE)
            .map(|index| {
                json!({ "function": { "name": format!("tool_{index}"), "arguments": "{}" } })
            })
            .collect();
        let error = parse_sample(0, &body(json!({ "tool_calls": calls })), &names()).unwrap_err();
        assert!(matches!(error, Error::RunnerResponse { .. }), "{error:?}");
        assert!(error.to_string().contains("tool calls"), "{error}");

        // Final text: one byte past the limit.
        let text = "x".repeat(MAX_FINAL_TEXT_BYTES + 1);
        let error = parse_sample(0, &body(json!({ "content": text })), &names()).unwrap_err();
        assert!(error.to_string().contains("final text"), "{error}");

        // Arguments: a string that is oversized before it is even parsed, and a
        // parsed value that is oversized after.
        let oversized = format!("\"{}\"", "x".repeat(MAX_TOOL_ARGUMENTS_BYTES));
        let error = parse_sample(
            0,
            &body(json!({
                "tool_calls": [{ "function": { "name": "read_file", "arguments": oversized } }]
            })),
            &names(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("arguments"), "{error}");

        let error = parse_sample(
            0,
            &body(json!({
                "tool_calls": [{ "function": { "name": "read_file",
                    "arguments": { "text": "x".repeat(MAX_TOOL_ARGUMENTS_BYTES) } } }]
            })),
            &names(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("canonicalize"), "{error}");
    }

    #[test]
    fn an_argument_value_that_is_not_an_object_is_a_recorded_parse_failure() {
        let sample = parse_sample(
            0,
            &body(json!({
                "tool_calls": [{ "function": { "name": "read_file", "arguments": true } }]
            })),
            &names(),
        )
        .unwrap();

        assert!(sample.tool_calls[0].arguments_parse_error.is_some());
    }
}
