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

    // Repeated keys such as `stop` accumulate in line order, but the order of a
    // stop-sequence set carries no meaning: sorting makes two Modelfiles that
    // differ only in that order fingerprint identically (spec §18 invariant 2).
    for values in parameters.values_mut() {
        values.sort();
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

/// Canonicalize an `openai-compatible` endpoint for identity: an HTTP(S) base URL
/// with a trailing slash removed.
///
/// Unsupported components are **rejected, never stripped**. A query string can
/// select a different deployment behind the same host, so quietly dropping one
/// would fingerprint two different models as the same — the false negative this
/// tool exists to catch. Credentials are rejected for that reason and a second one:
/// the lockfile is committed, and a sanitizer that parses a secret before
/// discarding it is a habit worth not having. Refusing is the only fail-safe
/// option, because a tool whose worse error is missing a change must not guess what
/// an input it cannot represent was meant to mean.
fn canonical_endpoint(provider: &str, endpoint: &str) -> Result<String> {
    let invalid = |reason: &str| Error::EndpointInvalid {
        provider: provider.to_string(),
        reason: reason.to_string(),
    };

    let url = reqwest::Url::parse(endpoint).map_err(|_| invalid("the value is not a valid URL"))?;

    match url.scheme() {
        "http" | "https" => {}
        scheme => {
            return Err(invalid(&format!(
                "the scheme `{scheme}` is not supported; use http or https"
            )));
        }
    }

    if url.host_str().is_none() {
        return Err(invalid("the URL has no host"));
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("URLs containing credentials are not supported"));
    }

    if url.query().is_some() {
        return Err(invalid(
            "query parameters are not supported; they can select a different deployment behind the same host",
        ));
    }

    if url.fragment().is_some() {
        return Err(invalid("fragments are not supported"));
    }

    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// Build the `params` facet payload from the two *sources* of inference
/// parameters: the ones this project configures, and the ones the provider
/// reports as model defaults.
///
/// These are deliberately fingerprinted separately rather than merged into an
/// "effective" set. Modelling provider override semantics correctly is more than
/// Phase 1 can promise, and a wrong merge is worse than a conservative one: it
/// would hide a change the user needs to see. The price of the conservative choice
/// is the opposite error — a `reported` change that `configured` overrides still
/// reads as a dependency change — which is a false positive the spec §8.3 risk
/// table already classifies as MEDIUM.
///
/// Returns `None` when neither source has anything, so a model with no parameters
/// at all carries no `params` facet.
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
            // This provider exposes no immutable content digest, so the endpoint is
            // the only thing that says *which* model is actually running: two hosts
            // serving a model with the same name can be entirely different backends
            // or weights. Leaving it out of the identity would make moving between
            // them look like no change at all, which is the false negative this tool
            // exists to catch.
            //
            // Ollama is the opposite case: it reports a content digest that already
            // pins the weights, so hashing the host there would turn a routine move
            // to another machine into a dependency change that says nothing about
            // behavior.
            let endpoint =
                config
                    .endpoint
                    .as_deref()
                    .ok_or_else(|| Error::ModelEndpointMissing {
                        provider: config.provider.clone(),
                    })?;

            identity.insert(
                "endpoint".to_string(),
                serde_json::Value::String(canonical_endpoint(&config.provider, endpoint)?),
            );

            warnings.push(format!(
                "model `{}`: the `openai-compatible` provider exposes no content digest, so a model swapped behind the same endpoint cannot be detected; the endpoint is part of the identity instead",
                config.id
            ));
        }
        other => {
            return Err(Error::ModelProvider {
                provider: other.to_string(),
            });
        }
    }

    // Both parameter sources are fingerprinted, not merged: `configured` is what the
    // probe runner will send, `reported` is what the provider says its defaults are.
    // See `params_payload` for why the merge is deliberately not attempted.
    let reported = match config.provider.as_str() {
        "ollama" => metadata
            .and_then(|meta| meta.parameters.as_deref())
            .map(parse_ollama_parameters),
        _ => None,
    };
    // An endpoint that writes an empty `parameters` block describes the same state as
    // one that omits it, so the two must not hash differently (spec §18 invariant 2).
    let reported = reported.filter(|reported| !reported.is_empty());
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

