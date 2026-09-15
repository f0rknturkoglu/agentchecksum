// SPDX-License-Identifier: MIT OR Apache-2.0

//! A local MCP server whose declarations come from a JSON file.
//!
//! It exists so the discovery path can be exercised against a real MCP server over
//! both transports, with a catalog and a protocol era that a test states in data
//! instead of in code. It is a development artifact: nothing under `src/` refers to
//! it, and it is only built as an example target.
//!
//! ```text
//! AC_FIXTURE_SPEC=/path/spec.json target/debug/examples/mcp_fixture_server [--stdio|--http]
//! AC_FIXTURE_PORT_FILE=/path/port   # --http: the ephemeral port, written once listening
//! AC_FIXTURE_PID_FILE=/path/pid     # this process's pid, written once running
//! ```
//!
//! `--stdio` (the default) serves MCP on stdin/stdout through the SDK's server
//! implementation. `--http` serves the stateless Streamable HTTP protocol on
//! `127.0.0.1` with an ephemeral port; that path is written on raw sockets rather
//! than through a web framework so the fixture has no dependency the binary does
//! not already have.
//!
//! Nothing here reads the environment it is started with beyond the three variables
//! above: a configured credential is handed to this process by the client and is
//! never looked at, echoed, or logged.

use std::borrow::Cow;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{
    DiscoverResult, Implementation, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
    ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::stdio;
use rmcp::{ErrorData, ServerHandler};
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// The newest session-protocol revision, used when the fixture is pinned to legacy.
const LEGACY: ProtocolVersion = ProtocolVersion::V_2025_11_25;

// ---------------------------------------------------------------------------
// The spec file
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Spec {
    #[serde(default)]
    server_info: ServerInfoSpec,
    /// Advertise only pre-`2026-07-28` revisions, so the client has to negotiate
    /// down to the session protocol.
    #[serde(default)]
    legacy_only: bool,
    /// Tools per `tools/list` page. `0` serves the whole catalog in one page.
    #[serde(default)]
    page_size: usize,
    /// Delay before answering `tools/list`, to exercise a client-side page timeout.
    #[serde(default)]
    hang_ms: u64,
    /// A line written to stderr at startup, to prove server stderr stays out of
    /// diagnostics.
    #[serde(default)]
    stderr_secret: String,
    /// The declared catalog. Duplicate names are expressible on purpose.
    #[serde(default)]
    tools: Vec<ToolSpec>,
}

#[derive(Debug, Deserialize)]
struct ServerInfoSpec {
    #[serde(default = "default_server_name")]
    name: String,
    #[serde(default = "default_server_version")]
    version: String,
}

impl Default for ServerInfoSpec {
    fn default() -> Self {
        Self {
            name: default_server_name(),
            version: default_server_version(),
        }
    }
}

fn default_server_name() -> String {
    "fixture".to_string()
}

fn default_server_version() -> String {
    "1.0.0".to_string()
}

#[derive(Debug, Deserialize)]
struct ToolSpec {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "empty_schema")]
    input_schema: Map<String, Value>,
    #[serde(default)]
    output_schema: Option<Map<String, Value>>,
    #[serde(default)]
    annotations: Option<ToolAnnotations>,
}

fn empty_schema() -> Map<String, Value> {
    let mut schema = Map::new();
    schema.insert("type".to_string(), json!("object"));
    schema.insert("properties".to_string(), json!({}));
    schema
}

impl Spec {
    /// Read `AC_FIXTURE_SPEC`. A missing or unreadable spec is the test's own
    /// configuration error, so it exits loudly rather than serving an empty catalog
    /// that a test could mistake for a successful discovery.
    fn load() -> Self {
        let Ok(path) = std::env::var("AC_FIXTURE_SPEC") else {
            eprintln!("AC_FIXTURE_SPEC is not set");
            std::process::exit(2);
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            eprintln!("cannot read the fixture spec at {path}");
            std::process::exit(2);
        };
        match serde_json::from_str(&text) {
            Ok(spec) => spec,
            Err(error) => {
                eprintln!("cannot parse the fixture spec at {path}: {error}");
                std::process::exit(2);
            }
        }
    }

