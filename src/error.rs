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

    #[error("dependency id collision: `{id}` is declared more than once")]
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
            Error::DependencyCollision { .. } => None,
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
        }
    }
}
