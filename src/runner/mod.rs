// SPDX-License-Identifier: MIT OR Apache-2.0

//! The behavioral runner: one model request per sample, and the trace it produces.
//!
//! This is the only place in Phase 4 that talks to a model, and it is deliberately
//! the narrowest client that can do the job: an OpenAI-compatible
//! `/v1/chat/completions` call with a function tool catalog, one request per sample,
//! and no retries — a retry would change what `repeat = N` means. Everything the
//! model returns is recorded as evidence, so that evaluation never needs the network
//! again.
//!
//! The runner observes an agent's tool decisions. It never executes a tool: the
//! endpoint is asked what the model would like to call, the answer becomes a
//! [`Trace`], and the sample ends. There is no `tools/call`, no MCP request, and no
//! sandbox anywhere on this path — [`Runner::capture`] is the function that would
//! have to change for that to stop being true.
//!
//! Capture reads three kinds of state and writes one:
//!
//! - the model endpoint, through [`openai::Client`];
//! - the folded tool catalog, from the lockfile;
//! - the cache, through [`cache::Cache`], which a hit turns into evidence without a
//!   request at all;
//! - `.agentchecksum/cache`, which is machine-local and never a baseline.
//!
//! What a run leaves behind is a [`RunArtifact`]: the traces it captured beside the
//! evaluation of those traces and their aggregate counts, addressed by its own
//! contents so that a reader can tell recorded evidence from an edited file.

pub mod artifact;
pub mod cache;
pub mod catalog;
pub mod openai;
pub mod trace;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::{Config, ModelConfig, normalize_rel_path};
use crate::error::{Error, Result};
use crate::fingerprint::normalize;
use crate::manifest::Digest;

pub use artifact::{RUN_VERSION, RunArtifact};
pub use cache::{CACHE_VERSION, Cache, CacheEntry, CacheInputs};
pub use catalog::{ToolCatalog, ToolContract};
pub use openai::{Client, MODEL_SAMPLE_TIMEOUT, RUNNER, RUNNER_CONTRACT, RUNNER_VERSION};
pub use trace::{CapturedWith, Sample, TRACE_VERSION, ToolCall, Trace};

/// The state directory every Phase 4 artifact lives under.
pub const STATE_DIR: &str = ".agentchecksum";

/// What separates two configured prompts in the assembled system message.
pub const SYSTEM_PROMPT_SEPARATOR: &str = "\n\n";

// ---------------------------------------------------------------------------
// System prompt assembly
// ---------------------------------------------------------------------------

/// Assemble the one system message from configured prompt texts.
///
/// Ordering is by dependency id, not by configuration order: the prompt list is not
/// part of the fingerprint's *content* ordering, so letting TOML order decide what
/// the model reads would make two runs over identical dependency state send different
/// messages. The texts are normalized with the same normalization the `content` facet
/// digests, so the bytes that were fingerprinted are the bytes that are sent.
///
/// An empty list is `None` — no prompts configured means **no system message at all**,
/// not an empty one.
pub fn assemble_system_prompt(prompts: &[(String, String)]) -> Option<String> {
    if prompts.is_empty() {
        return None;
    }

    let mut ordered: Vec<(&str, &str)> = prompts
        .iter()
        .map(|(id, text)| (id.as_str(), text.as_str()))
        .collect();
    ordered.sort_by_key(|(id, _)| *id);

    let mut message = String::new();
    for (index, (_, text)) in ordered.iter().enumerate() {
        if index > 0 {
            message.push_str(SYSTEM_PROMPT_SEPARATOR);
        }
        message.push_str(&normalize::normalize_text(text));
    }
    Some(message)
}

