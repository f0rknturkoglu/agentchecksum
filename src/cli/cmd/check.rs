// SPDX-License-Identifier: MIT OR Apache-2.0

//! The `check` command: the dependency diff, the probes, the policy, one verdict.
//!
//! This is the only command that fails a build, and the only one that writes the
//! behavioral baseline. Three things are kept apart on purpose, because the report has
//! to be able to say which of them happened: the static comparison (what the
//! dependencies did), the behavioral comparison (what the probes measured), and
//! whether either could honestly be made at all.
//!
//! Two rules shape the code more than any other:
//!
//! - **A check that could not be evaluated is an error, not a report.** Every capture
//!   and evaluation failure propagates: nothing here ever renders a report that says
//!   PASS because the part which would have said otherwise did not run.
//! - **`check` never writes the lockfile.** It is safe as a read-only CI step; the
//!   dependency baseline belongs to `snapshot` and the behavioral one to `--accept`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::task::JoinSet;

use crate::config::{Config, PolicyConfig, RiskLevel};
use crate::diff::{self, DiffReport};
use crate::discovery;
use crate::error::{Error, Result};
use crate::gate::policy;
use crate::gate::result::{DependencyHalf, combined_status};
use crate::gate::{
    BehaviorBaseline, BehaviorHalf, BehaviorOutcome, CheckReport, GateStatus, MetricComparison,
    changed_yardsticks, dependency_status,
};
use crate::lockfile::Lockfile;
use crate::manifest::AgentChecksum;
use crate::probes::{self, Metric, MetricScore, MetricScores, ProbeSuite, ResolvedProbe};
use crate::runner::{
    CaptureRequest, OwnedContext, RecordedEvidence, RunArtifact, Runner, STATE_DIR, ToolCatalog,
    Trace, system_prompt,
};

/// The committed behavioral baseline, relative to the project root.
const BASELINE_FILE: &str = "baseline.json";

/// Everything the `check` flags decided.
///
/// A struct rather than eight parameters: the command's behaviour is a function of the
/// flags it was given, and the two should be readable side by side.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Options {
    pub accept: bool,
    pub diff_only: bool,
    pub probes_only: bool,
    pub no_probes: bool,
    pub trace: Option<PathBuf>,
    pub refresh: bool,
    pub repeat: Option<u32>,
    pub jobs: Option<u32>,
    pub fail_on_drift: bool,
    pub fail_on_risk: Option<RiskLevel>,
    /// Whether the static half compares against a lockfile other than the committed
    /// one (`--from`).
    ///
    /// The command refuses `--accept` with it: the baseline that would be written
    /// belongs to the current agent, and asking for both is asking for two jobs at
    /// once. clap refuses the combination on the command line; this is the same rule
    /// stated where the command can enforce it.
    pub from_override: bool,
}

impl Options {
    /// Whether any probe is captured or evaluated.
    ///
    /// Two spellings, one meaning: `--diff-only` says what the gate is about,
    /// `--no-probes` says what to skip. Both leave the static half alone, and neither
    /// contacts a model.
    fn probes(&self) -> bool {
        !self.diff_only && !self.no_probes
    }

    /// How many probes may be in flight at once.
    fn jobs(&self) -> usize {
        self.jobs.unwrap_or(1) as usize
    }
}

