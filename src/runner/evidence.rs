// SPDX-License-Identifier: MIT OR Apache-2.0

//! Recorded evidence, and what it takes to trust it.
//!
//! `check --trace` accepts two shapes — one recorded trace, or a whole run artifact —
//! and both are files a user can edit, copy from another project, or simply leave
//! behind after the agent changed. Neither may be evaluated because it parses.
//!
//! Evidence is evaluated only when it demonstrably describes the agent being measured
//! *now*: the same agent checksum, the same tool catalog, the same probe suite, and
//! the same runner. Anything else is a runtime error, never a score — a verdict
//! derived from another agent's evidence would be a claim about an agent nobody
//! measured, and no exit code can express that except the one for "unusable".
//!
//! The distinction these rules keep is between an *unresolved* call and a *forged*
//! one. A model that invents a tool name records `tool_id: null`, which is real
//! behavior and is scored as such. A call that claims a canonical `tool_id` is
//! asserting an identity, and that assertion is checked against the catalog rather
//! than believed: the claimed tool must exist, and its declared name must be the name
//! the call reported.

use std::path::Path;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::runner::artifact::{RUN_VERSION, RunArtifact};
use crate::runner::catalog::ToolCatalog;
use crate::runner::openai::{RUNNER, RUNNER_VERSION};
use crate::runner::trace::{TRACE_VERSION, Trace};

/// What the current run is, for evidence to be checked against.
///
/// Every field is a value this run computed for itself: the agent it fingerprinted,
/// the catalog the model would be shown, and the probe suite it is about to score.
pub struct RunContext<'a> {
    /// The current agent's checksum, from the dependencies discovered now.
    pub agent_checksum: &'a str,
    /// The digest of the current tool catalog.
    pub tool_catalog_digest: &'a str,
    /// The digest of the probe suite being evaluated.
    pub probe_suite_digest: &'a str,
}

/// A `RunContext` that owns its strings, so a caller can compute the digests it needs
/// and then borrow them for the check.
#[derive(Debug, Clone, PartialEq)]
pub struct OwnedContext {
    pub agent_checksum: String,
    pub probe_suite_digest: String,
    pub tool_catalog_digest: String,
}

impl OwnedContext {
    /// Compute the context from the pieces a `check` run already has.
    ///
    /// The catalog digest is derived here rather than passed in, so it is always the
    /// digest of the catalog this run is about to evaluate against.
    pub fn new(agent_checksum: &str, suite: &str, catalog: &ToolCatalog) -> Result<Self> {
        Ok(Self {
            agent_checksum: agent_checksum.to_string(),
            probe_suite_digest: suite.to_string(),
            tool_catalog_digest: catalog.digest()?.to_string(),
        })
    }
}

impl OwnedContext {
    /// Borrow this context for a check.
    pub fn as_context(&self) -> RunContext<'_> {
        RunContext {
            agent_checksum: &self.agent_checksum,
            tool_catalog_digest: &self.tool_catalog_digest,
            probe_suite_digest: &self.probe_suite_digest,
        }
    }
}

/// The recorded evidence a `--trace` path holds.
///
/// The variant is decided by the version key the file carries rather than by its
/// name: a run artifact holds more context than a lone trace, and that context is
/// what the comparability checks need — which is why reading one keeps it rather
/// than reducing it to its traces.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordedEvidence {
    Trace(Box<Trace>),
    Run(Box<RunArtifact>),
}

