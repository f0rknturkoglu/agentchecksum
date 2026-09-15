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

# [[mcp.servers]]
# name = "github"
# transport = "stdio"
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-github"]

[probes]
path = "probes"
"#;

/// Scaffold the config and the probe directory. Returns the config path written.
pub fn run(root: &Path, config_path: &Path, force: bool) -> Result<PathBuf> {
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
        path: probes,
        source,
    })?;

    Ok(config_path.to_path_buf())
}
