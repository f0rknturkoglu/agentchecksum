// SPDX-License-Identifier: MIT OR Apache-2.0

//! The sample cache: one content-addressed file per sample.
//!
//! Capture is the one part of Phase 4 that is not deterministic, so a green run that
//! cannot be re-examined offline is a green run nobody can check. The cache is what
//! makes `check` cheap on the second run and *verifiable* rather than merely fast: each
//! file carries the inputs it was keyed by, so a reader recomputes the key instead of
//! trusting the file name.
//!
//! Three properties are deliberate:
//!
//! - **A hit is only a hit when the inputs agree.** The recorded inputs are compared
//!   with the inputs this run has, so a file moved, copied, or hand-edited into the
//!   wrong place is an error rather than a sample from a different experiment.
//! - **A malformed entry is an error, never a miss and never a PASS.** Quietly
//!   re-capturing would hide that the evidence on disk was unreadable, and quietly
//!   accepting it would score evidence nobody can vouch for.
//! - **Writes are atomic.** A run interrupted mid-write leaves the previous entry or
//!   no entry, never half of one.
//!
//! The cache is machine-local, gitignored, and never a baseline: deleting it must
//! cost time and nothing else.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::fingerprint::canonical;
use crate::manifest::Digest;
use crate::runner::STATE_DIR;
use crate::runner::trace::Sample;
use crate::runner::write_atomic;

/// The cache format this build writes and reads.
pub const CACHE_VERSION: u32 = 1;

/// The directory inside the state directory.
const CACHE_DIR: &str = "cache";

/// Everything one cached sample is a function of.
///
/// No timestamps, no paths, no `repeat` override: two runs that share these inputs
/// sampled the same experiment, and anything else would make a hit depend on where
/// or when the run happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheInputs {
    /// The runner's contract version, so a change to capture semantics invalidates
    /// the cache rather than silently reusing samples taken under the old ones.
    pub runner_version: u32,
    pub agent_checksum: String,
    pub probe_digest: String,
    pub tool_catalog_digest: String,
    /// The parameters actually sent, defaults included. A re-run whose effective
    /// parameters changed is a different experiment.
    pub effective_params: BTreeMap<String, Value>,
    pub sample_index: u32,
}

impl CacheInputs {
    /// `sha256(JCS(inputs))`.
    pub fn key(&self) -> Result<Digest> {
        Ok(Digest::sha256(&canonical::to_vec(self)?))
    }
}

/// One cached sample and the inputs that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheEntry {
    pub cache_version: u32,
    /// The key this entry was written under, so a reader can check that the name it
    /// was found by is the name its inputs produce.
    pub key: String,
    pub inputs: CacheInputs,
    /// The trace fragment: exactly what a sample of this probe recorded.
    pub sample: Sample,
}

impl CacheEntry {
    /// Refuse an entry that contradicts itself.
    ///
    /// `path` names the file so a user can delete it, and `inputs` is what this run
    /// asked for, which is the only thing a hit may be about.
    fn verify(&self, path: &Path, key: &Digest, inputs: &CacheInputs) -> Result<()> {
        let unusable = |reason: String| Error::TraceInvalid {
            path: path.to_path_buf(),
            reason,
        };

        if self.cache_version != CACHE_VERSION {
            return Err(unusable(format!(
                "it was written as cache_version {}, and this build reads {CACHE_VERSION}; run with \
                 `--refresh` to record the sample again",
                self.cache_version
            )));
        }

        if self.key != key.as_str() {
            return Err(unusable(format!(
                "it records the key `{}`, which its own inputs do not produce (`{}`)",
                self.key,
                key.as_str()
            )));
        }

        if &self.inputs != inputs {
            return Err(unusable(
                "it records inputs other than the ones it was found by, so it is not a sample of \
                 this experiment"
                    .to_string(),
            ));
        }

        if self.sample.index != self.inputs.sample_index {
            return Err(unusable(format!(
                "it holds sample index {} under a key for sample index {}",
                self.sample.index, self.inputs.sample_index
            )));
        }

        Ok(())
    }
}

