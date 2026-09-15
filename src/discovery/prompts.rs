// SPDX-License-Identifier: MIT OR Apache-2.0

//! Prompt discovery. Prompts are repo-local files, so only their digests are
//! recorded: git already versions the content, and a second copy in the
//! lockfile would be a second source of truth.

use std::collections::BTreeMap;
use std::path::Path;

use crate::config::{Config, normalize_rel_path};
use crate::error::{Error, Result};
use crate::fingerprint::normalize;
use crate::manifest::{Dependency, DependencyKind, Digest, Facet};

// `normalize_rel_path` lives in `crate::config`: duplicate declared paths must be
// detected on the normalized form during config validation, so the single
// implementation belongs where validation happens.

/// Content and shape facets for a text file. `content` is what a human would
/// call the document; `shape` ignores interior whitespace so a formatting-only
/// edit can be recognized later without a model.
fn text_facets(bytes: &[u8], path: &Path) -> Result<BTreeMap<String, Facet>> {
    let raw = std::str::from_utf8(bytes).map_err(|_| Error::PromptNotUtf8 {
        path: path.to_path_buf(),
    })?;

    let mut facets = BTreeMap::new();
    facets.insert(
        "content".to_string(),
        Facet {
            digest: Digest::sha256(normalize::normalize_text(raw).as_bytes()),
            shape: None,
            normalized: None,
        },
    );
    facets.insert(
        "shape".to_string(),
        Facet {
            digest: Digest::sha256(normalize::shape_text(raw).as_bytes()),
            shape: None,
            normalized: None,
        },
    );
    Ok(facets)
}

/// Discover every declared prompt. `root` is the directory the config lives in.
pub fn discover(config: &Config, root: &Path) -> Result<Vec<Dependency>> {
    let mut dependencies = Vec::with_capacity(config.prompts.len());

    for prompt in &config.prompts {
        let id_path = normalize_rel_path(&prompt.path)?;
        let absolute = root.join(&id_path);
        let bytes = std::fs::read(&absolute).map_err(|source| Error::Read {
            path: absolute.clone(),
            source,
        })?;

        dependencies.push(Dependency {
            id: format!("prompt:{id_path}"),
            kind: DependencyKind::Prompt,
            facets: text_facets(&bytes, &absolute)?,
            source: None,
        });
    }

    Ok(dependencies)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, Config, PromptConfig};

    fn config_with(paths: &[&str]) -> Config {
        Config {
            version: 1,
            agent: AgentConfig {
                name: "test".to_string(),
            },
            model: None,
            prompts: paths
                .iter()
                .map(|path| PromptConfig {
                    path: (*path).to_string(),
                })
                .collect(),
            mcp: Default::default(),
            probes: Default::default(),
            policy: Default::default(),
        }
    }

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn content_digest(dependency: &Dependency) -> String {
        dependency.facets["content"].digest.as_str().to_string()
    }

    fn shape_digest(dependency: &Dependency) -> String {
        dependency.facets["shape"].digest.as_str().to_string()
    }

    // Path normalization is owned by `crate::config`: duplicate declared paths must
    // be detected on the normalized form, so the single implementation lives where
    // validation happens. Its tests are in src/config.rs, and `discover` imports
    // the function.

    #[test]
    fn a_prompt_produces_content_and_shape_facets_and_a_stable_id() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "prompts/system.md", "Be concise.\n");

        let deps = discover(&config_with(&["prompts/system.md"]), dir.path()).unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].id, "prompt:prompts/system.md");
        assert_eq!(deps[0].kind, DependencyKind::Prompt);
        assert!(deps[0].facets.contains_key("content"));
        assert!(deps[0].facets.contains_key("shape"));
        assert!(
            deps[0]
                .facets
                .values()
                .all(|facet| facet.normalized.is_none()),
            "repo-local prompt facets are digest-only: git already versions the content"
        );
        assert_eq!(deps[0].source, None);
    }

    #[test]
    fn line_endings_do_not_change_the_content_digest() {
        let crlf = tempfile::tempdir().unwrap();
        write(crlf.path(), "p.md", "a\r\nb\r\n");
        let lf = tempfile::tempdir().unwrap();
        write(lf.path(), "p.md", "a\nb\n");

        let a = discover(&config_with(&["p.md"]), crlf.path()).unwrap();
        let b = discover(&config_with(&["p.md"]), lf.path()).unwrap();

        assert_eq!(content_digest(&a[0]), content_digest(&b[0]));
    }

    #[test]
    fn a_formatting_only_edit_keeps_the_shape_digest() {
        let before = tempfile::tempdir().unwrap();
        write(before.path(), "p.md", "Summarize   this.\n");
        let after = tempfile::tempdir().unwrap();
        write(after.path(), "p.md", "Summarize this.\n");

        let a = discover(&config_with(&["p.md"]), before.path()).unwrap();
        let b = discover(&config_with(&["p.md"]), after.path()).unwrap();

        assert_ne!(content_digest(&a[0]), content_digest(&b[0]));
        assert_eq!(shape_digest(&a[0]), shape_digest(&b[0]));
    }

    #[test]
    fn a_semantic_edit_changes_both_digests() {
        let before = tempfile::tempdir().unwrap();
        write(before.path(), "p.md", "Be concise.\n");
        let after = tempfile::tempdir().unwrap();
        write(after.path(), "p.md", "Be thorough.\n");

        let a = discover(&config_with(&["p.md"]), before.path()).unwrap();
        let b = discover(&config_with(&["p.md"]), after.path()).unwrap();

        assert_ne!(content_digest(&a[0]), content_digest(&b[0]));
        assert_ne!(shape_digest(&a[0]), shape_digest(&b[0]));
    }

    #[test]
    fn a_missing_prompt_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = discover(&config_with(&["prompts/missing.md"]), dir.path()).unwrap_err();
        assert!(matches!(err, Error::Read { .. }), "{err:?}");
    }

    #[test]
    fn a_prompt_that_is_not_utf8_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.md"), [0xff, 0xfe, 0xfd]).unwrap();
        let err = discover(&config_with(&["p.md"]), dir.path()).unwrap_err();
        assert!(matches!(err, Error::PromptNotUtf8 { .. }), "{err:?}");
    }

    #[test]
    fn prompt_order_in_config_does_not_affect_the_discovered_set() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.md", "a");
        write(dir.path(), "b.md", "b");

        let mut forward = discover(&config_with(&["a.md", "b.md"]), dir.path()).unwrap();
        let mut reversed = discover(&config_with(&["b.md", "a.md"]), dir.path()).unwrap();
        forward.sort_by(|x, y| x.id.cmp(&y.id));
        reversed.sort_by(|x, y| x.id.cmp(&y.id));
        assert_eq!(forward, reversed);
    }
}
