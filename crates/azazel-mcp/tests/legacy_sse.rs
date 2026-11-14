//! Drives `azazel_mcp::sse` with a real rmcp client against a hand-written legacy HTTP+SSE server.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use azazel_mcp::sse::{LegacySseTransport, SseError, connect};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

const MESSAGES_PATH: &str = "/messages?sessionId=t1";
const EVENT_STREAM_HEAD: &str =
    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";

#[derive(Clone, Copy)]
enum Behaviour {
    /// A working MCP server with one `echo` tool.
    Mcp,
    /// Answers every request with 401 and a `WWW-Authenticate` challenge.
    Unauthorized,
    /// Opens the event stream and closes it without sending an `endpoint` event.
    NoEndpoint,
}

/// One HTTP request as the server saw it; header names are lower-cased.
#[derive(Clone)]
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Default)]
struct Shared {
    requests: Mutex<Vec<Request>>,
    /// Feeds JSON-RPC replies to the connection holding the open event stream.
    events: Mutex<Option<mpsc::UnboundedSender<String>>>,
}

/// A legacy SSE server on a loopback port; it stops accepting when dropped.
struct Server {
    addr: SocketAddr,
    shared: Arc<Shared>,
    accept: JoinHandle<()>,
}

impl Server {
    async fn start(behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(Shared::default());
        let accept = tokio::spawn({
            let shared = shared.clone();
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(serve_connection(stream, behaviour, shared.clone()));
                }
            }
        });
        Self { addr, shared, accept }
    }

    /// Opens the event stream with a client that ignores any proxy the environment names.
    async fn connect(&self, headers: &BTreeMap<String, String>) -> Result<LegacySseTransport, SseError> {
        let client = azazel_ai::tls::client_builder().no_proxy().build().unwrap();
        connect(
            &format!("http://{}/sse", self.addr),
            client,
            headers,
            CONNECT_TIMEOUT,
        )
        .await
    }

    fn requests(&self) -> Vec<Request> {
        self.shared.requests.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

/// Serves exactly one request on `stream`; every response asks the client to close the connection.
async fn serve_connection(mut stream: TcpStream, behaviour: Behaviour, shared: Arc<Shared>) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    shared.requests.lock().unwrap().push(request.clone());
    match (behaviour, request.method.as_str(), request.target.as_str()) {
        (Behaviour::Unauthorized, _, _) => {
            respond(
                &mut stream,
                "401 Unauthorized",
                "WWW-Authenticate: Bearer realm=\"x\"\r\n",
            )
            .await;
        }
        (Behaviour::NoEndpoint, "GET", "/sse") => {
            let opening = format!("{EVENT_STREAM_HEAD}: this stream never names an endpoint\n\n");
            let _ = stream.write_all(opening.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
        (Behaviour::Mcp, "GET", "/sse") => {
            let (events, mut queued) = mpsc::unbounded_channel::<String>();
            *shared.events.lock().unwrap() = Some(events);
            let opening = format!("{EVENT_STREAM_HEAD}event: endpoint\ndata: {MESSAGES_PATH}\n\n");
            if stream.write_all(opening.as_bytes()).await.is_err() {
                return;
            }
            while let Some(message) = queued.recv().await {
                let frame = format!("event: message\ndata: {message}\n\n");
                if stream.write_all(frame.as_bytes()).await.is_err() {
                    return;
                }
            }
        }
        (Behaviour::Mcp, "POST", MESSAGES_PATH) => {
            let message = serde_json::from_slice::<Value>(&request.body).ok();
            let reply = message.as_ref().and_then(reply_to);
            let events = shared.events.lock().unwrap().clone();
            if let (Some(reply), Some(events)) = (reply, events) {
                let _ = events.send(reply.to_string());
            }
            respond(&mut stream, "202 Accepted", "").await;
        }
        _ => respond(&mut stream, "404 Not Found", "").await,
    }
}

/// Reads one HTTP/1.1 request: request line, headers and a `Content-Length` body.
async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut received = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(position) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        let count = stream.read(&mut chunk).await.ok()?;
        if count == 0 {
            return None;
        }
        received.extend_from_slice(&chunk[..count]);
    };

    let head = String::from_utf8_lossy(&received[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_string();
    let target = request_line.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);

    let mut body = received[head_end + 4..].to_vec();
    while body.len() < length {
        let count = stream.read(&mut chunk).await.ok()?;
        if count == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..count]);
    }
    body.truncate(length);
    Some(Request {
        method,
        target,
        headers,
        body,
    })
}

