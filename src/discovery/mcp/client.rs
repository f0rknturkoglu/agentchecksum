// SPDX-License-Identifier: MIT OR Apache-2.0

//! The I/O edge: connect, introspect, close.
//!
//! One server at a time, one connection, closed before the next one starts. Every
//! stage is bounded, and every failure path closes the session first: an orphaned
//! MCP server is a process leak, and a leaked process is worse than a failed
//! snapshot because nothing reports it.
//!
//! Nothing in this module decides severity or shapes a dependency. It produces a
//! [`DiscoveredServer`] and hands it to the pure layer.

use std::error::Error as StdError;
use std::process::Stdio;
use std::time::Duration;

use rmcp::model::{
    ClientCapabilities, ClientConfig, Implementation, MetaObject, PaginatedRequestParams,
    ProtocolVersion, RequestMetaObject,
};
use rmcp::service::{
    ClientCacheConfig, ClientLifecycleMode, ClientServiceExt, RoleClient, RunningService,
};
use rmcp::transport::{
    ConfigureCommandExt, IntoTransport, StreamableHttpClientTransport, TokioChildProcess,
};
use tokio::process::Command;

use crate::config::{McpServerConfig, Transport};
use crate::error::{Error, Result};

use super::limits;
use super::normalize::{self, Rejected};
use super::{DiscoveredServer, DiscoveredTool};

/// The protocol revisions AgentChecksum asks for, most preferred first.
///
/// The stateless revision is named explicitly because the SDK's own `LATEST`
/// constant still points at the previous one: asking for `LATEST` here would
/// negotiate a session protocol on a server that supports both, and the fingerprint
/// would then describe an era the server does not have to be in.
fn preferred_versions() -> Vec<ProtocolVersion> {
    vec![ProtocolVersion::V_2026_07_28, ProtocolVersion::V_2025_11_25]
}

/// How the session is established.
///
/// `Auto` is the SDK's compatibility path: it probes with the stateless handshake
/// and falls back to the session protocol only when the server answers with a
/// correlated JSON-RPC error — a legacy server saying "I do not know that method".
/// A transport failure, a TLS problem, an authorization rejection, or a malformed
/// response is *not* a fallback: it propagates, because retrying those as legacy
/// would turn an outage into a fingerprint of something else.
fn lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Auto {
        preferred_versions: preferred_versions(),
        legacy_version: Some(ProtocolVersion::V_2025_11_25),
    }
}

/// The client identity we declare.
///
/// Stable and self-describing, and nothing else: no hostname, no user, no working
/// directory, no random value. A server that groups or rate-limits clients by
/// identity must see the same client on every run, and a fingerprint must not
/// depend on which machine produced it.
fn client_config() -> ClientConfig {
    let mut config = ClientConfig::default();
    config.client_info = Implementation::new("agentchecksum", env!("CARGO_PKG_VERSION"));
    // The smallest set that is true. Discovery reads; it does not sample, does not
    // offer roots, and does not render UI, so it must not claim it can — a server is
    // entitled to change what it exposes based on what a client says it supports.
    config.capabilities = ClientCapabilities::default();
    config.protocol_version = ProtocolVersion::V_2026_07_28;
    config
}

/// Discover one configured server.
pub async fn discover(server: &McpServerConfig) -> Result<DiscoveredServer> {
    let secrets = Secrets::from_config(server);

    match server.transport {
        Transport::Stdio => {
            let transport = stdio_transport(server, &secrets)?;
            discover_over(server, "stdio", transport, &secrets).await
        }
        Transport::StreamableHttp => {
            let transport = http_transport(server, &secrets)?;
            discover_over(server, "streamable-http", transport, &secrets).await
        }
    }
}