/// Run a check and return the verdict.
///
/// `baseline_lock` is the lockfile the static half compares against: the committed
/// one, or the one named by `--from` when CI is comparing against a base revision.
pub async fn run(
    root: &Path,
    config_path: &Path,
    baseline_lock: &Path,
    options: &Options,
) -> Result<CheckReport> {
    validate(options)?;

    // Baseline first, as `diff` does: a missing or self-inconsistent lockfile means
    // the comparison cannot happen at all, and the user should be told that without
    // the tool first contacting a model.
    let baseline_lockfile = diff::load_baseline(baseline_lock)?;

    let config = Config::load(config_path)?;
    // `--fail-on-risk` overrides the configured threshold; it never rewrites it.
    let policy = effective_policy(&config.policy, options.fail_on_risk);

    let discovery = discovery::run(&config, root).await?;
    for warning in &discovery.warnings {
        tracing::warn!("{warning}");
    }
    let current = Lockfile::from_dependencies(&discovery.dependencies)?;
    let report = diff::diff(&baseline_lockfile, &current);

    let static_status = dependency_status(&report, &policy);
    // `--probes-only` narrows the gate to behavior: the dependency half is still
    // computed and reported (the tool catalog comes from it), but it does not decide
    // the verdict, and `--fail-on-drift` has nothing to fail on in that mode.
    let (gated_static, dependency_drift) = if options.probes_only {
        (GateStatus::Pass, false)
    } else {
        (static_status, report.changed)
    };

    // `--accept` refuses before anything is sampled: scores captured while the agent
    // itself has moved would be attributed to the wrong revision, and not one model
    // request is needed to find that out.
    if options.accept && static_status != GateStatus::Pass {
        return Err(Error::InvalidUsage {
            reason: format!(
                "`--accept` records a baseline for the current agent, and the dependency gate did \
                 not pass: {}. Run `agentchecksum snapshot`, review `agentchecksum diff`, and \
                 commit the new lockfile first",
                drift_clause(&report)
            ),
        });
    }

    if !options.probes() {
        let status = combined_status(gated_static, None, dependency_drift);
        return Ok(assemble(status, &report, &current.agent_checksum, None));
    }

    // The catalog the model is shown and the evaluator validates against, taken from
    // the current dependency state: `argument_validity` measures the arguments the
    // model emitted against the schemas in force now, and the baseline's yardstick
    // digests are what make a moved schema visible instead of silent.
    let catalog = ToolCatalog::from_lockfile(&current)?;
    let suite = probes::load_suite(root, &config.probes, &catalog, options.repeat)?;
    let agent_checksum = current.agent_checksum.clone();

    let traces = match options.trace.as_deref() {
        // Recorded evidence needs no endpoint and no model request: the whole point of
        // a replay is that it can be evaluated where the capture happened. It does
        // need to *be* about this agent, and that is what the binding check decides —
        // evidence from another agent, catalog, suite or runner is a runtime error,
        // never a score for an agent nobody measured.
        Some(path) => recorded_traces(path, &suite, &agent_checksum, &catalog)?,
        None => {
            let model = config.model.as_ref().ok_or_else(|| Error::ConfigInvalid {
                reason: "`check` samples the agent through `[model]`, and this configuration \
                         declares none; add a `[model]` section, or run `agentchecksum check \
                         --diff-only` to gate on dependencies alone"
                    .to_string(),
            })?;
            let runner = Runner::new(model, catalog.clone(), system_prompt(&config, root)?, root)?
                .refreshing(options.refresh);
            capture_all(&runner, &suite, agent_checksum.as_str(), options.jobs()).await?
        }
    };

    let mut outcomes = Vec::with_capacity(suite.probes.len());
    for (probe, trace) in suite.probes.iter().zip(&traces) {
        outcomes.push(probes::evaluate(probe, trace, &catalog)?);
    }
    let aggregate = probes::aggregate(&outcomes);

    let baseline = BehaviorBaseline::read_optional(&baseline_file(root))?;
    let suite_matches = baseline
        .as_ref()
        .is_some_and(|baseline| baseline.probe_suite_digest == suite.digest);
    let outcome = apply_policy(
        &policy,
        baseline.as_ref(),
        suite_matches,
        &aggregate,
        &suite,
        &catalog,
    )?;

    let behavior = BehaviorHalf::of(&outcomes, &outcome, baseline.as_ref(), &policy);
    let status = combined_status(gated_static, Some(outcome.status()), dependency_drift);
    let probe_scores: Vec<(String, MetricScore)> = outcomes
        .iter()
        .map(|outcome| {
            (
                outcome.probe.clone(),
                MetricScore::new(outcome.passed, outcome.total),
            )
        })
        .collect();

    // Evidence before contract: the run artifact is the machine-local record of what
    // the model answered, and it is written before the baseline so a reviewer can
    // re-derive the scores that were about to be accepted.
    if options.trace.is_none() {
        let path = RunArtifact::new(
            agent_checksum.as_str(),
            suite.digest.as_str(),
            traces,
            outcomes,
        )?
        .write(root)?;
        tracing::debug!("run artifact written to {}", path.display());
    }

    if options.accept {
        record_baseline(
            root,
            &agent_checksum,
            &suite,
            &aggregate,
            &outcome,
            &catalog,
            &probe_scores,
        )?;
    }

    Ok(assemble(status, &report, &agent_checksum, Some(behavior)))
}

