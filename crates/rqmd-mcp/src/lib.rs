mod server;

pub use server::RqmdServer;

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

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

/// Run an MCP server over Streamable HTTP on the given port (blocks until
/// the server is shut down).
pub async fn run_http(server: RqmdServer, port: u16) -> Result<()> {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };

    spawn_idle_eviction(server.clone());

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