/// Writes a body-less response and closes the connection.
async fn respond(stream: &mut TcpStream, status: &str, extra: &str) {
    let response = format!("HTTP/1.1 {status}\r\n{extra}Content-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// The JSON-RPC answer to a client message; notifications and responses get none.
fn reply_to(message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message.get("method")?.as_str()?;
    let params = &message["params"];
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params["protocolVersion"],
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "legacy-sse-test", "version": "0.0.0" },
        }),
        "ping" => json!({}),
        "tools/list" => json!({
            "tools": [{
                "name": "echo",
                "description": "Returns its text argument.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"],
                },
            }],
        }),
        "tools/call" => json!({
            "content": [{ "type": "text", "text": params["arguments"]["text"] }],
            "isError": false,
        }),
        _ => {
            let error = json!({ "code": -32601, "message": format!("method not found: {method}") });
            return Some(json!({ "jsonrpc": "2.0", "id": id, "error": error }));
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

async fn within<T>(test: impl Future<Output = T>) -> T {
    tokio::time::timeout(TEST_TIMEOUT, test)
        .await
        .expect("the test timed out")
}

#[tokio::test(flavor = "multi_thread")]
async fn rmcp_client_lists_and_calls_tools_over_legacy_sse() {
    within(async {
        let server = Server::start(Behaviour::Mcp).await;
        let transport = server.connect(&BTreeMap::new()).await.unwrap();
        let client = ().serve(transport).await.unwrap();

        let tools = client.list_all_tools().await.unwrap();
        let names: Vec<&str> = tools.iter().map(|tool| &*tool.name).collect();
        assert_eq!(names, ["echo"]);

        let arguments = json!({ "text": "hi" }).as_object().cloned().unwrap();
        let result = client
            .call_tool(CallToolRequestParams::new("echo").with_arguments(arguments))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false));
        let texts: Vec<&str> = result
            .content
            .iter()
            .filter_map(|block| block.as_text())
            .map(|text| text.text.as_str())
            .collect();
        assert_eq!(texts, ["hi"]);

        client.cancel().await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unauthorized_stream_reports_the_challenge() {
    within(async {
        let server = Server::start(Behaviour::Unauthorized).await;
        let error = server.connect(&BTreeMap::new()).await.unwrap_err();
        match error {
            SseError::Unauthorized { www_authenticate } => {
                assert_eq!(www_authenticate.as_deref(), Some("Bearer realm=\"x\""));
            }
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_ending_without_an_endpoint_is_a_protocol_error() {
    within(async {
        let server = Server::start(Behaviour::NoEndpoint).await;
        let error = server.connect(&BTreeMap::new()).await.unwrap_err();
        assert!(
            matches!(error, SseError::Protocol(_)),
            "expected Protocol, got {error:?}"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_headers_are_sent_with_the_stream_and_every_post() {
    within(async {
        let server = Server::start(Behaviour::Mcp).await;
        let headers = BTreeMap::from([
            ("Authorization".to_string(), "Bearer sesame".to_string()),
            ("X-Azazel-Test".to_string(), "on".to_string()),
        ]);
        let transport = server.connect(&headers).await.unwrap();
        let client = ().serve(transport).await.unwrap();
        client.list_all_tools().await.unwrap();
        client.cancel().await.unwrap();

        let requests = server.requests();
        let (gets, posts): (Vec<&Request>, Vec<&Request>) =
            requests.iter().partition(|request| request.method == "GET");
        assert_eq!(gets.len(), 1);
        assert_eq!(gets[0].target, "/sse");
        assert_eq!(gets[0].header("accept"), Some("text/event-stream"));
        // `initialize`, `notifications/initialized` and `tools/list`.
        assert!(posts.len() >= 3, "only {} messages were posted", posts.len());
        for post in &posts {
            assert_eq!(post.method, "POST");
            assert_eq!(post.target, MESSAGES_PATH);
            assert_eq!(post.header("content-type"), Some("application/json"));
        }
        for request in gets.iter().chain(&posts) {
            assert_eq!(
                request.header("authorization"),
                Some("Bearer sesame"),
                "{}",
                request.method
            );
            assert_eq!(request.header("x-azazel-test"), Some("on"), "{}", request.method);
        }
    })
    .await;
}
