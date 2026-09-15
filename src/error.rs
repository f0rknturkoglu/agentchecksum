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
        }
    }
}
