// SPDX-License-Identifier: MIT OR Apache-2.0

//! A local OpenAI-compatible chat-completions endpoint whose answers come from a
//! JSON file.
//!
//! It exists so the capture path can be exercised against a real HTTP endpoint that
//! answers with the shapes backends actually produce — a text turn, a tool-call turn,
//! arguments as a JSON string and as an already-parsed object, malformed arguments, a
//! tool name that does not exist, an HTTP error, a reply that arrives too late, and a
//! body that is not a chat completion at all — with each of those stated in data
//! rather than in code. It also records every request it receives, which is the only
//! way a test can see *what the runner sent* rather than what the runner believes it
//! sent.
//!
//! It is a development artifact: nothing under `src/` refers to it, and it is only
//! built as an example target. It serves on `127.0.0.1` with an ephemeral port, on raw
//! sockets rather than through a web framework, so it has no dependency the binary
//! does not already have.
//!
//! ```text
//! AC_FIXTURE_SPEC=/path/spec.json examples/openai_fixture_server
//! AC_FIXTURE_PORT_FILE=/path/port   # the ephemeral port, written once listening
//! ```
//!
//! The spec:
//!
//! ```jsonc
//! {
//!   // Served in order, one per request; the last one repeats, so a `repeat = N`
//!   // capture against a single-response spec is N identical samples.
//!   "responses": [
//!     { "type": "text", "content": "Hello." },
//!     { "type": "text" },                                  // content: null
//!     { "type": "tool_calls", "calls": [
//!         { "name": "search", "arguments": "{\"query\":\"x\"}" },   // wire string
//!         { "name": "read_file", "arguments": { "path": "a.txt" } },// parsed value
//!         { "name": "delete_everything", "arguments": "{}" },      // not in the catalog
//!         { "name": "search", "arguments": "{not json" }           // malformed
//!     ] },
//!     { "type": "raw", "body": "<html>not json</html>", "status": 200 },
//!     { "type": "status", "status": 500 }
//!   ],
//!   "delay_ms": 0,                 // added to every response; the timeout fixture
//!   "record_file": "/path/requests.jsonl"   // one JSON line per request
//! }
//! ```
//!
//! Each response may also carry its own `delay_ms`, added to the global one.
//!
//! A missing or unreadable spec is the test's own configuration error, so this exits
//! loudly rather than serving a plausible-looking default that a test could mistake
//! for a successful capture.
//!
//! Nothing here reads its environment beyond those two variables: a credential a
//! client sends is recorded in the request log exactly as the client sent it, and is
//! never looked at, echoed, or logged anywhere else.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value, json};

