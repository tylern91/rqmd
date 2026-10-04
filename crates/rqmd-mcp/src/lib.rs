mod server;

pub use server::RqmdServer;

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::sync::Arc;
use std::time::Duration;
use tower_http::limit::RequestBodyLimitLayer;

type McpHttpService = StreamableHttpService<RqmdServer, LocalSessionManager>;

/// Cap on the body of any single HTTP request to the MCP server. Nothing this
/// server accepts (JSON-RPC tool calls) legitimately needs more than a few KB;
/// this bounds worst-case memory use per request against a client that sends
/// an oversized or endless body. 4 MiB comfortably covers a `multi_get` of
/// several large documents' worth of request framing while staying far below
/// what would let a handful of concurrent requests exhaust host memory.
const MAX_MCP_REQUEST_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Default `RQMD_MODEL_IDLE_TTL` in seconds — how long a GGUF model may sit
/// unused before the periodic sweep releases it. `0` disables the sweep.
const DEFAULT_MODEL_IDLE_TTL_SECS: u64 = 300;

/// Spawn a background task that periodically releases GGUF models idle for
/// longer than `RQMD_MODEL_IDLE_TTL` seconds (default 300; `0` disables).
/// Without this, query expansion (on by default) permanently ratchets a
/// long-lived daemon up by the ~2 GB generate model the first time it fires.
fn spawn_idle_eviction(server: RqmdServer) {
    let ttl_secs = std::env::var("RQMD_MODEL_IDLE_TTL")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MODEL_IDLE_TTL_SECS);

    if ttl_secs == 0 {
        return;
    }

    let ttl = Duration::from_secs(ttl_secs);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            let released = server.release_idle_models(ttl);
            if released > 0 {
                eprintln!("[rqmd-mcp] released {released} idle model(s) (ttl={ttl_secs}s)");
            }
        }
    });
}

/// Run an MCP server over stdio (blocks until the client disconnects).
pub async fn run_stdio(server: RqmdServer) -> Result<()> {
    use rmcp::{serve_server, transport::stdio};
    spawn_idle_eviction(server.clone());
    let transport = stdio();
    serve_server(server, transport).await?.waiting().await?;
    Ok(())
}

/// Minimal, unauthenticated-safe response for `/health` — just confirms the
/// process behind this Host-validated endpoint is up. Anything more (pid,
/// on-disk paths) belongs behind `/health/daemon`, which only the CLI's own
/// daemon-lifecycle code (`rqmd mcp status`/`stop`) consumes.
#[derive(serde::Serialize)]
struct StatusBody {
    status: &'static str,
}

async fn health() -> Json<StatusBody> {
    Json(StatusBody { status: "ok" })
}

/// Full health payload used only by this CLI's own daemon lifecycle
/// (`daemon::fetch_health`) to confirm pid identity and index directory.
#[derive(serde::Serialize)]
struct DaemonHealthBody {
    pid: u32,
    index_dir: String,
}

/// The host as typed on the command line, minus the brackets a user may wrap
/// around an IPv6 literal (`[::1]` → `::1`). This is the form `bind` and
/// `IpAddr` parsing want.
pub fn bare_host(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

/// The host as it appears in a URL authority or `Host`/`Origin` header: IPv6
/// literals are canonicalised and bracketed (`0:0:0:0:0:0:0:1` → `[::1]`),
/// everything else is returned unchanged.
pub fn authority_host(host: &str) -> String {
    let bare = bare_host(host);
    match bare.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(ip)) => format!("[{ip}]"),
        _ => bare.to_string(),
    }
}

/// `host:port` safe to embed in a URL, with IPv6 literals bracketed.
pub fn host_port(host: &str, port: u16) -> String {
    format!("{}:{port}", authority_host(host))
}

/// Strip a trailing `:<port>` from a `Host` header value; a bracketed IPv6
/// literal (`[::1]:8181`) keeps its brackets (`[::1]`).
fn host_only(header_value: &str) -> &str {
    match header_value.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => header_value,
    }
}

/// Reject any request whose `Host` header isn't in the allowlist, before it
/// reaches `/health` or `/mcp` — closes the gap where `/health` previously
/// sat outside the Host validation `StreamableHttpServerConfig` applies to
/// `/mcp`, leaking to anything that could reach the port regardless of Host.
async fn enforce_host_allowlist(
    State(allowed_hosts): State<Arc<Vec<String>>>,
    req: Request,
    next: Next,
) -> Response {
    let allowed = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| allowed_hosts.iter().any(|a| a == host_only(h)))
        .unwrap_or(false);

    if allowed {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "host not allowed").into_response()
    }
}

