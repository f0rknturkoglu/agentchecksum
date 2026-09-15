// SPDX-License-Identifier: MIT OR Apache-2.0

//! The generated dependency baseline.
//!
//! Read policy differs from config policy (spec §14.1). The lockfile tolerates
//! unknown fields inside a known `lock_version`, because an unknown field cannot
//! change the meaning of the digests we do understand. A newer `lock_version` is
//! refused outright: silently comparing an unknown format could report PASS for a
//! change we cannot see, and a wrong PASS is worse than a parse failure.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::manifest::{AgentChecksum, Dependency, DependencyKind, Facet, agent_checksum};

pub const SUPPORTED_LOCK_VERSION: u32 = 1;

/// Peeked before the full parse so a newer lockfile is refused as a version
/// problem rather than reported as a parse error.
#[derive(Deserialize)]
struct VersionProbe {
    lock_version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lockfile {
    pub lock_version: u32,
    pub generator: Generator,
    pub agent_checksum: AgentChecksum,
    pub dependencies: BTreeMap<String, LockedDependency>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Generator {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockedDependency {
    pub kind: DependencyKind,
    pub facets: BTreeMap<String, Facet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl Lockfile {
    pub fn from_dependencies(dependencies: &[Dependency]) -> Result<Self> {
        let checksum = agent_checksum(dependencies)?;

        let mut locked = BTreeMap::new();
        for dependency in dependencies {
            locked.insert(
                dependency.id.clone(),
                LockedDependency {
                    kind: dependency.kind,
                    facets: dependency.facets.clone(),
                    source: dependency.source.clone(),
                },
            );
        }

        Ok(Self {
            lock_version: SUPPORTED_LOCK_VERSION,
            generator: Generator {
                name: "agentchecksum".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            agent_checksum: checksum,
            dependencies: locked,
        })
    }

    /// Deterministic serialization: sorted keys, two-space indent, one trailing
    /// newline. These bytes are never an input to the checksum.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut text =
            serde_json::to_string_pretty(self).map_err(|source| Error::Json { source })?;
        text.push('\n');
        Ok(text.into_bytes())
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        std::fs::write(path, self.to_bytes()?).map_err(|source| Error::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_path_buf(),
            source,
        })?;

        // Peek the version before the full parse. A newer lockfile whose structure
        // also changed would otherwise be reported as a parse error, and the
        // parse-error suggestion invites the user to regenerate a file that is
        // perfectly valid — only newer than this binary understands.
        if let Ok(probe) = serde_json::from_str::<VersionProbe>(&text)
            && probe.lock_version > SUPPORTED_LOCK_VERSION
        {
            return Err(Error::LockVersion {
                path: path.to_path_buf(),
                found: probe.lock_version,
                supported: SUPPORTED_LOCK_VERSION,
            });
        }

        serde_json::from_str(&text).map_err(|source| Error::LockParse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Refuse to overwrite a lockfile that a newer binary wrote.
    ///
    /// `snapshot` regenerates the lockfile from live state, so writing over a
    /// newer format would silently discard fields this build cannot reproduce.
    /// A file this build cannot recognize as our format at all is left to
    /// `snapshot` to regenerate, which is what the user asked for.
    pub fn ensure_writable(path: &Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }

        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_path_buf(),
            source,
        })?;

        if let Ok(probe) = serde_json::from_str::<VersionProbe>(&text)
            && probe.lock_version > SUPPORTED_LOCK_VERSION
        {
            return Err(Error::LockVersion {
                path: path.to_path_buf(),
                found: probe.lock_version,
                supported: SUPPORTED_LOCK_VERSION,
            });
        }

        Ok(())
    }
}