    /// The protocol revisions this server implements.
    fn versions(&self) -> Vec<ProtocolVersion> {
        if self.legacy_only {
            vec![LEGACY]
        } else {
            ProtocolVersion::KNOWN_VERSIONS.to_vec()
        }
    }

    fn implementation(&self) -> Implementation {
        Implementation::new(
            self.server_info.name.clone(),
            self.server_info.version.clone(),
        )
    }

    fn capabilities() -> ServerCapabilities {
        ServerCapabilities::builder().enable_tools().build()
    }

    /// What the session protocol's `initialize` answers with.
    fn server_config(&self) -> ServerConfig {
        ServerConfig::new(Self::capabilities()).with_server_info(self.implementation())
    }

    /// What the stateless protocol's `server/discover` answers with.
    fn discover_result(&self) -> DiscoverResult {
        DiscoverResult::new(self.versions(), Self::capabilities())
            .with_server_info(self.implementation())
    }

    /// The declared delay, applied where the catalog is read.
    fn hang(&self) {
        if self.hang_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.hang_ms));
        }
    }

    /// One page of the catalog, starting at the cursor's offset.
    fn page(&self, cursor: Option<&str>) -> ListToolsResult {
        let total = self.tools.len();
        let size = if self.page_size == 0 {
            total
        } else {
            self.page_size
        };
        let start = cursor
            .and_then(|cursor| cursor.parse::<usize>().ok())
            .unwrap_or(0)
            .min(total);
        let end = start.saturating_add(size).min(total);
        let tools = (start..end)
            .map(|index| self.declared_tool(&self.tools[index]))
            .collect();
        // Offset cursors rather than opaque ones: a test can see which page it is on.
        let next_cursor = (end < total).then(|| end.to_string());
        ListToolsResult {
            tools,
            next_cursor,
            ..ListToolsResult::default()
        }
    }

    /// One tool, in the wire shape the client parses.
    fn declared_tool(&self, tool: &ToolSpec) -> Tool {
        let mut declared = Tool::new_with_raw(
            tool.name.clone(),
            tool.description.clone().map(Cow::Owned),
            Arc::new(tool.input_schema.clone()),
        );
        if let Some(output_schema) = &tool.output_schema {
            declared = declared.with_raw_output_schema(Arc::new(output_schema.clone()));
        }
        if let Some(annotations) = &tool.annotations {
            declared = declared.with_annotations(annotations.clone());
        }
        declared
    }
}

// ---------------------------------------------------------------------------
// stdio: the SDK's server on stdin/stdout
// ---------------------------------------------------------------------------

struct Fixture {
    spec: Arc<Spec>,
}

impl ServerHandler for Fixture {
    fn get_info(&self) -> ServerConfig {
        self.spec.server_config()
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Owned(self.spec.versions())
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.spec.hang();
        Ok(self
            .spec
            .page(request.and_then(|params| params.cursor).as_deref()))
    }
}

fn serve_stdio(spec: Arc<Spec>) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cannot start the runtime: {error}");
            std::process::exit(2);
        }
    };

    runtime.block_on(async move {
        // The service ends when the transport does, which for stdio is stdin EOF:
        // the client closing the pipe is what ends this process.
        if let Ok(service) = rmcp::serve_server(Fixture { spec }, stdio()).await {
            let _ = service.waiting().await;
        }
    });
}

// ---------------------------------------------------------------------------
// Streamable HTTP: a stateless server on raw sockets
// ---------------------------------------------------------------------------

fn serve_http(spec: Arc<Spec>) {
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("cannot bind a port: {error}");
            std::process::exit(2);
        }
    };

    if let Ok(path) = std::env::var("AC_FIXTURE_PORT_FILE")
        && let Ok(address) = listener.local_addr()
    {
        let _ = std::fs::write(path, address.port().to_string());
    }

    // A connection per thread: the client may hold an idle connection open while it
    // opens another, and a sequential accept loop would deadlock on the first.
    for incoming in listener.incoming() {
        let Ok(stream) = incoming else { continue };
        let spec = Arc::clone(&spec);
        std::thread::spawn(move || {
            let _ = serve_connection(stream, &spec);
        });
    }
}

