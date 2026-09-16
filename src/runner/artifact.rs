// SPDX-License-Identifier: MIT OR Apache-2.0

//! The local run artifact: one file per run, addressed by its own contents.
//!
//! A run produces two things that must not drift apart: the **traces** — what the
//! model actually answered — and the **evaluation** of those traces. Bundling them
//! makes the pair auditable: a score can be re-derived from the evidence beside it,
//! and evidence that does not match its evaluation is refused rather than re-scored.
//!
//! Three properties are deliberate:
//!
//! - **The address is the content.** The file under `.agentchecksum/runs/` is named
//!   after the SHA-256 of its own canonical form, so a hand-edited artifact is
//!   refused by `read` before anything evaluates it. There is no timestamp in the
//!   artifact for the same reason: identical evidence must have one address.
//! - **Read-back validates, it does not trust.** Run version, trace versions, probe
//!   digests, sample indices, the agent checksum, the recorded aggregate, and the
//!   address itself are all checked. Stale evidence is an error, never a score: a
//!   trace captured for a different revision of a probe is not evidence about the
//!   probe being asserted now.
//! - **Nothing here re-runs anything.** Evaluation is a pure function of a trace, so
//!   re-checking an artifact needs no network, no clock, and no model.
//!
//! The artifact is machine-local and gitignored: `baseline.json` is the committed
//! record, and it holds counts, scores, and digests only.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::fingerprint::canonical;
use crate::manifest::Digest;
use crate::probes::{MetricScores, ProbeOutcome, aggregate};
use crate::runner::STATE_DIR;
use crate::runner::trace::{CapturedWith, TRACE_VERSION, Trace};
use crate::runner::write_atomic;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::Value;

/// The run artifact format this build writes and reads.
pub const RUN_VERSION: u32 = 1;

/// The directory inside the state directory.
const RUNS_DIR: &str = "runs";

/// Peeked before the full parse, so a newer artifact is refused as a version problem
/// rather than reported as an unreadable file.
#[derive(Deserialize)]
struct VersionProbe {
    run_version: u32,
}

/// One run: its evidence, its evaluation, and the aggregate of that evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunArtifact {
    pub run_version: u32,
    pub agent_checksum: String,
    /// The probe suite the run was captured against. A different suite is a different
    /// test, and the recorded evaluation cannot be compared across the two.
    pub probe_suite_digest: String,
    /// What produced the traces. Every trace in one run was captured under the same
    /// circumstances, so this is recorded once rather than repeated per trace.
    pub captured_with: CapturedWith,
    pub traces: Vec<Trace>,
    /// One outcome per trace, in trace order.
    pub probes: Vec<ProbeOutcome>,
    /// The run-level metric counts: exact totals, folded from the outcomes.
    pub metrics: MetricScores,
}

impl RunArtifact {
    /// Bundle one run.
    ///
    /// The circumstances are taken from the traces rather than passed in, so an
    /// artifact cannot claim a model, a parameter set, or a catalog digest that its
    /// own evidence disagrees with. Traces that do not agree, a probe with no trace or
    /// no evaluation, and an empty run are all refusals: each would produce an
    /// artifact nobody can interpret.
    pub fn new(
        agent_checksum: &str,
        probe_suite_digest: &str,
        traces: Vec<Trace>,
        probes: Vec<ProbeOutcome>,
    ) -> Result<Self> {
        let captured_with = traces
            .first()
            .map(|trace| trace.captured_with.clone())
            .ok_or_else(|| Error::RunnerUnsupported {
                what: "bundle a run artifact".to_string(),
                reason: "the run holds no traces, so there is nothing to record and no \
                         circumstances to record them under"
                    .to_string(),
            })?;

        let artifact = Self {
            run_version: RUN_VERSION,
            agent_checksum: agent_checksum.to_string(),
            probe_suite_digest: probe_suite_digest.to_string(),
            captured_with,
            traces,
            metrics: aggregate(&probes),
            probes,
        };

        contradictions(&artifact).map_err(|reason| Error::RunnerUnsupported {
            what: "bundle a run artifact".to_string(),
            reason,
        })?;

        Ok(artifact)
    }