/// The system message for a project: every configured prompt, read under `root`.
pub fn system_prompt(config: &Config, root: &Path) -> Result<Option<String>> {
    if config.prompts.is_empty() {
        return Ok(None);
    }

    let mut prompts = Vec::with_capacity(config.prompts.len());
    for prompt in &config.prompts {
        let relative = normalize_rel_path(&prompt.path)?;
        let absolute = root.join(&relative);
        let bytes = std::fs::read(&absolute).map_err(|source| Error::Read {
            path: absolute.clone(),
            source,
        })?;
        let text = String::from_utf8(bytes).map_err(|_| Error::PromptNotUtf8 {
            path: absolute.clone(),
        })?;
        // The id is the same one the prompt dependency carries, so an assembled
        // message can be traced back to the file it came from.
        prompts.push((format!("prompt:{relative}"), text));
    }

    Ok(assemble_system_prompt(&prompts))
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

/// What one capture of one probe needs.
///
/// The digests come from the probe loader and the fingerprint layer. The runner
/// records them rather than computing them: a trace that recomputed its own probe
/// digest could not be checked against the probe it claims to be about.
#[derive(Debug, Clone, Copy)]
pub struct CaptureRequest<'a> {
    pub probe: &'a str,
    pub probe_digest: &'a str,
    pub agent_checksum: &'a str,
    /// The probe prompt, as written. The system message is not part of it: that is
    /// the agent's, and it arrives through the [`Client`].
    pub prompt: &'a str,
    /// The effective repeat. `samples.len()` equals it: the metric denominator is
    /// the number of samples the probe asked for.
    pub repeat: u32,
}

/// A model endpoint, the catalog it is shown, and the cache it is recorded in.
#[derive(Debug, Clone)]
pub struct Runner {
    client: Client,
    catalog: ToolCatalog,
    /// Computed once: the catalog cannot change under a constructed runner, and the
    /// digest is part of every cache key.
    catalog_digest: Digest,
    cache: Cache,
    refresh: bool,
}

impl Runner {
    /// Build a runner for a model, a catalog, and a project root.
    ///
    /// The client is built here, so a catalog the wire cannot carry (a duplicated
    /// name, a name the function grammar rejects) fails before any probe runs rather
    /// than on the first sample.
    pub fn new(
        model: &ModelConfig,
        catalog: ToolCatalog,
        system: Option<String>,
        root: &Path,
    ) -> Result<Self> {
        let client = Client::with_default_timeout(model, &catalog, system)?;
        let catalog_digest = catalog.digest()?;

        Ok(Self {
            client,
            catalog,
            catalog_digest,
            cache: Cache::at(root),
            refresh: false,
        })
    }

    /// Change the per-sample timeout. The CLI always uses
    /// [`MODEL_SAMPLE_TIMEOUT`]; a test drives a slow endpoint with something short.
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.client = self.client.with_timeout(timeout);
        self
    }

    /// Ignore cache hits and re-record every sample of every capture.
    pub fn refreshing(mut self, refresh: bool) -> Self {
        self.refresh = refresh;
        self
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn catalog(&self) -> &ToolCatalog {
        &self.catalog
    }

    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    /// What every sample this runner takes will record about its circumstances.
    pub fn captured_with(&self) -> CapturedWith {
        CapturedWith {
            runner: RUNNER.to_string(),
            runner_version: RUNNER_VERSION,
            model_id: self.client.model_id().to_string(),
            effective_params: self.client.effective_params().clone(),
            tool_catalog_digest: self.catalog_digest.to_string(),
        }
    }

    /// The cache key for one sample of this runner.
    ///
    /// Public so a report can say *which* entry a sample came from, and so a test can
    /// state the key rather than reverse it.
    pub fn cache_inputs(&self, request: &CaptureRequest<'_>, sample_index: u32) -> CacheInputs {
        CacheInputs {
            runner_version: RUNNER_VERSION,
            agent_checksum: request.agent_checksum.to_string(),
            probe_digest: request.probe_digest.to_string(),
            tool_catalog_digest: self.catalog_digest.to_string(),
            effective_params: self.client.effective_params().clone(),
            sample_index,
        }
    }

    /// Capture one probe: `repeat` samples, each of which is one request or one
    /// cache hit.
    ///
    /// No retries and no concurrency. A sample that fails fails the capture: a
    /// partially captured probe cannot be scored, and scoring the samples that
    /// happened to succeed would report a pass rate over an unknown denominator.
    pub async fn capture(&self, request: &CaptureRequest<'_>) -> Result<Trace> {
        if request.repeat == 0 {
            return Err(Error::RunnerUnsupported {
                what: format!("capture the probe `{}`", request.probe),
                reason: "a probe needs at least one sample; `repeat` must be 1 or more".to_string(),
            });
        }

        let mut samples = Vec::with_capacity(request.repeat as usize);
        for index in 0..request.repeat {
            let inputs = self.cache_inputs(request, index);

            let sample = if self.refresh {
                None
            } else {
                self.cache.get(&inputs)?
            };

            let sample = match sample {
                Some(sample) => sample,
                None => {
                    let sample = self.client.sample(index, request.prompt).await?;
                    self.cache.put(&inputs, &sample)?;
                    sample
                }
            };

            samples.push(sample);
        }

        Ok(Trace {
            trace_version: TRACE_VERSION,
            probe: request.probe.to_string(),
            probe_digest: request.probe_digest.to_string(),
            agent_checksum: request.agent_checksum.to_string(),
            captured_with: self.captured_with(),
            samples,
        })
    }
}