/// The report, with the static half already computed.
fn assemble(
    status: GateStatus,
    report: &DiffReport,
    agent_checksum: &AgentChecksum,
    behavior: Option<BehaviorHalf>,
) -> CheckReport {
    CheckReport {
        status,
        agent_checksum: agent_checksum.clone(),
        baseline_checksum: Some(report.baseline_checksum.clone()),
        dependency: DependencyHalf::of(report),
        behavior,
        // A run that could not be evaluated returns an error instead of a report, so
        // this field is only ever set by a caller assembling a report by hand.
        error: None,
    }
}

/// Apply every configured metric policy to this run's aggregate.
fn apply_policy(
    policy: &PolicyConfig,
    baseline: Option<&BehaviorBaseline>,
    suite_matches: bool,
    aggregate: &MetricScores,
    suite: &ProbeSuite,
    catalog: &ToolCatalog,
) -> Result<BehaviorOutcome> {
    // A baseline from another runner contract was produced by different capture rules,
    // so its scores are not this run's "before". Absolute constraints still apply: a
    // threshold this run misses is a fact about this run, whatever the baseline did.
    let runner_contract_matches = baseline.is_some_and(BehaviorBaseline::runner_contract_matches);
    let comparable = suite_matches && runner_contract_matches;
    let mut failures = Vec::new();
    let mut notes = Vec::new();
    let mut relative_undecided = false;

    // In metric order rather than in the policy's own order, so a report reads the
    // same way whatever order the configuration was written in.
    for metric in Metric::ALL {
        let Some(metric_policy) = policy.metrics.get(metric.as_str()) else {
            continue;
        };

        let comparison = MetricComparison {
            current: aggregate.get(&metric),
            baseline: baseline.and_then(|baseline| baseline.metric(metric)),
            baseline_comparable: comparable,
            baseline_present: baseline.is_some(),
        };
        let evaluated = policy::evaluate(metric, metric_policy, &comparison);

        failures.extend(evaluated.failures);
        notes.extend(evaluated.notes);
        relative_undecided |= evaluated.relative_undecided;
    }

    Ok(BehaviorOutcome {
        baseline_present: baseline.is_some(),
        probe_suite_digest: suite.digest.as_str().to_string(),
        suite_matches,
        runner_contract_matches,
        yardsticks_changed: changed_yardsticks(baseline, &catalog.input_schema_digests()?),
        failures,
        notes,
        relative_undecided,
    })
}

/// Record the current behavior as the committed baseline.
///
/// Refused when a policy failed: a baseline is a record of *accepted* behavior, and
/// accepting a run that just failed its own thresholds would be a lie every later
/// comparison is built on. The refusal is never silent — the exit code already says
/// the run did not pass, and the diagnostic says nothing was written.
fn record_baseline(
    root: &Path,
    agent_checksum: &AgentChecksum,
    suite: &ProbeSuite,
    aggregate: &MetricScores,
    outcome: &BehaviorOutcome,
    catalog: &ToolCatalog,
    probe_scores: &[(String, MetricScore)],
) -> Result<()> {
    let path = baseline_file(root);

    if !outcome.failures.is_empty() {
        tracing::warn!(
            "`--accept` did not write `{}`: the run did not pass ({} failed policy constraint{}), \
             and a baseline records accepted behavior only",
            path.display(),
            outcome.failures.len(),
            if outcome.failures.len() == 1 { "" } else { "s" }
        );
        return Ok(());
    }

    let baseline = BehaviorBaseline::new(
        agent_checksum.clone(),
        suite.digest.clone(),
        aggregate.clone(),
        probe_scores.iter().cloned().collect(),
        catalog.input_schema_digests()?,
    );
    baseline.write(&path)
}

/// Capture every probe, up to `jobs` at a time, in probe order.
///
/// Concurrency changes when the requests are sent and nothing else: each trace is
/// filed under its probe's index, so the aggregate and the report are the same bytes
/// whatever `--jobs` says.
async fn capture_all(
    runner: &Runner,
    suite: &ProbeSuite,
    agent_checksum: &str,
    jobs: usize,
) -> Result<Vec<Trace>> {
    let runner = Arc::new(runner.clone());
    let mut traces: Vec<Option<Trace>> = suite.probes.iter().map(|_| None).collect();
    let mut pending: JoinSet<(usize, Result<Trace>)> = JoinSet::new();
    let mut next = 0;

    while next < suite.probes.len() || !pending.is_empty() {
        while next < suite.probes.len() && pending.len() < jobs {
            let probe: ResolvedProbe = suite.probes[next].clone();
            let index = next;
            let agent_checksum = agent_checksum.to_string();
            let runner = Arc::clone(&runner);

            pending.spawn(async move {
                let request = CaptureRequest {
                    probe: &probe.name,
                    probe_digest: probe.digest.as_str(),
                    agent_checksum: &agent_checksum,
                    prompt: &probe.prompt,
                    repeat: probe.repeat,
                };
                (index, runner.capture(&request).await)
            });
            next += 1;
        }

        match pending.join_next().await {
            Some(Ok((index, captured))) => traces[index] = Some(captured?),
            Some(Err(join)) => {
                return Err(Error::RunnerUnsupported {
                    what: "capture the probe suite".to_string(),
                    reason: format!("a capture task did not finish: {join}"),
                });
            }
            None => break,
        }
    }

    traces
        .into_iter()
        .collect::<Option<Vec<Trace>>>()
        .ok_or_else(|| Error::RunnerUnsupported {
            what: "capture the probe suite".to_string(),
            reason: "the capture finished without a trace for every probe".to_string(),
        })
}

