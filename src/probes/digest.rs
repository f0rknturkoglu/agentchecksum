// SPDX-License-Identifier: MIT OR Apache-2.0

//! What makes two probes the same probe.
//!
//! A digest decides whether a recorded trace, a cache entry and a baseline still
//! describe the assertion in front of them. So it covers the *meaning* — the prompt,
//! the declared repeat, the canonical tool ids, the matchers, and the output schema's
//! content — and nothing else. Paths, file layout, mtimes and the CLI's repeat
//! override are deliberately outside it: moving a probe between files, reformatting a
//! schema, or sampling more often does not change what the probe asserts, and a digest
//! that moved with them would report every such change as a behavioral one.
//!
//! Both digests are order-independent by construction: JCS sorts object members, the
//! matcher map is a `BTreeMap`, and the two lists that could arrive in any order
//! (forbidden tools, the suite's probes) are sorted before hashing.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::error::{Error, Result};
use crate::fingerprint::canonical;
use crate::manifest::Digest;

use super::matchers::Matcher;
use super::model::{OutputSchema, ResolvedProbe};
use super::resolve::ResolvedTool;

/// Everything a probe's identity covers, borrowed from the probe being built.
#[derive(Debug, Clone, Copy)]
pub struct ProbeIdentity<'a> {
    pub name: &'a str,
    pub prompt: &'a str,
    /// The *declared* repeat: the byte-identical probe sampled ten times instead of
    /// once is still the same assertion.
    pub repeat: u32,
    pub expect_tool: Option<&'a ResolvedTool>,
    pub expect_args: &'a BTreeMap<String, Matcher>,
    pub forbid_tools: &'a [ResolvedTool],
    pub expect_no_tool: bool,
    pub output_schema: Option<&'a OutputSchema>,
}

impl ProbeIdentity<'_> {
    /// The probe's canonical digest.
    pub fn digest(&self) -> Result<Digest> {
        let mut matchers = Map::new();
        for (pointer, matcher) in self.expect_args {
            matchers.insert(
                pointer.clone(),
                serde_json::to_value(matcher).map_err(|source| Error::Json { source })?,
            );
        }

        // A forbidden tool is forbidden once; the order it was listed in, and any
        // repetition, are spelling rather than meaning.
        let mut forbidden: Vec<&str> = self
            .forbid_tools
            .iter()
            .map(|tool| tool.id.as_str())
            .collect();
        forbidden.sort_unstable();
        forbidden.dedup();

        let document = json!({
            "name": self.name,
            "prompt": self.prompt,
            "repeat": self.repeat,
            // The canonical id, not the reference the file happened to spell: two
            // files naming one tool two ways assert the same thing.
            "expect_tool": self.expect_tool.map(|tool| tool.id.as_str()),
            "expect_args": Value::Object(matchers),
            "forbid_tools": forbidden,
            "expect_no_tool": self.expect_no_tool,
            // The schema's content, not its path.
            "output_schema": self.output_schema.map(|schema| schema.digest.as_str()),
        });

        Ok(Digest::sha256(&canonical::to_vec(&document)?))
    }
}

