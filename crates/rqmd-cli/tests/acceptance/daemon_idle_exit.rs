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

/// `rqmd mcp stop --force` (issue #155). Every process signalled here is a
/// stand-in child this module spawned itself; the pidfiles are written by
/// hand, so no real daemon, port or index is touched.
mod stop_force {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tempfile::TempDir;

    use super::{free_port, pidfile, rqmd_command, wait_until};

    /// A shell that traps SIGTERM (records it in a marker file and carries
    /// on, or exits) so the tests can see whether it was signalled. A thread
    /// reaps it as soon as it dies, because a zombie still answers
    /// `kill(pid, 0)`.
    struct Stub {
        pid: u32,
        marker: PathBuf,
        child: Arc<Mutex<Child>>,
        exited: Arc<AtomicBool>,
        reaper: Option<std::thread::JoinHandle<()>>,
    }

    impl Stub {
        fn spawn(dir: &Path, exit_on_term: bool) -> Stub {
            let marker = dir.join("term.marker");
            let on_term = if exit_on_term {
                "echo term >> \"$1\"; exit 0"
            } else {
                "echo term >> \"$1\""
            };
            let script = format!("trap '{on_term}' TERM; echo ready; while :; do sleep 0.1; done");
            let mut child = Command::new("sh")
                .args(["-c", &script, "stub"])
                .arg(&marker)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn stub");
            let mut line = String::new();
            BufReader::new(child.stdout.take().expect("piped"))
                .read_line(&mut line)
                .expect("stub ready line");
            assert_eq!(line.trim(), "ready");
            let pid = child.id();
            let child = Arc::new(Mutex::new(child));
            let exited = Arc::new(AtomicBool::new(false));
            let reaper = {
                let (child, exited) = (child.clone(), exited.clone());
                std::thread::spawn(move || {
                    while !exited.load(Ordering::SeqCst) {
                        if child
                            .lock()
                            .unwrap()
                            .try_wait()
                            .expect("try_wait")
                            .is_some()
                        {
                            exited.store(true, Ordering::SeqCst);
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                })
            };
            Stub {
                pid,
                marker,
                child,
                exited,
                reaper: Some(reaper),
            }
        }

        fn alive(&self) -> bool {
            !self.exited.load(Ordering::SeqCst)
        }

        fn was_sent_sigterm(&self) -> bool {
            self.marker.exists()
        }

        fn gone_soon(&self) -> bool {
            wait_until(Duration::from_secs(5), || !self.alive())
        }
    }

    impl Drop for Stub {
        fn drop(&mut self) {
            let _ = self.child.lock().unwrap().kill();
            if let Some(reaper) = self.reaper.take() {
                let _ = reaper.join();
            }
        }
    }

    /// When `pid` started, in unix seconds, from `ps` (second resolution).
    fn start_time_of(pid: u32) -> u64 {
        let out = Command::new("ps")
            .args(["-o", "etime=", "-p", &pid.to_string()])
            .output()
            .expect("run ps");
        let etime = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let (days, rest) = etime.split_once('-').unwrap_or(("0", &etime));
        let mut secs = days.parse::<u64>().expect("etime days") * 86_400;
        for part in rest.split(':') {
            secs = secs * 60 + part.parse::<u64>().expect("etime part");
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        now - secs
    }

    /// Answers `GET /health/daemon` with the given pid until dropped.
    struct FakeHealth {
        port: u16,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeHealth {
        fn serve(pid: u32) -> FakeHealth {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
            listener.set_nonblocking(true).expect("nonblocking");
            let port = listener.local_addr().expect("addr").port();
            let stop = Arc::new(AtomicBool::new(false));
            let thread = {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let body = format!(r#"{{"pid":{pid},"index_dir":"fake"}}"#);
                    while !stop.load(Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((mut conn, _)) => {
                                conn.set_nonblocking(false).ok();
                                conn.set_read_timeout(Some(Duration::from_millis(300))).ok();
                                let mut buf = [0u8; 1024];
                                let _ = conn.read(&mut buf);
                                let _ = write!(
                                    conn,
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                    body.len()
                                );
                            }
                            Err(_) => std::thread::sleep(Duration::from_millis(20)),
                        }
                    }
                })
            };
            FakeHealth {
                port,
                stop,
                thread: Some(thread),
            }
        }
    }

    impl Drop for FakeHealth {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// Write a pidfile for `pid`. `start` is the recorded process start time,
    /// or `None` for a record from before the field existed.
    fn write_record(index_dir: &Path, pid: u32, port: u16, start: Option<u64>) {
        let mut record = serde_json::json!({
            "pid": pid,
            "host": "127.0.0.1",
            "port": port,
            "index_dir": index_dir.to_string_lossy(),
            "started_at": "2026-01-01T00:00:00Z",
            "started_at_unix": 1_767_225_600u64,
        });
        if let Some(start) = start {
            record["process_start_unix"] = start.into();
        }
        std::fs::write(pidfile(index_dir), record.to_string()).expect("write pidfile");
    }

    fn run_stop(index_dir: &Path, force: bool) -> (bool, String) {
        let mut cmd = rqmd_command(index_dir, None);
        cmd.args(["mcp", "stop"]);
        if force {
            cmd.arg("--force");
        }
        let out = cmd.stdin(Stdio::null()).output().expect("run mcp stop");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    #[test]
    fn ac_1_stop_without_force_never_sigkills_and_keeps_the_pidfile() {
        let dir = TempDir::new().expect("tempdir");
        let stub = Stub::spawn(dir.path(), false);
        let health = FakeHealth::serve(stub.pid);
        write_record(
            dir.path(),
            stub.pid,
            health.port,
            Some(start_time_of(stub.pid)),
        );

        let (ok, text) = run_stop(dir.path(), false);

        assert!(
            !ok,
            "stop reported success for a process that ignored SIGTERM: {text}"
        );
        assert!(stub.was_sent_sigterm(), "SIGTERM was never sent: {text}");
        assert!(
            text.contains("still running") && text.contains("--force"),
            "unclear message: {text}"
        );
        assert!(
            stub.alive(),
            "stop without --force killed the process: {text}"
        );
        assert!(
            pidfile(dir.path()).exists(),
            "the pidfile was removed: {text}"
        );
    }

    #[test]
    fn ac_2_force_kills_a_daemon_that_ignores_sigterm_and_removes_the_pidfile() {
        let dir = TempDir::new().expect("tempdir");
        let stub = Stub::spawn(dir.path(), false);
        let health = FakeHealth::serve(stub.pid);
        write_record(
            dir.path(),
            stub.pid,
            health.port,
            Some(start_time_of(stub.pid)),
        );

        let (ok, text) = run_stop(dir.path(), true);

        assert!(ok, "stop --force failed: {text}");
        assert!(stub.was_sent_sigterm(), "SIGTERM must come first: {text}");
        assert!(stub.gone_soon(), "the process survived --force: {text}");
        assert!(text.contains("SIGKILL"), "no mention of SIGKILL: {text}");
        assert!(
            !pidfile(dir.path()).exists(),
            "the pidfile was kept: {text}"
        );
    }

    #[test]
    fn ac_3_force_refuses_a_pid_whose_start_time_does_not_match() {
        // Health is down and the pid is alive but started long after the
        // recorded daemon: the pid was recycled.
        let dir = TempDir::new().expect("tempdir");
        let stub = Stub::spawn(dir.path(), false);
        write_record(
            dir.path(),
            stub.pid,
            free_port(),
            Some(start_time_of(stub.pid) - 3600),
        );

        let (ok, text) = run_stop(dir.path(), true);

        assert!(!ok, "a recycled pid was reported as stopped: {text}");
        assert!(
            text.contains("reused") && text.contains("not signalling"),
            "the refusal does not say why: {text}"
        );
        assert!(
            !stub.was_sent_sigterm(),
            "a recycled pid got SIGTERM: {text}"
        );
        assert!(stub.alive(), "a recycled pid was killed: {text}");
        assert!(!pidfile(dir.path()).exists(), "the stale pidfile was kept");

        // A record from before the start time was recorded cannot prove the
        // pid is the daemon either, so it is never force-killed.
        write_record(dir.path(), stub.pid, free_port(), None);
        let (ok, text) = run_stop(dir.path(), true);
        assert!(!ok, "{text}");
        assert!(
            text.contains("no process start time") && text.contains("refusing"),
            "unclear message: {text}"
        );
        assert!(!stub.was_sent_sigterm() && stub.alive(), "{text}");
        assert!(
            pidfile(dir.path()).exists(),
            "the pidfile was removed: {text}"
        );
    }

    #[test]
    fn ac_3_force_rechecks_the_start_time_before_sigkill() {
        // Health names the pid, so SIGTERM goes out; the recorded start time
        // is wrong, so after the grace period --force must not escalate.
        let dir = TempDir::new().expect("tempdir");
        let stub = Stub::spawn(dir.path(), false);
        let health = FakeHealth::serve(stub.pid);
        write_record(
            dir.path(),
            stub.pid,
            health.port,
            Some(start_time_of(stub.pid) - 3600),
        );

        let (ok, text) = run_stop(dir.path(), true);

        assert!(!ok, "{text}");
        assert!(text.contains("will not kill it"), "no reason given: {text}");
        assert!(stub.alive(), "the mismatching pid was killed: {text}");
        assert!(
            pidfile(dir.path()).exists(),
            "the pidfile was removed: {text}"
        );
    }

    #[test]
    fn ac_4_a_live_process_with_no_health_answer_is_not_reported_stale() {
        let dir = TempDir::new().expect("tempdir");
        let stub = Stub::spawn(dir.path(), true);
        write_record(
            dir.path(),
            stub.pid,
            free_port(),
            Some(start_time_of(stub.pid)),
        );

        let out = rqmd_command(dir.path(), None)
            .args(["mcp", "status"])
            .output()
            .expect("run mcp status");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{text}");
        assert!(
            text.contains("running but not answering") && !text.contains("stale"),
            "a live process was reported as stale: {text}"
        );
        assert!(pidfile(dir.path()).exists(), "status deleted the pidfile");
        assert!(stub.alive() && !stub.was_sent_sigterm());

        // `stop` treats it as the daemon: SIGTERM, then the pidfile goes.
        let (ok, text) = run_stop(dir.path(), false);
        assert!(ok, "{text}");
        assert!(stub.was_sent_sigterm(), "{text}");
        assert!(stub.gone_soon(), "{text}");
        assert!(!pidfile(dir.path()).exists(), "{text}");
    }

    #[test]
    fn ac_4_an_old_pidfile_without_a_start_time_keeps_a_live_process_and_its_pidfile() {
        // A pidfile written before the start-time field existed cannot tell
        // the daemon from a recycled pid: report it, signal nothing, delete nothing.
        let dir = TempDir::new().expect("tempdir");
        let stub = Stub::spawn(dir.path(), true);
        write_record(dir.path(), stub.pid, free_port(), None);

        let out = rqmd_command(dir.path(), None)
            .args(["mcp", "status"])
            .output()
            .expect("run mcp status");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{text}");
        assert!(
            text.contains("running but not answering")
                && text.contains("no process start time")
                && !text.contains("stale"),
            "a live process was reported as stale: {text}"
        );
        assert!(pidfile(dir.path()).exists(), "status deleted the pidfile");

        for force in [false, true] {
            let (ok, text) = run_stop(dir.path(), force);
            assert!(!ok, "{text}");
            assert!(
                text.contains("refusing to signal") && !text.contains("no longer running"),
                "{text}"
            );
            assert!(pidfile(dir.path()).exists(), "stop deleted the pidfile");
            assert!(stub.alive() && !stub.was_sent_sigterm(), "{text}");
        }
    }

    #[test]
    fn ac_5_a_recycled_pid_pidfile_never_signals_the_foreign_process() {
        // Without --force a pidfile naming a live pid that is not the daemon
        // is removed as stale and the pid is left alone.
        let dir = TempDir::new().expect("tempdir");
        let stub = Stub::spawn(dir.path(), false);
        write_record(
            dir.path(),
            stub.pid,
            free_port(),
            Some(start_time_of(stub.pid) - 86_400),
        );

        let (ok, text) = run_stop(dir.path(), false);

        assert!(!ok, "{text}");
        assert!(text.contains("stale"), "{text}");
        assert!(
            !stub.was_sent_sigterm(),
            "a foreign pid got SIGTERM: {text}"
        );
        assert!(stub.alive(), "a foreign pid was killed: {text}");
        assert!(!pidfile(dir.path()).exists(), "{text}");
    }

    #[test]
    fn ac_6_cli_and_mcp_docs_describe_the_force_flag() {
        let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
        for name in ["CLI.md", "MCP.md"] {
            let text = std::fs::read_to_string(docs.join(name)).expect("read doc");
            assert!(
                text.lines()
                    .any(|l| l.contains("rqmd mcp stop") && l.contains("--force")),
                "docs/{name} does not document `rqmd mcp stop --force`"
            );
        }
    }
}