/// The recorded evidence a `--trace` path names, bound to this run and aligned with
/// the suite.
///
/// Two things happen here, in this order, and both are refusals rather than warnings:
/// the evidence has to describe the agent being measured now (same checksum, catalog,
/// suite and runner), and it has to hold a trace for every probe in the suite. A probe
/// with no trace cannot be scored — reporting a pass rate over the probes that
/// happened to be present would be a rate over an unknown denominator.
fn recorded_traces(
    path: &Path,
    suite: &ProbeSuite,
    agent_checksum: &AgentChecksum,
    catalog: &ToolCatalog,
) -> Result<Vec<Trace>> {
    let evidence = RecordedEvidence::read(path)?;
    let context = OwnedContext::new(agent_checksum.as_str(), suite.digest.as_str(), catalog)?;
    evidence.validate(path, &context.as_context())?;

    let traces = evidence.traces();

    suite
        .probes
        .iter()
        .map(|probe| {
            traces
                .iter()
                .find(|trace| trace.probe == probe.name)
                .cloned()
                .ok_or_else(|| Error::TraceInvalid {
                    path: path.to_path_buf(),
                    reason: format!(
                        "the {} holds no trace for the probe `{}`, so there is nothing to evaluate",
                        evidence.shape(),
                        probe.name
                    ),
                })
        })
        .collect()
}

/// Flag combinations the command cannot honour.
///
/// clap rejects these on the command line, and the same rules are enforced here: the
/// CLI is not the only possible caller, and a combination with two readings must fail
/// loudly rather than quietly pick one.
fn validate(options: &Options) -> Result<()> {
    let reject = |first: &str, second: &str| {
        Err(Error::InvalidUsage {
            reason: format!("`{first}` and `{second}` cannot be combined"),
        })
    };

    if options.diff_only && options.probes_only {
        return reject("--diff-only", "--probes-only");
    }
    if options.probes_only && options.no_probes {
        return reject("--probes-only", "--no-probes");
    }
    if options.accept && options.no_probes {
        return reject("--accept", "--no-probes");
    }
    if options.accept && options.diff_only {
        return reject("--accept", "--diff-only");
    }
    if options.accept && options.trace.is_some() {
        return reject("--accept", "--trace");
    }
    if options.accept && options.from_override {
        return reject("--accept", "--from");
    }
    if options.trace.is_some() && options.refresh {
        return reject("--trace", "--refresh");
    }
    if options.trace.is_some() && options.jobs.is_some() {
        return reject("--trace", "--jobs");
    }
    if options.trace.is_some() && options.repeat.is_some() {
        return reject("--trace", "--repeat");
    }

    Ok(())
}

/// The policy this run applies: the configuration, with the CLI threshold on top.
fn effective_policy(config: &PolicyConfig, override_level: Option<RiskLevel>) -> PolicyConfig {
    let mut policy = config.clone();
    if let Some(level) = override_level {
        policy.fail_on_risk = Some(level);
    }
    policy
}

/// What moved in the dependency state, as one clause.
fn drift_clause(report: &DiffReport) -> String {
    if report.changes.is_empty() {
        return if report.changed {
            "the agent checksum changed".to_string()
        } else {
            "nothing changed, and the configured risk threshold failed the current state anyway"
                .to_string()
        };
    }

    let count = report.changes.len();
    format!(
        "{count} dependenc{} changed",
        if count == 1 { "y" } else { "ies" }
    )
}

/// The committed behavioral baseline for a project.
fn baseline_file(root: &Path) -> PathBuf {
    root.join(STATE_DIR).join(BASELINE_FILE)
}
