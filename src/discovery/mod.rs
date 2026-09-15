// SPDX-License-Identifier: MIT OR Apache-2.0

//! Dependency discovery. All I/O lives here; everything downstream is pure.

pub mod model;
pub mod prompts;

use std::path::Path;

use crate::config::Config;
use crate::error::Result;
use crate::manifest::Dependency;

/// The result of a discovery pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Discovery {
    pub dependencies: Vec<Dependency>,
    /// Non-fatal observations that must be visible to the user rather than
    /// silently swallowed (for example: a provider that exposes no model digest).
    pub warnings: Vec<String>,
}

/// Run every configured discovery source.
pub async fn run(config: &Config, root: &Path) -> Result<Discovery> {
    let mut dependencies = prompts::discover(config, root)?;
    let mut warnings = Vec::new();

    if let Some(model) = &config.model {
        let client = model::client()?;
        let metadata = model::fetch(&client, model).await?;
        let (dependency, model_warnings) = model::dependency(model, metadata.as_ref())?;
        dependencies.push(dependency);
        warnings.extend(model_warnings);
    }

    Ok(Discovery {
        dependencies,
        warnings,
    })
}
