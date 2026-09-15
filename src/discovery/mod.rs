// SPDX-License-Identifier: MIT OR Apache-2.0

//! Dependency discovery. All I/O lives here; everything downstream is pure.

pub mod mcp;
pub mod model;
pub mod prompts;

use std::collections::BTreeSet;
use std::path::Path;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::manifest::Dependency;

/// The result of a discovery pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Discovery {
    pub dependencies: Vec<Dependency>,
    /// Non-fatal observations that must be visible to the user rather than
    /// silently swallowed (for example: a provider that exposes no model digest).
    pub warnings: Vec<String>,
}

/// Run every configured discovery source.
pub async fn run(config: &Config, root: &Path) -> Result<Discovery> {
    let mut dependencies = prompts::discover(config, root)?;
    let mut warnings = Vec::new();

    if let Some(model) = &config.model {
        let client = model::client()?;
        let metadata = model::fetch(&client, model).await?;
        let (dependency, model_warnings) = model::dependency(model, metadata.as_ref())?;
        dependencies.push(dependency);
        warnings.extend(model_warnings);
    }

    // MCP servers are discovered after the sources that need no external process,
    // so a configuration error in a prompt is reported before anything is spawned.
    let (mcp_dependencies, mcp_warnings) = mcp::discover(config).await?;
    dependencies.extend(mcp_dependencies);
    warnings.extend(mcp_warnings);

    // Before anything is built from this list: every identity has to be unique on
    // its own. Two dependencies with one id would otherwise be resolved by whatever
    // container is used downstream, and a lockfile that quietly keeps one of two
    // declared things is worse than one that refuses to be written.
    verify_unique(&dependencies)?;

    Ok(Discovery {
        dependencies,
        warnings,
    })
}

/// Every dependency id is unique across every source.
fn verify_unique(dependencies: &[Dependency]) -> Result<()> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for dependency in dependencies {
        if !seen.insert(dependency.id.as_str()) {
            return Err(Error::DependencyCollision {
                id: dependency.id.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::DependencyKind;
    use crate::manifest::{Digest, Facet};

    fn dependency(id: &str, kind: DependencyKind) -> Dependency {
        Dependency {
            id: id.to_string(),
            kind,
            facets: std::collections::BTreeMap::from([(
                "content".to_string(),
                Facet {
                    digest: Digest::sha256(id.as_bytes()),
                    shape: None,
                    normalized: None,
                },
            )]),
            source: None,
        }
    }

    #[test]
    fn two_sources_claiming_one_identity_is_refused() {
        // The aggregate would keep one of the two, and a lockfile describing one of
        // two declared dependencies is worse than a refusal: nothing downstream can
        // tell that something is missing.
        let colliding = vec![
            dependency("tool:github.search", DependencyKind::Tool),
            dependency("tool:github.search", DependencyKind::McpServer),
        ];

        let error = verify_unique(&colliding).unwrap_err();
        assert!(
            matches!(error, Error::DependencyCollision { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("tool:github.search"), "{error}");
        assert!(error.suggestion().is_some());
    }

    #[test]
    fn distinct_identities_pass() {
        let distinct = vec![
            dependency("mcp:github", DependencyKind::McpServer),
            dependency("tool:github.search", DependencyKind::Tool),
            dependency("prompt:prompts/system.md", DependencyKind::Prompt),
        ];

        assert!(verify_unique(&distinct).is_ok());
    }
}