/// How long any single model-metadata request may take. A black-holed endpoint
/// must fail rather than hang `snapshot` in CI.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The client used for model metadata requests.
pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|source| Error::ModelClient { source })
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

    // Every closure captures only references, so they are `Copy` and can be handed to
    // every `.map_err(...)` and error return below. Six hand-written constructions
    // meant a change to the error shape had to be made in six places.
    let endpoint_error = |source: reqwest::Error| Error::ModelEndpoint {
        provider: config.provider.clone(),
        endpoint: endpoint.clone(),
        source,
    };
    let response_error = |source: reqwest::Error| Error::ModelResponse {
        provider: config.provider.clone(),
        endpoint: endpoint.clone(),
        source,
    };
    let status_error = |status: u16| Error::ModelStatus {
        provider: config.provider.clone(),
        endpoint: endpoint.clone(),
        status,
    };

    let tags_response = client
        .get(format!("{base}/api/tags"))
        .send()
        .await
        .map_err(endpoint_error)?;

    let status = tags_response.status();
    if !status.is_success() {
        return Err(status_error(status.as_u16()));
    }

    let tags: TagsResponse = tags_response.json().await.map_err(response_error)?;

    let entry = tags
        .models
        .into_iter()
        .find(|entry| {
            entry.name == config.id
                || entry.model.as_deref() == Some(config.id.as_str())
                // Ollama resolves an untagged name to `:latest`, so a config
                // saying `qwen3` must match a server reporting `qwen3:latest`.
                || entry.name == format!("{}:latest", config.id)
        })
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
        .map_err(endpoint_error)?;

    let status = show_response.status();
    if !status.is_success() {
        return Err(status_error(status.as_u16()));
    }

    let show: ShowResponse = show_response.json().await.map_err(response_error)?;

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
    fn an_empty_reported_parameters_block_produces_no_facet() {
        // A provider answering with an empty `parameters` string describes the same
        // state as one omitting the field: no reported parameters. The two must not
        // hash differently (spec §18 invariant 2).
        let mut empty_string = metadata();
        empty_string.parameters = Some(String::new());

        let mut whitespace_only = metadata();
        whitespace_only.parameters = Some("  \n\t".to_string());

        let mut absent = metadata();
        absent.parameters = None;

        let from_empty = build(&ollama_config(), Some(&empty_string));
        let from_whitespace = build(&ollama_config(), Some(&whitespace_only));
        let from_absent = build(&ollama_config(), Some(&absent));

        assert_eq!(from_empty, from_absent);
        assert_eq!(from_whitespace, from_absent);
        assert!(!from_absent.facets.contains_key("params"));
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
    fn trailing_whitespace_in_the_chat_template_changes_its_digest() {
        // The template is what the model is actually rendered through, so trailing
        // whitespace there is real content, not formatting. The template facet is
        // digest-only, so this surfaces as a change — the conservative direction: a
        // template reflow that mattered must not be invisible.
        let mut reflowed = metadata();
        reflowed.template = Some("{{ .Prompt }}   ".to_string());
        assert_ne!(
            build(&ollama_config(), Some(&metadata())).facets["template"].digest,
            build(&ollama_config(), Some(&reflowed)).facets["template"].digest
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
    fn repeated_stop_values_produce_the_same_dependency_in_any_order() {
        // `stop` is a set of sequences, so the order they happen to be written in
        // has no behavioral meaning (spec §18 invariant 2): the two orderings must
        // fingerprint identically.
        let mut forward = metadata();
        forward.parameters = Some("stop \"END\"\nstop \"STOP\"\ntemperature 0.7\n".to_string());
        let mut reverse = metadata();
        reverse.parameters = Some("stop \"STOP\"\nstop \"END\"\ntemperature 0.7\n".to_string());

        assert_eq!(
            build(&ollama_config(), Some(&forward)),
            build(&ollama_config(), Some(&reverse))
        );
    }

    #[test]
    fn an_openai_compatible_provider_records_the_endpoint_and_warns() {
        let (dependency, warnings) = dependency(
            &openai_compatible_config("https://server-a.example/v1"),
            None,
        )
        .unwrap();

        assert_eq!(dependency.id, "model:openai-compatible/same-model");
        assert!(!dependency.facets.contains_key("template"));
        assert!(!dependency.facets.contains_key("params"));

        // With no digest to fingerprint, the endpoint is what identifies the model.
        let identity = dependency.facets["identity"].normalized.clone().unwrap();
        assert_eq!(identity["endpoint"], "https://server-a.example/v1");

        assert_eq!(warnings.len(), 1, "the missing digest must be surfaced");
        assert!(warnings[0].contains("no content digest"), "{warnings:?}");
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

    fn openai_compatible_config(endpoint: &str) -> ModelConfig {
        ModelConfig {
            provider: "openai-compatible".to_string(),
            id: "same-model".to_string(),
            endpoint: Some(endpoint.to_string()),
            params: BTreeMap::new(),
        }
    }

    #[test]
    fn an_ollama_endpoint_is_not_part_of_the_identity() {
        // Ollama reports a content digest that already pins the weights, so hashing
        // the host as well would turn moving the same model to another machine into
        // a dependency change that says nothing about behavior.
        let mut moved = ollama_config();
        moved.endpoint = Some("http://other-host:11434".to_string());
        assert_eq!(
            build(&ollama_config(), Some(&metadata())),
            build(&moved, Some(&metadata()))
        );
    }

    #[test]
    fn an_openai_compatible_base_url_is_accepted() {
        let model = build(&openai_compatible_config("https://example.com/v1"), None);
        let identity = model.facets["identity"].normalized.clone().unwrap();
        assert_eq!(identity["endpoint"], "https://example.com/v1");

        // `http` and a bare local origin are just as valid.
        assert!(dependency(&openai_compatible_config("http://localhost:8000/v1"), None).is_ok());
    }

    #[test]
    fn an_openai_compatible_endpoint_is_part_of_the_identity() {
        // No immutable digest exists for this provider, so two hosts — or two paths
        // on one host — can be entirely different deployments. Moving between them
        // must not look like no change at all.
        let base = build(&openai_compatible_config("https://example.com/v1"), None);
        let other_host = build(
            &openai_compatible_config("https://other.example.com/v1"),
            None,
        );
        let other_path = build(&openai_compatible_config("https://example.com/v2"), None);

        assert_ne!(
            base.facets["identity"].digest,
            other_host.facets["identity"].digest
        );
        assert_ne!(
            base.facets["identity"].digest,
            other_path.facets["identity"].digest
        );
        assert_ne!(base.digest().unwrap(), other_host.digest().unwrap());
    }

    #[test]
    fn a_trailing_slash_is_insignificant_in_an_openai_compatible_endpoint() {
        assert_eq!(
            build(&openai_compatible_config("https://example.com/v1/"), None),
            build(&openai_compatible_config("https://example.com/v1"), None)
        );
    }

    /// The endpoint failures share a shape, so they share a helper.
    fn endpoint_error(endpoint: &str) -> Error {
        dependency(&openai_compatible_config(endpoint), None).unwrap_err()
    }

    #[test]
    fn a_query_parameter_is_rejected_rather_than_stripped() {
        // `?deployment=a` and `?deployment=b` can route to different models behind one
        // host, so stripping the query would fingerprint two deployments as one — the
        // false negative this tool exists to catch.
        let err = endpoint_error("https://example.com/v1?deployment=a");

        assert!(matches!(err, Error::EndpointInvalid { .. }), "{err:?}");
        assert!(err.to_string().contains("query"), "{err}");
    }

    #[test]
    fn a_fragment_is_rejected_rather_than_stripped() {
        let err = endpoint_error("https://example.com/v1#deployment-a");
        assert!(matches!(err, Error::EndpointInvalid { .. }), "{err:?}");
    }

    #[test]
    fn credentials_in_an_endpoint_are_rejected_and_never_echoed() {
        // Refusing is also what keeps a secret out of the committed lockfile: a
        // sanitizer would have parsed the credential before discarding it.
        let err = endpoint_error("https://user:secret@example.com/v1");

        assert!(matches!(err, Error::EndpointInvalid { .. }), "{err:?}");

        let rendered = format!("{err} {}", err.suggestion().unwrap_or_default());
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(!rendered.contains("user:"), "{rendered}");
    }

    #[test]
    fn a_malformed_endpoint_is_rejected_rather_than_hashed_as_written() {
        // Hashing the raw string on a parse failure would fingerprint a value nobody
        // validated, and an unparsable value cannot be shown to mean one thing.
        let err = endpoint_error("not a url");

        assert!(matches!(err, Error::EndpointInvalid { .. }), "{err:?}");
        assert!(err.to_string().contains("not a valid URL"), "{err}");
    }

    #[test]
    fn an_unsupported_scheme_is_rejected() {
        let err = endpoint_error("ftp://example.com/v1");

        assert!(matches!(err, Error::EndpointInvalid { .. }), "{err:?}");
        assert!(err.to_string().contains("ftp"), "{err}");
    }

    #[test]
    fn an_openai_compatible_provider_without_an_endpoint_is_an_error() {
        // With no endpoint there is nothing to fingerprint this provider's model by,
        // so accepting the config would mean silently fingerprinting nothing at all.
        let mut config = openai_compatible_config("https://server-a.example/v1");
        config.endpoint = None;

        let err = dependency(&config, None).unwrap_err();
        assert!(matches!(err, Error::ModelEndpointMissing { .. }), "{err:?}");
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
                    "modified_at": modified_at
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

    #[tokio::test]
    async fn an_untagged_config_id_resolves_against_a_tagged_server_entry() {
        // Ollama reports tagged names (`qwen3:latest`) and resolves an untagged
        // config id to `:latest` itself, so `id = "qwen3"` must find the model.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/tags"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "models": [{ "name": "qwen3:latest", "model": "qwen3:latest" }]
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/show"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let config = ModelConfig {
            provider: "ollama".to_string(),
            id: "qwen3".to_string(),
            endpoint: Some(server.uri()),
            params: BTreeMap::new(),
        };
        assert!(
            fetch(&client().unwrap(), &config).await.unwrap().is_some(),
            "`qwen3` must resolve against a server reporting `qwen3:latest`"
        );

        // The `:latest` fallback must stay an exact match, not a prefix match: a
        // tag-qualified id naming a different tag is still missing.
        let other = ModelConfig {
            id: "qwen3:8b".to_string(),
            ..config
        };
        let err = fetch(&client().unwrap(), &other).await.unwrap_err();
        assert!(matches!(err, Error::ModelMissing { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn a_non_json_body_is_reported_as_a_response_problem() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/tags"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let config = ModelConfig {
            provider: "ollama".to_string(),
            id: "qwen3:8b".to_string(),
            endpoint: Some(server.uri()),
            params: BTreeMap::new(),
        };

        // A 200 with an unparseable body means the server answered; reporting the
        // endpoint as unreachable sends the user to check the wrong thing.
        let err = fetch(&client().unwrap(), &config).await.unwrap_err();
        assert!(matches!(err, Error::ModelResponse { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn fetch_does_not_contact_a_non_ollama_provider() {
        let server = wiremock::MockServer::start().await;
        let config = ModelConfig {
            provider: "openai-compatible".to_string(),
            id: "gpt-4o".to_string(),
            endpoint: Some(server.uri()),
            params: BTreeMap::new(),
        };

        assert!(
            fetch(&reqwest::Client::new(), &config)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "fetch must not issue any request for a non-ollama provider"
        );
    }
}
