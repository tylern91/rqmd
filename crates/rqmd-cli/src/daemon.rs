//! MCP daemon lifecycle: pidfile, health-based identity, stop/status.
//!
//! A pidfile alone is not enough to safely signal a process — after a reboot
//! or a long-lived daemon's exit, its pid can be recycled by an unrelated
//! process. `stop`/`status` therefore never trust the pidfile's pid in
//! isolation: they cross-check it against the daemon's own `/health` response
//! on the recorded port. Only an exact pid match on a live `/health` counts as
//! "confirmed"; anything else (`/health` unreachable, or answered by a
//! different pid) is resolved without ever calling `kill`.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PidRecord {
    pub pid: u32,
    pub host: String,
    pub port: u16,
    pub index_dir: String,
    pub started_at: String,
    pub started_at_unix: u64,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct HealthResponse {
    pub pid: u32,
    pub index_dir: String,
}

pub enum Identity {
    /// Recorded pid matches what `/health` reports on the recorded port.
    Confirmed(HealthResponse),
    /// `/health` is unreachable — the recorded process is gone.
    Stale,
    /// `/health` answered, but with a different pid — someone else owns the port.
    Foreign(HealthResponse),
}

pub fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

pub fn pidfile_path(index_dir: &Path) -> PathBuf {
    index_dir.join("mcp.pid")
}

pub fn log_path(index_dir: &Path) -> PathBuf {
    index_dir.join("mcp.log")
}

pub fn write_pidfile(index_dir: &Path, pid: u32, host: &str, port: u16) -> Result<()> {
    let started_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let record = PidRecord {
        pid,
        host: host.to_string(),
        port,
        index_dir: index_dir.to_string_lossy().to_string(),
        started_at: rqmd_core::store::rfc3339_now(),
        started_at_unix,
    };
    std::fs::create_dir_all(index_dir)?;
    let path = pidfile_path(index_dir);
    std::fs::write(&path, serde_json::to_string_pretty(&record)?)
        .with_context(|| format!("failed to write pidfile at {}", path.display()))
}

