use anyhow::{Context, Result};
use rqmd_mcp::RqmdServer;
use std::path::Path;

pub fn run_mcp(index_dir: &Path, http: bool, port: u16) -> Result<()> {
    eprintln!("Initialising RQMD MCP server...");
    let server =
        RqmdServer::new(index_dir.to_path_buf()).context("failed to create RQMD server")?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        if http {
            rqmd_mcp::run_http(server, port).await
        } else {
            rqmd_mcp::run_stdio(server).await
        }
    })
}
