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
"#;

/// What `init` wrote, so the caller can report it without re-deriving paths.
#[derive(Debug, Clone, PartialEq)]
pub struct InitOutcome {
    /// The config path exactly as the user named it.
    pub config: PathBuf,
    /// The probe directory, resolved against the config's directory.
    pub probes: PathBuf,
}

/// Scaffold the config and the probe directory.
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

    Ok(InitOutcome {
        config: config_path.to_path_buf(),
        probes,
    })
}
