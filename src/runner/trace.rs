// SPDX-License-Identifier: MIT OR Apache-2.0

//! The recorded evidence, and the only seam between capture and evaluation.
//!
//! Capture talks to a model and is statistical; evaluation reads one of these and is
//! a pure function. Everything the evaluator needs is in here — which probe this is,
//! which agent state produced it, which tool catalog the model saw, and what it
//! answered — so that re-evaluating a trace never touches a network, a clock, or a
//! random number generator.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};

/// The trace schema this build writes and understands.
pub const TRACE_VERSION: u32 = 1;

/// Peeked before the full parse, so a newer trace is refused as a version problem
/// rather than reported as a parse error.
#[derive(Deserialize)]
struct VersionProbe {
    trace_version: u32,
}

/// One tool call the model emitted.
///
/// `tool_id` is the canonical dependency id when the name resolves to a declared
/// tool, and `None` when the model invented one: that is behavior worth recording,
/// not a reason to fail the run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The name exactly as the model emitted it — the wire name, unrenamed.
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_id: Option<String>,
    /// Parsed arguments, when they parsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
    /// Why they did not, when they did not. Malformed arguments are evidence: the
    /// model produced them, and the metrics for this call fail accordingly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments_parse_error: Option<String>,
}

impl ToolCall {
    /// Whether the arguments are usable as JSON.
    pub fn arguments_are_parsed(&self) -> bool {
        self.arguments.is_some() && self.arguments_parse_error.is_none()
    }
}

/// One sample: one model request and what came back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub index: u32,
    /// In the order the model emitted them.
    pub tool_calls: Vec<ToolCall>,
    /// The final assistant text, when there was any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_text: Option<String>,
}

/// What produced the trace, recorded so a reader can tell two runs apart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapturedWith {
    pub runner: String,
    pub runner_version: u32,
    pub model_id: String,
    /// The parameters actually used, defaults included. A hidden default would make
    /// two runs incomparable for reasons nobody can see.
    pub effective_params: BTreeMap<String, Value>,
    /// What the model saw. The catalog decides which choices were even available, so
    /// a trace without it cannot be compared against a different one.
    pub tool_catalog_digest: String,
}

/// A complete recording of one probe's samples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trace {
    pub trace_version: u32,
    pub probe: String,
    pub probe_digest: String,
    pub agent_checksum: String,
    pub captured_with: CapturedWith,
    pub samples: Vec<Sample>,
}

impl Trace {
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut text =
            serde_json::to_string_pretty(self).map_err(|source| Error::Json { source })?;
        text.push('\n');
        Ok(text.into_bytes())
    }

    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_path_buf(),
            source,
        })?;

        if let Ok(probe) = serde_json::from_str::<VersionProbe>(&text)
            && probe.trace_version > TRACE_VERSION
        {
            return Err(Error::TraceVersion {
                path: path.to_path_buf(),
                found: probe.trace_version,
                supported: TRACE_VERSION,
            });
        }

        serde_json::from_str(&text).map_err(|source| Error::TraceInvalid {
            path: path.to_path_buf(),
            reason: source.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace() -> Trace {
        Trace {
            trace_version: TRACE_VERSION,
            probe: "repository-search".to_string(),
            probe_digest: "sha256:aa".to_string(),
            agent_checksum: "ac1:bb".to_string(),
            captured_with: CapturedWith {
                runner: "openai-chat-completions".to_string(),
                runner_version: 1,
                model_id: "qwen3:8b".to_string(),
                effective_params: BTreeMap::from([
                    ("temperature".to_string(), serde_json::json!(0.0)),
                    ("seed".to_string(), serde_json::json!(42)),
                ]),
                tool_catalog_digest: "sha256:cc".to_string(),
            },
            samples: vec![Sample {
                index: 0,
                tool_calls: vec![ToolCall {
                    name: "search_repositories".to_string(),
                    tool_id: Some("tool:github.search_repositories".to_string()),
                    arguments: Some(serde_json::json!({ "query": "postgres" })),
                    arguments_parse_error: None,
                }],
                final_text: None,
            }],
        }
    }

    #[test]
    fn a_trace_round_trips_through_its_own_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.json");
        std::fs::write(&path, trace().to_bytes().unwrap()).unwrap();

        assert_eq!(Trace::read(&path).unwrap(), trace());
    }

    #[test]
    fn a_newer_trace_is_refused_as_a_version_problem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.json");
        let mut value = serde_json::to_value(trace()).unwrap();
        value["trace_version"] = serde_json::json!(TRACE_VERSION + 1);
        std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();

        let error = Trace::read(&path).unwrap_err();
        assert!(matches!(error, Error::TraceVersion { .. }), "{error:?}");
        assert!(error.suggestion().is_some());
    }

    #[test]
    fn an_unreadable_trace_is_an_error_rather_than_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.json");
        std::fs::write(&path, "{ not json").unwrap();

        assert!(matches!(
            Trace::read(&path).unwrap_err(),
            Error::TraceInvalid { .. }
        ));
    }

    #[test]
    fn an_invented_tool_name_is_recorded_without_a_dependency_id() {
        // The model hallucinating a tool is behavior to measure, not a crash.
        let call = ToolCall {
            name: "delete_everything".to_string(),
            tool_id: None,
            arguments: Some(serde_json::json!({})),
            arguments_parse_error: None,
        };

        assert!(call.tool_id.is_none());
        assert!(call.arguments_are_parsed());

        let unparsed = ToolCall {
            arguments: None,
            arguments_parse_error: Some("expected value".to_string()),
            ..call
        };
        assert!(!unparsed.arguments_are_parsed());
    }
}