/// The shared session: connect, introspect, close.
async fn discover_over<T, E, A>(
    server: &McpServerConfig,
    transport: &'static str,
    transport_impl: T,
    secrets: &Secrets,
) -> Result<DiscoveredServer>
where
    T: IntoTransport<RoleClient, E, A>,
    E: StdError + Send + Sync + 'static,
{
    let alias = server.name.as_str();

    let connecting = client_config().serve_with_lifecycle(transport_impl, lifecycle());
    let mut client = tokio::time::timeout(limits::CONNECT_TIMEOUT, connecting)
        .await
        .map_err(|_| {
            secrets.timeout(
                alias,
                transport,
                "connecting and negotiating",
                limits::CONNECT_TIMEOUT,
            )
        })?
        .map_err(|error| {
            secrets.failed(
                alias,
                transport,
                "connecting and negotiating",
                &describe(&error),
            )
        })?;

    let introspected = match tokio::time::timeout(
        limits::SERVER_BUDGET,
        introspect(&client, alias, transport, secrets),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(secrets.timeout(alias, transport, "discovery", limits::SERVER_BUDGET)),
    };

    // Closing is not optional and not best-effort: the SDK hands the child process
    // to a cleanup task that only runs when the service is closed, and its `Drop`
    // path is asynchronous. Waiting here is what makes "no orphaned servers" a
    // property of the command rather than a hope.
    let closing = tokio::time::timeout(limits::SHUTDOWN_TIMEOUT, client.close()).await;

    let mut discovered = introspected?;
    match closing {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => discovered.warnings.push(format!(
            "the session did not close cleanly: {}",
            describe(&error)
        )),
        Err(_) => discovered.warnings.push(format!(
            "the session did not close within {}s",
            limits::SHUTDOWN_TIMEOUT.as_secs()
        )),
    }

    Ok(discovered)
}

async fn introspect(
    client: &RunningService<RoleClient, ClientConfig>,
    alias: &str,
    transport: &'static str,
    secrets: &Secrets,
) -> Result<DiscoveredServer> {
    // A snapshot must describe *now*. The SDK caches `tools/list` per session and,
    // by default, serves a stale entry when a refresh fails — which would let an
    // outage produce a confident fingerprint of the previous contract. One-shot
    // discovery has nothing to gain from a cache and the wrong answer to lose.
    client
        .set_response_cache_config(ClientCacheConfig::disabled())
        .await;

    let peer = client.peer_info().ok_or_else(|| {
        secrets.failed(
            alias,
            transport,
            "negotiating the protocol",
            "the server reported no protocol information",
        )
    })?;

    // Optional in the protocol, so its absence is recorded rather than fatal: a
    // valid server that does not expose discovery metadata still has a tool
    // contract worth fingerprinting.
    let (supported_versions, mut warnings) =
        supported_versions(client, alias, transport, secrets).await;

    let (identity, identity_warnings) = normalize::identity(
        &peer.protocol_version,
        &peer.capabilities,
        peer.server_info.as_ref(),
        supported_versions.as_deref(),
    )
    .map_err(|rejected| {
        as_diagnostic(
            alias,
            transport,
            "reading the server identity",
            &rejected,
            secrets,
        )
    })?;
    warnings.extend(identity_warnings);

    let tools = list_tools(client, alias, transport, secrets).await?;

    // One line for the category rather than one per tool: a server with fifty
    // unusual names must not produce fifty warnings, and the names themselves are
    // already in the lockfile.
    let encoded = tools
        .iter()
        .filter(|tool| super::name_was_encoded(&tool.name))
        .count();
    if encoded > 0 {
        warnings.push(format!(
            "{encoded} tool name{} outside the plain identifier grammar; their dependency ids are \
             percent-encoded",
            if encoded == 1 { " is" } else { "s are" }
        ));
    }

    Ok(DiscoveredServer {
        alias: alias.to_string(),
        identity,
        tools,
        warnings,
    })
}

