// SPDX-License-Identifier: MIT OR Apache-2.0

//! Structured errors. The CLI boundary renders `Display` plus the optional
//! `suggestion()` as the what-failed / how-to-fix diagnostic from spec §14.

use std::path::PathBuf;
use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to read `{path}`")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write `{path}`")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to serialize a value to JSON")]
    Json {
        #[source]
        source: serde_json::Error,
    },

    #[error("invalid configuration in `{path}`")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("invalid configuration: {reason}")]
    ConfigInvalid { reason: String },

    #[error("unsupported config version {found}; this build supports version {supported}")]
    ConfigVersion { found: u32, supported: u32 },

    #[error("`[policy.metrics.{name}]` does not name a metric this build measures")]
    PolicyMetricUnknown { name: String, known: String },

    #[error("`[policy.metrics.{metric}]` is not a usable policy: {detail}")]
    PolicyRange { metric: String, detail: String },

    #[error("{reason}")]
    InvalidUsage { reason: String },

    #[error("dependency identity collision: `{id}` is claimed more than once")]
    DependencyCollision { id: String },

    #[error(
        "prompt path `{path}` must be relative, must not contain `..`, and must not contain a backslash"
    )]
    PromptPath { path: String },

    #[error("prompt `{path}` is not valid UTF-8")]
    PromptNotUtf8 { path: PathBuf },

    #[error("model `{id}` was not found on the `{provider}` endpoint `{endpoint}`")]
    ModelMissing {
        provider: String,
        id: String,
        endpoint: String,
    },

    #[error("provider `{provider}` requires an `endpoint` to be configured")]
    ModelEndpointMissing { provider: String },

    #[error("failed to reach the `{provider}` endpoint `{endpoint}`")]
    ModelEndpoint {
        provider: String,
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("the `{provider}` endpoint `{endpoint}` returned HTTP {status}")]
    ModelStatus {
        provider: String,
        endpoint: String,
        status: u16,
    },

    #[error("the `{provider}` endpoint `{endpoint}` returned a body that is not valid Ollama JSON")]
    ModelResponse {
        provider: String,
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("failed to build the HTTP client")]
    ModelClient {
        #[source]
        source: reqwest::Error,
    },

    #[error("unsupported model provider `{provider}`")]
    ModelProvider { provider: String },

    #[error("invalid `{provider}` endpoint: {reason}")]
    EndpointInvalid { provider: String, reason: String },

    #[error("`{path}` already exists")]
    AlreadyExists { path: PathBuf },

    #[error("lockfile `{path}` is not valid JSON")]
    LockParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("lockfile `{path}` uses lock_version {found}, but this build supports {supported}")]
    LockVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },

    #[error("baseline lockfile `{path}` does not exist")]
    BaselineMissing { path: PathBuf },

    #[error(
        "baseline lockfile `{path}` does not describe its own contents (recorded {recorded}, computed {computed})"
    )]
    BaselineChecksumMismatch {
        path: PathBuf,
        recorded: String,
        computed: String,
    },

    #[error(
        "lockfile `{path}` records the wrong digest for `{facet}` of `{id}` (recorded {recorded}, computed from its payload {computed})"
    )]
    FacetPayloadMismatch {
        path: PathBuf,
        id: String,
        facet: String,
        recorded: String,
        computed: String,
    },

    /// One variant for the whole discovery path, with the stage carried as a field
    /// rather than folded into the message: "MCP failed" tells a user nothing, while
    /// "failed during reading the tool catalog" is the difference between a broken
    /// server and a broken configuration.
    #[error("MCP server `{server}` ({transport}) failed during {stage}: {reason}")]
    McpFailed {
        server: String,
        transport: String,
        stage: String,
        reason: String,
    },

    #[error("MCP server `{server}` ({transport}) did not complete {stage} within {seconds}s")]
    McpTimeout {
        server: String,
        transport: String,
        stage: String,
        seconds: u64,
    },
    #[error("probe file `{path}` could not be read: {reason}")]
    ProbeParse { path: PathBuf, reason: String },

    #[error("probe `{name}` in `{path}` is not usable: {reason}")]
    ProbeInvalid {
        name: String,
        path: PathBuf,
        reason: String,
    },

    #[error("probe `{name}` is declared twice, in `{first}` and `{second}`")]
    ProbeDuplicate {
        name: String,
        first: PathBuf,
        second: PathBuf,
    },

    #[error("probe `{probe}` references the tool `{reference}`, which no discovered tool provides")]
    ProbeToolUnknown { probe: String, reference: String },

    #[error(
        "probe `{probe}` references the tool `{reference}`, which {matches} discovered tools provide: {candidates}"
    )]
    ProbeToolAmbiguous {
        probe: String,
        reference: String,
        matches: usize,
        candidates: String,
    },

    #[error("the behavioral runner cannot {what}: {reason}")]
    RunnerUnsupported { what: String, reason: String },

    #[error("the model request to `{endpoint}` failed: {reason}")]
    RunnerRequest { endpoint: String, reason: String },

    #[error("the model returned a response this build cannot read: {reason}")]
    RunnerResponse { reason: String },

    #[error("trace `{path}` is not usable: {reason}")]
    TraceInvalid { path: PathBuf, reason: String },

    #[error("trace `{path}` uses trace_version {found}, but this build supports {supported}")]
    TraceVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },

    #[error("the schema in `{path}` asks for something this build cannot do: {reason}")]
    SchemaUnsupported { path: PathBuf, reason: String },

    #[error("no behavioral baseline exists at `{path}`")]
    BehaviorBaselineMissing { path: PathBuf },

    #[error(
        "behavioral baseline `{path}` uses baseline_version {found}, but this build supports {supported}"
    )]
    BehaviorBaselineVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },

    #[error("behavioral baseline `{path}` describes a different probe suite than the one loaded")]
    BehaviorBaselineStale { path: PathBuf },
}

