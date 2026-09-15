// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use crate::config::Config;
use crate::discovery::{self, Discovery};
use crate::error::Result;
use crate::lockfile::Lockfile;

/// Discover dependencies and write the lockfile.
pub async fn run(
    root: &Path,
    config_path: &Path,
    lock_path: &Path,
) -> Result<(Lockfile, Discovery)> {
    let config = Config::load(config_path)?;
    let discovery = discovery::run(&config, root).await?;
    let lock = Lockfile::from_dependencies(&discovery.dependencies)?;
    lock.write(lock_path)?;
    Ok((lock, discovery))
}
