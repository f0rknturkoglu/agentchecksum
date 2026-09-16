// SPDX-License-Identifier: MIT OR Apache-2.0

//! The probe: what a file declares, and what it means once resolved.
//!
//! Two forms, on purpose. `ProbeSpec` is what TOML says, validated but still holding
//! tool *references*; `ResolvedProbe` holds canonical tool ids, the loaded output
//! schema, the effective repeat and the digest that identifies the probe in a trace,
//! a cache key and a baseline. Everything downstream of a probe file is a pure
//! function of the resolved form, which is why the resolution — not the text — is
//! what a run records.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::normalize_rel_path;
use crate::error::{Error, Result};
use crate::manifest::Digest;

use super::digest::ProbeIdentity;
use super::matchers::{self, Matcher, Matchers};
use super::metrics::Metric;
use super::resolve::ResolvedTool;

/// How many probe files one suite may hold.
pub const MAX_PROBE_FILES: usize = 64;
/// How many probes one suite may hold.
pub const MAX_PROBES: usize = 256;
/// How long a prompt may be. Longer is refused, never truncated: a truncated prompt
/// measures a prompt the user did not write.
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;
/// How large a self-contained output schema may be.
pub const MAX_OUTPUT_SCHEMA_BYTES: u64 = 512 * 1024;
/// How many argument matchers one probe may declare.
pub const MAX_EXPECT_ARG_ENTRIES: usize = 64;
/// How many tools one probe may forbid.
pub const MAX_FORBID_TOOLS: usize = 64;
pub const MIN_REPEAT: u32 = 1;
pub const MAX_REPEAT: u32 = 100;
/// The repeat when the CLI, the probe and the configuration all stay silent.
pub const DEFAULT_REPEAT: u32 = 1;
/// The longest probe name, in bytes: the grammar is ASCII, so bytes and characters
/// agree.
pub const MAX_NAME_BYTES: usize = 64;

/// The five expectation keys, for diagnostics that have to list them.
pub const EXPECTATION_KEYS: &str =
    "`expect_tool`, `expect_args`, `forbid_tools`, `expect_no_tool`, `output_schema`";

/// Whether a name fits `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`.
///
/// The grammar is this narrow because a probe name is a durable identity: it names a
/// probe in the baseline, in a cache key and in a report, and a name that needed
/// quoting or normalizing in any of those places would be a name that means two
/// things.
pub fn is_valid_name(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    name.len() <= MAX_NAME_BYTES
        && first.is_ascii_alphanumeric()
        && characters.all(|character| {
            character.is_ascii_alphanumeric()
                || character == '.'
                || character == '_'
                || character == '-'
        })
}

/// One probe file: an array of `[[probe]]` tables, and nothing else.
///
/// Strict, like the config: an unknown key is a mistake worth failing on, not a
/// feature to ignore.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeFile {
    #[serde(default)]
    pub probe: Vec<ProbeSpec>,
}

/// A probe exactly as TOML wrote it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeSpec {
    pub name: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_args: Option<BTreeMap<String, RawMatcher>>,
    /// `Option` rather than a plain list so that `forbid_tools = []` is
    /// distinguishable from an absent key: one is a probe that asserts nothing,
    /// the other is a probe that forbids nothing, and only one of those is a typo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forbid_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_no_tool: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<String>,
}

/// A matcher as TOML wrote it: three optional operators, exactly one of which must
/// be present.
///
/// Deserializing into `Matcher` directly would let serde pick the first variant it
/// recognized and silently ignore the second operator, which is precisely the mistake
/// the "exactly one" rule exists to catch.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawMatcher {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub one_of: Option<Vec<Value>>,
}

impl RawMatcher {
    /// The single operator this matcher carries, or why it carries no usable one.
    fn operator(self) -> std::result::Result<Matcher, String> {
        let mut operators = Vec::new();
        if let Some(value) = self.equals {
            operators.push(Matcher::Equals(value));
        }
        if let Some(value) = self.contains {
            operators.push(Matcher::Contains(value));
        }
        if let Some(values) = self.one_of {
            if values.is_empty() {
                return Err("`one_of` needs at least one value".to_string());
            }
            operators.push(Matcher::OneOf(values));
        }

        match operators.len() {
            0 => Err(
                "it carries no operator; use exactly one of `equals`, `contains`, or `one_of`"
                    .to_string(),
            ),
            1 => Ok(operators.remove(0)),
            count => Err(format!(
                "it carries {count} operators; use exactly one of `equals`, `contains`, or `one_of`"
            )),
        }
    }
}

