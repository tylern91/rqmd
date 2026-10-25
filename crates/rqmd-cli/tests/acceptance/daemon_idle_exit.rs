//! `RQMD_MCP_IDLE_EXIT_SECS` makes the HTTP daemon exit after that many seconds
//! without a request. Only requests count: `rqmd mcp status` health checks do
//! not. Unset, the daemon runs until it is stopped.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::helpers::rqmd;

const ENV: &str = "RQMD_MCP_IDLE_EXIT_SECS";

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    done()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("bind port 0")
        .local_addr()
        .expect("local addr")
        .port()
}

fn pidfile(index_dir: &Path) -> std::path::PathBuf {
    index_dir.join("mcp.pid")
}

/// Kills the daemon if a test fails while it is still running.
struct Reap(Child);

impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Stops a background daemon left behind by a failing test.
struct StopDaemon<'a>(&'a Path);

impl Drop for StopDaemon<'_> {
    fn drop(&mut self) {
        let _ = rqmd(self.0).args(["mcp", "stop"]).output();
    }
}

fn rqmd_command(index_dir: &Path, idle: Option<&str>) -> Command {
    let program = assert_cmd::Command::cargo_bin("rqmd").expect("rqmd binary");
    let mut command = Command::new(program.get_program());
    command
        .arg("--index-dir")
        .arg(index_dir)
        .env_remove("RQMD_MCP_HOST")
        .env_remove("RQMD_MCP_PORT")
        .env_remove(ENV);
    if let Some(idle) = idle {
        command.env(ENV, idle);
    }
    command
}

struct Serving {
    child: Reap,
    port: u16,
    stderr_path: std::path::PathBuf,
}

impl Serving {
    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.0.try_wait().expect("try_wait")
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
}

/// Start `rqmd mcp --http` in the foreground and wait until it has recorded
/// its pidfile, which it writes once the port is bound.
fn serve(index_dir: &Path, idle: Option<&str>) -> Serving {
    let port = free_port();
    let stderr_path = index_dir.join("serve.stderr");
    let child = rqmd_command(index_dir, idle)
        .args(["mcp", "--http", "--port", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr_path).expect("stderr file"))
        .spawn()
        .expect("spawn rqmd");
    let mut serving = Serving {
        child: Reap(child),
        port,
        stderr_path,
    };
    assert!(
        wait_until(Duration::from_secs(30), || pidfile(index_dir).exists()
            || serving.exited().is_some()),
        "the server never became ready"
    );
    assert!(
        serving.exited().is_none(),
        "the server exited at startup: {}",
        serving.stderr()
    );
    serving
}

#[test]
fn ac_1_idle_daemon_exits_and_removes_its_pidfile() {
    let index_dir = TempDir::new().expect("tempdir");
    let mut serving = serve(index_dir.path(), Some("2"));
    let ready = Instant::now();

    assert!(
        wait_until(Duration::from_secs(30), || serving.exited().is_some()),
        "the daemon was still running 30s after it went idle"
    );
    let status = serving.exited().expect("exited");
    assert!(
        status.success(),
        "exit status {status}: {}",
        serving.stderr()
    );
    assert!(
        ready.elapsed() >= Duration::from_millis(1500),
        "the daemon left before its idle period was over: {:?}",
        ready.elapsed()
    );
    assert!(
        !pidfile(index_dir.path()).exists(),
        "the pidfile outlived the daemon"
    );
    assert!(
        serving.stderr().contains("idle for 2s; shutting down"),
        "no idle line in: {}",
        serving.stderr()
    );
}

#[test]
fn ac_2_zero_or_invalid_value_is_an_error_before_anything_starts() {
    for value in ["0", "abc", "-5", "1.5", ""] {
        let index_dir = TempDir::new().expect("tempdir");
        let port = free_port().to_string();
        let out = rqmd_command(index_dir.path(), Some(value))
            .args(["mcp", "--http", "--port", &port])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn rqmd");
        let mut out = Reap(out);
        assert!(
            wait_until(Duration::from_secs(20), || out
                .0
                .try_wait()
                .expect("try_wait")
                .is_some()),
            "{ENV}={value:?} did not stop the server from starting"
        );
        let status = out.0.try_wait().expect("try_wait").expect("exited");
        let mut stderr = String::new();
        std::io::Read::read_to_string(out.0.stderr.as_mut().expect("piped"), &mut stderr)
            .expect("read stderr");
        assert!(!status.success(), "{ENV}={value:?} was accepted");
        assert!(
            stderr.contains(ENV) && stderr.contains("positive whole number"),
            "{ENV}={value:?} gave an unclear error: {stderr}"
        );
        assert!(!pidfile(index_dir.path()).exists());
    }
}

#[test]
fn ac_3_daemon_launch_rejects_an_invalid_value_without_starting() {
    let index_dir = TempDir::new().expect("tempdir");
    let _stop = StopDaemon(index_dir.path());
    let port = free_port().to_string();
    let started = Instant::now();
    let out = rqmd_command(index_dir.path(), Some("0"))
        .args(["mcp", "--daemon", "--port", &port])
        .stdin(Stdio::null())
        .output()
        .expect("run rqmd");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a daemon was started: {stderr}");
    assert!(
        stderr.contains(ENV) && stderr.contains("positive whole number"),
        "unclear error: {stderr}"
    );
    assert!(
        !stderr.contains("failed to become healthy"),
        "the value was only rejected after a daemon had been spawned: {stderr}"
    );
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(!pidfile(index_dir.path()).exists());
}

#[test]
fn ac_4_unset_keeps_the_daemon_running() {
    let index_dir = TempDir::new().expect("tempdir");
    let mut serving = serve(index_dir.path(), None);

    std::thread::sleep(Duration::from_secs(4));
    assert!(
        serving.exited().is_none(),
        "the daemon exited with no idle limit set: {}",
        serving.stderr()
    );
    assert!(pidfile(index_dir.path()).exists());
}

#[test]
fn ac_5_status_checks_do_not_count_as_activity() {
    let index_dir = TempDir::new().expect("tempdir");
    let mut serving = serve(index_dir.path(), Some("2"));
    let mut polls = 0;

    let deadline = Instant::now() + Duration::from_secs(30);
    while serving.exited().is_none() && Instant::now() < deadline {
        let status = rqmd_command(index_dir.path(), None)
            .args(["mcp", "status"])
            .output()
            .expect("run mcp status");
        if serving.exited().is_none() {
            assert!(
                status.status.success(),
                "mcp status failed while the daemon ran: {}",
                String::from_utf8_lossy(&status.stderr)
            );
        }
        polls += 1;
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(
        serving.exited().is_some(),
        "status checks kept the daemon alive"
    );
    assert!(
        polls >= 2,
        "the daemon went away before it was polled twice"
    );
}

#[test]
fn ac_6_requests_keep_the_daemon_alive() {
    let index_dir = TempDir::new().expect("tempdir");
    let mut serving = serve(index_dir.path(), Some("3"));
    let url = format!("http://127.0.0.1:{}/mcp", serving.port);
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");

    let busy_until = Instant::now() + Duration::from_secs(8);
    while Instant::now() < busy_until {
        let _ = http
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body("{}")
            .send()
            .expect("request reaches the daemon");
        assert!(
            serving.exited().is_none(),
            "the daemon left while it was being used: {}",
            serving.stderr()
        );
        std::thread::sleep(Duration::from_millis(400));
    }

    assert!(
        wait_until(Duration::from_secs(30), || serving.exited().is_some()),
        "the daemon stayed up after the requests stopped"
    );
    assert!(serving.exited().expect("exited").success());
}
