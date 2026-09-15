// SPDX-License-Identifier: MIT OR Apache-2.0

//! Model dependency discovery.
//!
//! Behavior-relevant model state is not just the model name. Quantization, the
//! chat template, inference parameters, and the capability set all change how an
//! agent behaves while every source file stays byte-identical.
//!
//! Nothing here records `modified_at`, `size`, or `license`: they are not
//! behavior-relevant, and recording them would make the checksum depend on when
//! and where it was computed. `OllamaMetadata` simply has no fields for them, so
//! the omission is enforced by the type rather than by remembering to filter.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::ModelConfig;
use crate::error::{Error, Result};
use crate::fingerprint::{canonical, normalize};
use crate::manifest::{Dependency, DependencyKind, Digest, Facet};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OllamaMetadata {
    pub digest: Option<String>,
    pub family: Option<String>,
    pub parameter_size: Option<String>,
    pub quantization_level: Option<String>,
    /// Raw `parameters` text from `/api/show`.
    pub parameters: Option<String>,
    pub template: Option<String>,
    pub capabilities: Vec<String>,
}

#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagEntry>,
}

#[derive(Deserialize)]
struct TagEntry {
    name: String,
    model: Option<String>,
    digest: Option<String>,
    #[serde(default)]
    details: TagDetails,
}

#[derive(Default, Deserialize)]
struct TagDetails {
    family: Option<String>,
    parameter_size: Option<String>,
    quantization_level: Option<String>,
}

#[derive(Deserialize)]
struct ShowResponse {
    parameters: Option<String>,
    template: Option<String>,
    #[serde(default)]
    capabilities: Vec<String>,
}

#[derive(Serialize)]
struct ShowRequest<'a> {
    model: &'a str,
}

/// Parse Ollama's `parameters` text into a map.
///
/// The text is line-oriented (`temperature 0.7`), so the original ordering is an
/// artifact of how the Modelfile was written. Storing a map makes the digest
/// independent of that artifact while keeping repeated keys such as `stop`.
pub fn parse_ollama_parameters(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut parameters: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = match line.split_once(char::is_whitespace) {
            Some((key, value)) => (key, value.trim()),
            None => (line, ""),
        };
        parameters
            .entry(key.to_string())
            .or_default()
            .push(value.trim_matches('"').to_string());
    }
    parameters
}

/// Digest a JSON value without recording it.
fn facet(value: &serde_json::Value) -> Result<Facet> {
    Ok(Facet {
        digest: Digest::sha256(&canonical::to_vec(value)?),
        shape: None,
        normalized: None,
    })
}

/// Digest a JSON value and record it, because the source is external and
/// therefore not version-controlled inside this repository.
fn recorded_facet(value: &serde_json::Value) -> Result<Facet> {
    let mut facet = facet(value)?;
    facet.normalized = Some(value.clone());
    Ok(facet)
}

/// Build the `params` facet payload from both sources of effective inference
/// parameters: the ones this project configures, and the ones the provider
/// reports as model defaults. Both are behavior-relevant (design spec §5,
/// §11.2). Returns `None` when neither source has anything, so a model with no
/// parameters at all carries no `params` facet.
fn params_payload(
    configured: &BTreeMap<String, serde_json::Value>,
    reported: Option<&BTreeMap<String, Vec<String>>>,
) -> Result<Option<serde_json::Value>> {
    if configured.is_empty() && reported.is_none() {
        return Ok(None);
    }

    let mut payload = serde_json::Map::new();
    if !configured.is_empty() {
        payload.insert(
            "configured".to_string(),
            serde_json::to_value(configured).map_err(|source| Error::Json { source })?,
        );
    }
    if let Some(reported) = reported {
        payload.insert(
            "reported".to_string(),
            serde_json::to_value(reported).map_err(|source| Error::Json { source })?,
        );
    }
    Ok(Some(serde_json::Value::Object(payload)))
}