impl ProbeSpec {
    /// Validate a declared probe, returning the form resolution works from.
    ///
    /// Every rejection names the probe and its file: a probe file is user input, and
    /// "invalid probe" without the name sends the reader looking through the suite.
    pub fn validate(&self, path: &Path) -> Result<ValidProbe> {
        let invalid = |reason: String| Error::ProbeInvalid {
            name: self.name.clone(),
            path: path.to_path_buf(),
            reason,
        };

        if !is_valid_name(&self.name) {
            return Err(invalid(format!(
                "the name must match `[A-Za-z0-9][A-Za-z0-9._-]{{0,63}}`, so it is at most \
                 {MAX_NAME_BYTES} characters and starts with a letter or a digit"
            )));
        }

        if self.prompt.trim().is_empty() {
            return Err(invalid(
                "the prompt is empty or only whitespace".to_string(),
            ));
        }
        if self.prompt.len() > MAX_PROMPT_BYTES {
            return Err(invalid(format!(
                "the prompt is {} bytes, more than the maximum of {MAX_PROMPT_BYTES}",
                self.prompt.len()
            )));
        }

        if let Some(repeat) = self.repeat
            && !(MIN_REPEAT..=MAX_REPEAT).contains(&repeat)
        {
            return Err(invalid(repeat_problem(repeat)));
        }

        if self.expect_no_tool == Some(false) {
            return Err(invalid(
                "`expect_no_tool = false` asserts nothing; remove the key, and declare what should \
                 happen instead"
                    .to_string(),
            ));
        }
        let expect_no_tool = self.expect_no_tool.unwrap_or(false);
        if expect_no_tool {
            let conflicts: Vec<&str> = [
                self.expect_tool.is_some().then_some("`expect_tool`"),
                self.expect_args.is_some().then_some("`expect_args`"),
                self.forbid_tools.is_some().then_some("`forbid_tools`"),
            ]
            .into_iter()
            .flatten()
            .collect();
            if !conflicts.is_empty() {
                return Err(invalid(format!(
                    "`expect_no_tool` says no tool may be called, so it cannot be combined with {}",
                    conflicts.join(", ")
                )));
            }
        }

        let expect_args = self.argument_matchers(&invalid)?;
        let forbid_tools = self.forbidden_tools(&invalid)?;

        if self.expect_tool.is_none()
            && expect_args.is_empty()
            && forbid_tools.is_empty()
            && !expect_no_tool
            && self.output_schema.is_none()
        {
            return Err(invalid(format!(
                "it declares no expectation; add at least one of {EXPECTATION_KEYS}"
            )));
        }

        let output_schema = match &self.output_schema {
            None => None,
            Some(declared) => Some(normalize_rel_path(declared).map_err(|_| {
                invalid(format!(
                    "`output_schema` (`{declared}`) must be a project-relative path: not absolute, \
                     with no `..` and no backslash"
                ))
            })?),
        };

        Ok(ValidProbe {
            name: self.name.clone(),
            prompt: self.prompt.clone(),
            repeat: self.repeat,
            expect_tool: self.expect_tool.clone(),
            expect_args,
            forbid_tools,
            expect_no_tool,
            output_schema,
        })
    }