/// Missing or unparseable pidfiles are both treated as "no record" — a
/// pidfile is advisory state, and any problem reading it must never block a
/// fresh start.
pub fn read_pidfile(index_dir: &Path) -> Option<PidRecord> {
    let data = std::fs::read_to_string(pidfile_path(index_dir)).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn remove_pidfile(index_dir: &Path) {
    let _ = std::fs::remove_file(pidfile_path(index_dir));
}

pub fn fetch_health(host: &str, port: u16) -> Option<HealthResponse> {
    let resp = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .ok()?
        .get(format!("http://{host}:{port}/health/daemon"))
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<HealthResponse>().ok()
}

pub fn verify_identity(record: &PidRecord) -> Identity {
    match fetch_health(&record.host, record.port) {
        Some(health) if health.pid == record.pid => Identity::Confirmed(health),
        Some(health) => Identity::Foreign(health),
        None => Identity::Stale,
    }
}

/// Bind-then-drop: fails fast if the port is already occupied. Inherently
/// racy against whatever binds next (the daemon child, in our case) — this is
/// a fail-fast check, not a reservation.
pub fn check_port_free(host: &str, port: u16) -> Result<()> {
    std::net::TcpListener::bind((host, port))
        .with_context(|| format!("port {port} on {host} is already in use"))?;
    Ok(())
}

pub fn wait_for_health(host: &str, port: u16, expected_pid: u32, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(health) = fetch_health(host, port) {
            if health.pid == expected_pid {
                return Ok(());
            }
            bail!(
                "port {port} answered with pid {} instead of the expected {expected_pid}",
                health.pid
            );
        }
        if std::time::Instant::now() >= deadline {
            bail!("no response from http://{host}:{port}/health within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

pub fn tail_log(index_dir: &Path, n: usize) -> String {
    let Ok(content) = std::fs::read_to_string(log_path(index_dir)) else {
        return String::from("(no log file)");
    };
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

pub fn stop_daemon(index_dir: &Path) -> Result<()> {
    let Some(record) = read_pidfile(index_dir) else {
        bail!("no MCP daemon is running for this index (no pidfile found)");
    };

    match verify_identity(&record) {
        Identity::Stale => {
            remove_pidfile(index_dir);
            bail!("MCP daemon pidfile was stale (process no longer running) — removed it");
        }
        Identity::Foreign(health) => {
            bail!(
                "refusing to stop: port {} is held by pid {}, not the recorded pid {} — \
                 the recorded daemon is gone and something else now owns this port",
                record.port,
                health.pid,
                record.pid
            );
        }
        Identity::Confirmed(_) => {}
    }

    signal_terminate(record.pid)?;
    wait_for_exit(record.pid, Duration::from_secs(5))?;
    remove_pidfile(index_dir);
    eprintln!("rqmd MCP daemon (pid {}) stopped", record.pid);
    Ok(())
}

pub fn status_daemon(index_dir: &Path) -> Result<()> {
    let Some(record) = read_pidfile(index_dir) else {
        println!("rqmd MCP daemon: not running (no pidfile)");
        return Ok(());
    };

    match verify_identity(&record) {
        Identity::Confirmed(health) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(record.started_at_unix);
            println!("rqmd MCP daemon: running");
            println!("  pid:        {}", record.pid);
            println!("  address:    http://{}:{}", record.host, record.port);
            println!("  index_dir:  {}", health.index_dir);
            println!("  started_at: {}", record.started_at);
            println!(
                "  uptime:     {}",
                format_duration(now.saturating_sub(record.started_at_unix))
            );
        }
        Identity::Stale => {
            remove_pidfile(index_dir);
            println!("rqmd MCP daemon: not running (stale pidfile removed)");
        }
        Identity::Foreign(health) => {
            println!(
                "rqmd MCP daemon: pidfile present but port {} is now held by a different \
                 process (pid {}) — recorded pid {} is gone",
                record.port, health.pid, record.pid
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
fn signal_terminate(pid: u32) -> Result<()> {
    let ret = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to send SIGTERM to pid {pid}"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn signal_terminate(_pid: u32) -> Result<()> {
    bail!("stopping the MCP daemon is only supported on unix platforms");
}

#[cfg(unix)]
fn wait_for_exit(pid: u32, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while unsafe { libc::kill(pid as i32, 0) } == 0 {
        if std::time::Instant::now() >= deadline {
            bail!("pid {pid} did not exit within {timeout:?} after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Ok(())
}

#[cfg(not(unix))]
fn wait_for_exit(_pid: u32, _timeout: Duration) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case this whole design exists for: a pidfile whose pid was
    /// recycled by an unrelated process after the daemon died (or the
    /// machine rebooted) must never be signalled, and must never block a
    /// fresh start.
    #[test]
    fn stale_pidfile_is_not_signalled_and_does_not_block_startup() {
        let dir = tempfile::tempdir().unwrap();

        // No server is actually listening on this port, so `verify_identity`
        // must resolve to `Stale` — never `Confirmed` or `Foreign` — for any
        // pid, including one that legitimately belongs to another live
        // process (like this very test process).
        let record = PidRecord {
            pid: std::process::id(),
            host: "127.0.0.1".to_string(),
            port: 39_812,
            index_dir: dir.path().to_string_lossy().to_string(),
            started_at: "2020-01-01T00:00:00Z".to_string(),
            started_at_unix: 0,
        };
        write_pidfile(dir.path(), record.pid, &record.host, record.port).unwrap();

        assert!(matches!(verify_identity(&record), Identity::Stale));

        // A stale identity must not block startup: the port itself is free,
        // so a fresh daemon can bind it once the caller removes the pidfile.
        check_port_free(&record.host, record.port).unwrap();

        // status/stop both self-heal by removing the stale pidfile rather
        // than ever calling kill() on the recycled pid.
        status_daemon(dir.path()).unwrap();
        assert!(read_pidfile(dir.path()).is_none());
    }

    #[test]
    fn read_pidfile_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_pidfile(dir.path()).is_none());
    }

    #[test]
    fn read_pidfile_corrupt_json_is_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(pidfile_path(dir.path()), "not json").unwrap();
        assert!(read_pidfile(dir.path()).is_none());
    }
}