// ---------------------------------------------------------------------------
// The spec file
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Spec {
    /// The answers to serve, in order.
    responses: Vec<ResponseSpec>,
    /// A delay added to every response, for the timeout case.
    #[serde(default)]
    delay_ms: u64,
    /// Where to append the requests this server receives, one JSON object per line.
    #[serde(default)]
    record_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct ResponseSpec {
    /// `text`, `tool_calls`, `raw` or `status`.
    #[serde(rename = "type")]
    kind: String,
    /// `text`: the assistant text. Absent means `null`, which is what a pure
    /// tool-call turn carries.
    #[serde(default)]
    content: Option<String>,
    /// `tool_calls`: the calls the model emitted, in order.
    #[serde(default)]
    calls: Vec<CallSpec>,
    /// `raw`: the body to send verbatim, so a test can state a body that is not a
    /// chat completion at all.
    #[serde(default)]
    body: Option<String>,
    /// `raw` and `status`: the HTTP status. Defaults to 200.
    #[serde(default)]
    status: Option<u16>,
    /// `raw`: the `content-type` to answer with.
    #[serde(default)]
    content_type: Option<String>,
    /// Extra delay before this response, on top of the global one.
    #[serde(default)]
    delay_ms: u64,
}

#[derive(Debug, Deserialize)]
struct CallSpec {
    name: String,
    /// A JSON string is sent as the wire's string form; a JSON object or array as the
    /// already-parsed form. Absent means the call carries no `arguments` field at
    /// all, which is a third wire shape a client has to survive.
    #[serde(default)]
    arguments: Option<Value>,
}

impl Spec {
    fn load() -> Self {
        let Ok(path) = std::env::var("AC_FIXTURE_SPEC") else {
            die("AC_FIXTURE_SPEC is not set");
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            die(&format!("cannot read the fixture spec at {path}"));
        };
        let spec: Self = match serde_json::from_str(&text) {
            Ok(spec) => spec,
            Err(error) => die(&format!("cannot parse the fixture spec at {path}: {error}")),
        };
        spec.validate();
        spec
    }

    /// A spec that cannot be served is a test bug, and serving something else would
    /// turn it into a mysterious failure somewhere else.
    fn validate(&self) {
        if self.responses.is_empty() {
            die("the fixture spec declares no responses");
        }
        for (index, response) in self.responses.iter().enumerate() {
            match response.kind.as_str() {
                "text" => {}
                "tool_calls" if response.calls.is_empty() => {
                    die(&format!(
                        "response {index} is a `tool_calls` response with no calls"
                    ));
                }
                "tool_calls" => {}
                "raw" if response.body.is_none() => {
                    die(&format!(
                        "response {index} is a `raw` response with no body"
                    ));
                }
                "raw" => {}
                "status" if response.status.is_none() => {
                    die(&format!(
                        "response {index} is a `status` response with no status"
                    ));
                }
                "status" => {}
                other => die(&format!("response {index} has the unknown type `{other}`")),
            }
        }
    }

    /// The response for the `n`th request this process has received.
    fn response(&self, served: usize) -> &ResponseSpec {
        let index = served.min(self.responses.len() - 1);
        &self.responses[index]
    }

    /// How long to wait before answering, so a client's own timeout can be exercised.
    fn delay(&self, response: &ResponseSpec) -> Duration {
        Duration::from_millis(self.delay_ms.saturating_add(response.delay_ms))
    }
}

fn die(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(2);
}

// ---------------------------------------------------------------------------
// Serving
// ---------------------------------------------------------------------------

/// The server: the spec, and how many requests it has answered.
struct Fixture {
    spec: Spec,
    served: AtomicUsize,
    /// Appended to, in order, once the log file is opened.
    log: Option<std::sync::Mutex<std::fs::File>>,
}

impl Fixture {
    fn new(spec: Spec) -> Self {
        let log = spec.record_file.as_ref().map(|path| {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path);
            match file {
                Ok(file) => std::sync::Mutex::new(file),
                // A test that asked for the log and cannot have it fails on the
                // requests it reads back, which is better than a fixture that exits
                // for a log file.
                Err(_) => std::sync::Mutex::new(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open("/dev/null")
                        .expect("a discard file"),
                ),
            }
        });

        Self {
            spec,
            served: AtomicUsize::new(0),
            log,
        }
    }

    /// One line per request: the method, the path it arrived on, and the body as
    /// parsed. A test asserts what the runner sent from here.
    fn record(&self, method: &str, path: &str, body: Option<&Value>) {
        let Some(log) = &self.log else { return };
        let line = json!({ "method": method, "path": path, "body": body });
        if let Ok(mut file) = log.lock() {
            let _ = writeln!(file, "{line}");
        }
    }

    /// The answer to one request.
    fn answer(&self, request_body: Option<&Value>) -> (u16, String, Vec<u8>) {
        let served = self.served.fetch_add(1, Ordering::SeqCst);
        let response = self.spec.response(served);

        let delay = self.spec.delay(response);
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }

        match response.kind.as_str() {
            "status" => (
                response.status.unwrap_or(200),
                "application/json".to_string(),
                Vec::new(),
            ),
            "raw" => (
                response.status.unwrap_or(200),
                response
                    .content_type
                    .clone()
                    .unwrap_or_else(|| "application/json".to_string()),
                response.body.clone().unwrap_or_default().into_bytes(),
            ),
            "tool_calls" => {
                let calls: Vec<Value> = response
                    .calls
                    .iter()
                    .enumerate()
                    .map(|(index, call)| self.wire_call(served, index, call))
                    .collect();
                let message =
                    json!({ "role": "assistant", "content": Value::Null, "tool_calls": calls });
                self.completion(served, request_body, message)
            }
            _ => {
                let content = match &response.content {
                    Some(text) => Value::String(text.clone()),
                    None => Value::Null,
                };
                let message = json!({ "role": "assistant", "content": content });
                self.completion(served, request_body, message)
            }
        }
    }

    /// One tool call in the wire shape: `arguments` is a string when the spec said
    /// string, a value when it said value, and absent when it said nothing.
    fn wire_call(&self, served: usize, index: usize, call: &CallSpec) -> Value {
        let mut function = Map::new();
        function.insert("name".to_string(), Value::String(call.name.clone()));
        if let Some(arguments) = &call.arguments {
            function.insert("arguments".to_string(), arguments.clone());
        }
        json!({
            "id": format!("call_{served}_{index}"),
            "type": "function",
            "function": Value::Object(function)
        })
    }

    /// A complete chat-completion envelope around one message. The model name is
    /// echoed from the request, so a response and the request that produced it agree.
    fn completion(
        &self,
        served: usize,
        request_body: Option<&Value>,
        message: Value,
    ) -> (u16, String, Vec<u8>) {
        let model = request_body
            .and_then(|body| body.get("model"))
            .cloned()
            .unwrap_or_else(|| Value::String("fixture".to_string()));

        let completion = json!({
            "id": format!("chatcmpl-fixture-{served}"),
            "object": "chat.completion",
            "created": 0,
            "model": model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": "stop"
            }]
        });

        (
            200,
            "application/json".to_string(),
            serde_json::to_vec(&completion).unwrap_or_default(),
        )
    }
}

