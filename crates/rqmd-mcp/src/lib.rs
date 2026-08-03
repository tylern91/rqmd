mod server;

pub use server::RqmdServer;

use anyhow::Result;
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
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

/// Default `RRQMD_MODEL_IDLE_TTL` in seconds — how long a GGUF model may sit
/// unused before the periodic sweep releases it. `0` disables the sweep.
const DEFAULT_MODEL_IDLE_TTL_SECS: u64 = 300;

/// Spawn a background task that periodically releases GGUF models idle for
/// longer than `RRQMD_MODEL_IDLE_TTL` seconds (default 300; `0` disables).
/// Without this, query expansion (on by default) permanently ratchets a
/// long-lived daemon up by the ~2 GB generate model the first time it fires.
fn spawn_idle_eviction(server: RqmdServer) {
    let ttl_secs = std::env::var("RRQMD_MODEL_IDLE_TTL")
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

/// Strip a trailing `:<port>` from a `Host` header value. Bracketed IPv6
/// literals (e.g. `[::1]:8181`) aren't a case this server's `--host` handling
/// supports today (see `run_http`'s plain `host:port` addr string), so this
/// stays intentionally simple — matching the scope of what `--host` accepts.
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
pub async fn run_http(server: RqmdServer, host: &str, port: u16) -> Result<()> {
    spawn_idle_eviction(server.clone());

    let mut allowed_hosts = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    if !allowed_hosts.iter().any(|h| h == host) {
        allowed_hosts.push(host.to_string());
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

    let addr = format!("{host}:{port}");
    eprintln!("RQMD MCP server listening on http://{addr}/mcp");
    eprintln!("Health endpoint:            http://{addr}/health");

    let router = build_router(service, allowed_hosts, pid, index_dir);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
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
            vec!["localhost".to_string(), "127.0.0.1".to_string()],
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
}
