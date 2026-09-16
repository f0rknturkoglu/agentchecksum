// SPDX-License-Identifier: MIT OR Apache-2.0

//! The pure half of Phase 4: a resolved probe and a recorded trace in, per-sample
//! judgements out.
//!
//! Nothing here touches a file, a clock, a random number generator or a network. That
//! is the whole point of separating capture from evaluation: a green run can be
//! re-derived offline from the trace that produced it, and a disputed score can be
//! re-derived from the same evidence by a second reader.
//!
//! Every metric is a quality score — 1.0 good, 0.0 bad — and a metric that no sample
//! made applicable is **absent** rather than perfect. Zero tool calls leaving
//! `argument_validity` out of the report is the difference between "no arguments were
//! wrong" and "there were no arguments to be wrong".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jsonschema::Validator;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::runner::catalog::{ToolCatalog, ToolContract};
use crate::runner::trace::{Sample, ToolCall, Trace};

use super::matchers;
use super::metrics::{Metric, MetricScore, MetricScores};
use super::model::ResolvedProbe;
use super::resolve::ResolvedTool;

/// One expectation's verdict on one sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub metric: Metric,
    pub passed: bool,
    /// Why, in one deterministic sentence: detail for a reader, never an input to a
    /// metric. It can quote the model's own values, so it belongs in a run report
    /// rather than in the committed baseline.
    pub detail: String,
}

/// One sample's verdict: the check that every applicable expectation passed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleOutcome {
    pub sample: u32,
    pub passed: bool,
    pub checks: Vec<Check>,
}

/// One probe's verdict over its samples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeOutcome {
    pub probe: String,
    pub probe_digest: String,
    pub samples: Vec<SampleOutcome>,
    /// Samples that passed every applicable check.
    pub passed: u32,
    /// Samples evaluated: the probe's effective repeat.
    pub total: u32,
    /// Exact counts per metric, in metric order. A metric missing from this map was
    /// not applicable to any sample, which is not the same as scoring zero.
    pub metrics: MetricScores,
}

impl ProbeOutcome {
    /// The probe's score, or `None` when it had no samples to score.
    pub fn score(&self) -> Option<f64> {
        (self.total > 0).then(|| f64::from(self.passed) / f64::from(self.total))
    }
}

/// Fold several probes into one run's metric counts.
///
/// Exact counts are added, never averaged: averaging per-probe scores would let a
/// probe with one sample weigh as much as a probe with a hundred, and the totals are
/// what a baseline records so a reader can audit the arithmetic.
pub fn aggregate(outcomes: &[ProbeOutcome]) -> MetricScores {
    let mut totals = MetricScores::new();
    for outcome in outcomes {
        for (metric, score) in &outcome.metrics {
            let entry = totals
                .entry(*metric)
                .or_insert_with(|| MetricScore::new(0, 0));
            entry.passed += score.passed;
            entry.total += score.total;
        }
    }
    totals
}

/// Compile a self-contained JSON Schema.
///
/// `jsonschema` is built without `resolve-http` and `resolve-file` on purpose: a
/// validator that can fetch a URL would turn a probe file into network access. A
/// schema that needs anything this build cannot resolve is refused here — as an error
/// that stops the run, never as a zero score, because "we could not check" and "the
/// model was wrong" are different facts.
pub fn compile_schema(path: &Path, schema: &Value) -> Result<Validator> {
    Validator::new(schema).map_err(|source| Error::SchemaUnsupported {
        path: path.to_path_buf(),
        reason: source.to_string(),
    })
}

/// Evaluate one probe against its trace.
///
/// The trace is checked against the probe first: a trace captured for another probe,
/// another revision of this probe, or another number of samples is not evidence about
/// what is being asserted now, and scoring it would produce a number that means
/// nothing.
pub fn evaluate(
    probe: &ResolvedProbe,
    trace: &Trace,
    catalog: &ToolCatalog,
) -> Result<ProbeOutcome> {
    let inconsistent = |reason: String| Error::ProbeInvalid {
        name: probe.name.clone(),
        path: probe.path.clone(),
        reason,
    };

    if trace.probe != probe.name {
        return Err(inconsistent(format!(
            "trace `{}` was captured for probe `{}`",
            trace.probe, probe.name
        )));
    }
    if trace.probe_digest != probe.digest.as_str() {
        return Err(inconsistent(format!(
            "the trace records probe digest {}, but this probe now digests {}; re-capture it",
            trace.probe_digest, probe.digest
        )));
    }
    if trace.samples.len() != probe.repeat as usize {
        return Err(inconsistent(format!(
            "the trace holds {}, but the probe asks for {}",
            count(trace.samples.len(), "sample"),
            count(probe.repeat as usize, "sample")
        )));
    }

    // Compiled once per probe rather than once per sample: a schema is a yardstick,
    // and re-deriving it a hundred times would only add ways to disagree.
    let output_schema = match &probe.output_schema {
        None => None,
        Some(declared) => Some(compile_schema(
            &PathBuf::from(&declared.relative),
            &declared.schema,
        )?),
    };
    let mut compiled: BTreeMap<String, Validator> = BTreeMap::new();

    let mut samples = Vec::with_capacity(trace.samples.len());
    let mut metrics = MetricScores::new();
    let mut passed = 0;

    for sample in &trace.samples {
        let checks = check_sample(
            probe,
            sample,
            catalog,
            output_schema.as_ref(),
            &mut compiled,
        )?;
        let sample_passed = checks.iter().all(|check| check.passed);
        for check in &checks {
            metrics
                .entry(check.metric)
                .or_insert_with(|| MetricScore::new(0, 0))
                .record(check.passed);
        }
        if sample_passed {
            passed += 1;
        }
        samples.push(SampleOutcome {
            sample: sample.index,
            passed: sample_passed,
            checks,
        });
    }

    Ok(ProbeOutcome {
        probe: probe.name.clone(),
        probe_digest: probe.digest.as_str().to_string(),
        samples,
        passed,
        total: probe.repeat,
        metrics,
    })
}