    /// The artifact's address: SHA-256 over its canonical form.
    pub fn digest(&self) -> Result<Digest> {
        Ok(Digest::sha256(&canonical::to_vec(self)?))
    }

    /// Where this artifact's contents belong.
    pub fn path_in(&self, root: &Path) -> Result<PathBuf> {
        Ok(root
            .join(STATE_DIR)
            .join(RUNS_DIR)
            .join(format!("{}.json", self.digest()?.hex())))
    }

    /// The traces of this run, ready to be evaluated or re-evaluated.
    pub fn traces(&self) -> &[Trace] {
        &self.traces
    }

    /// The trace for one probe.
    pub fn trace_for(&self, probe: &str) -> Option<&Trace> {
        self.traces.iter().find(|trace| trace.probe == probe)
    }

    /// Write the artifact under a project root, atomically, and name the file.
    ///
    /// The bytes are pretty-printed for a reader; the *address* is the canonical form,
    /// so formatting is never part of what the artifact is.
    pub fn write(&self, root: &Path) -> Result<PathBuf> {
        let path = self.path_in(root)?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let mut text =
            serde_json::to_string_pretty(self).map_err(|source| Error::Json { source })?;
        text.push('\n');
        write_atomic(&path, text.as_bytes())?;

        Ok(path)
    }

    /// Read one back and check that it is usable evidence.
    ///
    /// Everything a score depends on is verified here, including the file's own name:
    /// the caller gets an artifact it can evaluate, or an explanation of why it cannot.
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_path_buf(),
            source,
        })?;

        let unusable = |reason: String| Error::TraceInvalid {
            path: path.to_path_buf(),
            reason,
        };

        if let Ok(probe) = serde_json::from_str::<VersionProbe>(&text)
            && probe.run_version > RUN_VERSION
        {
            return Err(unusable(format!(
                "it was written as run_version {}, and this build reads {RUN_VERSION}",
                probe.run_version
            )));
        }

        let artifact: Self = serde_json::from_str(&text)
            .map_err(|source| unusable(format!("it is not a readable run artifact: {source}")))?;

        if artifact.run_version != RUN_VERSION {
            return Err(unusable(format!(
                "it was written as run_version {}, and this build reads {RUN_VERSION}",
                artifact.run_version
            )));
        }

        for trace in &artifact.traces {
            if trace.trace_version != TRACE_VERSION {
                return Err(unusable(format!(
                    "trace `{}` was written as trace_version {}, and this build reads {TRACE_VERSION}",
                    trace.probe, trace.trace_version
                )));
            }
        }

        contradictions(&artifact).map_err(unusable)?;

        // Last, because every other refusal above names a fact about the evidence
        // rather than about the file: a mismatched address can have any cause.
        let computed = artifact.digest()?;
        let named = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default();
        if named != computed.hex() {
            return Err(unusable(format!(
                "it is not the artifact its name claims: the file is named for `{named}` and its \
                 contents hash to `{}`, so it was edited or written by something that does not \
                 address artifacts by content",
                computed.hex()
            )));
        }

        Ok(artifact)
    }
}