impl RecordedEvidence {
    /// Read a `--trace` path as whichever shape it holds.
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_path_buf(),
            source,
        })?;

        let peek: Value = serde_json::from_str(&text).map_err(|source| Error::TraceInvalid {
            path: path.to_path_buf(),
            reason: format!("it is not a readable trace or run artifact: {source}"),
        })?;

        if peek.get("run_version").is_some() {
            return Ok(RecordedEvidence::Run(Box::new(RunArtifact::read(path)?)));
        }
        if peek.get("trace_version").is_some() {
            return Ok(RecordedEvidence::Trace(Box::new(Trace::read(path)?)));
        }

        Err(Error::TraceInvalid {
            path: path.to_path_buf(),
            reason: "it carries neither a `trace_version` nor a `run_version`, so it is not \
                     recorded evidence"
                .to_string(),
        })
    }

    /// What this evidence is, for a diagnostic.
    pub fn shape(&self) -> &'static str {
        match self {
            RecordedEvidence::Trace(_) => "trace",
            RecordedEvidence::Run(_) => "run artifact",
        }
    }

    /// Every trace in the evidence, in the order it was read.
    pub fn traces(&self) -> &[Trace] {
        match self {
            RecordedEvidence::Trace(trace) => std::slice::from_ref(trace),
            RecordedEvidence::Run(artifact) => &artifact.traces,
        }
    }

    /// Whether the evidence describes the agent this run is measuring.
    ///
    /// Every check here answers the same question — *is this evidence about the agent
    /// in front of me?* — and each one fails closed with the fact that disagrees,
    /// because the caller has to be able to see which part of the context moved.
    pub fn validate(&self, path: &Path, context: &RunContext<'_>) -> Result<()> {
        let unusable = |reason: String| Error::TraceInvalid {
            path: path.to_path_buf(),
            reason,
        };

        match self {
            RecordedEvidence::Trace(trace) => validate_trace(trace, context).map_err(unusable),
            RecordedEvidence::Run(artifact) => {
                if artifact.run_version != RUN_VERSION {
                    return Err(unusable(format!(
                        "it is a run artifact of version {}, and this build records version \
                         {RUN_VERSION}",
                        artifact.run_version
                    )));
                }
                if let Some(reason) = context_mismatch(
                    &artifact.captured_with.runner,
                    artifact.captured_with.runner_version,
                    artifact.captured_with.tool_catalog_digest.as_str(),
                    context,
                ) {
                    return Err(unusable(format!("the run it records {reason}")));
                }
                // Redundant with the per-trace check below for any artifact that went
                // through `new` or `read`, which tie the two together — and kept
                // anyway, because this is the check that can name the *run* as the
                // thing captured under another agent, which is what a reader holding a
                // whole file needs to hear.
                if artifact.agent_checksum != context.agent_checksum {
                    return Err(unusable(agent_reason(&artifact.agent_checksum, context)));
                }
                if artifact.probe_suite_digest != context.probe_suite_digest {
                    return Err(unusable(format!(
                        "it was captured against the probe suite `{}`, and the loaded suite is \
                         `{}`; a score from one suite is not a score for the other",
                        artifact.probe_suite_digest, context.probe_suite_digest
                    )));
                }

                // The artifact's own consistency is already enforced when it is read;
                // re-checking each trace here keeps the per-trace rules in one place.
                for trace in &artifact.traces {
                    validate_trace(trace, context).map_err(|reason| {
                        unusable(format!("the trace for `{}` {reason}", trace.probe))
                    })?;
                }

                Ok(())
            }
        }
    }
}

/// Why one trace is not evidence about the current agent, or `None` when it is.
fn validate_trace(trace: &Trace, context: &RunContext<'_>) -> std::result::Result<(), String> {
    if trace.trace_version != TRACE_VERSION {
        return Err(format!(
            "was recorded by trace version {}, and this build reads version {TRACE_VERSION}",
            trace.trace_version
        ));
    }
    if let Some(reason) = context_mismatch(
        &trace.captured_with.runner,
        trace.captured_with.runner_version,
        trace.captured_with.tool_catalog_digest.as_str(),
        context,
    ) {
        return Err(reason);
    }
    if trace.agent_checksum != context.agent_checksum {
        return Err(agent_reason(&trace.agent_checksum, context));
    }
    Ok(())
}

/// The runner and catalog half of the check, shared by a trace and a run.
fn context_mismatch(
    runner: &str,
    runner_version: u32,
    tool_catalog_digest: &str,
    context: &RunContext<'_>,
) -> Option<String> {
    if runner != RUNNER {
        return Some(format!(
            "was captured by the runner `{runner}`, and this build captures with `{RUNNER}`; a \
             different runner measures different things, so its samples cannot be scored by this \
             evaluator"
        ));
    }
    if runner_version != RUNNER_VERSION {
        return Some(format!(
            "was captured by runner version {runner_version}, and this build records version \
             {RUNNER_VERSION}; the capture contract changed, so the samples were taken under \
             different rules"
        ));
    }
    if tool_catalog_digest != context.tool_catalog_digest {
        return Some(format!(
            "saw the tool catalog `{tool_catalog_digest}`, and the current catalog is `{}`; the \
             model was choosing from a different set of tools, so its choices measure something \
             else",
            context.tool_catalog_digest
        ));
    }
    None
}

