//! `rqmd update` forwards SIGINT, SIGTERM and SIGHUP to a running update hook,
//! so stopping rqmd (Ctrl-C, a service manager, a closing terminal) does not
//! leave the hook running after rqmd has exited.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::helpers::{fixture_collection, rqmd};

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only probes for existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}

fn read_pid(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Kills rqmd and every pid the hook recorded, so a failing test leaves no
/// stray process behind.
struct Cleanup {
    rqmd: Child,
    pidfiles: Vec<PathBuf>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.rqmd.kill();
        let _ = self.rqmd.wait();
        for pid in self.pidfiles.iter().filter_map(|p| read_pid(p)) {
            // SAFETY: plain syscall on a pid the hook wrote itself.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

struct Run {
    code: Option<i32>,
    /// Hook pids still alive when rqmd had exited and the hook had its chance to go.
    survivors: Vec<i32>,
    elapsed: Duration,
}

/// Run `rqmd update` with `hook` as the collection's `update_command`, send
/// `signal` to rqmd once the hook has written every pidfile, and report how
/// rqmd exited and which hook pids were recorded.
fn signal_rqmd_during_hook(hook: &dyn Fn(&[PathBuf]) -> String, signal: i32) -> Run {
    let index_dir = TempDir::new().expect("tempdir");
    let scratch = TempDir::new().expect("tempdir");
    let pidfiles = vec![
        scratch.path().join("hook.pid"),
        scratch.path().join("sub.pid"),
    ];
    let fx = fixture_collection("c", "hello\n");

    rqmd(index_dir.path())
        .args(["collection", "add"])
        .arg(&fx.path)
        .args(["--name", "c"])
        .assert()
        .success();
    rqmd(index_dir.path())
        .args(["collection", "update-cmd", "c"])
        .arg(hook(&pidfiles))
        .assert()
        .success();

    let program = assert_cmd::Command::cargo_bin("rqmd").expect("rqmd binary");
    // stdin is not a terminal, so the hook runs in its own process group.
    let child = Command::new(program.get_program())
        .arg("--index-dir")
        .arg(index_dir.path())
        .arg("update")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn rqmd");
    let rqmd_pid = i32::try_from(child.id()).expect("pid");
    let mut cleanup = Cleanup {
        rqmd: child,
        pidfiles: pidfiles.clone(),
    };

    assert!(
        wait_until(Duration::from_secs(20), || pidfiles
            .iter()
            .all(|p| read_pid(p).is_some())),
        "the hook never started"
    );
    let mut pids: Vec<i32> = pidfiles.iter().filter_map(|p| read_pid(p)).collect();
    pids.dedup();

    let sent = Instant::now();
    // SAFETY: plain syscall on the rqmd child this test spawned.
    assert_eq!(unsafe { libc::kill(rqmd_pid, signal) }, 0);

    let mut code = None;
    let exited = wait_until(Duration::from_secs(20), || {
        match cleanup.rqmd.try_wait().expect("try_wait") {
            Some(status) => {
                code = status.code();
                true
            }
            None => false,
        }
    });
    let elapsed = sent.elapsed();
    assert!(exited, "rqmd did not exit after signal {signal}");
    // Give the hook a moment to go away on its own; the assertions below say
    // whether it did.
    wait_until(Duration::from_secs(8), || pids.iter().all(|p| !alive(*p)));
    // Measured here: `cleanup` kills whatever is left when this function returns.
    let survivors = pids.into_iter().filter(|p| alive(*p)).collect();
    Run {
        code,
        survivors,
        elapsed,
    }
}

/// A hook that records the pid of its shell and then becomes `sleep`.
fn plain_hook(pidfiles: &[PathBuf]) -> String {
    let sub = pidfiles[1].display();
    format!(
        "echo $$ > '{}'; echo $$ > '{sub}'; exec sleep 60",
        pidfiles[0].display()
    )
}

/// A hook that handles the signal exits at once; one that is only killed at
/// the end of the grace period takes at least this long.
const PROMPT: Duration = Duration::from_secs(4);

fn assert_forwarded(signal: i32, expected_code: i32) {
    let run = signal_rqmd_during_hook(&plain_hook, signal);
    assert!(
        run.elapsed < PROMPT,
        "rqmd took {:?} to exit: the hook did not get the signal",
        run.elapsed
    );
    assert!(
        run.survivors.is_empty(),
        "hook processes {:?} survived signal {signal}",
        run.survivors
    );
    assert_eq!(run.code, Some(expected_code));
}

#[test]
fn ac_1_sigterm_to_rqmd_stops_the_hook_and_exits_143() {
    assert_forwarded(libc::SIGTERM, 143);
}

#[test]
fn ac_2_sigint_to_rqmd_stops_the_hook_and_exits_130() {
    assert_forwarded(libc::SIGINT, 130);
}

#[test]
fn ac_3_sighup_to_rqmd_stops_the_hook_and_exits_129() {
    assert_forwarded(libc::SIGHUP, 129);
}

/// The signal reaches the whole hook group, not only the shell: a background
/// child of the hook goes too.
#[test]
fn ac_4_sigterm_reaches_a_child_the_hook_started() {
    let hook = |pidfiles: &[PathBuf]| {
        format!(
            "sleep 60 & echo $! > '{}'; echo $$ > '{}'; wait",
            pidfiles[1].display(),
            pidfiles[0].display()
        )
    };
    let run = signal_rqmd_during_hook(&hook, libc::SIGTERM);
    assert!(run.elapsed < PROMPT, "rqmd took {:?} to exit", run.elapsed);
    assert!(
        run.survivors.is_empty(),
        "hook processes {:?} survived SIGTERM",
        run.survivors
    );
    assert_eq!(run.code, Some(143));
}

/// A hook that ignores the signal is killed once the grace period ends, so
/// rqmd still exits instead of waiting for it.
#[test]
fn ac_5_hook_that_ignores_sigterm_is_killed_after_the_grace_period() {
    let hook = |pidfiles: &[PathBuf]| {
        format!(
            "trap '' TERM; sleep 60 & echo $! > '{}'; echo $$ > '{}'; wait",
            pidfiles[1].display(),
            pidfiles[0].display()
        )
    };
    let run = signal_rqmd_during_hook(&hook, libc::SIGTERM);
    assert!(
        run.survivors.is_empty(),
        "hook processes {:?} survived the grace period",
        run.survivors
    );
    assert_eq!(run.code, Some(143));
    assert!(
        (PROMPT..Duration::from_secs(15)).contains(&run.elapsed),
        "rqmd took {:?} to exit",
        run.elapsed
    );
}