/// Every applicable check for one sample, in metric order.
fn check_sample(
    probe: &ResolvedProbe,
    sample: &Sample,
    catalog: &ToolCatalog,
    output_schema: Option<&Validator>,
    compiled: &mut BTreeMap<String, Validator>,
) -> Result<Vec<Check>> {
    let mut checks = Vec::new();

    if let Some(expected) = &probe.expect_tool {
        let calls = calls_to(sample, expected);
        checks.push(Check {
            metric: Metric::ToolSelection,
            passed: !calls.is_empty(),
            detail: if calls.is_empty() {
                format!("`{}` was not called", expected.name)
            } else {
                format!("`{}` was called {} times", expected.name, calls.len())
            },
        });
    }

    if !probe.expect_args.is_empty() {
        // `expect_args` cannot exist without `expect_tool`: the loader rejected it.
        let expected = probe
            .expect_tool
            .as_ref()
            .expect("a probe with matchers always has an expected tool");
        let calls = calls_to(sample, expected);
        let satisfied = calls.iter().find(|call| {
            call.arguments_are_parsed()
                && call
                    .arguments
                    .as_ref()
                    .is_some_and(|arguments| matchers::satisfies(arguments, &probe.expect_args))
        });

        checks.push(Check {
            metric: Metric::ArgumentExpectation,
            passed: satisfied.is_some(),
            detail: match satisfied {
                Some(_) => format!(
                    "a call to `{}` satisfied {}",
                    expected.name,
                    matchers::describe_all(&probe.expect_args)
                ),
                None if calls.is_empty() => format!(
                    "`{}` was not called, so {} cannot hold",
                    expected.name,
                    matchers::describe_all(&probe.expect_args)
                ),
                None => format!(
                    "none of the {} calls to `{}` satisfied {}",
                    calls.len(),
                    expected.name,
                    matchers::describe_all(&probe.expect_args)
                ),
            },
        });
    }

    if !probe.forbid_tools.is_empty() {
        let called: Vec<&str> = probe
            .forbid_tools
            .iter()
            .filter(|tool| sample.tool_calls.iter().any(|call| tool.is_called_by(call)))
            .map(|tool| tool.name.as_str())
            .collect();
        checks.push(Check {
            metric: Metric::ForbiddenToolUsage,
            passed: called.is_empty(),
            detail: if called.is_empty() {
                "no forbidden tool was called".to_string()
            } else {
                format!("a forbidden tool was called: {}", quote_all(&called))
            },
        });
    }

    if probe.expect_no_tool {
        checks.push(Check {
            metric: Metric::ToolRestraint,
            passed: sample.tool_calls.is_empty(),
            detail: match sample.tool_calls.len() {
                0 => "no tool was called".to_string(),
                1 => "1 tool was called".to_string(),
                count => format!("{count} tools were called"),
            },
        });
    }

    if let Some(validator) = output_schema {
        let (passed, detail) = check_final_text(sample.final_text.as_deref(), validator);
        checks.push(Check {
            metric: Metric::StructuredOutputValidity,
            passed,
            detail,
        });
    }

    // Automatic, and applicable only when the model actually called something: an
    // absent metric is honest, a free 1.0 would be a claim about nothing.
    if !sample.tool_calls.is_empty() {
        let (passed, detail) = check_arguments(sample, catalog, compiled)?;
        checks.push(Check {
            metric: Metric::ArgumentValidity,
            passed,
            detail,
        });
    }

    // Metric order rather than declaration order, so a report reads the same way for
    // every probe.
    checks.sort_unstable_by_key(|check| check.metric);

    Ok(checks)
}

