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
