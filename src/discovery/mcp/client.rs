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
    ClientCapabilities, ClientConfig, ErrorCode, Implementation, MetaObject,
    PaginatedRequestParams, ProtocolVersion, RequestMetaObject,
};
use rmcp::service::{
    ClientCacheConfig, ClientInitializeError, ClientLifecycleMode, ClientServiceExt, RoleClient,
    RunningService,
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

/// The stateless handshake, and nothing else.
///
/// `Discover` is deliberately used instead of the SDK's `Auto`, which also falls
/// back to the session lifecycle when the modern probe does not answer *in time*.
/// That would let transient latency decide the protocol era — and therefore the
/// fingerprint: a server under load would be recorded as a legacy server, and the
/// same declarations would produce a different dependency identity run to run. In
/// this mode the SDK never falls back on its own, so the fallback below happens only
/// where the peer explicitly said it is legacy.
fn discover_lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Discover {
        preferred_versions: preferred_versions(),
    }
}

/// The session handshake, for a peer that told us it is legacy.
fn legacy_lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Initialize
}

/// Whether a failed modern probe is evidence that the peer is a legacy server.
///
/// The one signal that means this is a *correlated JSON-RPC error that is not a
/// modern-era rejection*: the server understood the request and answered that it
/// does not implement the method. That classification lives in the SDK's private
/// `DiscoverOutcome`, so the same two-line test is repeated here against the public
/// error-code constants.
///
/// Everything else is a failure, and none of it is evidence about the peer's age:
/// a timeout (the server was slow), a transport or TLS error (it was unreachable), a
/// TLS or authorization rejection, an uncorrelated or malformed response, and the
/// modern rejections `-32021` (a capability the client must have) and `-32020`
/// (header mismatch) — in fact the last two explicitly say the peer *is* modern.
///
/// One transport fact rather than a policy choice: over Streamable HTTP the SDK's own
/// client transport synthesises a correlated error of this shape when a sessionless
/// `server/discover` is answered with a 4xx other than 401/403, which is its way of
/// saying the endpoint serves the session protocol. The fallback therefore fires
/// there too — and it still has to succeed to fingerprint anything.
fn is_legacy_signal(error: &ClientInitializeError) -> bool {
    let ClientInitializeError::JsonRpcError(data) = error else {
        return false;
    };
    !matches!(
        data.code,
        ErrorCode::MISSING_REQUIRED_CLIENT_CAPABILITY | ErrorCode::HEADER_MISMATCH
    )
}

/// The client identity we declare.
///
/// Stable and self-describing, and nothing else: no hostname, no user, no working
/// directory, no random value. A server that groups or rate-limits clients by
/// identity must see the same client on every run, and a fingerprint must not
/// depend on which machine produced it.
fn client_config(protocol_version: ProtocolVersion) -> ClientConfig {
    let mut config = ClientConfig::default();
    config.client_info = Implementation::new("agentchecksum", env!("CARGO_PKG_VERSION"));
    // The smallest set that is true. Discovery reads; it does not sample, does not
    // offer roots, and does not render UI, so it must not claim it can — a server is
    // entitled to change what it exposes based on what a client says it supports.
    config.capabilities = ClientCapabilities::default();
    config.protocol_version = protocol_version;
    config
}

/// Discover one configured server.
pub async fn discover(server: &McpServerConfig) -> Result<DiscoveredServer> {
    let secrets = Secrets::from_config(server);

    match server.transport {
        Transport::Stdio => {
            discover_over(
                server,
                "stdio",
                || stdio_transport(server, &secrets),
                &secrets,
            )
            .await
        }
        Transport::StreamableHttp => {
            discover_over(
                server,
                "streamable-http",
                || http_transport(server, &secrets),
                &secrets,
            )
            .await
        }
    }
}

/// Establish a session, with a fallback that only the peer can trigger.
///
/// The transport is built per attempt because the first attempt consumes it: a
/// legacy fallback therefore opens a second connection or spawns a second child.
/// That cost is paid only on the path a server explicitly asked for, and it buys the
/// rule this function exists to enforce — nothing about timing, reachability, or
/// authorization can move a dependency from one protocol era to another.
async fn connect<T, E, A>(
    server: &McpServerConfig,
    transport: &'static str,
    open: impl Fn() -> Result<T>,
    secrets: &Secrets,
) -> Result<RunningService<RoleClient, ClientConfig>>
where
    T: IntoTransport<RoleClient, E, A>,
    E: StdError + Send + Sync + 'static,
{
    let alias = server.name.as_str();

    let modern = client_config(ProtocolVersion::V_2026_07_28)
        .serve_with_lifecycle(open()?, discover_lifecycle());
    match tokio::time::timeout(limits::CONNECT_TIMEOUT, modern).await {
        Ok(Ok(client)) => return Ok(client),
        Ok(Err(error)) if is_legacy_signal(&error) => {
            tracing::debug!(
                server = alias,
                "the peer answered the stateless handshake as a legacy server; retrying \
                 with the session lifecycle"
            );
        }
        Ok(Err(error)) => {
            return Err(secrets.failed(
                alias,
                transport,
                "connecting and negotiating",
                &describe(&error),
            ));
        }
        Err(_) => {
            return Err(secrets.timeout(
                alias,
                transport,
                "connecting and negotiating",
                limits::CONNECT_TIMEOUT,
            ));
        }
    }

    // Only reachable from the branch above: the peer gave the correlated error that
    // means "I do not implement that method".
    let legacy = client_config(ProtocolVersion::V_2025_11_25)
        .serve_with_lifecycle(open()?, legacy_lifecycle());
    match tokio::time::timeout(limits::CONNECT_TIMEOUT, legacy).await {
        Ok(Ok(client)) => Ok(client),
        Ok(Err(error)) => Err(secrets.failed(
            alias,
            transport,
            "connecting with the legacy lifecycle",
            &describe(&error),
        )),
        Err(_) => Err(secrets.timeout(
            alias,
            transport,
            "connecting with the legacy lifecycle",
            limits::CONNECT_TIMEOUT,
        )),
    }
}

