// SPDX-License-Identifier: MIT OR Apache-2.0

//! `agentchecksum.toml`: user-owned configuration.
//!
//! Parsing is strict. A silently ignored typo means AgentChecksum fingerprinted
//! something other than what the user believes they declared.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The config format version this build understands.
pub const SUPPORTED_CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub agent: AgentConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelConfig>,
    #[serde(default)]
    pub prompts: Vec<PromptConfig>,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub probes: ProbesConfig,
    #[serde(default)]
    pub policy: PolicyConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub provider: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub params: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptConfig {
    pub path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: Transport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    Stdio,
    StreamableHttp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbesConfig {
    #[serde(default = "default_probes_path")]
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<u32>,
}

impl Default for ProbesConfig {
    fn default() -> Self {
        Self {
            path: default_probes_path(),
            repeat: None,
        }
    }
}

fn default_probes_path() -> String {
    "probes".to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_on_risk: Option<RiskLevel>,
    #[serde(default)]
    pub metrics: BTreeMap<String, MetricPolicy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    None,
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_drop: Option<f64>,
}

impl Config {
    /// Read and validate a config file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_at(&text, path)
    }

    /// Parse and validate text. Parse errors are reported against `path` so the
    /// diagnostic can name the file without a separate code path.
    pub fn from_toml_at(text: &str, path: &Path) -> Result<Self> {
        let config: Self = toml::from_str(text).map_err(|source| Error::ConfigParse {
            path: path.to_path_buf(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Structural checks that serde cannot express.
    pub fn validate(&self) -> Result<()> {
        if self.version != SUPPORTED_CONFIG_VERSION {
            return Err(Error::ConfigVersion {
                found: self.version,
                supported: SUPPORTED_CONFIG_VERSION,
            });
        }

        let mut prompt_ids: BTreeSet<&str> = BTreeSet::new();
        for prompt in &self.prompts {
            if !prompt_ids.insert(prompt.path.as_str()) {
                return Err(Error::DependencyCollision {
                    id: format!("prompt:{}", prompt.path),
                });
            }
        }

        let mut server_names: BTreeSet<&str> = BTreeSet::new();
        for server in &self.mcp.servers {
            if !server_names.insert(server.name.as_str()) {
                return Err(Error::ConfigInvalid {
                    reason: format!(
                        "MCP server name `{}` is declared more than once",
                        server.name
                    ),
                });
            }
            match server.transport {
                Transport::Stdio if server.command.is_none() => {
                    return Err(Error::ConfigInvalid {
                        reason: format!(
                            "MCP server `{}` uses the stdio transport but declares no `command`",
                            server.name
                        ),
                    });
                }
                Transport::StreamableHttp if server.url.is_none() => {
                    return Err(Error::ConfigInvalid {
                        reason: format!(
                            "MCP server `{}` uses the streamable-http transport but declares no `url`",
                            server.name
                        ),
                    });
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Directory that project-relative paths are resolved against.
    pub fn root_for(config_path: &Path) -> PathBuf {
        config_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config> {
        Config::from_toml_at(text, Path::new("agentchecksum.toml"))
    }

    const MINIMAL: &str = r#"
version = 1

[agent]
name = "research-agent"

[[prompts]]
path = "prompts/system.md"
"#;

    #[test]
    fn a_minimal_config_parses() {
        let config = parse(MINIMAL).unwrap();
        assert_eq!(config.version, 1);
        assert_eq!(config.agent.name, "research-agent");
        assert_eq!(config.prompts.len(), 1);
        assert_eq!(config.probes.path, "probes");
        assert_eq!(config.probes.repeat, None);
    }

    #[test]
    fn a_typo_in_a_table_name_is_an_error_not_a_silent_ignore() {
        let text = MINIMAL.replace("[agent]", "[agentt]");
        assert!(parse(&text).is_err(), "unknown fields must be rejected");
    }

    #[test]
    fn a_typo_in_a_field_name_is_an_error() {
        let text = MINIMAL.replace("name = ", "naem = ");
        assert!(parse(&text).is_err());
    }

    #[test]
    fn an_unsupported_version_is_an_error() {
        let text = MINIMAL.replace("version = 1", "version = 2");
        let err = parse(&text).unwrap_err();
        assert!(
            matches!(
                err,
                Error::ConfigVersion {
                    found: 2,
                    supported: 1
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_missing_version_is_an_error() {
        assert!(parse(&MINIMAL.replace("version = 1", "")).is_err());
    }

    #[test]
    fn the_full_documented_config_surface_parses() {
        let text = format!(
            r#"{MINIMAL}
[model]
provider = "ollama"
id = "qwen3:8b"
endpoint = "http://localhost:11434"
params = {{ temperature = 0.0, seed = 42 }}

[probes]
path = "probes"
repeat = 3

[[mcp.servers]]
name = "github"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[policy]
fail_on_risk = "critical"

[policy.metrics.tool_selection]
min = 0.95
"#
        );
        let config = parse(&text).unwrap();
        let model = config.model.as_ref().unwrap();
        assert_eq!(model.id, "qwen3:8b");
        assert_eq!(model.params.len(), 2);
        assert_eq!(config.probes.repeat, Some(3));
        assert_eq!(config.mcp.servers[0].name, "github");
        assert_eq!(config.mcp.servers[0].transport, Transport::Stdio);
        assert_eq!(config.policy.fail_on_risk, Some(RiskLevel::Critical));
        assert_eq!(config.policy.metrics["tool_selection"].min, Some(0.95));
    }

    #[test]
    fn a_stdio_server_without_a_command_is_rejected() {
        let text = format!(
            r#"{MINIMAL}

[[mcp.servers]]
name = "github"
transport = "stdio"
"#
        );
        let err = parse(&text).unwrap_err();
        assert!(matches!(err, Error::ConfigInvalid { .. }), "{err:?}");
    }

    #[test]
    fn an_http_server_without_a_url_is_rejected() {
        let text = format!(
            r#"{MINIMAL}

[[mcp.servers]]
name = "remote"
transport = "streamable-http"
"#
        );
        let err = parse(&text).unwrap_err();
        assert!(matches!(err, Error::ConfigInvalid { .. }), "{err:?}");
    }

    #[test]
    fn duplicate_mcp_server_names_are_rejected() {
        let text = format!(
            r#"{MINIMAL}

[[mcp.servers]]
name = "github"
transport = "stdio"
command = "a"

[[mcp.servers]]
name = "github"
transport = "stdio"
command = "b"
"#
        );
        let err = parse(&text).unwrap_err();
        assert!(matches!(err, Error::ConfigInvalid { .. }), "{err:?}");
    }

    #[test]
    fn duplicate_prompt_paths_are_rejected_because_they_would_collide_in_the_lockfile() {
        let text = format!(
            r#"{MINIMAL}

[[prompts]]
path = "prompts/system.md"
"#
        );
        let err = parse(&text).unwrap_err();
        assert!(matches!(err, Error::DependencyCollision { .. }), "{err:?}");
    }
}
