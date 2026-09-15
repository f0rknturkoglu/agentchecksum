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
        }
    }
}
