// SPDX-License-Identifier: MIT OR Apache-2.0

//! Behavioral probes: what to ask an agent, what to expect from it, and what a run
//! actually measured.
//!
//! Probes are declared, not inferred. A probe says what a correct decision looks
//! like — a tool that should be chosen, arguments that should carry a phrase, tools
//! that must be avoided, a shape the answer should have — and everything downstream
//! of a captured trace is a pure function of that declaration.
//!
//! The distinction the module is built around: **capture** talks to a model and is
//! statistical; **evaluation** reads a recorded trace and is deterministic. Loading
//! (`load`) and resolving (`resolve`) sit on the near side of the seam; `eval` and
//! `metrics` are on the far side and touch nothing but their arguments.

pub mod digest;
pub mod eval;
pub mod load;
pub mod matchers;
pub mod metrics;
pub mod model;
pub mod resolve;

pub use digest::{ProbeIdentity, suite_digest};
pub use eval::{Check, ProbeOutcome, SampleOutcome, aggregate, compile_schema, evaluate};
pub use load::{ProbeSuite, load_suite};
pub use matchers::Matcher;
pub use metrics::{Metric, MetricScore, MetricScores};
pub use model::{
    DEFAULT_REPEAT, MAX_EXPECT_ARG_ENTRIES, MAX_FORBID_TOOLS, MAX_NAME_BYTES,
    MAX_OUTPUT_SCHEMA_BYTES, MAX_PROBE_FILES, MAX_PROBES, MAX_PROMPT_BYTES, MAX_REPEAT, MIN_REPEAT,
    OutputSchema, ResolvedExpectations, ResolvedProbe, is_valid_name,
};
pub use resolve::{ResolvedTool, resolve_tool, resolve_tools};
