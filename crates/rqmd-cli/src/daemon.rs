//! MCP daemon lifecycle: pidfile, health-based identity, stop/status.
//!
//! A pidfile alone is not enough to safely signal a process — after a reboot
//! or a long-lived daemon's exit, its pid can be recycled by an unrelated
//! process. `stop`/`status` therefore never trust the pidfile's pid in
//! isolation: they cross-check it against the daemon's own `/health` response
//! on the recorded port. Only an exact pid match on a live `/health` counts as
//! "confirmed"; a `/health` answered by a different pid is never signalled.
//!
//! `/health` can also be silent while the process is alive (a daemon that is
//! slow to shut down). The pidfile therefore records the daemon's own process
//! start time as well, read from the OS, so a live pid whose start time still
//! matches is recognised as the daemon even without an answer from `/health`.
//! A match sends the `SIGTERM` of a plain `stop`, and it is the only thing
//! that lets `rqmd mcp stop --force` escalate to `SIGKILL`; a recycled pid
//! starts at a different time and is never signalled.

use anyhow::{Context, Result, bail};
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
    /// The daemon's own process start time (unix seconds) as the OS reports
    /// it. Added after the first release of the pidfile: a record without it
    /// still parses, and `stop --force` treats such a record as not
    /// force-killable because the pid cannot be told apart from a recycled one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_start_unix: Option<u64>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct HealthResponse {
    pub pid: u32,
    pub index_dir: String,
}

pub enum Identity {
    /// Recorded pid matches what `/health` reports on the recorded port.
    Confirmed(HealthResponse),
    /// `/health` is unreachable and the recorded pid is not the daemon any
    /// more (the process is gone, or the pid was recycled): nothing to stop.
    Stale,
    /// `/health` is unreachable but the recorded pid is alive and its start
    /// time matches the one in the pidfile: the daemon is still there, just
    /// not answering.
    AliveUnresponsive,
    /// `/health` is unreachable and the recorded pid is alive, but the pidfile
    /// has no start time to check it against (written by an older rqmd, or the
    /// OS would not report one). It may be the daemon or a recycled pid: it is
    /// reported, never signalled, and the pidfile is kept.
    AliveUnverified(NotOurs),
    /// `/health` answered, but with a different pid — someone else owns the port.
    Foreign(HealthResponse),
}

pub fn is_loopback(host: &str) -> bool {
    matches!(rqmd_mcp::bare_host(host), "127.0.0.1" | "localhost" | "::1")
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
        process_start_unix: process_start_time(pid),
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
        .get(format!(
            "http://{}/health/daemon",
            rqmd_mcp::host_port(host, port)
        ))
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
        None => match check_recorded_process(record) {
            None => Identity::AliveUnresponsive,
            Some(why @ (NotOurs::NoRecordedStart | NotOurs::UnreadableStart)) => {
                Identity::AliveUnverified(why)
            }
            Some(NotOurs::Gone | NotOurs::DifferentStart { .. }) => Identity::Stale,
        },
    }
}

/// Start times come from the same OS source on both sides, so they normally
/// match exactly; the slack covers second-granularity rounding (and, on
/// Linux, a small drift of the boot time), while a recycled pid differs by
/// at least the time the daemon had been gone.
const START_TIME_SLACK_SECS: u64 = 2;

/// Why the recorded pid is not (or cannot be shown to be) the daemon we
/// recorded. `None` means it is: the pid is alive and its start time matches.
#[derive(Debug, PartialEq, Eq)]
pub enum NotOurs {
    /// The pid is not running.
    Gone,
    /// The pidfile has no process start time (written by an older rqmd, or by one
    /// on a platform that does not report it).
    NoRecordedStart,
    /// The OS would not tell us when the pid started.
    UnreadableStart,
    /// The pid is running but started at a different time: it was recycled.
    DifferentStart { actual: u64, recorded: u64 },
}

