//! End-to-end protocol tests over both transports. They pin what an MCP
//! client observes (tool names, input schemas, error shapes) so an rmcp
//! upgrade cannot change it unnoticed.

use super::*;
use rmcp::serve_server;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const TOOLS_LIST_FIXTURE: &str = include_str!("../tests/fixtures/tools_list.json");

fn expected_tools() -> Vec<Value> {
    serde_json::from_str(TOOLS_LIST_FIXTURE).expect("fixture is a JSON array")
}

fn initialize_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }
    })
}

fn call_request(id: u64, name: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": name, "arguments": {}}
    })
}

fn tools_sorted_by_name(mut tools: Vec<Value>) -> Vec<Value> {
    tools.sort_by_key(|t| t["name"].as_str().unwrap_or_default().to_string());
    tools
}

fn assert_tools_match_fixture(result: &Value) {
    let tools = result["tools"].as_array().expect("tools array").clone();
    assert_eq!(
        tools_sorted_by_name(tools),
        tools_sorted_by_name(expected_tools()),
        "tool names or schemas drifted from tests/fixtures/tools_list.json"
    );
}

fn assert_status_reports_an_index(result: &Value) {
    assert_ne!(result["isError"], json!(true), "status failed: {result}");
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(!text.is_empty(), "status returned no text: {result}");
}

/// An unknown tool is a JSON-RPC "invalid params" error that says so, not a
/// panic, a hang or a successful empty result.
fn assert_unknown_tool_fails(response: &Value) {
    assert_eq!(response["error"]["code"], json!(-32602), "{response}");
    assert_eq!(response["error"]["message"], "tool not found", "{response}");
    assert!(response.get("result").is_none(), "{response}");
}

// ── stdio ────────────────────────────────────────────────────────────────────

struct StdioClient {
    write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    _dir: tempfile::TempDir,
}

impl StdioClient {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let server = RqmdServer::new(dir.path().to_path_buf()).unwrap();
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let running = serve_server(server, server_io).await.unwrap();
            let _ = running.waiting().await;
        });
        let (read, write) = tokio::io::split(client_io);
        Self {
            write,
            lines: BufReader::new(read).lines(),
            _dir: dir,
        }
    }

    async fn send(&mut self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.write.write_all(line.as_bytes()).await.unwrap();
    }

    async fn request(&mut self, message: Value) -> Value {
        let id = message["id"].clone();
        self.send(&message).await;
        loop {
            let line = self
                .lines
                .next_line()
                .await
                .unwrap()
                .expect("server closed the stream");
            let value: Value = serde_json::from_str(&line).unwrap();
            if value.get("method").is_none() && value["id"] == id {
                return value;
            }
        }
    }

    async fn handshake(&mut self) -> Value {
        let init = self.request(initialize_request(1)).await;
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        init
    }
}

#[tokio::test]
async fn stdio_initialize_lists_tools_calls_status_and_rejects_unknown_tool() {
    let mut client = StdioClient::start();

    let init = client.handshake().await;
    assert_eq!(init["result"]["serverInfo"]["name"], "rqmd");
    assert!(init["result"]["capabilities"]["tools"].is_object());

    let list = client
        .request(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .await;
    assert_tools_match_fixture(&list["result"]);

    let status = client.request(call_request(3, "status")).await;
    assert_status_reports_an_index(&status["result"]);

    let unknown = client.request(call_request(4, "no_such_tool")).await;
    assert_unknown_tool_fails(&unknown);

    // The session survives the failed call.
    let again = client.request(call_request(5, "status")).await;
    assert_status_reports_an_index(&again["result"]);
}

// ── Streamable HTTP over a real loopback socket ──────────────────────────────

struct HttpDaemon {
    port: u16,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

impl HttpDaemon {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let index_dir = dir.path().to_path_buf();
        let server = RqmdServer::new(index_dir.clone()).unwrap();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let allowed_hosts = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        let service: McpHttpService = StreamableHttpService::new(
            move || Ok(server.clone()),
            Arc::new(LocalSessionManager::default()),
            http_server_config(&allowed_hosts, port),
        );
        let router = build_router(
            service,
            allowed_hosts,
            std::process::id(),
            index_dir.to_string_lossy().to_string(),
        );
        tokio::spawn(async move { axum::serve(listener, router).await });
        Self {
            port,
            http: reqwest::Client::new(),
            _dir: dir,
        }
    }

    fn post(&self, session: Option<&str>, body: &Value) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .post(format!("http://127.0.0.1:{}/mcp", self.port))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(body.to_string());
        if let Some(id) = session {
            req = req
                .header("Mcp-Session-Id", id)
                .header("MCP-Protocol-Version", "2025-06-18");
        }
        req
    }

    /// POST one JSON-RPC request and return the message answering it. The
    /// reply arrives as a `text/event-stream` whose `data:` lines carry it.
    async fn request(&self, session: Option<&str>, body: Value) -> (Option<String>, Value) {
        let resp = self.post(session, &body).send().await.unwrap();
        assert_eq!(resp.status(), 200, "request {body} was refused");
        let session_id = resp
            .headers()
            .get("Mcp-Session-Id")
            .map(|v| v.to_str().unwrap().to_string());
        let text = resp.text().await.unwrap();
        let message = text
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
            .find(|v| v["id"] == body["id"])
            .unwrap_or_else(|| panic!("no reply to {body} in: {text}"));
        (session_id, message)
    }
}

