// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

const TEMPLATE: &str = r#"version = 1

[agent]
name = "my-agent"

# [model]
# provider = "ollama"
# id = "qwen3:8b"
# endpoint = "http://localhost:11434"
# params = { temperature = 0.0, seed = 42 }

[[prompts]]
path = "prompts/system.md"

# MCP servers are discovered by `snapshot` and `diff`. Discovery reads the tool
# catalog; it never calls a tool.
#
# stdio: the command is executed directly, without a shell, so it must be an
# executable and every argument its own array entry. `env` values are passed to
# the server and are never written to the lockfile.
# [[mcp.servers]]
# name = "github"
# transport = "stdio"
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-github"]
# env = { GITHUB_TOKEN = "..." }
#
# streamable-http: the url must be http or https, with no credentials, no query
# string, and no fragment. Configure the exact endpoint; redirects are not
# followed.
# [[mcp.servers]]
# name = "remote"
# transport = "streamable-http"
# url = "https://example.com/mcp"

[probes]
path = "probes"
# How many times each probe is sampled. A probe whose expectations hold in every
# sample is more convincing than one that held once, and repeat = 1 turns a
# measurement into a coin flip.
# repeat = 3

# The Behavior Gate. Every metric runs from 0.0 to 1.0 and, for every one of them,
# 1.0 is good — so `min` is a floor and `max` is a ceiling, whichever metric they
# are attached to. `max_drop` compares against the committed baseline instead of an
# absolute number, and is only applied when the baseline describes the same probe
# suite.
#
# Metrics: tool_selection, argument_validity, argument_expectation,
#          forbidden_tool_usage, tool_restraint, structured_output_validity
#
# `fail_on_risk` turns the static dependency risk into a gate: without it, a
# changed dependency is reported and does not fail the check.
#
# [policy]
# fail_on_risk = "critical"
#
# [policy.metrics.tool_selection]
# min = 0.95
#
# [policy.metrics.argument_validity]
# max_drop = 0.05
#
# [policy.metrics.forbidden_tool_usage]
# max = 0.0
"#;

/// The starter probe, relative to the probe directory.
const STARTER_PROBE_FILE: &str = "no-tools.toml";

/// One valid probe, so a freshly initialized project can run `check`.
///
/// The loader refuses an empty suite, and an empty directory would teach nothing
/// anyway: this file states the grammar in comments and asserts the one expectation
/// that needs no tool catalog to be interesting — that the agent does not reach for a
/// tool when the question does not need one.
const STARTER_PROBE: &str = r#"# An example probe. Edit it, or replace it with probes about your own agent.
#
# A probe asks one question and declares what a correct answer looks like. `check`
# samples your model through `[model]` and scores what came back.
#
# `name` is the durable identity of the assertion — a baseline records it — so renaming
# a probe retires the score kept under the old name.
#
# Every probe carries at least one expectation:
#
#   expect_tool    = "search_repositories"    the named tool must be called
#   expect_args    = { query = { contains = "postgres" } }   requires `expect_tool`;
#                                              a JSON Pointer to a matcher, where a
#                                              matcher is exactly one of `equals`,
#                                              `contains`, `one_of`
#   forbid_tools   = ["delete_file"]          none of these may be called
#   expect_no_tool = true                     no tool may be called at all
#   output_schema  = "schemas/answer.json"    the final message must be JSON that
#                                              validates against this schema
#
# `repeat` is the number of samples. An expectation that held once is a coin flip; one
# that held in five of five is a measurement. The default is 1.

[[probe]]
name = "no-tools"
prompt = """
Answer from what you already know, without calling any tool: what is the capital of
Portugal?
"""
expect_no_tool = true
"#;

/// What `init` wrote, so the caller can report it without re-deriving paths.
#[derive(Debug, Clone, PartialEq)]
pub struct InitOutcome {
    /// The config path exactly as the user named it.
    pub config: PathBuf,
    /// The probe directory, resolved against the config's directory.
    pub probes: PathBuf,
    /// The starter probe this run wrote, when it wrote one.
    ///
    /// An existing `probes/no-tools.toml` is left alone: `--force` overwrites the
    /// configuration, and it must not overwrite a probe somebody has edited.
    pub starter: Option<PathBuf>,
}

/// Scaffold the config, the probe directory, and a starter probe.
pub fn run(root: &Path, config_path: &Path, force: bool) -> Result<InitOutcome> {
    if config_path.exists() && !force {
        return Err(Error::AlreadyExists {
            path: config_path.to_path_buf(),
        });
    }

    std::fs::write(config_path, TEMPLATE).map_err(|source| Error::Write {
        path: config_path.to_path_buf(),
        source,
    })?;

    let probes = root.join("probes");
    std::fs::create_dir_all(&probes).map_err(|source| Error::Write {
        path: probes.clone(),
        source,
    })?;

    let starter = probes.join(STARTER_PROBE_FILE);
    let starter = if starter.exists() {
        None
    } else {
        std::fs::write(&starter, STARTER_PROBE).map_err(|source| Error::Write {
            path: starter.clone(),
            source,
        })?;
        Some(starter)
    };

    Ok(InitOutcome {
        config: config_path.to_path_buf(),
        probes,
        starter,
    })
}