/// The versions the server says it implements, when it says so.
async fn supported_versions(
    client: &RunningService<RoleClient, ClientConfig>,
    alias: &str,
    transport: &'static str,
    secrets: &Secrets,
) -> (Option<Vec<ProtocolVersion>>, Vec<String>) {
    let asking = client.discover(RequestMetaObject(MetaObject::default()));
    match tokio::time::timeout(limits::PAGE_TIMEOUT, asking).await {
        Ok(Ok(result)) => (Some(result.supported_versions), Vec::new()),
        Ok(Err(error)) => (
            None,
            vec![format!(
                "the server did not report its supported protocol versions ({}); only the negotiated version is recorded",
                describe(&error)
            )],
        ),
        Err(_) => {
            let _ = (alias, transport, secrets);
            (
                None,
                vec!["the server did not report its supported protocol versions in time; only the negotiated version is recorded".to_string()],
            )
        }
    }
}

/// The complete tool catalog, page by page.
///
/// Pagination is driven here rather than by the SDK's convenience method so that
/// the bounds are ours and are testable: a server cannot make us read a hundred
/// thousand tools, and a cursor that comes back around is an error rather than an
/// infinite loop.
async fn list_tools(
    client: &RunningService<RoleClient, ClientConfig>,
    alias: &str,
    transport: &'static str,
    secrets: &Secrets,
) -> Result<Vec<DiscoveredTool>> {
    let mut tools: Vec<DiscoveredTool> = Vec::new();
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut cursors: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0usize;

    loop {
        pages += 1;
        if pages > limits::MAX_TOOL_PAGES {
            return Err(secrets.failed(
                alias,
                transport,
                "reading the tool catalog",
                &format!(
                    "more than {} pages of tools; the catalog is not bounded",
                    limits::MAX_TOOL_PAGES
                ),
            ));
        }

        let params = cursor
            .clone()
            .map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor)));
        let listing = client.list_tools(params);
        let page = tokio::time::timeout(limits::PAGE_TIMEOUT, listing)
            .await
            .map_err(|_| {
                secrets.timeout(
                    alias,
                    transport,
                    "reading a page of tools",
                    limits::PAGE_TIMEOUT,
                )
            })?
            .map_err(|error| {
                secrets.failed(
                    alias,
                    transport,
                    "reading the tool catalog",
                    &describe(&error),
                )
            })?;

        for declared in &page.tools {
            let tool = normalize::tool(declared).map_err(|rejected| {
                as_diagnostic(
                    alias,
                    transport,
                    "reading the tool catalog",
                    &rejected,
                    secrets,
                )
            })?;

            // Two tools with one name is not a catalog we can describe: taking the
            // first or the last would hide a dependency behind another one.
            if !names.insert(tool.name.clone()) {
                return Err(secrets.failed(
                    alias,
                    transport,
                    "reading the tool catalog",
                    &format!(
                        "the server declared the tool `{}` more than once",
                        tool.name
                    ),
                ));
            }
            tools.push(tool);

            if tools.len() > limits::MAX_TOOLS_PER_SERVER {
                return Err(secrets.failed(
                    alias,
                    transport,
                    "reading the tool catalog",
                    &format!(
                        "more than {} tools on one server",
                        limits::MAX_TOOLS_PER_SERVER
                    ),
                ));
            }
        }

        match page.next_cursor {
            None => break,
            Some(next) => {
                if !cursors.insert(next.clone()) {
                    return Err(secrets.failed(
                        alias,
                        transport,
                        "reading the tool catalog",
                        &format!("the server repeated the cursor `{next}`"),
                    ));
                }
                cursor = Some(next);
            }
        }
    }

    // A server answers in whatever order it likes, and discovery order must never
    // reach the fingerprint.
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(tools)
}