/// Pure mapping from config plus optional provider metadata to a dependency.
pub fn dependency(
    config: &ModelConfig,
    metadata: Option<&OllamaMetadata>,
) -> Result<(Dependency, Vec<String>)> {
    let mut warnings: Vec<String> = Vec::new();
    let id = format!("model:{}/{}", config.provider, config.id);

    let mut identity = serde_json::Map::new();
    identity.insert(
        "provider".to_string(),
        serde_json::Value::String(config.provider.clone()),
    );
    identity.insert(
        "id".to_string(),
        serde_json::Value::String(config.id.clone()),
    );

    let mut facets: BTreeMap<String, Facet> = BTreeMap::new();

    // Which facets record their normalized payload: an external, un-versioned source
    // records it, so a later `diff` can name WHAT changed rather than only THAT it
    // changed (design spec §6.3). `identity`, `params`, and `capabilities` all
    // qualify, and each is small and named by a specific row of the §8.3 risk table
    // (in particular, naming which capability was lost is the whole point of the
    // `tools` row). `template` stays digest-only: it is a large payload and its diff
    // value is low, since "the chat template changed" is the entire message.

    match config.provider.as_str() {
        "ollama" => {
            if config.endpoint.is_none() {
                return Err(Error::ModelEndpointMissing {
                    provider: config.provider.clone(),
                });
            }

            let metadata = metadata.ok_or_else(|| Error::ModelMissing {
                provider: config.provider.clone(),
                id: config.id.clone(),
                endpoint: config.endpoint.clone().unwrap_or_default(),
            })?;

            match &metadata.digest {
                Some(digest) => {
                    identity.insert(
                        "digest".to_string(),
                        serde_json::Value::String(format!("sha256:{digest}")),
                    );
                }
                None => warnings.push(format!(
                    "model `{}`: the endpoint reported no content digest; upstream model updates may go undetected",
                    config.id
                )),
            }

            for (key, value) in [
                ("family", &metadata.family),
                ("parameter_size", &metadata.parameter_size),
                ("quantization_level", &metadata.quantization_level),
            ] {
                if let Some(value) = value {
                    identity.insert(key.to_string(), serde_json::Value::String(value.clone()));
                }
            }

            if let Some(template) = &metadata.template {
                facets.insert(
                    "template".to_string(),
                    facet(&serde_json::Value::String(normalize::normalize_text(
                        template,
                    )))?,
                );
            }

            let mut capabilities = metadata.capabilities.clone();
            capabilities.sort();
            capabilities.dedup();
            if !capabilities.is_empty() {
                let value =
                    serde_json::to_value(&capabilities).map_err(|source| Error::Json { source })?;
                facets.insert("capabilities".to_string(), recorded_facet(&value)?);
            }
        }
        "openai-compatible" => {
            warnings.push(format!(
                "model `{}`: digest unavailable for the `openai-compatible` provider; upstream model updates may go undetected",
                config.id
            ));
        }
        other => {
            return Err(Error::ModelProvider {
                provider: other.to_string(),
            });
        }
    }

    // Configured parameters are half of the effective inference behavior: they are
    // what the probe runner sends. Provider-reported parameters are the other half:
    // they are the defaults the model uses when the config says nothing.
    let reported = match config.provider.as_str() {
        "ollama" => metadata
            .and_then(|meta| meta.parameters.as_deref())
            .map(parse_ollama_parameters),
        _ => None,
    };
    if let Some(payload) = params_payload(&config.params, reported.as_ref())? {
        facets.insert("params".to_string(), recorded_facet(&payload)?);
    }

    let identity = serde_json::Value::Object(identity);
    facets.insert("identity".to_string(), recorded_facet(&identity)?);

    Ok((
        Dependency {
            id,
            kind: DependencyKind::Model,
            facets,
            source: Some(config.provider.clone()),
        },
        warnings,
    ))
}

