// SPDX-License-Identifier: MIT OR Apache-2.0

//! Dependency discovery. All I/O lives here; everything downstream is pure.

pub mod prompts;

use crate::manifest::Dependency;

/// The result of a discovery pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Discovery {
    pub dependencies: Vec<Dependency>,
    /// Non-fatal observations that must be visible to the user rather than
    /// silently swallowed (for example: a provider that exposes no model digest).
    pub warnings: Vec<String>,
}
