use anyhow::{Context, Result};
use rqmd_mcp::RqmdServer;
use std::path::Path;

pub fn run_mcp(index_dir: &Path, http: bool, port: u16, daemon: bool) -> Result<()> {
    if daemon {
        // Re-spawn self as a background HTTP process: parent exits, child serves.
        let exe = std::env::current_exe().context("cannot locate current executable")?;
        std::process::Command::new(exe)
            .args([
                "--index-dir",
                &index_dir.to_string_lossy(),
                "mcp",
                "--http",
                "--port",
                &port.to_string(),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("failed to start background MCP server")?;
        eprintln!("rqmd MCP server started in background on port {port}");
        return Ok(());
    }

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