/// Fetch metadata from an Ollama endpoint. Returns `Ok(None)` for providers that
/// expose no metadata endpoint.
pub async fn fetch(
    client: &reqwest::Client,
    config: &ModelConfig,
) -> Result<Option<OllamaMetadata>> {
    if config.provider != "ollama" {
        return Ok(None);
    }

    let endpoint = config
        .endpoint
        .clone()
        .ok_or_else(|| Error::ModelEndpointMissing {
            provider: config.provider.clone(),
        })?;
    let base = endpoint.trim_end_matches('/');

    let tags_response = client
        .get(format!("{base}/api/tags"))
        .send()
        .await
        .map_err(|source| Error::ModelEndpoint {
            provider: config.provider.clone(),
            endpoint: endpoint.clone(),
            source,
        })?;

    let status = tags_response.status();
    if !status.is_success() {
        return Err(Error::ModelStatus {
            provider: config.provider.clone(),
            endpoint: endpoint.clone(),
            status: status.as_u16(),
        });
    }

    let tags: TagsResponse = tags_response
        .json()
        .await
        .map_err(|source| Error::ModelEndpoint {
            provider: config.provider.clone(),
            endpoint: endpoint.clone(),
            source,
        })?;

    let entry = tags
        .models
        .into_iter()
        .find(|entry| entry.name == config.id || entry.model.as_deref() == Some(config.id.as_str()))
        .ok_or_else(|| Error::ModelMissing {
            provider: config.provider.clone(),
            id: config.id.clone(),
            endpoint: endpoint.clone(),
        })?;

    let show_response = client
        .post(format!("{base}/api/show"))
        .json(&ShowRequest { model: &config.id })
        .send()
        .await
        .map_err(|source| Error::ModelEndpoint {
            provider: config.provider.clone(),
            endpoint: endpoint.clone(),
            source,
        })?;

    let status = show_response.status();
    if !status.is_success() {
        return Err(Error::ModelStatus {
            provider: config.provider.clone(),
            endpoint: endpoint.clone(),
            status: status.as_u16(),
        });
    }

    let show: ShowResponse = show_response
        .json()
        .await
        .map_err(|source| Error::ModelEndpoint {
            provider: config.provider.clone(),
            endpoint: endpoint.clone(),
            source,
        })?;

    Ok(Some(OllamaMetadata {
        digest: entry.digest,
        family: entry.details.family,
        parameter_size: entry.details.parameter_size,
        quantization_level: entry.details.quantization_level,
        parameters: show.parameters,
        template: show.template,
        capabilities: show.capabilities,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelConfig;

    fn ollama_config() -> ModelConfig {
        ModelConfig {
            provider: "ollama".to_string(),
            id: "qwen3:8b".to_string(),
            endpoint: Some("http://localhost:11434".to_string()),
            params: BTreeMap::new(),
        }
    }

    fn metadata() -> OllamaMetadata {
        OllamaMetadata {
            digest: Some("aa".repeat(32)),
            family: Some("qwen3".to_string()),
            parameter_size: Some("8.0B".to_string()),
            quantization_level: Some("Q8_0".to_string()),
            parameters: Some("temperature 0.7\nnum_ctx 2048\n".to_string()),
            template: Some("{{ .Prompt }}\n".to_string()),
            capabilities: vec!["completion".to_string(), "tools".to_string()],
        }
    }

    fn build(config: &ModelConfig, metadata: Option<&OllamaMetadata>) -> Dependency {
        dependency(config, metadata).unwrap().0
    }

    #[test]
    fn the_ollama_digest_is_normalized_to_the_sha256_prefixed_form() {
        let dependency = build(&ollama_config(), Some(&metadata()));
        let identity = dependency.facets["identity"].normalized.clone().unwrap();
        assert_eq!(
            identity["digest"],
            serde_json::Value::String(format!("sha256:{}", "aa".repeat(32)))
        );
    }

    #[test]
    fn the_dependency_id_names_the_provider_and_the_model() {
        let dependency = build(&ollama_config(), Some(&metadata()));
        assert_eq!(dependency.id, "model:ollama/qwen3:8b");
        assert_eq!(dependency.kind, DependencyKind::Model);
        assert_eq!(dependency.source.as_deref(), Some("ollama"));
    }

    #[test]
    fn changing_the_quantization_changes_the_identity_digest() {
        let baseline = build(&ollama_config(), Some(&metadata()));
        let mut quantized = metadata();
        quantized.quantization_level = Some("Q4_K_M".to_string());
        let after = build(&ollama_config(), Some(&quantized));
        assert_ne!(
            baseline.facets["identity"].digest,
            after.facets["identity"].digest
        );
    }

    #[test]
    fn changing_the_model_content_digest_changes_the_identity_digest() {
        let baseline = build(&ollama_config(), Some(&metadata()));
        let mut swapped = metadata();
        swapped.digest = Some("bb".repeat(32));
        let after = build(&ollama_config(), Some(&swapped));
        assert_ne!(
            baseline.facets["identity"].digest,
            after.facets["identity"].digest
        );
    }

    #[test]
    fn losing_the_tools_capability_changes_the_capabilities_digest() {
        let baseline = build(&ollama_config(), Some(&metadata()));
        let mut stripped = metadata();
        stripped.capabilities = vec!["completion".to_string()];
        let after = build(&ollama_config(), Some(&stripped));
        assert_ne!(
            baseline.facets["capabilities"].digest,
            after.facets["capabilities"].digest
        );
        // A digest can only say that something changed; the risk table's `tools` row
        // needs the diff to name the capability that was lost, so the normalized
        // payload is recorded, in the sorted-and-deduped form the facet hashes.
        assert_eq!(
            baseline.facets["capabilities"].normalized.clone().unwrap(),
            serde_json::json!(["completion", "tools"])
        );
    }

    #[test]
    fn capability_order_is_insignificant() {
        let mut reordered = metadata();
        reordered.capabilities = vec!["tools".to_string(), "completion".to_string()];
        assert_eq!(
            build(&ollama_config(), Some(&metadata())),
            build(&ollama_config(), Some(&reordered))
        );
    }

    #[test]
    fn duplicated_capabilities_are_insignificant() {
        let mut duplicated = metadata();
        duplicated.capabilities = vec![
            "tools".to_string(),
            "completion".to_string(),
            "tools".to_string(),
        ];
        assert_eq!(
            build(&ollama_config(), Some(&metadata())),
            build(&ollama_config(), Some(&duplicated))
        );
    }

    #[test]
    fn parameter_line_order_is_insignificant() {
        let mut reordered = metadata();
        reordered.parameters = Some("num_ctx 2048\ntemperature 0.7\n".to_string());
        assert_eq!(
            build(&ollama_config(), Some(&metadata())),
            build(&ollama_config(), Some(&reordered))
        );
    }

    #[test]
    fn a_parameter_value_change_changes_the_params_digest() {
        let baseline = build(&ollama_config(), Some(&metadata()));
        let mut retuned = metadata();
        retuned.parameters = Some("temperature 0.0\nnum_ctx 2048\n".to_string());
        let after = build(&ollama_config(), Some(&retuned));
        assert_ne!(
            baseline.facets["params"].digest,
            after.facets["params"].digest
        );
    }

    #[test]
    fn configured_inference_parameters_are_hashed() {
        let baseline = build(&ollama_config(), Some(&metadata()));
        let mut retuned = ollama_config();
        retuned
            .params
            .insert("temperature".to_string(), serde_json::json!(0.9));
        let after = build(&retuned, Some(&metadata()));
        assert_ne!(
            baseline.facets["params"].digest,
            after.facets["params"].digest
        );
    }

    #[test]
    fn configured_parameters_are_recorded_alongside_reported_ones() {
        let mut config = ollama_config();
        config
            .params
            .insert("seed".to_string(), serde_json::json!(42));
        let dependency = build(&config, Some(&metadata()));
        let payload = dependency.facets["params"].normalized.clone().unwrap();
        assert_eq!(payload["configured"]["seed"], serde_json::json!(42));
        assert!(payload["reported"]["temperature"].is_array());
    }

    #[test]
    fn configured_parameters_are_hashed_for_a_provider_that_reports_none() {
        let mut config = ModelConfig {
            provider: "openai-compatible".to_string(),
            id: "gpt-4o".to_string(),
            endpoint: Some("http://localhost:8000".to_string()),
            params: BTreeMap::new(),
        };
        config
            .params
            .insert("temperature".to_string(), serde_json::json!(0.7));
        let baseline = dependency(&config, None).unwrap().0;
        config
            .params
            .insert("temperature".to_string(), serde_json::json!(0.9));
        let after = dependency(&config, None).unwrap().0;
        assert_ne!(
            baseline.facets["params"].digest,
            after.facets["params"].digest
        );
    }

    #[test]
    fn a_model_with_no_parameters_at_all_has_no_params_facet() {
        let mut without = metadata();
        without.parameters = None;
        let dependency = build(&ollama_config(), Some(&without));
        assert!(!dependency.facets.contains_key("params"));
    }

    #[test]
    fn a_chat_template_change_changes_the_template_digest() {
        let baseline = build(&ollama_config(), Some(&metadata()));
        let mut retemplated = metadata();
        retemplated.template = Some("<|im_start|>{{ .Prompt }}".to_string());
        let after = build(&ollama_config(), Some(&retemplated));
        assert_ne!(
            baseline.facets["template"].digest,
            after.facets["template"].digest
        );
    }

    #[test]
    fn a_template_difference_that_is_only_trailing_whitespace_is_insignificant() {
        let mut reflowed = metadata();
        reflowed.template = Some("{{ .Prompt }}   ".to_string());
        assert_eq!(
            build(&ollama_config(), Some(&metadata())),
            build(&ollama_config(), Some(&reflowed))
        );
    }

    #[test]
    fn an_absent_template_produces_no_template_facet() {
        let mut without = metadata();
        without.template = None;
        let dependency = build(&ollama_config(), Some(&without));
        assert!(!dependency.facets.contains_key("template"));
    }

    #[test]
    fn parameters_parse_into_an_order_insensitive_map() {
        let parsed = parse_ollama_parameters("temperature 0.7\nstop \"END\"\nstop \"STOP\"\n");
        assert_eq!(parsed["temperature"], vec!["0.7".to_string()]);
        assert_eq!(parsed["stop"], vec!["END".to_string(), "STOP".to_string()]);
    }

    #[test]
    fn an_openai_compatible_provider_records_identity_without_a_digest_and_warns() {
        let config = ModelConfig {
            provider: "openai-compatible".to_string(),
            id: "gpt-4o".to_string(),
            endpoint: Some("http://localhost:8000".to_string()),
            params: BTreeMap::new(),
        };
        let (dependency, warnings) = dependency(&config, None).unwrap();

        assert_eq!(dependency.id, "model:openai-compatible/gpt-4o");
        assert!(!dependency.facets.contains_key("template"));
        assert!(!dependency.facets.contains_key("params"));
        assert_eq!(warnings.len(), 1, "a missing digest must be surfaced");
        assert!(warnings[0].contains("digest unavailable"), "{warnings:?}");
    }

    #[test]
    fn an_unsupported_provider_is_an_error() {
        let mut config = ollama_config();
        config.provider = "anthropic".to_string();
        let err = dependency(&config, None).unwrap_err();
        assert!(matches!(err, Error::ModelProvider { .. }), "{err:?}");
    }

    #[test]
    fn an_ollama_model_that_the_server_does_not_report_is_an_error() {
        let err = dependency(&ollama_config(), None).unwrap_err();
        assert!(matches!(err, Error::ModelMissing { .. }), "{err:?}");
    }

    #[test]
    fn the_endpoint_is_never_part_of_the_identity() {
        let mut moved = ollama_config();
        moved.endpoint = Some("http://other-host:11434".to_string());
        assert_eq!(
            build(&ollama_config(), Some(&metadata())),
            build(&moved, Some(&metadata()))
        );
    }

    #[test]
    fn an_ollama_provider_without_an_endpoint_is_an_error() {
        let mut config = ollama_config();
        config.endpoint = None;
        let err = dependency(&config, None).unwrap_err();
        assert!(matches!(err, Error::ModelEndpointMissing { .. }), "{err:?}");
    }

    async fn metadata_for_with(
        server: &wiremock::MockServer,
        model_digest: &str,
        modified_at: &str,
        size: u64,
        license: &str,
    ) -> OllamaMetadata {
        // Built as a map so the optional `digest` is inserted rather than
        // mutated in place: `serde_json::Value` implements `Index` but not
        // `IndexMut`.
        let mut model = serde_json::Map::new();
        model.insert("name".to_string(), serde_json::json!("qwen3:8b"));
        model.insert("model".to_string(), serde_json::json!("qwen3:8b"));
        model.insert("modified_at".to_string(), serde_json::json!(modified_at));
        model.insert("size".to_string(), serde_json::json!(size));
        if !model_digest.is_empty() {
            model.insert(
                "digest".to_string(),
                serde_json::Value::String(model_digest.to_string()),
            );
        }
        model.insert(
            "details".to_string(),
            serde_json::json!({
                "format": "gguf",
                "family": "qwen3",
                "parameter_size": "8.0B",
                "quantization_level": "Q8_0"
            }),
        );
        let tags = serde_json::json!({ "models": [serde_json::Value::Object(model)] });

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/tags"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(tags))
            .mount(server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/show"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "parameters": "temperature 0.7\n",
                    "template": "{{ .Prompt }}",
                    "capabilities": ["completion", "tools"],
                    "license": license,
                    "modified_at": "2025-08-14T15:49:43Z"
                })),
            )
            .mount(server)
            .await;

        let config = ModelConfig {
            provider: "ollama".to_string(),
            id: "qwen3:8b".to_string(),
            endpoint: Some(server.uri()),
            params: BTreeMap::new(),
        };

        fetch(&reqwest::Client::new(), &config)
            .await
            .unwrap()
            .unwrap()
    }

    /// The single-call form: every test but the metadata-exclusion one wants the
    /// same non-behavioral values on both sides.
    async fn metadata_for(server: &wiremock::MockServer, model_digest: &str) -> OllamaMetadata {
        metadata_for_with(
            server,
            model_digest,
            "2025-10-03T23:34:03Z",
            9608350245,
            "Apache-2.0",
        )
        .await
    }

    #[tokio::test]
    async fn fetch_reads_tags_and_show_from_the_endpoint() {
        let server = wiremock::MockServer::start().await;
        let metadata = metadata_for(&server, &"cc".repeat(32)).await;

        assert_eq!(metadata.quantization_level.as_deref(), Some("Q8_0"));
        assert_eq!(metadata.family.as_deref(), Some("qwen3"));
        assert_eq!(metadata.capabilities, vec!["completion", "tools"]);
        assert_eq!(metadata.template.as_deref(), Some("{{ .Prompt }}"));
    }

    #[tokio::test]
    async fn timestamps_sizes_and_licenses_never_reach_the_dependency() {
        let first = wiremock::MockServer::start().await;
        let second = wiremock::MockServer::start().await;

        // Same model, but the second response carries a different modification
        // time, size, and license. None of those are behavior-relevant, so the
        // dependency must be identical byte for byte.
        let a = metadata_for_with(
            &first,
            &"dd".repeat(32),
            "2025-10-03T23:34:03Z",
            9608350245,
            "Apache-2.0",
        )
        .await;
        let b = metadata_for_with(
            &second,
            &"dd".repeat(32),
            "2026-01-01T00:00:00Z",
            111111,
            "MIT",
        )
        .await;

        let config_a = ModelConfig {
            provider: "ollama".to_string(),
            id: "qwen3:8b".to_string(),
            endpoint: Some(first.uri()),
            params: BTreeMap::new(),
        };
        let config_b = ModelConfig {
            endpoint: Some(second.uri()),
            ..config_a.clone()
        };

        assert_eq!(
            build(&config_a, Some(&a)),
            build(&config_b, Some(&b)),
            "non-behavioral provider metadata must not influence the fingerprint"
        );
    }

    #[tokio::test]
    async fn a_model_with_no_reported_digest_produces_a_warning() {
        let server = wiremock::MockServer::start().await;
        let metadata = metadata_for(&server, "").await;

        let (dependency, warnings) = dependency(&ollama_config(), Some(&metadata)).unwrap();
        let identity = dependency.facets["identity"].normalized.clone().unwrap();
        assert!(identity.get("digest").is_none(), "{identity}");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("no content digest"), "{warnings:?}");
    }

    #[tokio::test]
    async fn fetch_reports_a_server_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let config = ModelConfig {
            provider: "ollama".to_string(),
            id: "qwen3:8b".to_string(),
            endpoint: Some(server.uri()),
            params: BTreeMap::new(),
        };

        let err = fetch(&reqwest::Client::new(), &config).await.unwrap_err();
        assert!(
            matches!(err, Error::ModelStatus { status: 500, .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn fetch_reports_a_model_the_server_does_not_have() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/tags"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "models": [] })),
            )
            .mount(&server)
            .await;

        let config = ModelConfig {
            provider: "ollama".to_string(),
            id: "qwen3:8b".to_string(),
            endpoint: Some(server.uri()),
            params: BTreeMap::new(),
        };

        let err = fetch(&reqwest::Client::new(), &config).await.unwrap_err();
        assert!(matches!(err, Error::ModelMissing { .. }), "{err:?}");
    }
}