fn build_router(
    mcp_service: McpHttpService,
    allowed_hosts: Vec<String>,
    pid: u32,
    index_dir: String,
) -> Router {
    let allowed_hosts = Arc::new(allowed_hosts);
    Router::new()
        .route("/health", get(health))
        .route(
            "/health/daemon",
            get(move || {
                let index_dir = index_dir.clone();
                async move { Json(DaemonHealthBody { pid, index_dir }) }
            }),
        )
        .nest_service("/mcp", mcp_service)
        .layer(RequestBodyLimitLayer::new(MAX_MCP_REQUEST_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            allowed_hosts,
            enforce_host_allowlist,
        ))
}

/// Wait for ctrl-c or SIGTERM so `axum::serve` can shut down gracefully
/// instead of leaving the daemon as an orphan that never returns from
/// `serve()` (its stdin is `/dev/null`, so there is never an EOF to catch).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

/// Run an MCP server over Streamable HTTP on the given host/port (blocks
/// until the server is shut down).
///
/// `on_bound` fires once the listener has actually bound the port — the
/// right moment for a caller to record this process as the daemon (e.g.
/// write a pidfile), rather than doing so speculatively before the bind is
/// known to succeed.
pub async fn run_http(
    server: RqmdServer,
    host: &str,
    port: u16,
    on_bound: impl FnOnce() -> Result<()>,
) -> Result<()> {
    spawn_idle_eviction(server.clone());

    let mut allowed_hosts = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let host_entry = authority_host(host);
    if !allowed_hosts.contains(&host_entry) {
        allowed_hosts.push(host_entry);
    }

    // Mirror the Host allowlist into `allowed_origins` — rmcp only applies
    // its Origin (DNS-rebinding) defense when this is non-empty, and it was
    // previously never set at all, silently disabling that check entirely.
    let allowed_origins: Vec<String> = allowed_hosts
        .iter()
        .flat_map(|h| [format!("http://{h}:{port}"), format!("https://{h}:{port}")])
        .collect();

    let mut config = StreamableHttpServerConfig::default();
    config.allowed_hosts = allowed_hosts.clone();
    config.allowed_origins = allowed_origins;

    let pid = std::process::id();
    let index_dir = server.index_dir().to_string_lossy().to_string();

    let service: McpHttpService = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );

    let addr = host_port(host, port);
    eprintln!("RQMD MCP server listening on http://{addr}/mcp");
    eprintln!("Health endpoint:            http://{addr}/health");

    let router = build_router(service, allowed_hosts, pid, index_dir);
    let listener = tokio::net::TcpListener::bind((bare_host(host), port)).await?;
    on_bound()?;
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::header::{CONTENT_LENGTH, HOST};
    use tower::ServiceExt;

    fn test_router() -> (Router, tempfile::TempDir) {
        test_router_with_hosts(&["localhost", "127.0.0.1"])
    }

    fn test_router_with_hosts(hosts: &[&str]) -> (Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let index_dir = dir.path().to_path_buf();
        let server = RqmdServer::new(index_dir.clone()).unwrap();
        let service: McpHttpService = StreamableHttpService::new(
            move || Ok(server.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
        let router = build_router(
            service,
            hosts.iter().map(|h| h.to_string()).collect(),
            std::process::id(),
            index_dir.to_string_lossy().to_string(),
        );
        (router, dir)
    }

    #[tokio::test]
    async fn health_allows_loopback_host_and_hides_internals() {
        let (router, _dir) = test_router();
        let req = Request::builder()
            .uri("/health")
            .header(HOST, "127.0.0.1")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(!text.contains("index_dir"), "response leaked: {text}");
        assert!(!text.contains("pid"), "response leaked: {text}");
    }

    #[tokio::test]
    async fn health_rejects_disallowed_host() {
        let (router, _dir) = test_router();
        let req = Request::builder()
            .uri("/health")
            .header(HOST, "evil.example.com")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn health_rejects_missing_host() {
        let (router, _dir) = test_router();
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn oversized_request_body_is_rejected() {
        let (router, _dir) = test_router();
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(HOST, "127.0.0.1")
            .header(CONTENT_LENGTH, (MAX_MCP_REQUEST_BODY_BYTES + 1).to_string())
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn health_daemon_reports_pid_and_index_dir() {
        let (router, dir) = test_router();
        let req = Request::builder()
            .uri("/health/daemon")
            .header(HOST, "127.0.0.1")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["pid"], std::process::id());
        assert_eq!(json["index_dir"], dir.path().to_string_lossy().as_ref());
    }

    /// rmcp 3.x still serves a legacy (pre-2026-07-28) `initialize` handshake
    /// with a session, exactly as 2.x did — this pins that down as a
    /// regression guard for the rmcp 2.2.0 -> 3.4.1 upgrade.
    #[tokio::test]
    async fn legacy_initialize_returns_a_session_id() {
        let (router, _dir) = test_router();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0"}
            }
        });
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(HOST, "127.0.0.1")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers().get("Mcp-Session-Id").is_some(),
            "legacy initialize should still hand back a session id"
        );
    }

    /// rmcp 3.x's new `2026-07-28` protocol serves `server/discover` (and
    /// every other request) statelessly — no session is created, regardless
    /// of `legacy_session_mode` (which we never set and stays at its
    /// default). This is the one behavior genuinely new to 3.x that this
    /// daemon's clients could hit.
    ///
    /// A `2026-07-28` request also requires the `Mcp-Method` standard header
    /// (SEP-2243) once the server negotiates that version — omitting it is a
    /// hard 400 (`-32020`), confirmed by running this test against the real
    /// server before adding the header below.
    ///
    /// TODO(tyler): the assertions below are my best read of "stateless" for
    /// this test (200 OK, no `Mcp-Session-Id` issued) — see the rmcp 3.x
    /// migration guide (rust-sdk discussion #969, §10 "Stateless HTTP and
    /// subscription streams"). Since `RqmdServer` is a long-lived daemon
    /// with `Arc`-shared model/index state (`ml()`/`fts()` in `server.rs`),
    /// you may want this test to also assert something about the response
    /// body (e.g. `resultType`/`supportedVersions`) or about repeat calls
    /// never accumulating session state — adjust as you see fit.
    #[tokio::test]
    async fn discover_2026_07_28_is_stateless() {
        let (router, _dir) = test_router();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "server/discover",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1.0"},
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        });
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(HOST, "127.0.0.1")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", "server/discover")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers().get("Mcp-Session-Id").is_none(),
            "a 2026-07-28 discover request must not create a session"
        );
    }

    #[test]
    fn host_port_brackets_and_canonicalises_ipv6_only() {
        assert_eq!(host_port("127.0.0.1", 8181), "127.0.0.1:8181");
        assert_eq!(host_port("localhost", 8181), "localhost:8181");
        assert_eq!(host_port("::1", 8181), "[::1]:8181");
        assert_eq!(host_port("[::1]", 8181), "[::1]:8181");
        assert_eq!(host_port("0:0:0:0:0:0:0:1", 8181), "[::1]:8181");
        assert_eq!(host_port("2001:DB8::1", 80), "[2001:db8::1]:80");
    }

    #[test]
    fn bare_host_strips_one_bracket_pair() {
        assert_eq!(bare_host("[::1]"), "::1");
        assert_eq!(bare_host("::1"), "::1");
        assert_eq!(bare_host("[::1"), "[::1");
    }

    #[test]
    fn host_only_keeps_ipv6_brackets() {
        assert_eq!(host_only("[::1]:8181"), "[::1]");
        assert_eq!(host_only("[::1]"), "[::1]");
        assert_eq!(host_only("localhost:8181"), "localhost");
    }

    async fn status_for_host(router: Router, host: &str) -> StatusCode {
        let req = Request::builder()
            .uri("/health")
            .header(HOST, host)
            .body(Body::empty())
            .unwrap();
        router.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn ipv6_loopback_host_header_is_allowed_only_when_configured() {
        let (router, _dir) = test_router_with_hosts(&["localhost", "127.0.0.1", "[::1]"]);
        assert_eq!(
            status_for_host(router.clone(), "[::1]:8181").await,
            StatusCode::OK
        );
        assert_eq!(
            status_for_host(router.clone(), "[::2]:8181").await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status_for_host(router.clone(), "evil.example:8181").await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(status_for_host(router, "::1").await, StatusCode::FORBIDDEN);

        let (v4_only, _dir) = test_router();
        assert_eq!(
            status_for_host(v4_only, "[::1]:8181").await,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn ipv6_literal_binds_through_bare_host() {
        // Hosts without an IPv6 loopback can't exercise this; skip rather than fail.
        let Ok(listener) = tokio::net::TcpListener::bind((bare_host("[::1]"), 0)).await else {
            return;
        };
        assert!(listener.local_addr().unwrap().is_ipv6());
    }
}