/// Stop when the process that started this one closes its end of stdin, which is the
/// same signal the stdio transport ends on. Nothing is read from it: the bytes are
/// not a protocol, and a fixture that consumed them would be reading the test's data.
fn exit_on_stdin_eof() {
    std::thread::spawn(|| {
        let mut input = std::io::stdin();
        let mut buffer = [0u8; 64];
        loop {
            match input.read(&mut buffer) {
                Ok(0) | Err(_) => std::process::exit(0),
                Ok(_) => {}
            }
        }
    });
}

fn serve_connection(stream: TcpStream, spec: &Spec) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(());
        }

        let mut content_length = 0usize;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header)? == 0 {
                return Ok(());
            }
            let header = header.trim_end_matches(['\r', '\n']);
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.trim().eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }

        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body)?;
        }

        let method = request_line.split_whitespace().next().unwrap_or_default();
        let response = match method {
            "POST" => answer_post(spec, &body),
            // A stateless server has no session to delete and no server-initiated
            // stream to offer. The transport reads both as "not offered" rather than
            // as a failure.
            "DELETE" => http(200, json!({})),
            _ => http_response(405, Vec::new()),
        };

        writer.write_all(&response)?;
        writer.flush()?;
    }
}

fn answer_post(spec: &Spec, body: &[u8]) -> Vec<u8> {
    let Ok(message) = serde_json::from_slice::<Value>(body) else {
        return http_response(
            400,
            serde_json::to_vec(&json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32700, "message": "Parse error" }
            }))
            .unwrap_or_default(),
        );
    };

    match dispatch(spec, &message) {
        Some(reply) => http(200, reply),
        // A notification is answered by not answering.
        None => http_response(202, Vec::new()),
    }
}

/// The JSON-RPC reply to `message`, or `None` when a reply is not what the protocol
/// asks for.
fn dispatch(spec: &Spec, message: &Value) -> Option<Value> {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let id = message.get("id").cloned();

    let result: Result<Value, Value> = match method {
        "server/discover" => return discover_reply(spec, message, id),
        "initialize" => Ok(json!(&spec.server_config())),
        "ping" => Ok(json!({})),
        "tools/list" => {
            spec.hang();
            let cursor = message
                .get("params")
                .and_then(|params| params.get("cursor"))
                .and_then(Value::as_str);
            Ok(json!(&spec.page(cursor)))
        }
        _ if id.is_none() => return None,
        other => Err(json!({
            "code": -32601,
            "message": format!("Method not found: {other}")
        })),
    };

    Some(match (result, id?) {
        (Ok(result), id) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        (Err(error), id) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
    })
}

/// `server/discover`, answered the way the SDK's server answers it: a revision this
/// server does not implement is refused by name, listed against the ones it does.
fn discover_reply(spec: &Spec, message: &Value, id: Option<Value>) -> Option<Value> {
    let id = id?;
    let requested = message
        .get("params")
        .and_then(|params| params.get("_meta"))
        .and_then(|meta| meta.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let supported = spec.versions();
    let refused = requested.as_deref().is_some_and(|requested| {
        !supported
            .iter()
            .any(|version| version.as_str() == requested)
    });

    let reply = if refused {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32022,
                "message": "Unsupported protocol version",
                "data": {
                    "requested": requested,
                    "supported": supported.iter().map(|version| version.as_str()).collect::<Vec<_>>(),
                }
            }
        })
    } else {
        json!({ "jsonrpc": "2.0", "id": id, "result": json!(&spec.discover_result()) })
    };
    Some(reply)
}

fn http(status: u16, body: Value) -> Vec<u8> {
    http_response(status, serde_json::to_vec(&body).unwrap_or_default())
}

fn http_response(status: u16, body: Vec<u8>) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    response
}

// ---------------------------------------------------------------------------

fn main() {
    let spec = Arc::new(Spec::load());

    if !spec.stderr_secret.is_empty() {
        eprintln!("{}", spec.stderr_secret);
    }
    if let Ok(path) = std::env::var("AC_FIXTURE_PID_FILE") {
        let _ = std::fs::write(path, std::process::id().to_string());
    }

    let http = std::env::args().nth(1).as_deref() == Some("--http");
    if http {
        exit_on_stdin_eof();
        serve_http(spec);
    } else {
        serve_stdio(spec);
    }

    // Only reachable if the accept loop ends, which it does not: the process is
    // ended by the transport closing (stdio) or by the test killing it (HTTP).
    std::process::exit(0);
}