/// Whether the final message is JSON that satisfies the schema.
///
/// No fence stripping and no repair: the probe declared a structure, and text that
/// needs unwrapping to parse is text that does not have it.
fn check_final_text(text: Option<&str>, validator: &Validator) -> (bool, String) {
    let Some(text) = text else {
        return (false, "the sample produced no final message".to_string());
    };
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return (false, "the final message is not JSON".to_string());
    };

    match validator.validate(&value) {
        Ok(()) => (true, "the final message parses and validates".to_string()),
        Err(error) => {
            let pointer = error.instance_path().to_string();
            (
                false,
                if pointer.is_empty() {
                    format!("the final message does not validate: {error}")
                } else {
                    format!("the final message does not validate at {pointer}: {error}")
                },
            )
        }
    }
}

/// Every call's arguments: declared tool, parsed, and valid against its schema.
fn check_arguments(
    sample: &Sample,
    catalog: &ToolCatalog,
    compiled: &mut BTreeMap<String, Validator>,
) -> Result<(bool, String)> {
    for call in &sample.tool_calls {
        let Some(tool) = declared_tool(catalog, call) else {
            return Ok((
                false,
                format!(
                    "`{}` is not a declared tool, so its arguments cannot be checked",
                    call.name
                ),
            ));
        };

        if let Some(error) = &call.arguments_parse_error {
            return Ok((
                false,
                format!("the arguments of `{}` are not JSON: {error}", call.name),
            ));
        }
        let Some(arguments) = &call.arguments else {
            return Ok((
                false,
                format!("the call to `{}` carries no arguments", call.name),
            ));
        };

        // Compiled once per tool per run; a tool whose schema this build cannot
        // resolve is an error, never a silent pass.
        if !compiled.contains_key(&tool.id) {
            let validator = compile_schema(&PathBuf::from(&tool.id), &tool.input_schema)?;
            compiled.insert(tool.id.clone(), validator);
        }
        let validator = &compiled[&tool.id];

        if let Err(error) = validator.validate(arguments) {
            let pointer = error.instance_path().to_string();
            return Ok((
                false,
                if pointer.is_empty() {
                    format!(
                        "the arguments of `{}` do not match its schema: {error}",
                        call.name
                    )
                } else {
                    format!(
                        "the arguments of `{}` do not match its schema at {pointer}: {error}",
                        call.name
                    )
                },
            ));
        }
    }

    Ok((
        true,
        format!(
            "every call's arguments match its tool's schema ({})",
            count(sample.tool_calls.len(), "call")
        ),
    ))
}

/// The tool a recorded call names, or `None` when the catalog cannot place it.
///
/// The id is what the runner resolved at capture time; the name is the fallback for a
/// call with no id, which is always either an invented name or a hand-authored trace.
fn declared_tool<'a>(catalog: &'a ToolCatalog, call: &ToolCall) -> Option<&'a ToolContract> {
    if let Some(id) = &call.tool_id {
        return catalog.by_id(id);
    }
    match catalog.by_name(&call.name).as_slice() {
        [tool] => Some(*tool),
        _ => None,
    }
}

/// The recorded calls that are a given tool.
fn calls_to<'a>(sample: &'a Sample, tool: &ResolvedTool) -> Vec<&'a ToolCall> {
    sample
        .tool_calls
        .iter()
        .filter(|call| tool.is_called_by(call))
        .collect()
}

fn count(number: usize, noun: &str) -> String {
    if number == 1 {
        format!("1 {noun}")
    } else {
        format!("{number} {noun}s")
    }
}

