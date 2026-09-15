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
    // Checked before discovery, not merely before the write: the refusal must
    // cost nothing and must not hide behind an unrelated failure — discovery can
    // reach the network, and a config error would mask the real reason.
    Lockfile::ensure_writable(lock_path)?;

    let config = Config::load(config_path)?;
    let discovery = discovery::run(&config, root).await?;
    let lock = Lockfile::from_dependencies(&discovery.dependencies)?;
    lock.write(lock_path)?;
    Ok((lock, discovery))
}