    /// The declared matchers, keyed by canonical JSON Pointer.
    fn argument_matchers(&self, invalid: &impl Fn(String) -> Error) -> Result<Matchers> {
        let Some(declared) = &self.expect_args else {
            return Ok(Matchers::new());
        };

        if self.expect_tool.is_none() {
            return Err(invalid(
                "`expect_args` matches the arguments of the expected tool, so it needs \
                 `expect_tool` to say which tool"
                    .to_string(),
            ));
        }
        if declared.len() > MAX_EXPECT_ARG_ENTRIES {
            return Err(invalid(format!(
                "`expect_args` declares {} entries, more than the maximum of {MAX_EXPECT_ARG_ENTRIES}",
                declared.len()
            )));
        }

        let mut matchers = Matchers::new();
        for (key, raw) in declared {
            let Some(pointer) = matchers::canonical_pointer(key) else {
                return Err(invalid(format!(
                    "`{key}` is not a usable argument path: use an RFC 6901 JSON Pointer such as \
                     `/options/per_page`, or one top-level key such as `query`"
                )));
            };
            let matcher = raw.clone().operator().map_err(|reason| {
                invalid(format!("the matcher for `{key}` is not usable: {reason}"))
            })?;
            // `query` and `/query` are one path. Accepting both would silently drop
            // one of two declared expectations, which is the kind of quiet loss the
            // rest of this contract exists to prevent.
            if matchers.insert(pointer.clone(), matcher).is_some() {
                return Err(invalid(format!(
                    "`{key}` names the same argument path as another matcher (`{pointer}`)"
                )));
            }
        }
        Ok(matchers)
    }

    /// The forbidden tool references, refusing a declared-but-empty list.
    fn forbidden_tools(&self, invalid: &impl Fn(String) -> Error) -> Result<Vec<String>> {
        match &self.forbid_tools {
            None => Ok(Vec::new()),
            Some(tools) if tools.is_empty() => Err(invalid(
                "`forbid_tools` is empty; remove the key, or list at least one tool".to_string(),
            )),
            Some(tools) if tools.len() > MAX_FORBID_TOOLS => Err(invalid(format!(
                "`forbid_tools` lists {} tools, more than the maximum of {MAX_FORBID_TOOLS}",
                tools.len()
            ))),
            Some(tools) => Ok(tools.clone()),
        }
    }
}

/// Why a repeat is not usable, phrased for both the probe and the configuration.
pub fn repeat_problem(repeat: u32) -> String {
    format!("the repeat must be between {MIN_REPEAT} and {MAX_REPEAT}, but is {repeat}")
}

/// A validated probe whose tool references are still *references*.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidProbe {
    pub name: String,
    pub prompt: String,
    pub repeat: Option<u32>,
    pub expect_tool: Option<String>,
    /// Keyed by canonical JSON Pointer, so `query` and `/query` are one expectation.
    pub expect_args: Matchers,
    pub forbid_tools: Vec<String>,
    pub expect_no_tool: bool,
    /// Project-relative, normalized.
    pub output_schema: Option<String>,
}

/// A loaded, self-contained JSON Schema.
///
/// The path is kept for diagnostics only. Identity is the *content* digest, so moving
/// or reformatting the file does not make every recorded run stale — which is what
/// would happen if the probe digest hashed a path.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputSchema {
    /// Project-relative, as configured.
    pub relative: String,
    pub schema: Value,
    /// The digest of the canonical JSON of `schema`.
    pub digest: Digest,
}

/// The declared expectations, after resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedExpectations {
    pub expect_tool: Option<ResolvedTool>,
    pub forbid_tools: Vec<ResolvedTool>,
    pub output_schema: Option<OutputSchema>,
}

/// A probe ready to be sampled and evaluated.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedProbe {
    /// The file it was read from. Diagnostics only; never part of its identity.
    pub path: PathBuf,
    pub name: String,
    pub prompt: String,
    /// How many samples this run takes: the CLI override, then the probe, then the
    /// configuration, then 1.
    pub repeat: u32,
    /// What the probe itself asks for. Part of its identity; the CLI override is not,
    /// because re-sampling more often does not change what is being asserted.
    pub declared_repeat: u32,
    pub expect_tool: Option<ResolvedTool>,
    pub expect_args: Matchers,
    pub forbid_tools: Vec<ResolvedTool>,
    pub expect_no_tool: bool,
    pub output_schema: Option<OutputSchema>,
    /// The canonical identity of everything above.
    pub digest: Digest,
}

impl ResolvedProbe {
    /// Assemble a probe and seal its identity.
    pub fn new(
        declared: ValidProbe,
        path: PathBuf,
        expectations: ResolvedExpectations,
        repeat: u32,
        declared_repeat: u32,
    ) -> Result<Self> {
        let digest = ProbeIdentity {
            name: &declared.name,
            prompt: &declared.prompt,
            repeat: declared_repeat,
            expect_tool: expectations.expect_tool.as_ref(),
            expect_args: &declared.expect_args,
            forbid_tools: &expectations.forbid_tools,
            expect_no_tool: declared.expect_no_tool,
            output_schema: expectations.output_schema.as_ref(),
        }
        .digest()?;

        Ok(Self {
            path,
            name: declared.name,
            prompt: declared.prompt,
            repeat,
            declared_repeat,
            expect_tool: expectations.expect_tool,
            expect_args: declared.expect_args,
            forbid_tools: expectations.forbid_tools,
            expect_no_tool: declared.expect_no_tool,
            output_schema: expectations.output_schema,
            digest,
        })
    }

