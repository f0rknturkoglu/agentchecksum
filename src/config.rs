// SPDX-License-Identifier: MIT OR Apache-2.0

//! `agentchecksum.toml`: user-owned configuration.
//!
//! Parsing is strict. A silently ignored typo means AgentChecksum fingerprinted
//! something other than what the user believes they declared.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

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

/// The longest an MCP server alias may be. The alias prefixes every dependency id
/// that server produces, so this bounds the ids as well as the name.
pub const MAX_ALIAS_BYTES: usize = 64;

/// Whether a configured MCP server alias is one this build will use.
///
/// An alias is a namespace, not a display name. It becomes the prefix of every
/// dependency id the server produces, so it has to be stable, portable, and — the
/// load-bearing part — unable to contain the tool separator. `tool:<alias>.<tool>`
/// splits on the first dot, so an alias containing one would let two different
/// servers produce the same tool identity.
pub fn is_valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= MAX_ALIAS_BYTES
        && alias.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}

/// Why an MCP endpoint URL is not one this build will use.
///
/// Returns the component that is wrong, never the URL itself: an endpoint is the one
/// configuration value that can carry a credential, and a diagnostic is the wrong
/// place to repeat one. Every unsupported component is *rejected* rather than
/// stripped — a query string can select a different deployment behind the same host,
/// so removing it would fingerprint a server the user did not configure.
fn endpoint_problem(url: &str) -> Option<&'static str> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Some("it is not an absolute URL with a scheme");
    };
    if scheme != "http" && scheme != "https" {
        return Some("only `http` and `https` endpoints are supported");
    }

    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Some("it names no host");
    }
    if authority.contains('@') {
        return Some("it contains embedded credentials, which are not supported");
    }
    if url.contains('?') {
        return Some(
            "it contains a query string, which can select a different deployment behind the same \
             host; configure the exact endpoint instead",
        );
    }
    if url.contains('#') {
        return Some("it contains a fragment");
    }
    None
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
        let config: Self = match toml::from_str(text) {
            Ok(config) => config,
            Err(source) => {
                // A newer config that also carries keys this build does not know
                // fails as an unknown field, which hides the real problem. Re-read
                // only the version and report that instead.
                #[derive(Deserialize)]
                struct VersionProbe {
                    version: u32,
                }

                if let Ok(probe) = toml::from_str::<VersionProbe>(text)
                    && probe.version != SUPPORTED_CONFIG_VERSION
                {
                    return Err(Error::ConfigVersion {
                        found: probe.version,
                        supported: SUPPORTED_CONFIG_VERSION,
                    });
                }

                return Err(Error::ConfigParse {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
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

        let mut prompt_ids: BTreeSet<String> = BTreeSet::new();
        for prompt in &self.prompts {
            // Keyed on the normalized path, because that is what becomes the
            // dependency id: two declarations differing only in spelling
            // (`prompts/a.md` and `./prompts/a.md`) would otherwise both pass and
            // the lockfile would silently keep just one.
            let normalized = normalize_rel_path(&prompt.path)?;
            if !prompt_ids.insert(normalized.clone()) {
                return Err(Error::DependencyCollision {
                    id: format!("prompt:{normalized}"),
                });
            }
        }

        let mut server_names: BTreeSet<&str> = BTreeSet::new();
        for server in &self.mcp.servers {
            // The grammar is checked before the collision set, so a name that could
            // not be an identity is reported as such rather than as a duplicate.
            if !is_valid_alias(&server.name) {
                return Err(Error::ConfigInvalid {
                    reason: format!(
                        "MCP server name `{}` is not a usable alias: use letters, digits, `-` and \
                         `_`, at most {MAX_ALIAS_BYTES} characters, and no `.` (the alias prefixes \
                         every dependency id the server produces, and a dot would make two \
                         different servers able to claim one tool identity)",
                        server.name
                    ),
                });
            }
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
                Transport::Stdio if server.url.is_some() => {
                    return Err(Error::ConfigInvalid {
                        reason: format!(
                            "MCP server `{}` uses the stdio transport but also declares `url`",
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
                // The URL itself is never repeated in the reason: an endpoint is the
                // one configuration value that can carry a credential.
                Transport::StreamableHttp
                    if endpoint_problem(server.url.as_deref().unwrap_or_default()).is_some() =>
                {
                    let problem = endpoint_problem(server.url.as_deref().unwrap_or_default())
                        .unwrap_or("it is not usable");
                    return Err(Error::ConfigInvalid {
                        reason: format!(
                            "MCP server `{}` has an unusable `url`: {problem}",
                            server.name
                        ),
                    });
                }
                Transport::StreamableHttp if server.command.is_some() => {
                    return Err(Error::ConfigInvalid {
                        reason: format!(
                            "MCP server `{}` uses the streamable-http transport but also declares `command`",
                            server.name
                        ),
                    });
                }
                Transport::StreamableHttp if !server.args.is_empty() => {
                    return Err(Error::ConfigInvalid {
                        reason: format!(
                            "MCP server `{}` uses the streamable-http transport but also declares `args`",
                            server.name
                        ),
                    });
                }
                Transport::StreamableHttp if !server.env.is_empty() => {
                    return Err(Error::ConfigInvalid {
                        reason: format!(
                            "MCP server `{}` uses the streamable-http transport but also declares `env`",
                            server.name
                        ),
                    });
                }
                _ => {}
            }
        }

        self.validate_policy()?;

        Ok(())
    }

    /// A policy that cannot be applied is a configuration error, not a runtime
    /// surprise: a typo in a metric name would otherwise silently gate nothing.
    fn validate_policy(&self) -> Result<()> {
        for (name, policy) in &self.policy.metrics {
            // The key is a metric name, matched by the same spelling the report and
            // the JSON output use.
            let metric =
                crate::probes::Metric::known(name).ok_or_else(|| Error::PolicyMetricUnknown {
                    name: name.clone(),
                    known: crate::probes::Metric::ALL
                        .iter()
                        .map(|metric| metric.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                })?;

            for (field, value) in [
                ("min", policy.min),
                ("max", policy.max),
                ("max_drop", policy.max_drop),
            ] {
                let Some(value) = value else { continue };
                // `nan` and `inf` are valid TOML floats and neither is a usable
                // threshold: every comparison against them is false, so a policy
                // carrying one would silently never fire.
                if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                    return Err(Error::PolicyRange {
                        metric: metric.as_str().to_string(),
                        detail: format!("`{field} = {value}` is outside 0.0..=1.0"),
                    });
                }
            }

            if let (Some(min), Some(max)) = (policy.min, policy.max)
                && min > max
            {
                return Err(Error::PolicyRange {
                    metric: metric.as_str().to_string(),
                    detail: format!(
                        "`min = {min}` is above `max = {max}`, so no score could ever satisfy both"
                    ),
                });
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

/// Normalize a config-declared path into a stable, project-relative,
/// forward-slash identity.
///
/// Absolute paths, `..`, and backslashes are rejected: an absolute path would
/// make the lockfile depend on the machine it was produced on, and a backslash is
/// a path separator on Windows but an ordinary filename character on Unix, so an
/// id containing one would not mean the same thing everywhere.
pub fn normalize_rel_path(path: &str) -> Result<String> {
    if path.contains('\\') {
        return Err(Error::PromptPath {
            path: path.to_string(),
        });
    }

    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err(Error::PromptPath {
            path: path.to_string(),
        });
    }

    let mut parts: Vec<&str> = Vec::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => {
                parts.push(part.to_str().ok_or_else(|| Error::PromptPath {
                    path: path.to_string(),
                })?);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Error::PromptPath {
                    path: path.to_string(),
                });
            }
        }
    }

    if parts.is_empty() {
        return Err(Error::PromptPath {
            path: path.to_string(),
        });
    }

    Ok(parts.join("/"))
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
    fn an_unknown_field_next_to_a_known_one_is_rejected() {
        let text = MINIMAL.replace("[agent]", "[agent]\nnaem = \"typo\"");
        assert!(parse(&text).is_err(), "unknown fields must be rejected");
    }

    #[test]
    fn an_unknown_table_name_is_rejected() {
        let text = format!("{MINIMAL}\n[modell]\nprovider = \"ollama\"\n");
        assert!(parse(&text).is_err(), "unknown fields must be rejected");
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
    fn a_newer_version_alongside_an_unknown_key_is_reported_as_a_version_problem() {
        // The unknown field is rejected first by serde, which would hide the real
        // problem: the file is newer than this build, not malformed (spec §14.1).
        let text = format!(
            "{}\n[modell]\nprovider = \"ollama\"\n",
            MINIMAL.replace("version = 1", "version = 2")
        );
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
    fn a_stdio_server_that_also_declares_a_url_is_rejected() {
        let text = format!(
            r#"{MINIMAL}

[[mcp.servers]]
name = "github"
transport = "stdio"
command = "npx"
url = "http://localhost:3000"
"#
        );
        let err = parse(&text).unwrap_err();
        assert!(matches!(err, Error::ConfigInvalid { .. }), "{err:?}");
    }

    #[test]
    fn an_http_server_that_also_declares_a_command_is_rejected() {
        let text = format!(
            r#"{MINIMAL}

[[mcp.servers]]
name = "remote"
transport = "streamable-http"
url = "http://localhost:3000"
command = "npx"
"#
        );
        let err = parse(&text).unwrap_err();
        assert!(matches!(err, Error::ConfigInvalid { .. }), "{err:?}");
    }

    #[test]
    fn an_http_server_that_also_declares_args_is_rejected() {
        let text = format!(
            r#"{MINIMAL}

[[mcp.servers]]
name = "remote"
transport = "streamable-http"
url = "http://localhost:3000"
args = ["-y"]
"#
        );
        let err = parse(&text).unwrap_err();
        assert!(matches!(err, Error::ConfigInvalid { .. }), "{err:?}");
    }

    #[test]
    fn an_http_server_that_also_declares_env_is_rejected() {
        let text = format!(
            r#"{MINIMAL}

[[mcp.servers]]
name = "remote"
transport = "streamable-http"
url = "http://localhost:3000"
env = {{ TOKEN = "x" }}
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

    #[test]
    fn paths_that_differ_only_in_spelling_collide() {
        let text = format!(
            r#"{MINIMAL}

[[prompts]]
path = "./prompts/system.md"
"#
        );
        let err = parse(&text).unwrap_err();
        assert!(matches!(err, Error::DependencyCollision { .. }), "{err:?}");
    }

    #[test]
    fn root_for_resolves_against_the_config_files_directory() {
        assert_eq!(
            Config::root_for(Path::new("agentchecksum.toml")),
            Path::new(".")
        );
        assert_eq!(
            Config::root_for(Path::new("/etc/agentchecksum.toml")),
            Path::new("/etc")
        );
    }

    #[test]
    fn a_leading_current_directory_component_is_normalized_away() {
        assert_eq!(
            normalize_rel_path("./prompts/a.md").unwrap(),
            "prompts/a.md"
        );
    }

    #[test]
    fn an_absolute_path_is_rejected() {
        let err = normalize_rel_path("/etc/prompts/a.md").unwrap_err();
        assert!(matches!(err, Error::PromptPath { .. }), "{err:?}");
    }

    #[test]
    fn a_parent_directory_component_is_rejected() {
        let err = normalize_rel_path("../prompts/a.md").unwrap_err();
        assert!(matches!(err, Error::PromptPath { .. }), "{err:?}");
    }

    #[test]
    fn a_backslash_is_rejected() {
        let err = normalize_rel_path("prompts\\a.md").unwrap_err();
        assert!(matches!(err, Error::PromptPath { .. }), "{err:?}");
    }

    #[test]
    fn a_path_with_no_components_is_rejected() {
        assert!(normalize_rel_path("").is_err());
        assert!(normalize_rel_path(".").is_err());
    }

    // ------------------------------------------------------------ MCP aliases

    fn with_server(server: &str) -> String {
        format!("{MINIMAL}\n[[mcp.servers]]\n{server}\n")
    }

    #[test]
    fn an_alias_may_not_contain_the_tool_separator() {
        // The whole reason the grammar exists: `tool:<alias>.<tool>` separates on the
        // first dot, so an alias with a dot would let two servers claim one identity.
        let err = parse(&with_server(
            "name = \"github.prod\"\ntransport = \"stdio\"\ncommand = \"server\"",
        ))
        .unwrap_err();

        let message = err.to_string();
        assert!(message.contains("github.prod"), "{message}");
        assert!(message.contains("no `.`"), "{message}");
        // The reason carries the fix, which is what a suggestion would say.
        assert!(message.contains("use letters, digits"), "{message}");
    }

    #[test]
    fn the_alias_grammar_is_exactly_letters_digits_dash_and_underscore() {
        for alias in ["github", "github-prod", "a_1", "A-B_c9"] {
            assert!(is_valid_alias(alias), "{alias}");
            parse(&with_server(&format!(
                "name = \"{alias}\"\ntransport = \"stdio\"\ncommand = \"server\""
            )))
            .unwrap_or_else(|error| panic!("{alias}: {error}"));
        }

        for alias in [
            "",
            ".",
            "a.b",
            "a b",
            "a/b",
            "server:1",
            &"x".repeat(MAX_ALIAS_BYTES + 1),
        ] {
            assert!(!is_valid_alias(alias), "{alias:?} should be refused");
        }
    }

    // ----------------------------------------------------------- MCP endpoints

    #[test]
    fn an_endpoint_with_embedded_credentials_is_refused_without_echoing_them() {
        // A URL is the one configuration value that can carry a credential, so the
        // diagnostic names the component and never the value.
        let err = parse(&with_server(
            "name = \"remote\"\ntransport = \"streamable-http\"\nurl = \"https://user:SECRET@example.com/mcp\"",
        ))
        .unwrap_err();

        let message = err.to_string();
        assert!(message.contains("embedded credentials"), "{message}");
        assert!(!message.contains("SECRET"), "{message}");
        assert!(!message.contains("user"), "{message}");
    }

    #[test]
    fn an_endpoint_with_a_query_or_fragment_is_refused() {
        for (url, expected) in [
            ("https://example.com/mcp?deployment=a", "query string"),
            ("https://example.com/mcp#frag", "fragment"),
        ] {
            let err = parse(&with_server(&format!(
                "name = \"remote\"\ntransport = \"streamable-http\"\nurl = \"{url}\""
            )))
            .unwrap_err();
            assert!(err.to_string().contains(expected), "{url}: {err}");
        }
    }

    #[test]
    fn an_endpoint_must_be_absolute_http_or_https() {
        for url in [
            "example.com/mcp",
            "ftp://example.com/mcp",
            "ws://example.com/mcp",
            "https:///mcp",
        ] {
            let err = parse(&with_server(&format!(
                "name = \"remote\"\ntransport = \"streamable-http\"\nurl = \"{url}\""
            )))
            .unwrap_err();
            assert!(matches!(err, Error::ConfigInvalid { .. }), "{url}: {err:?}");
        }
    }

    #[test]
    fn a_plain_endpoint_is_accepted() {
        let config = parse(&with_server(
            "name = \"remote\"\ntransport = \"streamable-http\"\nurl = \"http://127.0.0.1:8123/mcp\"",
        ))
        .unwrap();

        assert_eq!(config.mcp.servers.len(), 1);
        assert_eq!(
            config.mcp.servers[0].url.as_deref(),
            Some("http://127.0.0.1:8123/mcp")
        );
    }

    #[test]
    fn a_config_rejects_a_policy_for_a_metric_that_does_not_exist() {
        let text = r#"
version = 1
[agent]
name = "a"
[policy.metrics.tool_slection]
min = 0.95
"#;
        let error = Config::from_toml_at(text, Path::new("agentchecksum.toml")).unwrap_err();
        assert!(
            matches!(error, Error::PolicyMetricUnknown { .. }),
            "{error:?}"
        );
        let suggestion = error.suggestion().unwrap();
        assert!(suggestion.contains("tool_selection"), "{suggestion}");
    }

    #[test]
    fn a_config_rejects_a_threshold_outside_the_score_range() {
        // `nan` is a valid TOML float and every comparison against it is false, so a
        // policy carrying one would silently never fire.
        for bad in ["min = 1.5", "max = -0.1", "max_drop = nan"] {
            let text = format!(
                "version = 1\n[agent]\nname = \"a\"\n[policy.metrics.tool_selection]\n{bad}\n"
            );
            let error = Config::from_toml_at(&text, Path::new("agentchecksum.toml")).unwrap_err();
            assert!(
                matches!(error, Error::PolicyRange { .. }),
                "{bad}: {error:?}"
            );
        }
    }

    #[test]
    fn a_config_rejects_a_policy_no_score_could_satisfy() {
        let text = r#"
version = 1
[agent]
name = "a"
[policy.metrics.argument_validity]
min = 0.9
max = 0.5
"#;
        let error = Config::from_toml_at(text, Path::new("agentchecksum.toml")).unwrap_err();
        assert!(matches!(error, Error::PolicyRange { .. }), "{error:?}");
    }

    #[test]
    fn the_shipped_example_policy_is_valid() {
        // The documented policy names have to be the ones the config accepts.
        let text = r#"
version = 1
[agent]
name = "a"
[policy]
fail_on_risk = "critical"

[policy.metrics.tool_selection]
min = 0.95

[policy.metrics.argument_validity]
max_drop = 0.05

[policy.metrics.forbidden_tool_usage]
max = 0.0
"#;
        let config = Config::from_toml_at(text, Path::new("agentchecksum.toml")).unwrap();
        assert_eq!(config.policy.metrics.len(), 3);
    }
}
