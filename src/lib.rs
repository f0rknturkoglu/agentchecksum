// SPDX-License-Identifier: MIT OR Apache-2.0

//! AgentChecksum: a language-agnostic dependency fingerprint and behavioral
//! regression gate for AI agents.

pub mod cli;
pub mod config;
pub mod diff;
pub mod discovery;
pub mod error;
pub mod fingerprint;
pub mod lockfile;
pub mod manifest;
pub mod report;