    /// The metrics this probe feeds: the declared ones, plus `argument_validity`,
    /// which is automatic and applies to any sample that called a tool.
    pub fn metrics(&self) -> Vec<Metric> {
        Metric::ALL
            .into_iter()
            .filter(|metric| match metric {
                Metric::ToolSelection => self.expect_tool.is_some(),
                Metric::ArgumentValidity => true,
                Metric::ArgumentExpectation => !self.expect_args.is_empty(),
                Metric::ForbiddenToolUsage => !self.forbid_tools.is_empty(),
                Metric::ToolRestraint => self.expect_no_tool,
                Metric::StructuredOutputValidity => self.output_schema.is_some(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(text: &str) -> ProbeSpec {
        toml::from_str::<ProbeFile>(text).unwrap().probe.remove(0)
    }

    fn probe(name: &str, expectations: &str) -> ProbeSpec {
        spec(&format!(
            "[[probe]]\nname = \"{name}\"\nprompt = \"Ask something.\"\n{expectations}"
        ))
    }

    fn reject(spec: &ProbeSpec) -> String {
        match spec.validate(Path::new("probes/a.toml")) {
            Ok(_) => panic!("`{}` should not validate", spec.name),
            Err(error) => match error {
                Error::ProbeInvalid { name, reason, .. } => {
                    assert_eq!(name, spec.name, "the rejection must name the probe");
                    reason
                }
                other => panic!("expected ProbeInvalid, got {other:?}"),
            },
        }
    }

    #[test]
    fn the_name_grammar_is_letters_digits_and_three_separators() {
        for name in [
            "a",
            "A0",
            "repository-search",
            "structured_output.v1",
            "9lives",
            &"x".repeat(MAX_NAME_BYTES),
        ] {
            assert!(is_valid_name(name), "{name:?} should be accepted");
        }
        for name in [
            "",
            "-leading",
            ".leading",
            "_leading",
            "has space",
            "has/slash",
            "has:colon",
            "ünïcode",
            &"x".repeat(MAX_NAME_BYTES + 1),
        ] {
            assert!(!is_valid_name(name), "{name:?} should be refused");
        }
    }

    #[test]
    fn a_complete_probe_validates_into_its_resolved_shape() {
        let declared = probe(
            "repository-search",
            "repeat = 3\nexpect_tool = \"search_repos\"\n\
             expect_args = { query = { contains = \"postgres\" }, \"/per_page\" = { equals = 50 } }\n\
             forbid_tools = [\"delete_file\"]\noutput_schema = \"schemas/result.json\"\n",
        );
        let valid = declared.validate(Path::new("probes/a.toml")).unwrap();

        assert_eq!(valid.name, "repository-search");
        assert_eq!(valid.prompt, "Ask something.");
        assert_eq!(valid.repeat, Some(3));
        assert_eq!(valid.expect_tool.as_deref(), Some("search_repos"));
        assert_eq!(valid.forbid_tools, vec!["delete_file".to_string()]);
        assert!(!valid.expect_no_tool);
        assert_eq!(valid.output_schema.as_deref(), Some("schemas/result.json"));
        // The shorthand key is stored as the pointer it means.
        assert_eq!(
            valid.expect_args.keys().collect::<Vec<_>>(),
            vec!["/per_page", "/query"]
        );
        assert_eq!(
            valid.expect_args["/query"],
            Matcher::Contains(json!("postgres"))
        );
    }

    #[test]
    fn every_validation_rejection_names_the_probe_and_says_what_to_do() {
        let cases: Vec<(&str, &str)> = vec![
            (
                "bad name",
                "name = \"Bad!Name\"\nprompt = \"p\"\nexpect_no_tool = true",
            ),
            (
                "empty prompt",
                "name = \"a\"\nprompt = \"\"\nexpect_no_tool = true",
            ),
            (
                "whitespace prompt",
                "name = \"a\"\nprompt = \"   \"\nexpect_no_tool = true",
            ),
            (
                "zero repeat",
                "name = \"a\"\nprompt = \"p\"\nrepeat = 0\nexpect_no_tool = true",
            ),
            (
                "oversized repeat",
                "name = \"a\"\nprompt = \"p\"\nrepeat = 101\nexpect_no_tool = true",
            ),
            ("no expectations", "name = \"a\"\nprompt = \"p\""),
            (
                "expect_args without expect_tool",
                "name = \"a\"\nprompt = \"p\"\nexpect_args = { query = { equals = 1 } }",
            ),
            (
                "expect_no_tool with expect_tool",
                "name = \"a\"\nprompt = \"p\"\nexpect_no_tool = true\nexpect_tool = \"search_repos\"",
            ),
            (
                "expect_no_tool with expect_args",
                "name = \"a\"\nprompt = \"p\"\nexpect_no_tool = true\n\
                 expect_args = { query = { equals = 1 } }",
            ),
            (
                "expect_no_tool with forbid_tools",
                "name = \"a\"\nprompt = \"p\"\nexpect_no_tool = true\nforbid_tools = [\"delete_file\"]",
            ),
            (
                "false restraint",
                "name = \"a\"\nprompt = \"p\"\nexpect_no_tool = false",
            ),
            (
                "two operators",
                "name = \"a\"\nprompt = \"p\"\nexpect_tool = \"t\"\n\
                 expect_args = { q = { equals = 1, contains = 1 } }",
            ),
            (
                "no operator",
                "name = \"a\"\nprompt = \"p\"\nexpect_tool = \"t\"\nexpect_args = { q = { } }",
            ),
            (
                "empty one_of",
                "name = \"a\"\nprompt = \"p\"\nexpect_tool = \"t\"\n\
                 expect_args = { q = { one_of = [] } }",
            ),
            (
                "malformed pointer",
                "name = \"a\"\nprompt = \"p\"\nexpect_tool = \"t\"\n\
                 expect_args = { \"a/b\" = { equals = 1 } }",
            ),
            (
                "empty forbid list",
                "name = \"a\"\nprompt = \"p\"\nforbid_tools = []",
            ),
            (
                "absolute schema path",
                "name = \"a\"\nprompt = \"p\"\noutput_schema = \"/etc/schema.json\"",
            ),
            (
                "parent schema path",
                "name = \"a\"\nprompt = \"p\"\noutput_schema = \"../schema.json\"",
            ),
        ];

        for (label, body) in cases {
            let declared = spec(&format!("[[probe]]\n{body}\n"));
            let reason = reject(&declared);
            assert!(!reason.is_empty(), "{label} produced an empty reason");
        }
    }

    #[test]
    fn a_prompt_that_is_too_long_is_refused_rather_than_truncated() {
        let mut declared = probe("long", "expect_no_tool = true");
        declared.prompt = "x".repeat(MAX_PROMPT_BYTES + 1);
        assert!(reject(&declared).contains("more than the maximum"));
    }

    #[test]
    fn a_prompt_at_the_limit_is_accepted() {
        let mut declared = probe("long", "expect_no_tool = true");
        declared.prompt = "x".repeat(MAX_PROMPT_BYTES);
        assert!(declared.validate(Path::new("probes/a.toml")).is_ok());
    }

    #[test]
    fn a_repeat_at_either_bound_is_accepted() {
        for repeat in [MIN_REPEAT, MAX_REPEAT] {
            let declared = probe(
                "bounded",
                &format!("repeat = {repeat}\nexpect_no_tool = true"),
            );
            assert_eq!(
                declared
                    .validate(Path::new("probes/a.toml"))
                    .unwrap()
                    .repeat,
                Some(repeat)
            );
        }
    }

    #[test]
    fn a_declared_but_empty_forbid_list_is_refused_while_an_absent_one_is_fine() {
        let absent = probe("absent", "expect_tool = \"t\"");
        assert!(
            absent
                .validate(Path::new("probes/a.toml"))
                .unwrap()
                .forbid_tools
                .is_empty()
        );
        // `forbid_tools = []` asserts nothing but reads as though it does.
        assert!(reject(&probe("empty", "forbid_tools = []")).contains("empty"));
    }

    #[test]
    fn two_spellings_of_one_argument_path_are_refused() {
        // `query` and `/query` are the same path, so accepting both would drop one
        // of the two declared expectations without saying so.
        let declared = probe(
            "duplicate-path",
            "expect_tool = \"t\"\nexpect_args = { query = { contains = \"a\" }, \"/query\" = { contains = \"b\" } }",
        );
        let reason = reject(&declared);
        assert!(reason.contains("same argument path"), "{reason}");
    }

    #[test]
    fn too_many_matchers_or_forbidden_tools_are_refused() {
        let mut matchers = String::from("expect_tool = \"t\"\nexpect_args = {");
        for index in 0..=MAX_EXPECT_ARG_ENTRIES {
            matchers.push_str(&format!("q{index} = {{ equals = {index} }},"));
        }
        matchers.push('}');
        assert!(reject(&probe("many-matchers", &matchers)).contains("more than the maximum"));

        let mut forbids = String::from("forbid_tools = [");
        for index in 0..=MAX_FORBID_TOOLS {
            forbids.push_str(&format!("\"tool{index}\","));
        }
        forbids.push(']');
        assert!(reject(&probe("many-forbids", &forbids)).contains("more than the maximum"));
    }

    #[test]
    fn an_unknown_key_is_a_parse_error_not_a_probe_with_a_silent_extra() {
        let error = toml::from_str::<ProbeFile>(
            "[[probe]]\nname = \"a\"\nprompt = \"p\"\nexpect_toll = \"typo\"\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("expect_toll"), "{error}");
    }

    #[test]
    fn the_declared_expectations_reject_an_unknown_matcher_key() {
        let error = toml::from_str::<ProbeFile>(
            "[[probe]]\nname = \"a\"\nprompt = \"p\"\nexpect_tool = \"t\"\n\
             expect_args = { q = { eqals = 1 } }\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("eqals"), "{error}");
    }

    fn resolved(declared: ValidProbe, expectations: ResolvedExpectations) -> ResolvedProbe {
        ResolvedProbe::new(
            declared,
            PathBuf::from("probes/a.toml"),
            expectations,
            DEFAULT_REPEAT,
            DEFAULT_REPEAT,
        )
        .unwrap()
    }

    #[test]
    fn the_metrics_a_probe_feeds_come_from_its_expectations() {
        let restraint = probe("restraint", "expect_no_tool = true")
            .validate(Path::new("probes/a.toml"))
            .unwrap();
        let restrained = resolved(
            restraint,
            ResolvedExpectations {
                expect_tool: None,
                forbid_tools: Vec::new(),
                output_schema: None,
            },
        );

        assert_eq!(
            restrained.metrics(),
            vec![Metric::ArgumentValidity, Metric::ToolRestraint],
            "argument_validity is automatic, so every probe feeds it"
        );

        // A probe that declares four of the five expectations feeds five metrics.
        let everything = probe(
            "everything",
            "expect_tool = \"search_repos\"\nexpect_args = { query = { equals = 1 } }\n\
             forbid_tools = [\"delete_file\"]\noutput_schema = \"schemas/result.json\"\n",
        )
        .validate(Path::new("probes/a.toml"))
        .unwrap();
        let tool = |id: &str, name: &str| ResolvedTool {
            id: id.to_string(),
            name: name.to_string(),
            input_schema: json!({ "type": "object" }),
        };
        let complete = resolved(
            everything,
            ResolvedExpectations {
                expect_tool: Some(tool("tool:github.search_repositories", "search_repos")),
                forbid_tools: vec![tool("tool:files.delete_file", "delete_file")],
                output_schema: Some(OutputSchema {
                    relative: "schemas/result.json".to_string(),
                    schema: json!({ "type": "object" }),
                    digest: Digest::sha256(b"schema"),
                }),
            },
        );

        assert_eq!(
            complete.metrics(),
            vec![
                Metric::ToolSelection,
                Metric::ArgumentValidity,
                Metric::ArgumentExpectation,
                Metric::ForbiddenToolUsage,
                Metric::StructuredOutputValidity,
            ],
            "`expect_no_tool` is absent, so `tool_restraint` is not fed"
        );
    }
}