#[tokio::test]
async fn http_initialize_lists_tools_calls_status_and_rejects_unknown_tool() {
    let daemon = HttpDaemon::start().await;

    let (session, init) = daemon.request(None, initialize_request(1)).await;
    let session = session.expect("initialize hands back a session id");
    assert_eq!(init["result"]["serverInfo"]["name"], "rqmd");

    let notified = daemon
        .post(
            Some(&session),
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(notified.status(), 202);

    let sid = Some(session.as_str());
    let (_, list) = daemon
        .request(
            sid,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await;
    assert_tools_match_fixture(&list["result"]);

    let (_, status) = daemon.request(sid, call_request(3, "status")).await;
    assert_status_reports_an_index(&status["result"]);

    let (_, unknown) = daemon.request(sid, call_request(4, "no_such_tool")).await;
    assert_unknown_tool_fails(&unknown);
}

#[tokio::test]
async fn http_post_without_a_session_is_a_400_for_anything_but_initialize() {
    let daemon = HttpDaemon::start().await;
    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let resp = daemon.post(None, &list).send().await.unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn http_origin_must_match_the_listening_port() {
    let daemon = HttpDaemon::start().await;
    let init = initialize_request(1);
    let port = daemon.port;
    let other_port = if port == u16::MAX { port - 1 } else { port + 1 };

    for (origin, expected) in [
        (format!("http://127.0.0.1:{port}"), 200),
        (format!("http://localhost:{port}"), 200),
        (format!("http://127.0.0.1:{other_port}"), 403),
        ("http://127.0.0.1".to_string(), 403),
        ("http://evil.example".to_string(), 403),
    ] {
        let resp = daemon
            .post(None, &init)
            .header("Origin", &origin)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), expected, "Origin {origin}");
    }
}

#[tokio::test]
async fn http_requests_without_an_origin_header_are_served() {
    let daemon = HttpDaemon::start().await;
    let (session, _) = daemon.request(None, initialize_request(1)).await;
    assert!(session.is_some());
}

#[tokio::test]
async fn http2_prior_knowledge_requests_are_served() {
    let daemon = HttpDaemon::start().await;
    let h2 = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap();
    let base = format!("http://127.0.0.1:{}", daemon.port);

    let health = h2.get(format!("{base}/health")).send().await.unwrap();
    assert_eq!(health.version(), reqwest::Version::HTTP_2);
    assert_eq!(health.status(), 200);

    let init = h2
        .post(format!("{base}/mcp"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(initialize_request(1).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(init.version(), reqwest::Version::HTTP_2);
    assert_eq!(init.status(), 200);
    assert!(init.headers().contains_key("mcp-session-id"));
}

#[tokio::test]
async fn shutdown_completes_while_a_client_holds_a_stream_open() {
    let dir = tempfile::tempdir().unwrap();
    let server = RqmdServer::new(dir.path().to_path_buf()).unwrap();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let allowed_hosts = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(serve_http(listener, server, allowed_hosts, port, async {
        let _ = stopped.await;
    }));

    let http = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/mcp");
    let init = http
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(initialize_request(1).to_string())
        .send()
        .await
        .unwrap();
    let session = init.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string();
    init.text().await.unwrap();
    let notified = http
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Session-Id", &session)
        .header("MCP-Protocol-Version", "2025-06-18")
        .body(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(notified.status(), 202);

    let stream = http
        .get(&url)
        .header("Accept", "text/event-stream")
        .header("Mcp-Session-Id", &session)
        .header("MCP-Protocol-Version", "2025-06-18")
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    // The first event proves the stream is established and still open.
    let mut stream = stream;
    let first = stream.chunk().await.unwrap();
    assert!(first.is_some(), "the stream closed before shutdown");

    stop.send(()).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), serving).await;
    assert!(
        outcome.is_ok(),
        "the server was still running 5s after shutdown with a stream open"
    );
    assert!(outcome.unwrap().unwrap().is_ok());
    assert!(
        stream.chunk().await.unwrap().is_none(),
        "the stream outlived the server"
    );
}
