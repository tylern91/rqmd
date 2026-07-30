mod server;

pub use server::RqmdServer;

use anyhow::Result;
use std::sync::Arc;

/// Run an MCP server over stdio (blocks until the client disconnects).
pub async fn run_stdio(server: RqmdServer) -> Result<()> {
    use rmcp::{serve_server, transport::stdio};
    let transport = stdio();
    serve_server(server, transport).await?.waiting().await?;
    Ok(())
}

/// Run an MCP server over Streamable HTTP on the given port (blocks until
/// the server is shut down).
pub async fn run_http(server: RqmdServer, port: u16) -> Result<()> {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };

    // Only the HTTP/daemon path gets the sweep — stdio clients are cheap and
    // short-lived (measured ~3.6 MB idle), so there's nothing there to evict.
    // A long-lived daemon is what accumulates the generate-model ratchet.
    spawn_idle_eviction_sweep(server.clone());

    let mut config = StreamableHttpServerConfig::default();
    config.allowed_hosts = vec!["localhost".to_string(), "127.0.0.1".to_string()];

    let service: StreamableHttpService<RqmdServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(server.clone()),
            Arc::new(LocalSessionManager::default()),
            config,
        );

    let addr = format!("127.0.0.1:{port}");
    eprintln!("RQMD MCP server listening on http://{addr}/mcp");
    eprintln!("Health endpoint:            http://{addr}/health");

    let router = axum::Router::new()
        .route(
            "/health",
            axum::routing::get(|| async { (axum::http::StatusCode::OK, "ok") }),
        )
        .nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}

/// Spawn the background idle-model-eviction sweep. Ticks every 60s; each tick
/// releases any inference model idle for at least `RRQMD_MODEL_IDLE_TTL`
/// seconds (default 300, `0` disables the sweep entirely). This is what
/// actually reclaims the 2.0 GB generate model on a long-lived daemon —
/// query-expansion defaults to on, so lazy loading alone only delays the
/// reload by a few queries.
fn spawn_idle_eviction_sweep(server: RqmdServer) {
    let ttl_secs: u64 = std::env::var("RRQMD_MODEL_IDLE_TTL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);

    if ttl_secs == 0 {
        eprintln!("[rqmd-mcp] RRQMD_MODEL_IDLE_TTL=0 — idle model eviction disabled");
        return;
    }

    let ttl = std::time::Duration::from_secs(ttl_secs);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            if let Some(released) = server.release_idle_models(ttl) {
                if released > 0 {
                    eprintln!("[rqmd-mcp] evicted {released} idle inference model(s)");
                }
            }
        }
    });
}