/// A stdio server, spawned directly.
///
/// No shell is involved, and no shell is searched for: the configured command is
/// executed with the configured argument vector, which is both what the user asked
/// for and what keeps a name like `echo; rm -rf` an argument instead of a second
/// command.
fn stdio_transport(server: &McpServerConfig, secrets: &Secrets) -> Result<TokioChildProcess> {
    let Some(command) = server.command.as_deref() else {
        // Config validation refuses this combination, so reaching it means a caller
        // built the value directly.
        return Err(secrets.failed(
            &server.name,
            "stdio",
            "starting the server",
            "no command is configured",
        ));
    };

    let args = server.args.clone();
    let env = server.env.clone();

    let command = Command::new(command).configure(|cmd| {
        cmd.args(&args);
        for (key, value) in &env {
            cmd.env(key, value);
        }
    });

    // The protocol speaks on stdout; stderr is the server's own log, and it is
    // discarded here rather than on the `Command`: this transport re-applies its own
    // stdio defaults when it spawns, so a `Command` setting is overridden — and its
    // default is `inherit`, which would send a server's log lines straight to this
    // process's stderr before any redaction could see them. A server is free to log
    // whatever it likes, including the credentials it was started with.
    let (child, _stderr) = TokioChildProcess::builder(command)
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            // The reason comes from the operating system, never from the configured
            // environment: a failed spawn must not print what the server would have
            // been given.
            secrets.failed(
                &server.name,
                "stdio",
                "starting the server",
                &describe(&error),
            )
        })?;
    Ok(child)
}

/// A Streamable HTTP server.
///
/// The URL was validated by the config layer: `http` or `https`, no credentials, no
/// query, no fragment. Redirects are left to the SDK, which does not follow them —
/// that is the behavior we want, because a redirect moves the trust boundary and a
/// user who needs another endpoint should configure it.
fn http_transport(
    server: &McpServerConfig,
    secrets: &Secrets,
) -> Result<StreamableHttpClientTransport<reqwest::Client>> {
    let Some(url) = server.url.as_deref() else {
        return Err(secrets.failed(
            &server.name,
            "streamable-http",
            "connecting",
            "no url is configured",
        ));
    };

    Ok(StreamableHttpClientTransport::from_uri(url))
}

/// A failure, with the configured environment redacted out of it.
///
/// The values in `[mcp.servers.env]` are connection material. They are not
/// fingerprinted, they are not written anywhere, and they must not be readable in a
/// diagnostic either — which matters because the text of a failure can come from
/// the server, and a server is free to echo back whatever it was started with.
struct Secrets(Vec<String>);

impl Secrets {
    fn from_config(server: &McpServerConfig) -> Self {
        // Short values are excluded: redacting `1` or `true` from every message
        // would mangle diagnostics without protecting anything, since a value that
        // short carries no secret worth hiding.
        const MIN_REDACTED: usize = 6;
        Self(
            server
                .env
                .values()
                .filter(|value| value.len() >= MIN_REDACTED)
                .cloned()
                .collect(),
        )
    }

    fn redact(&self, text: &str) -> String {
        let mut redacted = text.to_string();
        for secret in &self.0 {
            if redacted.contains(secret.as_str()) {
                redacted = redacted.replace(secret.as_str(), "[redacted]");
            }
        }
        redacted
    }

    fn failed(&self, server: &str, transport: &str, stage: &str, reason: &str) -> Error {
        Error::McpFailed {
            server: server.to_string(),
            transport: transport.to_string(),
            stage: stage.to_string(),
            reason: self.redact(reason),
        }
    }

    fn timeout(&self, server: &str, transport: &str, stage: &str, budget: Duration) -> Error {
        Error::McpTimeout {
            server: server.to_string(),
            transport: transport.to_string(),
            stage: stage.to_string(),
            seconds: budget.as_secs(),
        }
    }
}

/// A failure's own text, for a diagnostic.
fn describe(error: &dyn StdError) -> String {
    error.to_string()
}

/// Turn a rejected declaration into a diagnostic that names the server and the unit
/// that failed.
fn as_diagnostic(
    server: &str,
    transport: &'static str,
    stage: &str,
    rejected: &Rejected,
    secrets: &Secrets,
) -> Error {
    secrets.failed(
        server,
        transport,
        stage,
        &format!("{}: {}", rejected.subject, rejected.reason),
    )
}