/// The digest of a whole suite: every probe's identity, in name order.
///
/// This is what a baseline records and what a comparison checks first, because a
/// changed suite means the recorded scores describe a different test.
pub fn suite_digest(probes: &[ResolvedProbe]) -> Result<Digest> {
    let mut entries: Vec<(&str, &Digest)> = probes
        .iter()
        .map(|probe| (probe.name.as_str(), &probe.digest))
        .collect();
    entries.sort_unstable_by(|left, right| left.0.cmp(right.0));

    let document = Value::Array(
        entries
            .into_iter()
            .map(|(name, digest)| json!({ "name": name, "digest": digest.as_str() }))
            .collect(),
    );

    Ok(Digest::sha256(&canonical::to_vec(&document)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::probes::model::{ProbeFile, ResolvedExpectations, ValidProbe};

    fn tool(id: &str, name: &str) -> ResolvedTool {
        ResolvedTool {
            id: id.to_string(),
            name: name.to_string(),
            input_schema: json!({ "type": "object" }),
        }
    }

    fn schema(relative: &str, body: &Value) -> OutputSchema {
        OutputSchema {
            relative: relative.to_string(),
            digest: Digest::sha256(&canonical::to_vec(body).unwrap()),
            schema: body.clone(),
        }
    }

    fn declared(name: &str, prompt: &str, matchers: &[(&str, Matcher)]) -> ValidProbe {
        ValidProbe {
            name: name.to_string(),
            prompt: prompt.to_string(),
            repeat: None,
            expect_tool: Some("tool:github.search_repositories".to_string()),
            expect_args: matchers
                .iter()
                .map(|(pointer, matcher)| (pointer.to_string(), matcher.clone()))
                .collect(),
            forbid_tools: Vec::new(),
            expect_no_tool: false,
            output_schema: None,
        }
    }

    fn probe(
        declared: ValidProbe,
        expectations: ResolvedExpectations,
        repeat: u32,
        declared_repeat: u32,
    ) -> ResolvedProbe {
        ResolvedProbe::new(
            declared,
            PathBuf::from("probes/a.toml"),
            expectations,
            repeat,
            declared_repeat,
        )
        .unwrap()
    }

    fn search_probe() -> ResolvedProbe {
        probe(
            declared(
                "repository-search",
                "Find repositories.",
                &[("/query", Matcher::Contains(json!("postgres")))],
            ),
            ResolvedExpectations {
                expect_tool: Some(tool(
                    "tool:github.search_repositories",
                    "search_repositories",
                )),
                forbid_tools: Vec::new(),
                output_schema: None,
            },
            3,
            3,
        )
    }

    #[test]
    fn a_digest_is_stable_for_the_same_probe() {
        assert_eq!(
            search_probe().digest,
            search_probe().digest,
            "two loads of one probe must be one identity"
        );
    }

    #[test]
    fn the_cli_repeat_override_is_not_part_of_the_identity() {
        let base = search_probe();
        let overridden = probe(
            declared(
                "repository-search",
                "Find repositories.",
                &[("/query", Matcher::Contains(json!("postgres")))],
            ),
            ResolvedExpectations {
                expect_tool: Some(tool(
                    "tool:github.search_repositories",
                    "search_repositories",
                )),
                forbid_tools: Vec::new(),
                output_schema: None,
            },
            9,
            3,
        );

        assert_eq!(overridden.repeat, 9);
        assert_eq!(
            overridden.digest, base.digest,
            "sampling more often does not change what is asserted"
        );
    }

    #[test]
    fn the_declared_repeat_is_part_of_the_identity() {
        let one = probe(
            declared("p", "Ask.", &[]),
            ResolvedExpectations {
                expect_tool: Some(tool(
                    "tool:github.search_repositories",
                    "search_repositories",
                )),
                forbid_tools: Vec::new(),
                output_schema: None,
            },
            1,
            1,
        );
        let three = probe(
            declared("p", "Ask.", &[]),
            ResolvedExpectations {
                expect_tool: Some(tool(
                    "tool:github.search_repositories",
                    "search_repositories",
                )),
                forbid_tools: Vec::new(),
                output_schema: None,
            },
            3,
            3,
        );

        assert_ne!(one.digest, three.digest);
    }

    #[test]
    fn every_asserted_part_moves_the_digest() {
        let base = search_probe();

        let other_prompt = probe(
            declared(
                "repository-search",
                "Find repositories differently.",
                &[("/query", Matcher::Contains(json!("postgres")))],
            ),
            ResolvedExpectations {
                expect_tool: Some(tool(
                    "tool:github.search_repositories",
                    "search_repositories",
                )),
                forbid_tools: Vec::new(),
                output_schema: None,
            },
            3,
            3,
        );
        assert_ne!(base.digest, other_prompt.digest);

        let other_matcher = probe(
            declared(
                "repository-search",
                "Find repositories.",
                &[("/query", Matcher::Contains(json!("mysql")))],
            ),
            ResolvedExpectations {
                expect_tool: Some(tool(
                    "tool:github.search_repositories",
                    "search_repositories",
                )),
                forbid_tools: Vec::new(),
                output_schema: None,
            },
            3,
            3,
        );
        assert_ne!(base.digest, other_matcher.digest);

        let other_tool = probe(
            declared(
                "repository-search",
                "Find repositories.",
                &[("/query", Matcher::Contains(json!("postgres")))],
            ),
            ResolvedExpectations {
                expect_tool: Some(tool(
                    "tool:gitlab.search_repositories",
                    "search_repositories",
                )),
                forbid_tools: Vec::new(),
                output_schema: None,
            },
            3,
            3,
        );
        assert_ne!(
            base.digest, other_tool.digest,
            "two servers' tools are different tools"
        );

        let forbidden = probe(
            {
                let mut declared = declared(
                    "repository-search",
                    "Find repositories.",
                    &[("/query", Matcher::Contains(json!("postgres")))],
                );
                declared.forbid_tools = vec!["delete_file".to_string()];
                declared
            },
            ResolvedExpectations {
                expect_tool: Some(tool(
                    "tool:github.search_repositories",
                    "search_repositories",
                )),
                forbid_tools: vec![tool("tool:files.delete_file", "delete_file")],
                output_schema: None,
            },
            3,
            3,
        );
        assert_ne!(base.digest, forbidden.digest);
    }

    #[test]
    fn a_shorthand_key_and_its_pointer_are_one_identity() {
        let shorthand = probe(
            declared("p", "Ask.", &[("/query", Matcher::Equals(json!("x")))]),
            ResolvedExpectations {
                expect_tool: Some(tool(
                    "tool:github.search_repositories",
                    "search_repositories",
                )),
                forbid_tools: Vec::new(),
                output_schema: None,
            },
            1,
            1,
        );
        // `model::validate` canonicalizes `query` to `/query`, so resolution is where
        // the two spellings converge; assert the convergence itself.
        let validated = toml::from_str::<ProbeFile>(
            "[[probe]]\nname = \"p\"\nprompt = \"Ask.\"\n\
             expect_tool = \"search_repositories\"\nexpect_args = { query = { equals = \"x\" } }\n",
        )
        .unwrap()
        .probe
        .remove(0)
        .validate(std::path::Path::new("probes/a.toml"))
        .unwrap();
        let from_toml = probe(
            validated,
            ResolvedExpectations {
                expect_tool: Some(tool(
                    "tool:github.search_repositories",
                    "search_repositories",
                )),
                forbid_tools: Vec::new(),
                output_schema: None,
            },
            1,
            1,
        );

        assert_eq!(shorthand.digest, from_toml.digest);
    }

    #[test]
    fn forbidden_tools_are_hashed_as_a_set() {
        let forbidden = |order: [&str; 3]| {
            let mut declared = declared("p", "Ask.", &[]);
            declared.expect_tool = None;
            declared.forbid_tools = order.iter().map(|id| (*id).to_string()).collect();
            probe(
                declared,
                ResolvedExpectations {
                    expect_tool: None,
                    forbid_tools: order
                        .iter()
                        .map(|id| tool(&format!("tool:files.{id}"), id))
                        .collect(),
                    output_schema: None,
                },
                1,
                1,
            )
        };

        assert_eq!(
            forbidden(["a", "b", "c"]).digest,
            forbidden(["c", "a", "b"]).digest,
            "order is spelling, not meaning"
        );
        assert_eq!(
            forbidden(["a", "a", "b"]).digest,
            forbidden(["a", "b", "b"]).digest,
            "the same tool forbidden twice is the same assertion"
        );
    }

    #[test]
    fn the_output_schema_contributes_its_content_and_not_its_path() {
        let with_schema = |relative: &str, body: &Value| {
            probe(
                declared("p", "Ask.", &[]),
                ResolvedExpectations {
                    expect_tool: None,
                    forbid_tools: Vec::new(),
                    output_schema: Some(schema(relative, body)),
                },
                1,
                1,
            )
        };

        let original = with_schema("schemas/result.json", &json!({ "type": "object" }));
        assert_eq!(
            original.digest,
            with_schema("other/place.json", &json!({ "type": "object" })).digest,
            "the same schema in another file asserts the same thing"
        );
        assert_ne!(
            original.digest,
            with_schema("schemas/result.json", &json!({ "type": "string" })).digest
        );
    }

    #[test]
    fn the_probe_file_path_is_not_part_of_the_identity() {
        let declared = declared("p", "Ask.", &[]);
        let expectations = || ResolvedExpectations {
            expect_tool: None,
            forbid_tools: Vec::new(),
            output_schema: None,
        };
        let here = ResolvedProbe::new(
            declared.clone(),
            PathBuf::from("probes/a.toml"),
            expectations(),
            1,
            1,
        )
        .unwrap();
        let moved = ResolvedProbe::new(
            declared,
            PathBuf::from("probes/nested/b.toml"),
            expectations(),
            1,
            1,
        )
        .unwrap();

        assert_ne!(here.path, moved.path);
        assert_eq!(
            here.digest, moved.digest,
            "moving a probe between files is not a behavioral change"
        );
    }

    #[test]
    fn the_suite_digest_is_order_independent_and_covers_every_probe() {
        let first = search_probe();
        let second = probe(
            declared("restraint", "Summarize.", &[]),
            ResolvedExpectations {
                expect_tool: None,
                forbid_tools: Vec::new(),
                output_schema: None,
            },
            1,
            1,
        );

        let forwards = suite_digest(&[first.clone(), second.clone()]).unwrap();
        assert_eq!(
            forwards,
            suite_digest(&[second.clone(), first.clone()]).unwrap(),
            "file order is not part of the identity"
        );
        assert_ne!(
            forwards,
            suite_digest(std::slice::from_ref(&first)).unwrap()
        );
        assert_ne!(
            suite_digest(&[first]).unwrap(),
            suite_digest(&[second]).unwrap()
        );
        assert!(suite_digest(&[]).is_ok(), "an empty suite still digests");
    }
}