fn quote_all(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| format!("`{value}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use serde_json::json;

    use crate::lockfile::Lockfile;
    use crate::manifest::{Dependency, DependencyKind, Digest, Facet};
    use crate::probes::model::{OutputSchema, ResolvedExpectations};
    use crate::runner::catalog::ToolCatalog;

    const SEARCH: &str = "tool:github.search_repositories";
    const DELETE: &str = "tool:files.delete_file";

    fn dependency(id: &str, schema: Value) -> Dependency {
        let mut facets = BTreeMap::new();
        facets.insert(
            "input_schema".to_string(),
            Facet {
                digest: Digest::sha256(b"schema"),
                shape: None,
                normalized: Some(schema),
            },
        );
        Dependency {
            id: id.to_string(),
            kind: DependencyKind::Tool,
            facets,
            source: None,
        }
    }

    fn catalog() -> ToolCatalog {
        ToolCatalog::from_lockfile(
            &Lockfile::from_dependencies(&[
                dependency(
                    SEARCH,
                    json!({
                        "type": "object",
                        "properties": { "query": { "type": "string" }, "per_page": { "type": "integer" } },
                        "required": ["query"]
                    }),
                ),
                dependency(DELETE, json!({ "type": "object" })),
            ])
            .unwrap(),
        )
        .unwrap()
    }

    /// A probe whose expectations are given as TOML, so the tests read like a file.
    fn probe(name: &str, expectations: &str, repeat: u32) -> ResolvedProbe {
        let declared = toml::from_str::<crate::probes::model::ProbeFile>(&format!(
            "[[probe]]\nname = \"{name}\"\nprompt = \"Ask something.\"\n{expectations}"
        ))
        .unwrap()
        .probe
        .remove(0)
        .validate(Path::new("probes/a.toml"))
        .unwrap();

        let catalog = catalog();
        let resolved = |reference: &str| {
            crate::probes::resolve::resolve_tool(&catalog, name, reference).unwrap()
        };
        let expect_tool = declared.expect_tool.as_deref().map(resolved);
        let forbid_tools = declared
            .forbid_tools
            .iter()
            .map(|reference| resolved(reference))
            .collect();
        let output_schema = declared
            .output_schema
            .as_ref()
            .map(|relative| OutputSchema {
                relative: relative.clone(),
                schema: json!({
                    "type": "object",
                    "properties": { "answer": { "type": "string" } },
                    "required": ["answer"]
                }),
                digest: Digest::sha256(b"schema"),
            });

        ResolvedProbe::new(
            declared,
            PathBuf::from("probes/a.toml"),
            ResolvedExpectations {
                expect_tool,
                forbid_tools,
                output_schema,
            },
            repeat,
            repeat,
        )
        .unwrap()
    }

    fn call(name: &str, id: Option<&str>, arguments: Value) -> ToolCall {
        ToolCall {
            name: name.to_string(),
            tool_id: id.map(str::to_string),
            arguments: Some(arguments),
            arguments_parse_error: None,
        }
    }

    fn search_call(arguments: Value) -> ToolCall {
        call("search_repositories", Some(SEARCH), arguments)
    }

    fn sample(index: u32, tool_calls: Vec<ToolCall>, final_text: Option<&str>) -> Sample {
        Sample {
            index,
            tool_calls,
            final_text: final_text.map(str::to_string),
        }
    }

    fn trace(probe: &ResolvedProbe, samples: Vec<Sample>) -> Trace {
        Trace {
            trace_version: crate::runner::trace::TRACE_VERSION,
            probe: probe.name.clone(),
            probe_digest: probe.digest.as_str().to_string(),
            agent_checksum: "ac1:00".to_string(),
            captured_with: crate::runner::trace::CapturedWith {
                runner: "openai-chat-completions".to_string(),
                runner_version: 1,
                model_id: "qwen3:8b".to_string(),
                effective_params: BTreeMap::from([("temperature".to_string(), json!(0.0))]),
                tool_catalog_digest: "sha256:00".to_string(),
            },
            samples,
        }
    }

    fn run(probe: &ResolvedProbe, samples: Vec<Sample>) -> ProbeOutcome {
        evaluate(probe, &trace(probe, samples), &catalog()).unwrap()
    }

    fn check(outcome: &ProbeOutcome, sample: usize, metric: Metric) -> &Check {
        outcome.samples[sample]
            .checks
            .iter()
            .find(|check| check.metric == metric)
            .unwrap_or_else(|| panic!("{metric:?} was not checked on sample {sample}"))
    }

    #[test]
    fn an_expected_tool_appearing_is_selection_credit() {
        let probe = probe("search", "expect_tool = \"search_repositories\"", 1);
        let outcome = run(
            &probe,
            vec![sample(0, vec![search_call(json!({ "query": "x" }))], None)],
        );

        assert_eq!(outcome.passed, 1);
        assert_eq!(
            outcome.metrics[&Metric::ToolSelection],
            MetricScore::new(1, 1)
        );
        assert!(check(&outcome, 0, Metric::ToolSelection).passed);
    }

    #[test]
    fn a_missing_expected_tool_fails_selection_without_failing_anything_else() {
        let probe = probe("search", "expect_tool = \"search_repositories\"", 1);
        let outcome = run(
            &probe,
            vec![sample(
                0,
                vec![call("delete_file", Some(DELETE), json!({}))],
                None,
            )],
        );

        assert_eq!(outcome.passed, 0);
        assert!(!check(&outcome, 0, Metric::ToolSelection).passed);
        // The model called *a* tool — just not the right one — so argument_validity
        // still applies to what it did call.
        assert!(check(&outcome, 0, Metric::ArgumentValidity).passed);
    }

    #[test]
    fn a_duplicated_expected_tool_still_passes_selection() {
        let probe = probe("search", "expect_tool = \"search_repositories\"", 1);
        let outcome = run(
            &probe,
            vec![sample(
                0,
                vec![
                    search_call(json!({ "query": "x" })),
                    search_call(json!({ "query": "y" })),
                ],
                None,
            )],
        );

        assert_eq!(outcome.passed, 1);
        assert!(
            check(&outcome, 0, Metric::ToolSelection)
                .detail
                .contains("2 times")
        );
    }

    #[test]
    fn the_canonical_id_and_the_bare_name_reference_the_same_tool() {
        for reference in ["search_repositories", SEARCH] {
            let probe = probe("search", &format!("expect_tool = \"{reference}\""), 1);
            let outcome = run(
                &probe,
                vec![sample(0, vec![search_call(json!({ "query": "x" }))], None)],
            );
            assert_eq!(outcome.passed, 1, "{reference}");
        }
    }

    #[test]
    fn an_invented_tool_name_is_matched_by_name_but_a_mismatched_id_is_not() {
        let probe = probe("search", "expect_tool = \"search_repositories\"", 1);
        let by_name = run(
            &probe,
            vec![sample(
                0,
                vec![call(
                    "search_repositories",
                    None,
                    json!({ "query": "postgres" }),
                )],
                None,
            )],
        );
        assert_eq!(by_name.passed, 1);

        // A call whose resolved id names a different tool is that other tool, however
        // it spells its name.
        let by_id = run(
            &probe,
            vec![sample(
                0,
                vec![call("search_repositories", Some(DELETE), json!({}))],
                None,
            )],
        );
        assert_eq!(by_id.passed, 0);
    }

    #[test]
    fn argument_matchers_decide_argument_expectation_per_operator() {
        // (label, argument path, matcher, the call's arguments, expected verdict)
        let cases: Vec<(&str, &str, &str, Value, bool)> = vec![
            (
                "equals",
                "per_page",
                "{ equals = 50 }",
                json!({ "per_page": 50 }),
                true,
            ),
            (
                "equals mismatch",
                "per_page",
                "{ equals = 50 }",
                json!({ "per_page": 25 }),
                false,
            ),
            (
                "contains substring",
                "query",
                "{ contains = \"postgres\" }",
                json!({ "query": "postgres vector search" }),
                true,
            ),
            (
                "contains substring absent",
                "query",
                "{ contains = \"postgres\" }",
                json!({ "query": "mysql" }),
                false,
            ),
            (
                "contains array element",
                "query",
                "{ contains = \"postgres\" }",
                json!({ "query": ["mysql", "postgres"] }),
                true,
            ),
            (
                "contains array element absent",
                "query",
                "{ contains = \"postgres\" }",
                json!({ "query": ["mysql"] }),
                false,
            ),
            (
                "one_of",
                "per_page",
                "{ one_of = [25, 50] }",
                json!({ "per_page": 50 }),
                true,
            ),
            (
                "one_of miss",
                "per_page",
                "{ one_of = [25, 50] }",
                json!({ "per_page": 10 }),
                false,
            ),
            (
                "nested pointer",
                "/options/tags/1",
                "{ equals = \"b\" }",
                json!({ "options": { "tags": ["a", "b"] } }),
                true,
            ),
            (
                "missing path",
                "query",
                "{ equals = 1 }",
                json!({ "other": 1 }),
                false,
            ),
            (
                "type mismatch",
                "per_page",
                "{ equals = 50 }",
                json!({ "per_page": "50" }),
                false,
            ),
        ];

        for (label, pointer, matcher, arguments, expected) in cases {
            let probe = probe(
                "search",
                &format!(
                    "expect_tool = \"search_repositories\"\nexpect_args = {{ \"{pointer}\" = {matcher} }}"
                ),
                1,
            );
            let outcome = run(&probe, vec![sample(0, vec![search_call(arguments)], None)]);

            let expectation = check(&outcome, 0, Metric::ArgumentExpectation);
            assert_eq!(
                expectation.passed, expected,
                "{label}: {}",
                expectation.detail
            );
        }
    }

    #[test]
    fn every_matcher_on_a_call_must_hold_for_argument_expectation() {
        let probe = probe(
            "search",
            "expect_tool = \"search_repositories\"\n\
             expect_args = { query = { contains = \"postgres\" }, per_page = { equals = 50 } }",
            1,
        );

        let both = run(
            &probe,
            vec![sample(
                0,
                vec![search_call(json!({ "query": "postgres", "per_page": 50 }))],
                None,
            )],
        );
        assert_eq!(both.passed, 1);

        let one_matcher_short = run(
            &probe,
            vec![sample(
                0,
                vec![search_call(json!({ "query": "postgres", "per_page": 25 }))],
                None,
            )],
        );
        assert_eq!(one_matcher_short.passed, 0);
    }

    #[test]
    fn a_call_with_malformed_arguments_satisfies_no_matcher_and_fails_validity() {
        let probe = probe(
            "search",
            "expect_tool = \"search_repositories\"\nexpect_args = { query = { equals = \"x\" } }",
            1,
        );
        let mut broken = search_call(json!({}));
        broken.arguments = None;
        broken.arguments_parse_error = Some("expected value at line 1".to_string());

        let outcome = run(&probe, vec![sample(0, vec![broken], None)]);

        assert_eq!(outcome.passed, 0);
        assert!(!check(&outcome, 0, Metric::ArgumentExpectation).passed);
        assert!(!check(&outcome, 0, Metric::ArgumentValidity).passed);
        assert!(
            check(&outcome, 0, Metric::ArgumentValidity)
                .detail
                .contains("are not JSON")
        );
    }

    #[test]
    fn argument_validity_checks_every_call_against_its_own_schema() {
        let probe = probe("valid", "expect_no_tool = true", 1);

        let valid = run(
            &probe,
            vec![sample(
                0,
                vec![
                    search_call(json!({ "query": "postgres" })),
                    call("delete_file", Some(DELETE), json!({})),
                ],
                None,
            )],
        );
        assert!(check(&valid, 0, Metric::ArgumentValidity).passed);

        let invalid = run(
            &probe,
            vec![sample(
                0,
                vec![
                    search_call(json!({ "query": "postgres" })),
                    // `query` is required, and `per_page` is not a string.
                    search_call(json!({ "per_page": "many" })),
                ],
                None,
            )],
        );
        assert!(!check(&invalid, 0, Metric::ArgumentValidity).passed);
        assert!(
            check(&invalid, 0, Metric::ArgumentValidity)
                .detail
                .contains("do not match its schema")
        );
    }

    #[test]
    fn an_undeclared_tool_fails_argument_validity() {
        let probe = probe("valid", "expect_no_tool = true", 1);
        let outcome = run(
            &probe,
            vec![sample(
                0,
                vec![call("delete_everything", None, json!({ "all": true }))],
                None,
            )],
        );

        assert!(!check(&outcome, 0, Metric::ArgumentValidity).passed);
        assert!(
            check(&outcome, 0, Metric::ArgumentValidity)
                .detail
                .contains("not a declared tool")
        );
    }

    #[test]
    fn a_forbidden_tool_fails_forbidden_usage_and_a_clean_sample_passes() {
        let probe = probe("safe", "forbid_tools = [\"delete_file\"]", 1);

        let clean = run(
            &probe,
            vec![sample(0, vec![search_call(json!({ "query": "x" }))], None)],
        );
        assert_eq!(
            clean.passed, 1,
            "calling a declared tool with valid arguments violates nothing"
        );
        assert!(check(&clean, 0, Metric::ForbiddenToolUsage).passed);

        let violation = run(
            &probe,
            vec![sample(
                0,
                vec![call("delete_file", Some(DELETE), json!({}))],
                None,
            )],
        );
        assert!(!check(&violation, 0, Metric::ForbiddenToolUsage).passed);
        assert_eq!(violation.passed, 0);
    }

    #[test]
    fn restraint_passes_only_when_nothing_was_called() {
        let probe = probe("restraint", "expect_no_tool = true", 1);

        let quiet = run(&probe, vec![sample(0, Vec::new(), None)]);
        assert_eq!(quiet.passed, 1);
        assert!(check(&quiet, 0, Metric::ToolRestraint).passed);
        // No calls means no arguments to be wrong: the metric is absent, not perfect.
        assert!(!quiet.metrics.contains_key(&Metric::ArgumentValidity));

        let noisy = run(
            &probe,
            vec![sample(0, vec![search_call(json!({ "query": "x" }))], None)],
        );
        assert_eq!(noisy.passed, 0);
        assert!(!check(&noisy, 0, Metric::ToolRestraint).passed);
    }

    #[test]
    fn structured_output_validity_needs_json_that_validates() {
        let probe = probe("structured", "output_schema = \"schemas/result.json\"", 1);

        let cases: Vec<(&str, Option<&str>, bool)> = vec![
            ("valid", Some(r#"{"answer": "42"}"#), true),
            ("not json", Some("the answer is 42"), false),
            ("wrong shape", Some(r#"{"answer": 42}"#), false),
            ("missing property", Some("{}"), false),
            // No fence stripping: a fenced block is not the declared structure.
            ("fenced", Some("```json\n{\"answer\": \"42\"}\n```"), false),
            ("no text", None, false),
        ];

        for (label, text, expected) in cases {
            let samples: Vec<Sample> = (0..1)
                .map(|index| sample(index, Vec::new(), text))
                .collect();
            let outcome = run(&probe, samples);
            assert_eq!(
                check(&outcome, 0, Metric::StructuredOutputValidity).passed,
                expected,
                "{label}"
            );
            assert_eq!(outcome.passed == 1, expected, "{label}");
        }
    }

    #[test]
    fn every_applicable_expectation_must_pass_for_a_sample_to_pass() {
        let probe = probe(
            "combined",
            "expect_tool = \"search_repositories\"\n\
             expect_args = { query = { contains = \"postgres\" } }\n\
             forbid_tools = [\"delete_file\"]\n\
             output_schema = \"schemas/result.json\"",
            1,
        );

        // The expected tool is called with matching arguments and forbidden tools are
        // avoided, but the answer has the wrong shape: the sample fails.
        let outcome = run(
            &probe,
            vec![sample(
                0,
                vec![search_call(json!({ "query": "postgres vectors" }))],
                Some(r#"{"answer": 42}"#),
            )],
        );

        assert_eq!(outcome.passed, 0);
        assert!(check(&outcome, 0, Metric::ToolSelection).passed);
        assert!(check(&outcome, 0, Metric::ArgumentExpectation).passed);
        assert!(check(&outcome, 0, Metric::ForbiddenToolUsage).passed);
        assert!(check(&outcome, 0, Metric::ArgumentValidity).passed);
        assert!(!check(&outcome, 0, Metric::StructuredOutputValidity).passed);

        // Each metric keeps its own count over the same samples.
        assert_eq!(
            outcome.metrics[&Metric::ToolSelection],
            MetricScore::new(1, 1)
        );
        assert_eq!(
            outcome.metrics[&Metric::StructuredOutputValidity],
            MetricScore::new(0, 1)
        );
    }

    #[test]
    fn a_pass_rate_is_exact_counts_over_the_effective_repeat() {
        let probe = probe("search", "expect_tool = \"search_repositories\"", 3);
        let outcome = run(
            &probe,
            vec![
                sample(0, vec![search_call(json!({ "query": "a" }))], None),
                sample(1, vec![call("delete_file", Some(DELETE), json!({}))], None),
                sample(2, vec![search_call(json!({ "query": "b" }))], None),
            ],
        );

        assert_eq!(outcome.passed, 2);
        assert_eq!(outcome.total, 3);
        assert_eq!(outcome.score(), Some(2.0 / 3.0));
        assert_eq!(
            outcome.metrics[&Metric::ToolSelection],
            MetricScore::new(2, 3)
        );
        assert_eq!(
            outcome.metrics[&Metric::ArgumentValidity],
            MetricScore::new(3, 3)
        );
        assert_eq!(outcome.samples.len(), 3);
        assert_eq!(outcome.samples[1].sample, 1);
    }

    #[test]
    fn a_sample_with_no_calls_leaves_argument_validity_out_of_the_denominator() {
        // The mutation this defends: scoring a sample that called nothing as a
        // perfect `argument_validity`. Then this metric would read 2/2 — one sample
        // that had arguments checked, plus one that had none, both "passing".
        let probe = probe("restraint", "expect_no_tool = true", 2);
        let outcome = run(
            &probe,
            vec![
                sample(0, Vec::new(), None),
                sample(1, vec![search_call(json!({ "query": "x" }))], None),
            ],
        );

        assert_eq!(
            outcome.metrics[&Metric::ToolRestraint],
            MetricScore::new(1, 2)
        );
        assert_eq!(
            outcome.metrics[&Metric::ArgumentValidity],
            MetricScore::new(1, 1),
            "only the sample that called a tool is in the denominator"
        );
        assert_eq!(
            outcome.metrics[&Metric::ArgumentValidity].score(),
            Some(1.0)
        );
        assert_eq!(outcome.passed, 1);
    }

    #[test]
    fn a_trace_for_another_probe_is_refused() {
        let probe = probe("search", "expect_tool = \"search_repositories\"", 1);
        let mut wrong = trace(&probe, vec![sample(0, Vec::new(), None)]);
        wrong.probe = "other".to_string();

        let error = evaluate(&probe, &wrong, &catalog()).unwrap_err();
        assert!(matches!(error, Error::ProbeInvalid { .. }), "{error:?}");
    }

    #[test]
    fn a_trace_of_another_revision_of_the_probe_is_refused() {
        let probe = probe("search", "expect_tool = \"search_repositories\"", 1);
        let mut stale = trace(&probe, vec![sample(0, Vec::new(), None)]);
        stale.probe_digest = "sha256:0000".to_string();

        let error = evaluate(&probe, &stale, &catalog()).unwrap_err();
        match error {
            Error::ProbeInvalid { reason, .. } => {
                assert!(reason.contains("re-capture"), "{reason}")
            }
            other => panic!("expected ProbeInvalid, got {other:?}"),
        }
    }

    #[test]
    fn a_trace_with_the_wrong_number_of_samples_is_refused() {
        let probe = probe("search", "expect_tool = \"search_repositories\"", 3);
        let error = evaluate(
            &probe,
            &trace(&probe, vec![sample(0, Vec::new(), None)]),
            &catalog(),
        )
        .unwrap_err();

        match error {
            Error::ProbeInvalid { reason, .. } => {
                assert!(reason.contains("holds 1 sample"), "{reason}");
                assert!(reason.contains("asks for 3 samples"), "{reason}");
            }
            other => panic!("expected ProbeInvalid, got {other:?}"),
        }
    }

    #[test]
    fn a_metric_never_scores_better_for_worse_behavior() {
        let probe = probe("search", "expect_tool = \"search_repositories\"", 2);
        let expected_tool =
            |index: u32| sample(index, vec![search_call(json!({ "query": "a" }))], None);
        let other_tool = |index: u32| {
            sample(
                index,
                vec![call("delete_file", Some(DELETE), json!({}))],
                None,
            )
        };

        let all_correct = run(&probe, vec![expected_tool(0), expected_tool(1)]);
        let half = run(&probe, vec![expected_tool(0), other_tool(1)]);
        let none_correct = run(&probe, vec![other_tool(0), other_tool(1)]);

        // Four samples: two with the expected tool, then one, then none.
        let better = aggregate(&[all_correct, half.clone()]);
        let worse = aggregate(&[half, none_correct]);
        assert_eq!(better[&Metric::ToolSelection], MetricScore::new(3, 4));
        assert_eq!(worse[&Metric::ToolSelection], MetricScore::new(1, 4));

        for metric in Metric::ALL {
            let better_score = better.get(&metric).and_then(MetricScore::score);
            let worse_score = worse.get(&metric).and_then(MetricScore::score);
            if let (Some(better_score), Some(worse_score)) = (better_score, worse_score) {
                assert!(
                    better_score >= worse_score,
                    "{metric:?} must never score better for worse behavior"
                );
            }
        }
        assert!(better[&Metric::ToolSelection].score() > worse[&Metric::ToolSelection].score());
    }

    #[test]
    fn aggregation_keeps_exact_counts_and_leaves_inapplicable_metrics_absent() {
        let restraint = probe("restraint", "expect_no_tool = true", 2);
        let search = probe("search", "expect_tool = \"search_repositories\"", 2);

        let quiet = run(
            &restraint,
            vec![sample(0, Vec::new(), None), sample(1, Vec::new(), None)],
        );
        let loud = run(
            &search,
            vec![
                sample(0, vec![search_call(json!({ "query": "a" }))], None),
                sample(1, vec![search_call(json!({ "query": "b" }))], None),
            ],
        );

        let totals = aggregate(&[quiet.clone(), loud.clone()]);
        // `tool_restraint` applies only to the restraint probe's two samples, and
        // `tool_selection` only to the search probe's two.
        assert_eq!(totals[&Metric::ToolRestraint], MetricScore::new(2, 2));
        assert_eq!(totals[&Metric::ToolSelection], MetricScore::new(2, 2));
        // argument_validity applies to exactly two of the four samples.
        assert_eq!(totals[&Metric::ArgumentValidity], MetricScore::new(2, 2));
        assert!(
            !totals.contains_key(&Metric::StructuredOutputValidity),
            "a metric no probe declared is absent, never 0/0"
        );
        assert_eq!(
            quiet.metrics[&Metric::ToolRestraint],
            MetricScore::new(2, 2)
        );
        assert_eq!(loud.metrics[&Metric::ToolSelection], MetricScore::new(2, 2));
        assert_eq!(aggregate(&[]).len(), 0);
    }

    #[test]
    fn an_outcome_round_trips_through_json_for_a_run_artifact() {
        let probe = probe("search", "expect_tool = \"search_repositories\"", 1);
        let outcome = run(
            &probe,
            vec![sample(0, vec![search_call(json!({ "query": "x" }))], None)],
        );

        let json = serde_json::to_value(&outcome).unwrap();
        assert_eq!(json["probe"], "search");
        assert_eq!(json["passed"], 1);
        assert_eq!(json["samples"][0]["checks"][0]["metric"], "tool_selection");
        assert_eq!(json["metrics"]["tool_selection"]["passed"], 1);
        assert_eq!(json["metrics"]["tool_selection"]["total"], 1);
        assert_eq!(
            serde_json::from_value::<ProbeOutcome>(json).unwrap(),
            outcome
        );
    }

    #[test]
    fn an_unsupported_schema_is_an_error_and_never_a_zero_score() {
        let error = compile_schema(
            Path::new("schemas/result.json"),
            &json!({ "$ref": "https://example.com/remote.json" }),
        )
        .unwrap_err();

        match error {
            Error::SchemaUnsupported { path, .. } => {
                assert_eq!(path, PathBuf::from("schemas/result.json"));
            }
            other => panic!("expected SchemaUnsupported, got {other:?}"),
        }
    }

    #[test]
    fn a_tool_whose_input_schema_cannot_be_resolved_stops_the_evaluation() {
        // The same rule as the output schema: a yardstick this build cannot derive is
        // an error, not a failed check and not a zero.
        let catalog = ToolCatalog::from_lockfile(
            &Lockfile::from_dependencies(&[dependency(
                SEARCH,
                json!({ "$ref": "https://example.com/remote.json" }),
            )])
            .unwrap(),
        )
        .unwrap();
        let probe = probe("search", "expect_tool = \"search_repositories\"", 1);

        let error = evaluate(
            &probe,
            &trace(
                &probe,
                vec![sample(0, vec![search_call(json!({ "query": "x" }))], None)],
            ),
            &catalog,
        )
        .unwrap_err();

        assert!(
            matches!(error, Error::SchemaUnsupported { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn an_internal_reference_is_supported() {
        let validator = compile_schema(
            Path::new("schemas/result.json"),
            &json!({
                "$defs": { "answer": { "type": "string" } },
                "type": "object",
                "properties": { "answer": { "$ref": "#/$defs/answer" } }
            }),
        )
        .unwrap();

        assert!(validator.validate(&json!({ "answer": "42" })).is_ok());
        assert!(validator.validate(&json!({ "answer": 42 })).is_err());
    }
}
