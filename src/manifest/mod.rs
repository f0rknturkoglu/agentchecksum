// SPDX-License-Identifier: MIT OR Apache-2.0

//! The dependency model and the checksum aggregate.
//!
//! A dependency is identified by `(kind, id)` and carries named facets. Each
//! facet is digested on its own, which is what lets a diff say *a description
//! changed* rather than *something changed*.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::fingerprint::{canonical, digest as hashing};

/// Format version of the checksum aggregate, independent of `lock_version`.
pub const CHECKSUM_FORMAT: &str = "ac1";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digest(String);

impl Digest {
    /// `sha256:<hex>`
    pub fn sha256(bytes: &[u8]) -> Self {
        Self(format!("sha256:{}", hashing::sha256_hex(bytes)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The hex payload, without the algorithm prefix.
    pub fn hex(&self) -> &str {
        self.0.strip_prefix("sha256:").unwrap_or(&self.0)
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentChecksum(String);

impl AgentChecksum {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_digest(digest: &Digest) -> Self {
        Self(format!("{CHECKSUM_FORMAT}:{}", digest.hex()))
    }
}

impl std::fmt::Display for AgentChecksum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    Model,
    Prompt,
    Tool,
    McpServer,
}

impl DependencyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DependencyKind::Model => "model",
            DependencyKind::Prompt => "prompt",
            DependencyKind::Tool => "tool",
            DependencyKind::McpServer => "mcp",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Facet {
    pub digest: Digest,
    /// Whitespace-collapsed digest, present for text facets so a formatting-only
    /// edit can be told apart from a semantic one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<Digest>,
    /// Normalized payload, recorded only for external un-versioned sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dependency {
    /// `<kind>:<identity>`, e.g. `tool:github.search_repositories`.
    pub id: String,
    pub kind: DependencyKind,
    pub facets: BTreeMap<String, Facet>,
    /// Where this came from (server alias, provider). Metadata: never hashed,
    /// because moving a dependency between sources does not change how the
    /// agent behaves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl Dependency {
    /// Digest of the facet set, order-independent by construction.
    pub fn digest(&self) -> Result<Digest> {
        dep_digest(&self.facets)
    }
}

/// SHA-256 over the canonical form of `{ facet_name: facet_digest }`.
pub fn dep_digest(facets: &BTreeMap<String, Facet>) -> Result<Digest> {
    let payload: BTreeMap<&str, &str> = facets
        .iter()
        .map(|(name, facet)| (name.as_str(), facet.digest.as_str()))
        .collect();
    Ok(Digest::sha256(&canonical::to_vec(&payload)?))
}

/// SHA-256 over the canonical form of `{"deps": [[kind, id, dep_digest], ...]}`,
/// after sorting by `(kind, id, dep_digest)`.
///
/// The sort is what makes the aggregate independent of discovery order. The
/// digest is part of the sort key so that two dependencies sharing a `(kind, id)`
/// still aggregate deterministically: upstream validation rejects that case, and
/// including it means the aggregate defends the invariant rather than relying on
/// a check that lives somewhere else.
pub fn agent_checksum(dependencies: &[Dependency]) -> Result<AgentChecksum> {
    let mut entries: Vec<(DependencyKind, String, Digest)> = Vec::with_capacity(dependencies.len());
    for dependency in dependencies {
        entries.push((dependency.kind, dependency.id.clone(), dependency.digest()?));
    }
    entries.sort();

    let digest = Digest::sha256(&canonical::to_vec(&Aggregate { deps: &entries })?);
    Ok(AgentChecksum::from_digest(&digest))
}

/// The aggregate payload shape is the committed `ac1` contract (design spec §8.1):
/// `{"deps": [[kind, id, dep_digest], ...]}`.
#[derive(Serialize)]
struct Aggregate<'a> {
    deps: &'a [(DependencyKind, String, Digest)],
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facet(seed: &str) -> Facet {
        Facet {
            digest: Digest::sha256(seed.as_bytes()),
            shape: None,
            normalized: None,
        }
    }

    fn dep(kind: DependencyKind, id: &str, facets: &[(&str, &str)]) -> Dependency {
        Dependency {
            id: id.to_string(),
            kind,
            facets: facets
                .iter()
                .map(|(name, seed)| ((*name).to_string(), facet(seed)))
                .collect(),
            source: None,
        }
    }

    #[test]
    fn digest_renders_with_an_algorithm_prefix() {
        let digest = Digest::sha256(b"");
        assert_eq!(
            digest.as_str(),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest.hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn the_agent_checksum_is_independent_of_dependency_order() {
        let a = dep(DependencyKind::Prompt, "prompt:a.md", &[("content", "a")]);
        let b = dep(
            DependencyKind::Model,
            "model:ollama/x",
            &[("identity", "b")],
        );
        let c = dep(DependencyKind::Tool, "tool:s.t", &[("description", "c")]);

        let forward = agent_checksum(&[a.clone(), b.clone(), c.clone()]).unwrap();
        let reversed = agent_checksum(&[c, b, a]).unwrap();
        assert_eq!(forward, reversed);
    }

    #[test]
    fn changing_one_facet_changes_the_agent_checksum() {
        let before = agent_checksum(&[dep(
            DependencyKind::Tool,
            "tool:s.t",
            &[("description", "v1")],
        )])
        .unwrap();
        let after = agent_checksum(&[dep(
            DependencyKind::Tool,
            "tool:s.t",
            &[("description", "v2")],
        )])
        .unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn renaming_a_facet_changes_the_dependency_digest() {
        let a = dep(DependencyKind::Tool, "tool:s.t", &[("description", "v1")]);
        let b = dep(DependencyKind::Tool, "tool:s.t", &[("input_schema", "v1")]);
        assert_ne!(a.digest().unwrap(), b.digest().unwrap());
    }

    #[test]
    fn metadata_outside_the_facets_does_not_affect_any_digest() {
        let original = dep(DependencyKind::Tool, "tool:s.t", &[("description", "v1")]);
        let mut renamed_source = original.clone();
        renamed_source.source = Some("some-other-server".to_string());

        assert_eq!(original.digest().unwrap(), renamed_source.digest().unwrap());
        assert_eq!(
            agent_checksum(std::slice::from_ref(&original)).unwrap(),
            agent_checksum(std::slice::from_ref(&renamed_source)).unwrap()
        );
    }

    #[test]
    fn adding_a_dependency_changes_the_agent_checksum() {
        let one = dep(DependencyKind::Prompt, "prompt:a.md", &[("content", "a")]);
        let two = dep(DependencyKind::Prompt, "prompt:b.md", &[("content", "b")]);
        assert_ne!(
            agent_checksum(std::slice::from_ref(&one)).unwrap(),
            agent_checksum(&[one, two]).unwrap()
        );
    }

    #[test]
    fn the_agent_checksum_is_prefixed_with_its_format_version() {
        let checksum = agent_checksum(&[dep(
            DependencyKind::Prompt,
            "prompt:a.md",
            &[("content", "a")],
        )])
        .unwrap();
        assert!(checksum.as_str().starts_with("ac1:"), "{checksum:?}");
        assert_eq!(checksum.as_str().len(), "ac1:".len() + 64);
    }

    #[test]
    fn the_empty_dependency_set_produces_the_documented_payload_shape() {
        // Pinned by value rather than compared with itself: the payload shape is
        // the committed `ac1` contract, and empty `deps` canonicalizes to
        // `{"deps":[]}`.
        let expected = Digest::sha256(b"{\"deps\":[]}");
        assert_eq!(
            agent_checksum(&[]).unwrap().as_str(),
            format!("ac1:{}", expected.hex())
        );
    }

    #[test]
    fn the_aggregate_payload_shape_is_the_documented_contract() {
        let dependency = dep(DependencyKind::Prompt, "prompt:a.md", &[("content", "a")]);
        let expected = Digest::sha256(
            format!(
                "{{\"deps\":[[\"prompt\",\"prompt:a.md\",\"{}\"]]}}",
                dependency.digest().unwrap().as_str()
            )
            .as_bytes(),
        );
        assert_eq!(
            agent_checksum(std::slice::from_ref(&dependency))
                .unwrap()
                .as_str(),
            format!("ac1:{}", expected.hex())
        );
    }

    #[test]
    fn the_aggregate_is_order_independent_even_for_duplicate_ids() {
        // Upstream validation rejects duplicate ids; the aggregate defends the
        // invariant anyway, so a duplicate cannot make the checksum depend on
        // discovery order.
        let v1 = dep(DependencyKind::Tool, "tool:s.t", &[("description", "v1")]);
        let v2 = dep(DependencyKind::Tool, "tool:s.t", &[("description", "v2")]);
        assert_eq!(
            agent_checksum(&[v1.clone(), v2.clone()]).unwrap(),
            agent_checksum(&[v2, v1]).unwrap()
        );
    }
}
