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
use super::{DiscoveredServer, DiscoveredTool, Secrets};

/// The revision the stateless lifecycle speaks.
///
/// Named explicitly because the SDK's own `LATEST` constant still points at the
/// previous, session-based one: asking for `LATEST` would negotiate a session
/// protocol against a server that supports both, and the fingerprint would then
/// describe an era the server does not have to be in.
const STATELESS_VERSION: ProtocolVersion = ProtocolVersion::V_2026_07_28;

/// The revision the session lifecycle speaks.
///
/// A separate concept from the candidate list below, deliberately: it is not a
/// fallback *version*, it is the revision used only after a peer has said, in so many
/// words, that it does not implement `server/discover`.
const LEGACY_VERSION: ProtocolVersion = ProtocolVersion::V_2025_11_25;

/// The revisions the stateless handshake may negotiate.
///
/// One entry, because the stateless era is one revision and anything newer is a
/// revision of the same era. Listing the session revision here would let a server
/// answer `server/discover`, negotiate a session revision, and be recorded as legacy
/// through a lifecycle that was never the session one — an era that disagrees with
/// the handshake that produced it.
fn preferred_versions() -> Vec<ProtocolVersion> {
    vec![STATELESS_VERSION]
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
/// Exactly one signal qualifies: the peer answered `server/discover` with
/// **`METHOD_NOT_FOUND`**, the protocol's own way of saying "I do not implement that
/// method". Nothing else is evidence about the peer's age, and nothing else may move
/// a dependency from one protocol era to another:
///
/// * `-32600`, `-32602`, and `-32603` are the peer saying the request was wrong or
///   that it is unwell. A transient internal error acting as a legacy signal would
///   re-fingerprint an entire server as something it may not be.
/// * `-32021` and `-32020` are explicit *modern* rejections.
/// * `-32022` is consumed by the SDK's own version-retry loop before this point.
/// * A timeout says the peer was slow, a transport or TLS error says it was
///   unreachable, an authorization rejection says we may not talk to it, and an
///   uncorrelated or malformed response says it is broken. None of that is evidence
///   of a protocol era.
///
/// One consequence, stated because it is a transport fact rather than a policy
/// choice: the SDK's Streamable HTTP transport synthesises a correlated `-32600` when
/// a sessionless `server/discover` is answered with a 4xx other than 401/403. That is
/// *not* method-not-found, so this build does not fall back over that transport — a
/// legacy server reachable only over HTTP is reported as a discovery failure instead
/// of being fingerprinted through a lifecycle the transport guessed at.
fn is_legacy_signal(error: &ClientInitializeError) -> bool {
    let ClientInitializeError::JsonRpcError(data) = error else {
        return false;
    };
    data.code == ErrorCode::METHOD_NOT_FOUND
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

    let modern =
        client_config(STATELESS_VERSION).serve_with_lifecycle(open()?, discover_lifecycle());
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
                &secrets.diagnostic(&error),
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
    let legacy = client_config(LEGACY_VERSION).serve_with_lifecycle(open()?, legacy_lifecycle());
    match tokio::time::timeout(limits::CONNECT_TIMEOUT, legacy).await {
        Ok(Ok(client)) => Ok(client),
        Ok(Err(error)) => Err(secrets.failed(
            alias,
            transport,
            "connecting with the legacy lifecycle",
            &secrets.diagnostic(&error),
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
            secrets.diagnostic(&error)
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
    let (supported_versions, mut warnings) = supported_versions(client, secrets).await;

    // A snapshot must describe *now*, so response caching is off for everything that
    // follows. The SDK's default is to serve a stale list entry when a refresh fails,
    // which would let an outage produce a confident fingerprint of the previous
    // contract; one-shot discovery has nothing to gain from a cache and the wrong
    // answer to lose.
    client
        .set_response_cache_config(ClientCacheConfig::disabled())
        .await;

    let instructions =
        normalize::instructions(peer.instructions.as_deref(), secrets).map_err(|rejected| {
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
        secrets,
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
///
/// Takes no server identity: its only failure mode is a warning, and the warning
/// text is sanitized before the caller attaches the alias to it.
async fn supported_versions(
    client: &RunningService<RoleClient, ClientConfig>,
    secrets: &Secrets,
) -> (Option<Vec<ProtocolVersion>>, Vec<String>) {
    let asking = client.discover(RequestMetaObject(MetaObject::default()));
    match tokio::time::timeout(limits::PAGE_TIMEOUT, asking).await {
        Ok(Ok(result)) => (Some(result.supported_versions), Vec::new()),
        Ok(Err(error)) => (
            None,
            vec![format!(
                "the server did not report its supported protocol versions ({}); only the negotiated version is recorded",
                secrets.diagnostic(&error)
            )],
        ),
        Err(_) => {
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
                    &secrets.diagnostic(&error),
                )
            })?;

        for declared in &page.tools {
            let tool = normalize::tool(declared, secrets).map_err(|rejected| {
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
                &secrets.diagnostic(&error),
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

/// Turn a rejected declaration into a diagnostic that names the server and the unit
/// that failed.
fn as_diagnostic(
    server: &str,
    transport: &'static str,
    stage: &str,
    rejected: &Rejected,
    secrets: &Secrets,
) -> Error {
    // A reflected credential is not a malformed declaration: it is connection
    // material where a contract should be, and the message says exactly that while
    // naming only the location.
    let reason = if rejected.reflection {
        format!("it {} in {}", rejected.reason, rejected.subject)
    } else {
        format!("{}: {}", rejected.subject, rejected.reason)
    };
    secrets.failed(server, transport, stage, &reason)
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

    /// The decision that keeps everything except an explicit answer out of the
    /// fingerprint.
    ///
    /// Only `METHOD_NOT_FOUND` means the peer is legacy: the protocol's own way of
    /// saying "I do not implement that method". Every other failure is just a
    /// failure, and treating one as legacy would let a transient error, an outage, or
    /// an authorization problem re-fingerprint an entire server as another era.
    #[test]
    fn only_method_not_found_is_evidence_of_a_legacy_peer() {
        assert!(is_legacy_signal(&json_rpc(-32601)));

        // The peer saying the request was wrong, or that it is unwell.
        assert!(!is_legacy_signal(&json_rpc(-32600)));
        assert!(!is_legacy_signal(&json_rpc(-32602)));
        assert!(!is_legacy_signal(&json_rpc(-32603)));

        // Explicit modern rejections, and the version error the SDK's retry loop
        // consumes before classification can happen at all.
        assert!(!is_legacy_signal(&json_rpc(
            ErrorCode::MISSING_REQUIRED_CLIENT_CAPABILITY.0
        )));
        assert!(!is_legacy_signal(&json_rpc(ErrorCode::HEADER_MISMATCH.0)));
        assert!(!is_legacy_signal(&json_rpc(
            ErrorCode::UNSUPPORTED_PROTOCOL_VERSION.0
        )));

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
        assert!(!is_legacy_signal(&ClientInitializeError::TransportError {
            // `from_parts` exists precisely for fixtures; the transport error
            // carries an I/O failure and must not be mistaken for an age signal.
            error: rmcp::transport::DynamicTransportError::from_parts(
                "fixture",
                std::any::TypeId::of::<()>(),
                Box::new(std::io::Error::other("connection refused")),
            ),
            context: "sending the probe".into(),
        }));
    }

    /// The stateless candidate list cannot contain the session revision.
    #[test]
    fn the_modern_candidates_never_include_the_legacy_revision() {
        let preferred = preferred_versions();

        assert_eq!(preferred.len(), 1, "one era, one revision: {preferred:?}");
        assert_eq!(preferred[0], STATELESS_VERSION);
        assert_ne!(
            preferred[0], LEGACY_VERSION,
            "selecting the session revision inside the stateless handshake would record \
             an era the handshake never used"
        );
        assert_eq!(STATELESS_VERSION.as_str(), "2026-07-28");
        assert_eq!(LEGACY_VERSION.as_str(), "2025-11-25");
    }

    fn secrets(values: &[(&str, &str)]) -> Secrets {
        Secrets::from_config(&McpServerConfig {
            name: "s".to_string(),
            transport: Transport::Stdio,
            command: Some("server".to_string()),
            args: Vec::new(),
            env: values
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
            url: None,
        })
    }

    /// Length is not a property of a secret.
    #[test]
    fn a_configured_value_is_redacted_however_short_it_is() {
        let configured = secrets(&[("TOKEN", "x7p"), ("OTHER", "abc"), ("EMPTY", "")]);

        for text in [
            "failure for x7p",
            "handshake failed: abc is invalid",
            "the server said: x7p",
        ] {
            let sanitized = configured.redact(text);
            assert!(!sanitized.contains("x7p"), "{sanitized}");
            assert!(!sanitized.contains("abc"), "{sanitized}");
            assert!(sanitized.contains("[redacted]"), "{sanitized}");
        }

        // An empty value would match everywhere, so it is not part of the set.
        assert_eq!(configured.0.len(), 2, "{:?}", configured.0);
    }

    /// A value that contains another must not leave a fragment behind.
    #[test]
    fn overlapping_values_are_replaced_longest_first() {
        let configured = secrets(&[("SHORT", "abc"), ("LONG", "abc123")]);

        let sanitized = configured.redact("token=abc123");
        assert_eq!(sanitized, "token=[redacted]");
        assert!(
            !sanitized.contains("123"),
            "a fragment survived: {sanitized}"
        );
        assert!(
            !sanitized.contains("abc"),
            "a fragment survived: {sanitized}"
        );
    }

    #[test]
    fn duplicate_values_are_collapsed() {
        let configured = secrets(&[("A", "same"), ("B", "same")]);
        assert_eq!(configured.0.len(), 1, "{:?}", configured.0);
    }

    /// The single door every free-form string goes through.
    #[test]
    fn every_diagnostic_string_is_sanitized_at_the_source() {
        let configured = secrets(&[("TOKEN", "x7p")]);

        // The shape the previously unredacted warning paths used.
        let error = std::io::Error::other("the server answered: failure for x7p");
        let diagnostic = configured.diagnostic(&error);

        assert!(diagnostic.contains("[redacted]"), "{diagnostic}");
        assert!(!diagnostic.contains("x7p"), "{diagnostic}");
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