/// The shared session: connect, introspect, close.
async fn discover_over<T, E, A>(
    server: &McpServerConfig,
    transport: &'static str,
    open: impl Fn() -> Result<T>,
    secrets: &Secrets,
) -> Result<DiscoveredServer>
where
    T: IntoTransport<RoleClient, E, A>,
    E: StdError + Send + Sync + 'static,
{
    let alias = server.name.as_str();

    let mut client = connect(server, transport, open, secrets).await?;

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
    let peer = client.peer_info().ok_or_else(|| {
        secrets.failed(
            alias,
            transport,
            "negotiating the protocol",
            "the server reported no protocol information",
        )
    })?;

    // Optional in the protocol, so its absence is recorded rather than fatal: a valid
    // server that does not expose discovery metadata still has a tool contract worth
    // fingerprinting.
    //
    // This costs one extra request per server, deliberately. The startup probe keeps
    // its result to itself — `peer_info()` exposes the negotiated version,
    // capabilities, and server info, but not `supported_versions` — and it does not
    // seed the response cache either, so asking is the only way to learn what the
    // server says it supports. That answer is part of the contract this layer records
    // and it is inside the same bounded budget.
    let (supported_versions, mut warnings) =
        supported_versions(client, alias, transport, secrets).await;

    // A snapshot must describe *now*, so response caching is off for everything that
    // follows. The SDK's default is to serve a stale list entry when a refresh fails,
    // which would let an outage produce a confident fingerprint of the previous
    // contract; one-shot discovery has nothing to gain from a cache and the wrong
    // answer to lose.
    client
        .set_response_cache_config(ClientCacheConfig::disabled())
        .await;

    let instructions =
        normalize::instructions(peer.instructions.as_deref()).map_err(|rejected| {
            as_diagnostic(
                alias,
                transport,
                "reading the server identity",
                &rejected,
                secrets,
            )
        })?;

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

    // One line for the category, like the encoded-name warning: a server with fifty
    // tools carrying `_meta` must not produce fifty lines, and the values themselves
    // are never read, so there is nothing per-tool to say.
    let opaque = tools.iter().filter(|tool| tool.opaque_metadata).count();
    if opaque > 0 {
        warnings.push(format!(
            "{opaque} tool{} declared opaque MCP metadata; metadata values are intentionally not \
             fingerprinted",
            if opaque == 1 { "" } else { "s" }
        ));
    }

    Ok(DiscoveredServer {
        alias: alias.to_string(),
        identity,
        instructions,
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

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ErrorData;

    fn json_rpc(code: i32) -> ClientInitializeError {
        ClientInitializeError::JsonRpcError(ErrorData::new(
            ErrorCode(code),
            "fixture".to_string(),
            None,
        ))
    }

    /// The decision that keeps timing out of the fingerprint.
    ///
    /// Only a *correlated JSON-RPC error that is not a modern-era rejection* means
    /// the peer is legacy. Everything else is a failure, and treating any of it as
    /// legacy would let latency, an outage, or an authorization problem move a
    /// dependency from one protocol era to another.
    #[test]
    fn only_an_explicit_legacy_answer_may_trigger_the_fallback() {
        // "I do not implement that method" — the signal, and the only one.
        assert!(is_legacy_signal(&json_rpc(-32601)));
        assert!(is_legacy_signal(&json_rpc(-32600)));

        // The two codes the protocol reserves for a *modern* peer refusing a request.
        assert!(!is_legacy_signal(&json_rpc(
            ErrorCode::MISSING_REQUIRED_CLIENT_CAPABILITY.0
        )));
        assert!(!is_legacy_signal(&json_rpc(ErrorCode::HEADER_MISMATCH.0)));

        // Everything that is not the peer answering a method question at all.
        assert!(!is_legacy_signal(&ClientInitializeError::ConnectionClosed(
            "closed".to_string()
        )));
        assert!(!is_legacy_signal(
            &ClientInitializeError::ExpectedInitResult(None)
        ));
        assert!(!is_legacy_signal(
            &ClientInitializeError::UncorrelatedErrorResponse {
                expected: rmcp::model::RequestId::Number(1),
                received: rmcp::model::NumberOrString::Number(2),
            }
        ));
    }

    /// The two preferred revisions are named, never taken from the SDK's `LATEST`.
    #[test]
    fn the_stateless_revision_is_asked_for_by_name() {
        let preferred = preferred_versions();
        assert_eq!(
            preferred.first().map(ProtocolVersion::as_str),
            Some("2026-07-28")
        );
        assert_ne!(
            preferred.first().map(ProtocolVersion::as_str),
            Some(ProtocolVersion::LATEST.as_str()),
            "the SDK's LATEST is still the previous revision, which would negotiate a session"
        );

        // And the two steps are distinct: the first never falls back on its own.
        assert!(matches!(
            discover_lifecycle(),
            ClientLifecycleMode::Discover { .. }
        ));
        assert!(matches!(
            legacy_lifecycle(),
            ClientLifecycleMode::Initialize
        ));
    }
}