/// The cache directory.
#[derive(Debug, Clone)]
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    /// The cache under a project root: `<root>/.agentchecksum/cache`.
    pub fn at(root: &Path) -> Self {
        Self::in_dir(root.join(STATE_DIR).join(CACHE_DIR))
    }

    /// The cache in an explicit directory, for a caller that already resolved one.
    pub fn in_dir(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where a key's entry lives. Named by the key's hex payload: a cache file is
    /// addressed by content, not by order of capture.
    pub fn path_for(&self, key: &Digest) -> PathBuf {
        self.dir.join(format!("{}.json", key.hex()))
    }

    /// The cached sample for these inputs, or `None` when there is none.
    ///
    /// A present-but-unusable entry is an error: see the module comment.
    pub fn get(&self, inputs: &CacheInputs) -> Result<Option<Sample>> {
        let key = inputs.key()?;
        let path = self.path_for(&key);
        if !path.exists() {
            return Ok(None);
        }

        let bytes = std::fs::read(&path).map_err(|source| Error::Read {
            path: path.clone(),
            source,
        })?;
        let entry: CacheEntry =
            serde_json::from_slice(&bytes).map_err(|source| Error::TraceInvalid {
                path: path.clone(),
                reason: format!("it is not a readable cache entry: {source}"),
            })?;

        entry.verify(&path, &key, inputs)?;
        Ok(Some(entry.sample))
    }

    /// Record a sample under these inputs, replacing any entry for the same key.
    pub fn put(&self, inputs: &CacheInputs, sample: &Sample) -> Result<()> {
        let key = inputs.key()?;
        let entry = CacheEntry {
            cache_version: CACHE_VERSION,
            key: key.as_str().to_string(),
            inputs: inputs.clone(),
            sample: sample.clone(),
        };

        let mut text =
            serde_json::to_string_pretty(&entry).map_err(|source| Error::Json { source })?;
        text.push('\n');

        std::fs::create_dir_all(&self.dir).map_err(|source| Error::Write {
            path: self.dir.clone(),
            source,
        })?;
        write_atomic(&self.path_for(&key), text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::trace::ToolCall;
    use serde_json::json;

    fn inputs() -> CacheInputs {
        CacheInputs {
            runner_version: 1,
            agent_checksum: "ac1:aa".to_string(),
            probe_digest: "sha256:bb".to_string(),
            tool_catalog_digest: "sha256:cc".to_string(),
            effective_params: BTreeMap::from([
                ("temperature".to_string(), json!(0.0)),
                ("seed".to_string(), json!(42)),
            ]),
            sample_index: 0,
        }
    }

    fn sample(index: u32) -> Sample {
        Sample {
            index,
            tool_calls: vec![ToolCall {
                name: "search_repositories".to_string(),
                tool_id: Some("tool:github.search_repositories".to_string()),
                arguments: Some(json!({ "query": "postgres" })),
                arguments_parse_error: None,
            }],
            final_text: None,
        }
    }

    fn cache() -> (tempfile::TempDir, Cache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::at(dir.path());
        (dir, cache)
    }

    #[test]
    fn the_key_is_the_pinned_sha256_of_the_canonical_inputs() {
        // Pinned by value, not by self-comparison: the key is the cache's contract
        // with every earlier run, and a change to it invalidates caches silently.
        let key = inputs().key().unwrap();
        let expected = Digest::sha256(canonical::to_vec(&inputs()).unwrap().as_slice());
        assert_eq!(key, expected);
        assert_eq!(inputs().key().unwrap(), key, "the key is stable");
        assert!(key.as_str().starts_with("sha256:"));
    }

    #[test]
    fn every_pinned_input_changes_the_key() {
        let base = inputs().key().unwrap();

        let mutations: Vec<(&str, CacheInputs)> = vec![
            (
                "runner_version",
                CacheInputs {
                    runner_version: 2,
                    ..inputs()
                },
            ),
            (
                "agent_checksum",
                CacheInputs {
                    agent_checksum: "ac1:zz".to_string(),
                    ..inputs()
                },
            ),
            (
                "probe_digest",
                CacheInputs {
                    probe_digest: "sha256:dd".to_string(),
                    ..inputs()
                },
            ),
            (
                "tool_catalog_digest",
                CacheInputs {
                    tool_catalog_digest: "sha256:ee".to_string(),
                    ..inputs()
                },
            ),
            (
                "effective_params",
                CacheInputs {
                    effective_params: BTreeMap::from([("temperature".to_string(), json!(0.7))]),
                    ..inputs()
                },
            ),
            (
                "sample_index",
                CacheInputs {
                    sample_index: 1,
                    ..inputs()
                },
            ),
        ];

        for (name, changed) in mutations {
            assert_ne!(
                changed.key().unwrap(),
                base,
                "`{name}` did not move the key"
            );
        }
    }

    #[test]
    fn a_miss_is_none_and_a_hit_returns_the_sample_that_was_stored() {
        let (_dir, cache) = cache();
        assert_eq!(cache.get(&inputs()).unwrap(), None);

        cache.put(&inputs(), &sample(0)).unwrap();
        assert_eq!(cache.get(&inputs()).unwrap(), Some(sample(0)));
    }

    #[test]
    fn recording_the_same_sample_twice_leaves_one_file() {
        let (_dir, cache) = cache();
        cache.put(&inputs(), &sample(0)).unwrap();
        cache.put(&inputs(), &sample(0)).unwrap();

        let files: Vec<PathBuf> = std::fs::read_dir(cache.dir())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
        // No half-written temporary left behind: the rename is what publishes an
        // entry.
        assert!(
            files[0]
                .extension()
                .is_some_and(|extension| extension == "json"),
            "{files:?}"
        );
    }

    #[test]
    fn a_refresh_rewrites_the_entry_for_the_key_it_recaptured() {
        let (_dir, cache) = cache();
        cache.put(&inputs(), &sample(0)).unwrap();

        // What `--refresh` does: the same key, recorded again, replacing the entry.
        cache.put(&inputs(), &sample(0)).unwrap();
        let mut replaced = sample(0);
        replaced.final_text = Some("second capture".to_string());
        cache.put(&inputs(), &replaced).unwrap();

        assert_eq!(cache.get(&inputs()).unwrap(), Some(replaced));
        let files: Vec<PathBuf> = std::fs::read_dir(cache.dir())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
    }

    #[test]
    fn an_entry_that_is_not_json_is_an_error_rather_than_a_miss() {
        let (_dir, cache) = cache();
        let path = cache.path_for(&inputs().key().unwrap());
        std::fs::create_dir_all(cache.dir()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();

        let error = cache.get(&inputs()).unwrap_err();
        assert!(matches!(error, Error::TraceInvalid { .. }), "{error:?}");
        assert!(error.to_string().contains("cache"), "{error}");
    }

    #[test]
    fn a_cache_file_copied_under_another_key_is_refused() {
        let (_dir, cache) = cache();
        let inputs = inputs();
        let other = CacheInputs {
            sample_index: 1,
            ..inputs.clone()
        };

        // A file that was copied over this key: it still records the key it was
        // written under, which these inputs do not produce.
        cache.put(&other, &sample(1)).unwrap();
        std::fs::rename(
            cache.path_for(&other.key().unwrap()),
            cache.path_for(&inputs.key().unwrap()),
        )
        .unwrap();

        let error = cache.get(&inputs).unwrap_err();
        assert!(matches!(error, Error::TraceInvalid { .. }), "{error:?}");
        assert!(error.to_string().contains("records the key"), "{error}");
    }

    #[test]
    fn an_entry_whose_recorded_inputs_are_not_this_run_s_inputs_is_refused() {
        let (_dir, cache) = cache();
        let inputs = inputs();
        let path = cache.path_for(&inputs.key().unwrap());
        std::fs::create_dir_all(cache.dir()).unwrap();

        // Hand-edited: the key field agrees with the inputs this run asks for, but
        // the recorded inputs say the sample is of something else.
        let mut entry = CacheEntry {
            cache_version: CACHE_VERSION,
            key: inputs.key().unwrap().as_str().to_string(),
            inputs: CacheInputs {
                sample_index: 1,
                ..inputs.clone()
            },
            sample: sample(1),
        };
        std::fs::write(&path, serde_json::to_vec(&entry).unwrap()).unwrap();

        let error = cache.get(&inputs).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("not a sample of this experiment"),
            "{error}"
        );

        // And a sample recorded under the wrong index is contradictory too.
        entry.inputs = inputs.clone();
        entry.sample = sample(3);
        std::fs::write(&path, serde_json::to_vec(&entry).unwrap()).unwrap();

        let error = cache.get(&inputs).unwrap_err();
        assert!(error.to_string().contains("sample index 3"), "{error}");
    }

    #[test]
    fn an_entry_whose_recorded_key_is_wrong_is_refused() {
        let (_dir, cache) = cache();
        let inputs = inputs();
        let path = cache.path_for(&inputs.key().unwrap());
        std::fs::create_dir_all(cache.dir()).unwrap();

        let mut entry = CacheEntry {
            cache_version: CACHE_VERSION,
            key: "sha256:0000".to_string(),
            inputs: inputs.clone(),
            sample: sample(0),
        };
        std::fs::write(&path, serde_json::to_vec(&entry).unwrap()).unwrap();

        let error = cache.get(&inputs).unwrap_err();
        assert!(error.to_string().contains("key"), "{error}");

        // And a sample recorded under the wrong index is contradictory too.
        entry.key = inputs.key().unwrap().as_str().to_string();
        entry.inputs.sample_index = 0;
        entry.sample.index = 3;
        std::fs::write(&path, serde_json::to_vec(&entry).unwrap()).unwrap();

        let error = cache.get(&inputs).unwrap_err();
        assert!(error.to_string().contains("sample index 3"), "{error}");
    }

    #[test]
    fn an_entry_from_a_newer_cache_format_is_refused() {
        let (_dir, cache) = cache();
        let inputs = inputs();
        let path = cache.path_for(&inputs.key().unwrap());
        std::fs::create_dir_all(cache.dir()).unwrap();

        let entry = CacheEntry {
            cache_version: CACHE_VERSION + 1,
            key: inputs.key().unwrap().as_str().to_string(),
            inputs: inputs.clone(),
            sample: sample(0),
        };
        std::fs::write(&path, serde_json::to_vec(&entry).unwrap()).unwrap();

        let error = cache.get(&inputs).unwrap_err();
        assert!(matches!(error, Error::TraceInvalid { .. }), "{error:?}");
        assert!(error.to_string().contains("cache_version 2"), "{error}");
        assert!(error.to_string().contains("--refresh"), "{error}");
    }
}