pub fn check_recorded_process(record: &PidRecord) -> Option<NotOurs> {
    if !pid_alive(record.pid) {
        return Some(NotOurs::Gone);
    }
    let Some(recorded) = record.process_start_unix else {
        return Some(NotOurs::NoRecordedStart);
    };
    let Some(actual) = process_start_time(record.pid) else {
        return Some(NotOurs::UnreadableStart);
    };
    if actual.abs_diff(recorded) > START_TIME_SLACK_SECS {
        return Some(NotOurs::DifferentStart { actual, recorded });
    }
    None
}

impl std::fmt::Display for NotOurs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotOurs::Gone => write!(f, "the process is no longer running"),
            NotOurs::NoRecordedStart => write!(
                f,
                "the pidfile has no process start time (an older rqmd wrote it, or this platform \
                 does not report one), so the pid cannot be told apart from a recycled one"
            ),
            NotOurs::UnreadableStart => write!(
                f,
                "the operating system would not report when the process started"
            ),
            NotOurs::DifferentStart { actual, recorded } => write!(
                f,
                "it started at unix time {actual} but the pidfile recorded a daemon that \
                 started at {recorded}, so the pid was reused by another process"
            ),
        }
    }
}

/// Bind-then-drop: fails fast if the port is already occupied. Inherently
/// racy against whatever binds next (the daemon child, in our case) — this is
/// a fail-fast check, not a reservation.
pub fn check_port_free(host: &str, port: u16) -> Result<()> {
    std::net::TcpListener::bind((rqmd_mcp::bare_host(host), port))
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
            bail!(
                "no response from http://{}/health within {timeout:?}",
                rqmd_mcp::host_port(host, port)
            );
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

const STOP_GRACE: Duration = Duration::from_secs(5);

/// `force` adds one step after the normal SIGTERM and wait: if the process is
/// still alive, SIGKILL it, but only once the pid has been shown to still be
/// the daemon we recorded. Without `force` this never sends SIGKILL.
pub fn stop_daemon(index_dir: &Path, force: bool) -> Result<()> {
    stop_daemon_with(index_dir, force, STOP_GRACE)
}

fn stop_daemon_with(index_dir: &Path, force: bool, grace: Duration) -> Result<()> {
    let Some(record) = read_pidfile(index_dir) else {
        bail!("no MCP daemon is running for this index (no pidfile found)");
    };

    match verify_identity(&record) {
        Identity::Stale => {
            remove_pidfile(index_dir);
            match check_recorded_process(&record) {
                // A live pid that is not the daemon: say so, and leave it alone.
                Some(why @ NotOurs::DifferentStart { .. }) => bail!(
                    "MCP daemon pidfile was stale — removed it; not signalling pid {}: {why}",
                    record.pid
                ),
                _ => bail!("MCP daemon pidfile was stale (process no longer running) — removed it"),
            }
        }
        Identity::AliveUnverified(why) => bail!(
            "refusing to signal pid {}: it is running but {why} — pidfile kept; if it is the \
             daemon, stop it yourself, and if not, delete {}",
            record.pid,
            pidfile_path(index_dir).display()
        ),
        Identity::Foreign(health) => {
            bail!(
                "refusing to stop: port {} is held by pid {}, not the recorded pid {} — \
                 the recorded daemon is gone and something else now owns this port",
                record.port,
                health.pid,
                record.pid
            );
        }
        Identity::Confirmed(_) | Identity::AliveUnresponsive => {}
    }

    signal_terminate(record.pid)?;
    if !wait_for_gone(record.pid, grace) {
        if !force {
            bail!(
                "pid {} did not exit within {grace:?} after SIGTERM and is still running — \
                 pidfile kept; run `rqmd mcp stop --force` to kill it",
                record.pid
            );
        }
        // Re-verify right before SIGKILL: only a pid that is still the
        // recorded daemon is ever killed.
        match check_recorded_process(&record) {
            None => {}
            Some(NotOurs::Gone) => {
                remove_pidfile(index_dir);
                eprintln!("rqmd MCP daemon (pid {}) stopped", record.pid);
                return Ok(());
            }
            Some(why) => bail!(
                "pid {} did not exit within {grace:?} after SIGTERM, and --force will not \
                 kill it: {why} — pidfile kept",
                record.pid
            ),
        }
        signal_kill(record.pid)?;
        if !wait_for_gone(record.pid, grace) {
            bail!(
                "pid {} is still running {grace:?} after SIGKILL — pidfile kept",
                record.pid
            );
        }
        remove_pidfile(index_dir);
        eprintln!(
            "rqmd MCP daemon (pid {}) did not exit within {grace:?} after SIGTERM; killed with SIGKILL",
            record.pid
        );
        return Ok(());
    }
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
            println!(
                "  address:    http://{}",
                rqmd_mcp::host_port(&record.host, record.port)
            );
            println!("  index_dir:  {}", health.index_dir);
            println!("  started_at: {}", record.started_at);
            println!(
                "  uptime:     {}",
                format_duration(now.saturating_sub(record.started_at_unix))
            );
        }
        Identity::AliveUnresponsive => {
            println!(
                "rqmd MCP daemon: pid {} is running but not answering on http://{} — \
                 pidfile kept; `rqmd mcp stop` stops it, `rqmd mcp stop --force` kills it \
                 if it will not exit",
                record.pid,
                rqmd_mcp::host_port(&record.host, record.port)
            );
        }
        Identity::AliveUnverified(why) => {
            println!(
                "rqmd MCP daemon: pid {} is running but not answering on http://{}, and {why} — \
                 pidfile kept, nothing signalled; if it is the daemon, stop it yourself, and if \
                 not, delete {}",
                record.pid,
                rqmd_mcp::host_port(&record.host, record.port),
                pidfile_path(index_dir).display()
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
fn signal_kill(pid: u32) -> Result<()> {
    let ret = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to send SIGKILL to pid {pid}"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn signal_kill(_pid: u32) -> Result<()> {
    bail!("stopping the MCP daemon is only supported on unix platforms");
}

/// Alive means `kill(pid, 0)` does not say ESRCH; EPERM is a live process we
/// may not signal.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let ret = unsafe { libc::kill(pid as i32, 0) };
    ret == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    false
}

/// True once the pid has gone, false if it is still there after `timeout`.
#[cfg(unix)]
fn wait_for_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while unsafe { libc::kill(pid as i32, 0) } == 0 {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

#[cfg(not(unix))]
fn wait_for_gone(_pid: u32, _timeout: Duration) -> bool {
    true
}

/// When `pid` started, in unix seconds, as the OS reports it. `None` when the
/// platform has no supported source or the process cannot be inspected.
#[cfg(target_os = "macos")]
pub fn process_start_time(pid: u32) -> Option<u64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let got = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (got == size).then_some(info.pbi_start_tvsec)
}

/// Field 22 of `/proc/<pid>/stat` (clock ticks since boot) plus the boot time
/// from `/proc/stat`.
#[cfg(target_os = "linux")]
pub fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let ticks = parse_stat_start_ticks(&stat)?;
    let btime = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("btime ")?.trim().parse::<u64>().ok())?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    (hz > 0).then(|| btime + ticks / hz as u64)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn process_start_time(_pid: u32) -> Option<u64> {
    None
}

/// `starttime` is field 22 of `/proc/<pid>/stat`. Field 2 (the command) is in
/// parentheses and may itself contain spaces and parentheses, so count from
/// the last `)`: the fields after it start at field 3.
#[cfg(any(target_os = "linux", test))]
fn parse_stat_start_ticks(stat: &str) -> Option<u64> {
    let after_comm = &stat[stat.rfind(')')? + 1..];
    after_comm.split_whitespace().nth(19)?.parse().ok()
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

        // No server is actually listening on this port. A live pid whose
        // pidfile has no start time (an older rqmd wrote it) cannot be told
        // apart from a recycled one: it is reported, never signalled, and the
        // pidfile is kept for the user to decide.
        let record = PidRecord {
            pid: std::process::id(),
            host: "127.0.0.1".to_string(),
            port: 39_812,
            index_dir: dir.path().to_string_lossy().to_string(),
            started_at: "2020-01-01T00:00:00Z".to_string(),
            started_at_unix: 0,
            process_start_unix: None,
        };
        assert!(matches!(
            verify_identity(&record),
            Identity::AliveUnverified(NotOurs::NoRecordedStart)
        ));

        // With a recorded start time that differs from this live process's,
        // the pid is known to be recycled: `verify_identity` must resolve to
        // `Stale` — never `Confirmed` or `Foreign` — even though the pid
        // belongs to another live process (this very test process).
        let recycled = PidRecord {
            process_start_unix: process_start_time(std::process::id()).map(|t| t - 3600),
            ..record.clone()
        };
        assert!(matches!(verify_identity(&recycled), Identity::Stale));

        // A stale identity must not block startup: the port itself is free,
        // so a fresh daemon can bind it once the caller removes the pidfile.
        check_port_free(&record.host, record.port).unwrap();

        // status/stop both self-heal by removing the stale pidfile rather
        // than ever calling kill() on the recycled pid.
        std::fs::write(
            pidfile_path(dir.path()),
            serde_json::to_string(&recycled).unwrap(),
        )
        .unwrap();
        status_daemon(dir.path()).unwrap();
        assert!(read_pidfile(dir.path()).is_none());
    }

    #[test]
    fn read_pidfile_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_pidfile(dir.path()).is_none());
    }

    #[test]
    fn is_loopback_accepts_bracketed_and_bare_ipv6_but_not_other_hosts() {
        for h in ["127.0.0.1", "localhost", "::1", "[::1]"] {
            assert!(is_loopback(h), "{h}");
        }
        for h in ["0.0.0.0", "::", "[::]", "192.168.1.5", "example.com"] {
            assert!(!is_loopback(h), "{h}");
        }
    }

    #[test]
    fn read_pidfile_corrupt_json_is_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(pidfile_path(dir.path()), "not json").unwrap();
        assert!(read_pidfile(dir.path()).is_none());
    }

    #[test]
    fn pidfile_written_before_the_start_time_field_still_parses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            pidfile_path(dir.path()),
            r#"{"pid":4242,"host":"127.0.0.1","port":8181,"index_dir":"/x",
                "started_at":"2020-01-01T00:00:00Z","started_at_unix":1577836800}"#,
        )
        .unwrap();
        let record = read_pidfile(dir.path()).expect("old record parses");
        assert_eq!(record.pid, 4242);
        assert_eq!(record.process_start_unix, None);
        // And a record without it does not write the key back out.
        assert!(
            !serde_json::to_string(&record)
                .unwrap()
                .contains("process_start_unix")
        );
    }

    #[test]
    fn stat_start_ticks_are_counted_from_the_last_paren() {
        // Field 22 (starttime) is 777; the command name holds spaces and parens.
        let stat = "123 (a) b (c) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 777 23 24";
        assert_eq!(parse_stat_start_ticks(stat), Some(777));
        assert_eq!(parse_stat_start_ticks("garbage"), None);
        assert_eq!(parse_stat_start_ticks("1 (x) S 1 2"), None);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn own_start_time_is_recent_and_matches_a_record_of_it() {
        let me = std::process::id();
        let start = process_start_time(me).expect("start time of this process");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(start <= now + START_TIME_SLACK_SECS && now - start < 86_400);

        let mut record = PidRecord {
            pid: me,
            host: "127.0.0.1".into(),
            port: 1,
            index_dir: String::new(),
            started_at: String::new(),
            started_at_unix: 0,
            process_start_unix: Some(start),
        };
        assert_eq!(check_recorded_process(&record), None);
        record.process_start_unix = Some(start - 3600);
        assert!(matches!(
            check_recorded_process(&record),
            Some(NotOurs::DifferentStart { .. })
        ));
        record.process_start_unix = None;
        assert_eq!(
            check_recorded_process(&record),
            Some(NotOurs::NoRecordedStart)
        );
    }
}