/// The agent-checksum refusal, with the next step in it.
fn agent_reason(captured: &str, context: &RunContext<'_>) -> String {
    format!(
        "was captured under the agent `{captured}`, and the current agent is `{}`; evidence about \
         one agent cannot be scored as another's behavior. Re-run `check` to capture evidence for \
         the current agent, or restore the dependency state this evidence describes",
        context.agent_checksum
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probes::{Metric, MetricScore, MetricScores, ProbeOutcome, SampleOutcome};
    use crate::runner::trace::{CapturedWith, Sample, ToolCall};
    use std::collections::BTreeMap;

    const AGENT: &str = "ac1:aaaa";
    const SUITE: &str = "sha256:suite";

    fn captured_with(catalog: &str) -> CapturedWith {
        CapturedWith {
            runner: RUNNER.to_string(),
            runner_version: RUNNER_VERSION,
            model_id: "fixture-model".to_string(),
            effective_params: BTreeMap::from([("temperature".to_string(), Value::from(0.0))]),
            tool_catalog_digest: catalog.to_string(),
        }
    }

    fn trace(catalog: &str) -> Trace {
        Trace {
            trace_version: TRACE_VERSION,
            probe: "no-tools".to_string(),
            probe_digest: "sha256:probe".to_string(),
            agent_checksum: AGENT.to_string(),
            captured_with: captured_with(catalog),
            samples: vec![Sample {
                index: 0,
                tool_calls: Vec::new(),
                final_text: Some("Lisbon.".to_string()),
            }],
        }
    }

    /// A whole, consistent run: the trace above and the evaluation that judged it.
    ///
    /// Built by `RunArtifact::new` rather than by hand, so these tests exercise
    /// evidence that really passes the runner's own checks — an artifact that fails
    /// them would be refused before any binding rule was reached.
    fn artifact(catalog: &str) -> RunArtifact {
        let trace = trace(catalog);
        let outcome = ProbeOutcome {
            probe: trace.probe.clone(),
            probe_digest: trace.probe_digest.clone(),
            samples: vec![SampleOutcome {
                sample: 0,
                passed: true,
                checks: Vec::new(),
            }],
            passed: 1,
            total: 1,
            metrics: MetricScores::from([(Metric::ToolRestraint, MetricScore::new(1, 1))]),
        };

        RunArtifact::new(AGENT, SUITE, vec![trace], vec![outcome]).expect("a consistent run")
    }

    /// A context whose digests the fixtures above agree with.
    fn context<'a>(agent: &'a str, suite: &'a str, catalog: &'a str) -> RunContext<'a> {
        RunContext {
            agent_checksum: agent,
            tool_catalog_digest: catalog,
            probe_suite_digest: suite,
        }
    }

    fn check(evidence: &RecordedEvidence, context: &RunContext<'_>) -> Result<()> {
        evidence.validate(Path::new("evidence.json"), context)
    }

    #[test]
    fn matching_evidence_is_accepted_in_both_shapes() {
        let trace = RecordedEvidence::Trace(Box::new(trace("sha256:catalog")));
        let run = RecordedEvidence::Run(Box::new(artifact("sha256:catalog")));
        let current = context(AGENT, SUITE, "sha256:catalog");

        assert!(check(&trace, &current).is_ok());
        assert!(check(&run, &current).is_ok());
    }

    #[test]
    fn another_agents_evidence_is_refused() {
        // The scenario the rule exists for: prompts change, a snapshot makes a new
        // agent, the probes happen to be untouched, and the old run is replayed.
        let run = RecordedEvidence::Run(Box::new(artifact("sha256:catalog")));
        let moved = context("ac1:bbbb", SUITE, "sha256:catalog");

        let error = check(&run, &moved).unwrap_err();
        assert!(matches!(error, Error::TraceInvalid { .. }), "{error:?}");
        let message = error.to_string();
        assert!(message.contains(AGENT), "{message}");
        assert!(message.contains("ac1:bbbb"), "{message}");
    }

    #[test]
    fn a_lone_trace_is_bound_the_same_way() {
        let trace = RecordedEvidence::Trace(Box::new(trace("sha256:catalog")));

        assert!(check(&trace, &context("ac1:bbbb", SUITE, "sha256:catalog")).is_err());
        assert!(check(&trace, &context(AGENT, SUITE, "sha256:other")).is_err());
    }

    #[test]
    fn another_catalogs_evidence_is_refused() {
        let run = RecordedEvidence::Run(Box::new(artifact("sha256:old-catalog")));
        let error = check(&run, &context(AGENT, SUITE, "sha256:new-catalog")).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("sha256:old-catalog"), "{message}");
        assert!(message.contains("different set of tools"), "{message}");
    }

    #[test]
    fn another_suites_run_artifact_is_refused() {
        // The artifact's suite digest is metadata a naive reader discards; keeping it
        // is what makes this refusal possible.
        let run = RecordedEvidence::Run(Box::new(artifact("sha256:catalog")));
        let error = check(
            &run,
            &context(AGENT, "sha256:another-suite", "sha256:catalog"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("probe suite"), "{error}");
    }

    #[test]
    fn an_unknown_runner_is_refused() {
        let mut evidence = trace("sha256:catalog");
        evidence.captured_with.runner = "some-other-runner".to_string();

        let error = check(
            &RecordedEvidence::Trace(Box::new(evidence)),
            &context(AGENT, SUITE, "sha256:catalog"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("some-other-runner"), "{error}");
    }

    #[test]
    fn a_newer_runner_version_is_refused() {
        let mut evidence = trace("sha256:catalog");
        evidence.captured_with.runner_version = RUNNER_VERSION + 1;

        let error = check(
            &RecordedEvidence::Trace(Box::new(evidence)),
            &context(AGENT, SUITE, "sha256:catalog"),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("capture contract changed"),
            "{error}"
        );
    }

    #[test]
    fn a_newer_trace_version_is_refused_rather_than_reinterpreted() {
        let mut evidence = trace("sha256:catalog");
        evidence.trace_version = TRACE_VERSION + 1;

        let error = check(
            &RecordedEvidence::Trace(Box::new(evidence)),
            &context(AGENT, SUITE, "sha256:catalog"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("trace version"), "{error}");
    }

    #[test]
    fn a_file_that_is_not_evidence_is_refused_by_shape_not_by_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-evidence.json");
        std::fs::write(&path, b"{\"hello\":\"world\"}").unwrap();

        let error = RecordedEvidence::read(&path).unwrap_err();
        assert!(error.to_string().contains("neither"), "{error}");

        let path = dir.path().join("broken.json");
        std::fs::write(&path, b"{").unwrap();
        assert!(RecordedEvidence::read(&path).is_err());
    }

    #[test]
    fn the_shape_is_decided_by_the_version_key_a_file_carries() {
        let dir = tempfile::tempdir().unwrap();

        let trace_path = dir.path().join("one.json");
        std::fs::write(
            &trace_path,
            serde_json::to_vec_pretty(&trace("sha256:catalog")).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            RecordedEvidence::read(&trace_path).unwrap(),
            RecordedEvidence::Trace(_)
        ));

        // A run artifact is written under the name its own contents hash to, so it is
        // written through its writer rather than named by this test.
        let run_path = artifact("sha256:catalog").write(dir.path()).unwrap();
        assert!(matches!(
            RecordedEvidence::read(&run_path).unwrap(),
            RecordedEvidence::Run(_)
        ));
    }

    #[test]
    fn a_forged_tool_call_is_caught_where_it_is_evaluated_not_here() {
        // Evidence binding is about *which agent* this is; whether one call inside it
        // is honest about its tool is a statement about the sample, and it is decided
        // by the evaluator, which has the catalog. This test pins the division.
        let mut evidence = trace("sha256:catalog");
        evidence.samples[0].tool_calls.push(ToolCall {
            name: "delete_everything".to_string(),
            tool_id: Some("tool:github.search_repositories".to_string()),
            arguments: None,
            arguments_parse_error: None,
        });

        assert!(
            check(
                &RecordedEvidence::Trace(Box::new(evidence)),
                &context(AGENT, SUITE, "sha256:catalog"),
            )
            .is_ok()
        );
    }
}
