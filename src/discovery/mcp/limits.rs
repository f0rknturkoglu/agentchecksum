// SPDX-License-Identifier: MIT OR Apache-2.0

//! The bounds an MCP server is discovered under.
//!
//! An MCP server is an external input, so none of these is a performance
//! preference: each one is the point where AgentChecksum refuses to be led by a
//! server that is broken, hostile, or merely enormous. They live in one file so
//! the policy is one thing to read, one thing to test, and one thing to change.
//!
//! Exceeding a bound fails the discovery. It never truncates: a truncated catalog
//! would produce a lockfile that describes a server that does not exist, and a
//! wrong checksum is worse than no checksum.

use std::time::Duration;

/// Connecting, spawning, and protocol negotiation for one server.
///
/// Generous enough for a cold `npx`-style launch, and short enough that a server
/// which never speaks does not hold a snapshot hostage.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// One `tools/list` page.
pub const PAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything after connecting, for one server: discovery metadata plus every page
/// of the catalog.
///
/// Per-page bounds alone are not a bound: a server allowed two hundred pages of
/// thirty seconds each has effectively been given no deadline at all. This is the
/// budget that keeps a broken server from holding a snapshot for an afternoon.
pub const SERVER_BUDGET: Duration = Duration::from_secs(300);

/// Closing the session. Shutdown must not be able to fail a discovery that
/// otherwise succeeded, so this is short and its expiry is a warning.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Pages followed before the catalog is called malformed. A catalog this long is
/// already abnormal; a cursor loop would otherwise be indistinguishable from real
/// pagination.
pub const MAX_TOOL_PAGES: usize = 200;

/// Tools accepted from one server.
pub const MAX_TOOLS_PER_SERVER: usize = 10_000;

/// Bytes in a tool name. The protocol's own naming guidance is far below this.
pub const MAX_TOOL_NAME_BYTES: usize = 256;

/// Bytes in a tool description or a server implementation name/version.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;

/// Bytes in one serialized tool schema. Covers any schema a tool contract needs,
/// and stops a server from making us hold an arbitrarily large document.
pub const MAX_SCHEMA_BYTES: usize = 512 * 1024;

/// Nesting depth inside one tool schema.
///
/// `serde_json` already refuses to parse deeper than its own recursion limit, so
/// this is the *tighter* bound on what we are willing to traverse and fingerprint.
/// It sits below the parser's limit on purpose: the schema normalizer recurses, and
/// a document that parses is not automatically one we want to walk.
pub const MAX_SCHEMA_DEPTH: usize = 32;