// ---------------------------------------------------------------------------
// Atomic publication
// ---------------------------------------------------------------------------

/// Publish bytes at `path` so a reader sees the previous contents or the new ones.
///
/// Every file under the state directory is written this way: a run interrupted
/// mid-write must not leave a half-written entry that a later run reads as evidence.
/// The temporary name carries the pid and a counter, so two processes writing the
/// same directory cannot collide on it.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("entry");
    let temporary: PathBuf = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    std::fs::write(&temporary, bytes).map_err(|source| Error::Write {
        path: temporary.clone(),
        source,
    })?;

    std::fs::rename(&temporary, path).map_err(|source| {
        // The rename failed, so the temporary is rubbish that would otherwise be
        // read as an entry by a future scan of the directory.
        let _ = std::fs::remove_file(&temporary);
        Error::Write {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, PromptConfig};
    use crate::runner::catalog::test_support;
    use serde_json::json;

    fn config_with(paths: &[&str]) -> Config {
        Config {
            version: 1,
            agent: AgentConfig {
                name: "runner-test".to_string(),
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

    fn model() -> ModelConfig {
        ModelConfig {
            provider: "openai-compatible".to_string(),
            id: "fixture-model".to_string(),
            // Port 9 is the discard port: a test that reaches the network here has
            // already failed, and it has to be reachable for a connection error
            // rather than for a response.
            endpoint: Some("http://127.0.0.1:9/v1".to_string()),
            params: Default::default(),
        }
    }

    fn request<'a>(prompt: &'a str, repeat: u32) -> CaptureRequest<'a> {
        CaptureRequest {
            probe: "repository-search",
            probe_digest: "sha256:probe",
            agent_checksum: "ac1:agent",
            prompt,
            repeat,
        }
    }

    #[test]
    fn configured_prompts_are_joined_in_dependency_id_order() {
        let forward = assemble_system_prompt(&[
            ("prompt:prompts/a.md".to_string(), "First.".to_string()),
            ("prompt:prompts/b.md".to_string(), "Second.".to_string()),
        ])
        .unwrap();

        // The same set, declared the other way round. The message must not move:
        // configuration order is not fingerprinted, so it may not be observable.
        let reversed = assemble_system_prompt(&[
            ("prompt:prompts/b.md".to_string(), "Second.".to_string()),
            ("prompt:prompts/a.md".to_string(), "First.".to_string()),
        ])
        .unwrap();

        assert_eq!(forward, "First.\n\nSecond.");
        assert_eq!(forward, reversed);
    }

    #[test]
    fn no_prompts_configured_is_no_system_message_at_all() {
        assert_eq!(assemble_system_prompt(&[]), None);

        let dir = tempfile::tempdir().unwrap();
        assert_eq!(system_prompt(&config_with(&[]), dir.path()).unwrap(), None);
    }

    #[test]
    fn the_assembled_message_carries_the_bytes_the_content_facet_digested() {
        // The prompt is read twice in a real run: once by discovery, to digest it,
        // and once here, to send it. CRLF and a byte-order mark are exactly the
        // differences a platform introduces on its own, and if the two reads
        // disagree the runner would send text nobody fingerprinted.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("prompts")).unwrap();
        std::fs::write(
            dir.path().join("prompts/system.md"),
            "\u{feff}Be terse.\r\nNever guess.\r\n",
        )
        .unwrap();

        let config = config_with(&["prompts/system.md"]);
        let assembled = system_prompt(&config, dir.path()).unwrap().unwrap();

        let discovered = crate::discovery::prompts::discover(&config, dir.path()).unwrap();
        let fingered = discovered[0].facets["content"].digest.as_str().to_string();

        assert_eq!(assembled, "Be terse.\nNever guess.\n");
        assert_eq!(Digest::sha256(assembled.as_bytes()).as_str(), fingered);
    }

    #[test]
    fn a_missing_prompt_file_is_an_error_rather_than_a_shorter_message() {
        let dir = tempfile::tempdir().unwrap();
        let error = system_prompt(&config_with(&["prompts/missing.md"]), dir.path()).unwrap_err();
        assert!(matches!(error, Error::Read { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_cache_hit_is_evidence_without_any_endpoint() {
        // The endpoint is unreachable on purpose: if `capture` contacted it, this
        // test would fail with a connection error instead of returning a trace.
        let dir = tempfile::tempdir().unwrap();
        let runner = Runner::new(
            &model(),
            test_support::catalog(),
            Some("Be terse.".to_string()),
            dir.path(),
        )
        .unwrap();
        let request = request("Find repositories.", 2);

        let captured = Sample {
            index: 1,
            tool_calls: Vec::new(),
            final_text: Some("from the cache".to_string()),
        };
        runner
            .cache()
            .put(&runner.cache_inputs(&request, 1), &captured)
            .unwrap();

        // Sample 0 is not cached, so this capture is expected to fail: what is under
        // test is that the cached sample was never requested.
        let error = runner.capture(&request).await.unwrap_err();
        assert!(matches!(error, Error::RunnerRequest { .. }), "{error:?}");

        // With both samples cached, capture completes with no endpoint at all.
        let first = Sample {
            index: 0,
            tool_calls: Vec::new(),
            final_text: None,
        };
        runner
            .cache()
            .put(&runner.cache_inputs(&request, 0), &first)
            .unwrap();

        let trace = runner.capture(&request).await.unwrap();
        assert_eq!(trace.samples, vec![first, captured]);
        assert_eq!(trace.samples.len(), request.repeat as usize);
        assert_eq!(trace.probe_digest, "sha256:probe");
        assert_eq!(trace.agent_checksum, "ac1:agent");
        assert_eq!(trace.captured_with.runner, RUNNER);
        assert_eq!(
            trace.captured_with.effective_params["temperature"],
            json!(0.0)
        );
        assert_eq!(
            trace.captured_with.tool_catalog_digest,
            runner.catalog().digest().unwrap().to_string()
        );
    }

    #[tokio::test]
    async fn a_probe_with_no_samples_is_refused_rather_than_captured_empty() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Runner::new(&model(), test_support::catalog(), None, dir.path()).unwrap();

        let error = runner.capture(&request("anything", 0)).await.unwrap_err();
        assert!(
            matches!(error, Error::RunnerUnsupported { .. }),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_refreshing_runner_asks_the_endpoint_instead_of_reading_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Runner::new(&model(), test_support::catalog(), None, dir.path())
            .unwrap()
            .refreshing(true);
        let request = request("Find repositories.", 1);

        runner
            .cache()
            .put(
                &runner.cache_inputs(&request, 0),
                &Sample {
                    index: 0,
                    tool_calls: Vec::new(),
                    final_text: Some("stale".to_string()),
                },
            )
            .unwrap();

        // The entry exists, and `--refresh` ignores it: the unreachable endpoint is
        // what proves the cache was not read.
        let error = runner.capture(&request).await.unwrap_err();
        assert!(matches!(error, Error::RunnerRequest { .. }), "{error:?}");
    }

    #[test]
    fn an_atomic_write_replaces_the_file_and_leaves_nothing_else_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("entry.json");

        write_atomic(&path, b"first").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");

        let entries: Vec<PathBuf> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(entries, vec![path]);
    }
}
