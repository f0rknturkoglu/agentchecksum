// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::discovery::Discovery;
use crate::lockfile::Lockfile;
use crate::manifest::DependencyKind;

/// The `snapshot` summary, in the shape described by the spec.
pub fn snapshot(lock: &Lockfile, discovery: &Discovery) -> String {
    let mut out = String::from("Agent checksum generated.\n\nChecksum:\n");
    out.push_str(lock.agent_checksum.as_str());
    out.push_str("\n\nDependencies:\n");

    for kind in [
        DependencyKind::Model,
        DependencyKind::Prompt,
        DependencyKind::Tool,
        DependencyKind::McpServer,
    ] {
        let count = discovery
            .dependencies
            .iter()
            .filter(|dependency| dependency.kind == kind)
            .count();
        if count == 0 {
            continue;
        }
        let plural = if count == 1 { "" } else { "s" };
        match kind {
            DependencyKind::McpServer => out.push_str(&format!("{count} MCP server{plural}\n")),
            other => out.push_str(&format!("{count} {}{plural}\n", other.as_str())),
        }
    }

    for warning in &discovery.warnings {
        out.push_str(&format!("\nwarning: {warning}\n"));
    }

    out
}