fn serve(fixture: Arc<Fixture>) {
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(listener) => listener,
        Err(error) => die(&format!("cannot bind a port: {error}")),
    };

    if let Ok(path) = std::env::var("AC_FIXTURE_PORT_FILE")
        && let Ok(address) = listener.local_addr()
    {
        let _ = std::fs::write(path, address.port().to_string());
    }

    // A connection per thread: a client may keep a connection alive across samples,
    // and a sequential accept loop would deadlock the moment it does.
    for incoming in listener.incoming() {
        let Ok(stream) = incoming else { continue };
        let fixture = Arc::clone(&fixture);
        std::thread::spawn(move || {
            let _ = serve_connection(stream, &fixture);
        });
    }
}

/// Stop when the process that started this one closes its end of stdin, so a test
/// that drops the child's stdin gets a process that ends by itself. Nothing is read
/// from it: the bytes are not a protocol.
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

fn serve_connection(stream: TcpStream, fixture: &Fixture) -> std::io::Result<()> {
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

        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();

        let parsed: Option<Value> = if body.is_empty() {
            None
        } else {
            serde_json::from_slice(&body).ok()
        };
        fixture.record(&method, &path, parsed.as_ref());

        let response = if method != "POST" {
            http_response(405, "application/json", b"{}".to_vec())
        } else if body.is_empty() || parsed.is_none() {
            http_response(400, "application/json", b"{}".to_vec())
        } else {
            let (status, content_type, body) = fixture.answer(parsed.as_ref());
            http_response(status, &content_type, body)
        };

        writer.write_all(&response)?;
        writer.flush()?;
    }
}

fn http_response(status: u16, content_type: &str, body: Vec<u8>) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    response
}

fn main() {
    let spec = Spec::load();
    exit_on_stdin_eof();
    serve(Arc::new(Fixture::new(spec)));

    // Only reachable if the accept loop ends, which it does not: the test kills this
    // process or closes its stdin.
    std::process::exit(0);
}