/// Everything two parts of one bundle must agree on.
///
/// The reason is returned as text rather than as an error so the same check can be
/// reported against the run that is being bundled (`RunnerUnsupported`) and against
/// the file being read (`TraceInvalid`).
fn contradictions(artifact: &RunArtifact) -> Result<(), String> {
    for trace in &artifact.traces {
        if trace.captured_with != artifact.captured_with {
            return Err(format!(
                "trace `{}` records different circumstances ({}) than the run does ({})",
                trace.probe, trace.captured_with.model_id, artifact.captured_with.model_id
            ));
        }

        if trace.agent_checksum != artifact.agent_checksum {
            return Err(format!(
                "trace `{}` was captured under `{}`, and the run records `{}`",
                trace.probe, trace.agent_checksum, artifact.agent_checksum
            ));
        }

        for (position, sample) in trace.samples.iter().enumerate() {
            if sample.index != position as u32 {
                return Err(format!(
                    "trace `{}` holds sample index {} in position {position}; a capture records \
                     its samples in index order, and a reordered file is not the capture",
                    trace.probe, sample.index
                ));
            }
        }
    }

    for (position, trace) in artifact.traces.iter().enumerate() {
        if artifact.traces[..position]
            .iter()
            .any(|earlier| earlier.probe == trace.probe)
        {
            return Err(format!(
                "the run holds more than one trace for the probe `{}`",
                trace.probe
            ));
        }
    }

    for outcome in &artifact.probes {
        let Some(trace) = artifact.trace_for(&outcome.probe) else {
            return Err(format!(
                "the evaluation names the probe `{}`, which the run holds no trace for",
                outcome.probe
            ));
        };

        if outcome.probe_digest != trace.probe_digest {
            return Err(format!(
                "the evaluation of `{}` is against probe digest `{}`, and its trace was captured \
                 against `{}`; the assertion changed after the evidence did",
                outcome.probe, outcome.probe_digest, trace.probe_digest
            ));
        }

        if outcome.total as usize != trace.samples.len()
            || outcome.samples.len() != trace.samples.len()
        {
            return Err(format!(
                "the evaluation of `{}` covers {} samples, and its trace holds {}",
                outcome.probe,
                outcome.samples.len(),
                trace.samples.len()
            ));
        }

        for (position, sample) in outcome.samples.iter().enumerate() {
            if sample.sample != trace.samples[position].index {
                return Err(format!(
                    "the evaluation of `{}` judges sample {} where its trace records sample {}",
                    outcome.probe, sample.sample, trace.samples[position].index
                ));
            }
        }
    }

    for trace in &artifact.traces {
        if !artifact
            .probes
            .iter()
            .any(|outcome| outcome.probe == trace.probe)
        {
            return Err(format!(
                "the run holds a trace for `{}` with no evaluation; a bundle without its verdicts \
                 cannot say what the run found",
                trace.probe
            ));
        }
    }

    let recomputed = aggregate(&artifact.probes);
    if recomputed != artifact.metrics {
        return Err(
            "the run-level metric counts are not the totals of the evaluations it holds"
                .to_string(),
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probes::eval::SampleOutcome;
    use crate::probes::metrics::{Metric, MetricScore};
    use crate::runner::trace::{Sample, ToolCall};
    use serde_json::json;

    fn trace(probe: &str, probe_digest: &str, samples: u32) -> Trace {
        Trace {
            trace_version: TRACE_VERSION,
            probe: probe.to_string(),
            probe_digest: probe_digest.to_string(),
            agent_checksum: "ac1:agent".to_string(),
            captured_with: CapturedWith {
                runner: "openai-chat-completions".to_string(),
                runner_version: 1,
                model_id: "fixture-model".to_string(),
                effective_params: Default::default(),
                tool_catalog_digest: "sha256:da".to_string(),
            },
            samples: (0..samples)
                .map(|index| Sample {
                    index,
                    tool_calls: vec![ToolCall {
                        name: "search_repositories".to_string(),
                        tool_id: Some("tool:fixture.search_repositories".to_string()),
                        arguments: Some(json!({ "query": "postgres" })),
                        arguments_parse_error: None,
                    }],
                    final_text: None,
                })
                .collect(),
        }
    }

    fn outcome(probe: &str, probe_digest: &str, samples: u32, passed: u32) -> ProbeOutcome {
        let mut metrics = MetricScores::new();
        metrics.insert(Metric::ToolSelection, MetricScore::new(passed, samples));

        ProbeOutcome {
            probe: probe.to_string(),
            probe_digest: probe_digest.to_string(),
            samples: (0..samples)
                .map(|index| SampleOutcome {
                    sample: index,
                    passed: index < passed,
                    checks: Vec::new(),
                })
                .collect(),
            passed,
            total: samples,
            metrics,
        }
    }

    fn artifact() -> RunArtifact {
        RunArtifact::new(
            "ac1:agent",
            "sha256:suite",
            vec![
                trace("repository-search", "sha256:p1", 2),
                trace("restraint", "sha256:p2", 1),
            ],
            vec![
                outcome("repository-search", "sha256:p1", 2, 2),
                outcome("restraint", "sha256:p2", 1, 1),
            ],
        )
        .unwrap()
    }

    fn written(artifact: &RunArtifact) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = artifact.write(dir.path()).unwrap();
        (dir, path)
    }

    /// Write `artifact` at the address its *own* contents produce, so a test can
    /// isolate a contradiction from the address check that would otherwise fire first.
    fn rewritten(artifact: &RunArtifact, dir: &Path) -> PathBuf {
        let path = artifact.path_in(dir).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = serde_json::to_string_pretty(artifact).unwrap();
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn a_bundled_run_reads_back_with_its_evidence_and_its_verdicts() {
        let artifact = artifact();
        let (dir, path) = written(&artifact);

        // The address is the content, and the file is named for it.
        let digest = artifact.digest().unwrap();
        assert_eq!(path.file_stem().unwrap().to_str().unwrap(), digest.hex());
        assert!(path.starts_with(dir.path().join(STATE_DIR).join(RUNS_DIR)));

        let read = RunArtifact::read(&path).unwrap();
        assert_eq!(read, artifact);
        assert_eq!(read.agent_checksum, "ac1:agent");
        assert_eq!(read.traces().len(), 2);
        assert_eq!(read.trace_for("restraint").unwrap().samples.len(), 1);
        assert!(read.trace_for("absent").is_none());
        // The recorded aggregate is the exact totals, not an average of scores.
        assert_eq!(read.metrics[&Metric::ToolSelection], MetricScore::new(3, 3));
    }

    #[test]
    fn the_same_evidence_has_one_address() {
        // No timestamp, no path, no capture order: identical captures are one file,
        // which is what makes the address a claim about the evidence.
        assert_eq!(artifact().digest().unwrap(), artifact().digest().unwrap());
    }

    #[test]
    fn an_edited_artifact_is_refused_by_its_own_address() {
        let artifact = artifact();
        let (_dir, path) = written(&artifact);

        // Evidence quietly rewritten in place — a tool call the model never made —
        // which is what an address is for: the bundle stays internally consistent, and
        // the file no longer hashes to the name it was found by.
        let mut value: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["traces"][0]["samples"][0]["tool_calls"][0]["name"] = json!("search_everything");
        std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();

        let error = RunArtifact::read(&path).unwrap_err();
        assert!(matches!(error, Error::TraceInvalid { .. }), "{error:?}");
        assert!(
            error
                .to_string()
                .contains("not the artifact its name claims"),
            "{error}"
        );
    }

    #[test]
    fn an_artifact_from_a_newer_build_is_refused() {
        let mut artifact = artifact();
        artifact.run_version = RUN_VERSION + 1;
        let dir = tempfile::tempdir().unwrap();
        // Named for its own contents, so the version is what is refused and not the
        // address.
        let path = rewritten(&artifact, dir.path());

        let error = RunArtifact::read(&path).unwrap_err();
        assert!(error.to_string().contains("run_version 2"), "{error}");
    }

    #[test]
    fn a_trace_from_a_newer_build_is_refused() {
        let mut artifact = artifact();
        artifact.traces[0].trace_version = TRACE_VERSION + 1;
        let dir = tempfile::tempdir().unwrap();
        let path = rewritten(&artifact, dir.path());

        let error = RunArtifact::read(&path).unwrap_err();
        assert!(error.to_string().contains("trace_version 2"), "{error}");
    }

    #[test]
    fn an_evaluation_against_a_changed_probe_is_refused() {
        // The assertion was edited after the evidence was captured. Scoring this would
        // compare a trace to a probe it was never about.
        let mut artifact = artifact();
        artifact.probes[0].probe_digest = "sha256:edited".to_string();
        let dir = tempfile::tempdir().unwrap();
        let path = rewritten(&artifact, dir.path());

        let error = RunArtifact::read(&path).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("the assertion changed after the evidence did"),
            "{error}"
        );
    }

    #[test]
    fn an_artifact_whose_samples_are_out_of_order_is_refused() {
        let mut artifact = artifact();
        artifact.traces[0].samples[0].index = 7;
        let dir = tempfile::tempdir().unwrap();
        let path = rewritten(&artifact, dir.path());

        let error = RunArtifact::read(&path).unwrap_err();
        assert!(error.to_string().contains("position 0"), "{error}");
    }

    #[test]
    fn a_run_holding_two_agents_checksums_is_refused() {
        let mut artifact = artifact();
        artifact.traces[1].agent_checksum = "ac1:other".to_string();
        let dir = tempfile::tempdir().unwrap();
        let path = rewritten(&artifact, dir.path());

        let error = RunArtifact::read(&path).unwrap_err();
        assert!(error.to_string().contains("ac1:other"), "{error}");
    }

    #[test]
    fn a_run_whose_aggregate_is_not_its_own_totals_is_refused() {
        let mut artifact = artifact();
        artifact
            .metrics
            .insert(Metric::ToolSelection, MetricScore::new(3, 2));
        let dir = tempfile::tempdir().unwrap();
        let path = rewritten(&artifact, dir.path());

        let error = RunArtifact::read(&path).unwrap_err();
        assert!(error.to_string().contains("not the totals"), "{error}");
    }

    #[test]
    fn a_trace_without_its_evaluation_is_refused() {
        let mut artifact = artifact();
        artifact.probes.pop();
        let dir = tempfile::tempdir().unwrap();
        let path = rewritten(&artifact, dir.path());

        let error = RunArtifact::read(&path).unwrap_err();
        assert!(error.to_string().contains("with no evaluation"), "{error}");
    }

    #[test]
    fn an_evaluation_without_its_trace_is_refused() {
        let mut artifact = artifact();
        artifact.traces.pop();
        let dir = tempfile::tempdir().unwrap();
        let path = rewritten(&artifact, dir.path());

        let error = RunArtifact::read(&path).unwrap_err();
        assert!(error.to_string().contains("no trace for"), "{error}");
    }

    #[test]
    fn a_body_that_is_not_a_run_artifact_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs").join("nonsense.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();

        let error = RunArtifact::read(&path).unwrap_err();
        assert!(matches!(error, Error::TraceInvalid { .. }), "{error:?}");
        assert!(
            error.to_string().contains("not a readable run artifact"),
            "{error}"
        );
    }

    #[test]
    fn bundling_traces_that_disagree_is_refused_at_the_call() {
        let mut other = trace("restraint", "sha256:p2", 1);
        other.captured_with.model_id = "another-model".to_string();

        let error = RunArtifact::new(
            "ac1:agent",
            "sha256:suite",
            vec![trace("repository-search", "sha256:p1", 1), other],
            vec![
                outcome("repository-search", "sha256:p1", 1, 1),
                outcome("restraint", "sha256:p2", 1, 1),
            ],
        )
        .unwrap_err();

        assert!(
            matches!(error, Error::RunnerUnsupported { .. }),
            "{error:?}"
        );
        assert!(
            error.to_string().contains("different circumstances"),
            "{error}"
        );
    }

    #[test]
    fn a_run_with_no_evidence_is_refused_at_the_call() {
        let error =
            RunArtifact::new("ac1:agent", "sha256:suite", Vec::new(), Vec::new()).unwrap_err();
        assert!(
            matches!(error, Error::RunnerUnsupported { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_bundle_whose_recorded_catalog_digest_is_not_its_traces_is_refused() {
        // The circumstances are recorded once, for the whole run, so a bundle that
        // claims a different catalog than its traces were captured with describes
        // nothing that happened.
        let mut artifact = artifact();
        artifact.captured_with.tool_catalog_digest = "sha256:other".to_string();
        let dir = tempfile::tempdir().unwrap();
        let path = rewritten(&artifact, dir.path());

        let error = RunArtifact::read(&path).unwrap_err();
        assert!(
            error.to_string().contains("different circumstances"),
            "{error}"
        );
    }
}
