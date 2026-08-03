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

#[derive(serde::Serialize)]
struct HealthBody {
    pid: u32,
    index_dir: String,
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
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };

    spawn_idle_eviction(server.clone());

    let mut allowed_hosts = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    if !allowed_hosts.iter().any(|h| h == host) {
        allowed_hosts.push(host.to_string());
    }
    let mut config = StreamableHttpServerConfig::default();
    config.allowed_hosts = allowed_hosts;

    let pid = std::process::id();
    let index_dir = server.index_dir().to_string_lossy().to_string();

    let service: StreamableHttpService<RqmdServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(server.clone()),
            Arc::new(LocalSessionManager::default()),
            config,
        );

    let addr = format!("{host}:{port}");
    eprintln!("RQMD MCP server listening on http://{addr}/mcp");
    eprintln!("Health endpoint:            http://{addr}/health");

    let router = axum::Router::new()
        .route(
            "/health",
            axum::routing::get(move || {
                let index_dir = index_dir.clone();
                async move { axum::Json(HealthBody { pid, index_dir }) }
            }),
        )
        .nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    on_bound()?;
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}
