use anyhow::{Context, Result, bail};
use rqmd_mcp::RqmdServer;
use std::path::Path;
use std::time::Duration;

use crate::daemon;

#[allow(clippy::too_many_arguments)]
pub fn run_mcp(
    index_dir: &Path,
    http: bool,
    host: &str,
    port: u16,
    is_daemon: bool,
    allow_non_loopback: bool,
) -> Result<()> {
    if http && !daemon::is_loopback(host) && !allow_non_loopback {
        bail!(
            "refusing to bind the MCP server to non-loopback host {host}: this exposes the \
             index's full-text and semantic search — including `get`, which returns arbitrary \
             indexed file content — with no authentication to anything that can reach \
             {host}:{port}.\n\nIf this is intentional (e.g. a trusted network or container), \
             pass --allow-non-loopback (or set RQMD_MCP_ALLOW_NON_LOOPBACK=1)."
        );
    }

    if is_daemon {
        return spawn_daemon(index_dir, host, port, allow_non_loopback);
    }

    if !http {
        eprintln!("Initialising RQMD MCP server...");
        let server =
            RqmdServer::new(index_dir.to_path_buf()).context("failed to create RQMD server")?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        return rt.block_on(rqmd_mcp::run_stdio(server));
    }

    daemon::check_port_free(host, port).context("cannot start MCP server")?;

    eprintln!("Initialising RQMD MCP server...");
    let server =
        RqmdServer::new(index_dir.to_path_buf()).context("failed to create RQMD server")?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let index_dir_owned = index_dir.to_path_buf();
    let host_owned = host.to_string();
    let pid = std::process::id();
    let result = rt.block_on(rqmd_mcp::run_http(server, host, port, || {
        daemon::write_pidfile(&index_dir_owned, pid, &host_owned, port)
    }));
    daemon::remove_pidfile(index_dir);
    result
}

pub fn run_mcp_status(index_dir: &Path) -> Result<()> {
    daemon::status_daemon(index_dir)
}

pub fn run_mcp_stop(index_dir: &Path) -> Result<()> {
    daemon::stop_daemon(index_dir)
}

fn spawn_daemon(index_dir: &Path, host: &str, port: u16, allow_non_loopback: bool) -> Result<()> {
    std::fs::create_dir_all(index_dir)
        .with_context(|| format!("failed to create index dir at {}", index_dir.display()))?;

    if let Some(existing) = daemon::read_pidfile(index_dir) {
        match daemon::verify_identity(&existing) {
            daemon::Identity::Confirmed(_) => bail!(
                "rqmd MCP daemon already running (pid {}) on http://{}:{} — run `rqmd mcp stop` first",
                existing.pid,
                existing.host,
                existing.port
            ),
            daemon::Identity::Foreign(health) => bail!(
                "{}:{} is already in use by a different process (pid {}) — pick a different --port",
                existing.host,
                existing.port,
                health.pid
            ),
            daemon::Identity::Stale => daemon::remove_pidfile(index_dir),
        }
    }

    daemon::check_port_free(host, port).context("cannot start daemon")?;

    let exe = std::env::current_exe().context("cannot locate current executable")?;
    let log_path = daemon::log_path(index_dir);
    let log_out = std::fs::File::create(&log_path).context("failed to create daemon log file")?;
    let log_err = log_out
        .try_clone()
        .context("failed to clone daemon log handle")?;

    let mut cmd = std::process::Command::new(exe);
    cmd.args([
        "--index-dir",
        &index_dir.to_string_lossy(),
        "mcp",
        "--http",
        "--host",
        host,
        "--port",
        &port.to_string(),
    ]);
    if allow_non_loopback {
        cmd.arg("--allow-non-loopback");
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(log_out)
        .stderr(log_err);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Detach into its own session so it isn't tied to this process's
        // controlling terminal/process group — otherwise it's a plain orphan
        // that a terminal-closing SIGHUP can still reach.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    let mut child = cmd
        .spawn()
        .context("failed to start background MCP server")?;
    let pid = child.id();

    match daemon::wait_for_health(host, port, pid, Duration::from_secs(5)) {
        Ok(()) => {
            // The daemon writes its own pidfile once its listener is bound
            // (see run_http's `on_bound` hook) — we only read/report here,
            // so there is a single writer for the file's whole lifecycle.
            eprintln!("rqmd MCP daemon started (pid {pid}) on http://{host}:{port}");
            Ok(())
        }
        Err(e) => {
            // Health check timed out — the child may still be alive (e.g.
            // stuck loading a large model past the check budget). Leaving it
            // running here would orphan it: we already told the user launch
            // failed, but the child would keep going, bind the port, and
            // write a pidfile with nothing tracking it. Kill and reap it
            // before reporting failure.
            let _ = child.kill();
            let _ = child.wait();
            let tail = daemon::tail_log(index_dir, 20);
            bail!(
                "daemon failed to become healthy within 5s: {e}\n\n--- last 20 lines of {} ---\n{tail}",
                log_path.display()
            );
        }
    }
}