impl Error {
    /// An actionable next step, printed under a `Suggested action:` heading.
    pub fn suggestion(&self) -> Option<String> {
        match self {
            Error::Read { .. } => Some("Check that the path exists and is readable.".to_string()),
            Error::Write { .. } => Some("Check directory permissions.".to_string()),
            Error::Json { .. } => Some(
                "This is a bug in agentchecksum; please report it with the input that triggered it."
                    .to_string(),
            ),
            Error::ConfigParse { .. } => Some(
                "Fix the reported key. Unknown keys are rejected so a typo cannot be silently ignored."
                    .to_string(),
            ),
            Error::ConfigInvalid { .. } => None,
            Error::ConfigVersion { .. } => {
                Some("Upgrade agentchecksum, or set `version` to a supported value.".to_string())
            }
            Error::InvalidUsage { .. } => Some("Run `agentchecksum --help` for the accepted flags.".to_string()),
            Error::PolicyMetricUnknown { known, .. } => Some(format!("Metrics this build measures: {known}.")),
            Error::PolicyRange { .. } => Some(
                "Scores run from 0.0 to 1.0 and every metric points the same way, where 1.0 is good. \
                 For a metric you want to keep low, use `max`."
                    .to_string(),
            ),
            Error::DependencyCollision { .. } => Some(
                "Two declarations produced one identity. Rename the configured MCP server alias, or \
                 fix the duplicate declaration — a lockfile that silently kept one of two declared \
                 dependencies would describe an agent nobody configured."
                    .to_string(),
            ),
            Error::PromptPath { .. } => {
                Some("Use a path relative to the config file, for example `prompts/system.md`.".to_string())
            }
            Error::PromptNotUtf8 { .. } => Some("Re-save the file as UTF-8.".to_string()),
            Error::ModelMissing { provider, id, .. } if provider == "ollama" => {
                Some(format!("Pull the model with `ollama pull {id}`, or correct `[model].id`."))
            }
            Error::ModelMissing { .. } => None,
            Error::ModelEndpointMissing { .. } => {
                Some("Add `endpoint = \"http://localhost:11434\"` to the `[model]` section.".to_string())
            }
            Error::ModelEndpoint { provider, .. } if provider == "ollama" => {
                Some("Check that the Ollama server is running (`ollama serve`).".to_string())
            }
            Error::ModelEndpoint { .. } => None,
            Error::ModelStatus { .. } => {
                Some("Run `ollama list` to confirm the model server is healthy.".to_string())
            }
            Error::ModelResponse { .. } => Some(
                "Check that `endpoint` points at an Ollama server (the native API, not the OpenAI-compatible path), and that nothing is intercepting the connection.".to_string(),
            ),
            Error::ModelClient { .. } => Some(
                "Retry, and if it persists, report it with the endpoint you configured.".to_string(),
            ),
            Error::ModelProvider { .. } => Some(
                "Providers supported in this version: `ollama`, `openai-compatible`.".to_string(),
            ),
            Error::EndpointInvalid { .. } => Some(
                "Use a credential-free HTTP(S) base URL such as `https://api.example.com/v1`, with no query \
                 parameters and no fragment, and supply authentication through the environment of the process \
                 that runs your agent."
                    .to_string(),
            ),
            Error::AlreadyExists { path } => {
                Some(format!("Re-run with `--force` to overwrite `{}`.", path.display()))
            }
            Error::LockParse { .. } => {
                Some("Regenerate it with `agentchecksum snapshot`, or restore it from git.".to_string())
            }
            Error::LockVersion { .. } => Some(
                "Upgrade agentchecksum. A newer lockfile is never silently reinterpreted.".to_string(),
            ),
            Error::BaselineMissing { .. } => Some(
                "Run `agentchecksum snapshot` and commit the generated lockfile before comparing \
                 changes."
                    .to_string(),
            ),
            Error::BaselineChecksumMismatch { .. } => Some(
                "Restore the lockfile from git, or regenerate it deliberately with `agentchecksum \
                 snapshot`."
                    .to_string(),
            ),
            Error::FacetPayloadMismatch { .. } => Some(
                "A recorded payload no longer matches the digest beside it, so the file was edited \
                 by hand or written by another tool. Restore it from version control, or \
                 regenerate it deliberately with `agentchecksum snapshot`."
                    .to_string(),
            ),
            Error::McpFailed { stage, .. } if stage.contains("starting the server") => Some(
                "Check `command` and `args` in `[[mcp.servers]]`. The command is executed directly, \
                 without a shell, so it must be an executable and each argument its own entry."
                    .to_string(),
            ),
            Error::McpFailed { stage, .. } if stage.contains("connecting") => Some(
                "Check that the server is reachable at the configured address, and that it speaks \
                 a supported MCP protocol revision."
                    .to_string(),
            ),
            Error::McpFailed { stage, .. } if stage.contains("tool catalog") => Some(
                "Fix the server's tool catalog, or exclude the server from `[[mcp.servers]]`. \
                 AgentChecksum never fingerprints a partial catalog."
                    .to_string(),
            ),
            Error::McpFailed { .. } => Some(
                "Run the server on its own to see what it reports, then re-run `agentchecksum \
                 snapshot`."
                    .to_string(),
            ),
            Error::McpTimeout { stage, .. } => Some(format!(
                "The server did not finish {stage}. Check that it responds to MCP requests, or \
                 point `[[mcp.servers]]` at a server that does."
            )),
            Error::ProbeParse { .. } => Some(
                "Fix the file's TOML. Probe parsing is strict: an unknown key is a mistake, not a \
                 feature to ignore."
                    .to_string(),
            ),
            Error::ProbeInvalid { .. } => Some(
                "Every probe needs at least one of the five expectation keys; see the design brief."
                    .to_string(),
            ),
            Error::ProbeDuplicate { .. } => Some(
                "Probe names are the durable identity of a behavioral assertion, so two probes cannot \
                 share one. Rename one of them."
                    .to_string(),
            ),
            Error::ProbeToolUnknown { .. } => Some(
                "Check the name against the tool dependencies in `agentchecksum.lock`, or run \
                 `agentchecksum inspect probes` to see what resolved."
                    .to_string(),
            ),
            Error::ProbeToolAmbiguous { .. } => Some(
                "Two servers expose a tool with that name, so use the canonical dependency id."
                    .to_string(),
            ),
            Error::RunnerUnsupported { .. } => Some(
                "The behavioral runner speaks the OpenAI-compatible chat-completions contract only; \
                 see the design brief for what it can express."
                    .to_string(),
            ),
            Error::RunnerRequest { .. } => Some(
                "Check that the model endpoint is reachable and that the configured `[model].id` \
                 exists there. AgentChecksum does not retry: a sample is one request."
                    .to_string(),
            ),
            Error::RunnerResponse { .. } => Some(
                "The endpoint answered, but not with an OpenAI-compatible chat completion."
                    .to_string(),
            ),
            Error::TraceInvalid { .. } => Some(
                "Re-record the trace with `agentchecksum check --refresh`.".to_string(),
            ),
            Error::TraceVersion { .. } => Some(
                "Upgrade agentchecksum. A newer trace is never silently reinterpreted.".to_string(),
            ),
            Error::SchemaUnsupported { .. } => Some(
                "AgentChecksum validates self-contained schemas with internal `#/\u{2026}` references \
                 only; it never resolves a schema over the network."
                    .to_string(),
            ),
            Error::BehaviorBaselineMissing { .. } => Some(
                "Run `agentchecksum check --accept` after reviewing the current behavior.".to_string(),
            ),
            Error::BehaviorBaselineVersion { .. } => Some(
                "Upgrade agentchecksum, or regenerate the baseline with `agentchecksum check \
                 --accept`."
                    .to_string(),
            ),
            Error::BehaviorBaselineStale { .. } => Some(
                "The probes changed, so the recorded scores describe different assertions. Review the \
                 drift, then accept a new baseline with `agentchecksum check --accept`."
                    .to_string(),
            ),
        }
    }
}
