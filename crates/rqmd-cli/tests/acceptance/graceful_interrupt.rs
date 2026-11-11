//! `rqmd update` stops at the next file when it receives SIGINT or SIGTERM:
//! the files already indexed are saved, a resume hint is printed, and the exit
//! code is `128 + n`. A second signal ends the process at once. The message
//! names the process that sent the signal when the kernel reports one.

#![cfg(unix)]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tempfile::TempDir;

use super::helpers::{fixture_collection, rqmd};

/// Enough files that an update is still running when it is frozen right after
/// its first document lands.
const FILES: usize = 3000;

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    done()
}

fn indexed_documents(index_dir: &Path) -> Option<i64> {
    let db = index_dir.join("index.sqlite");
    if !db.exists() {
        return None;
    }
    let conn = Connection::open(db).ok()?;
    conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .ok()
}

fn send(pid: i32, signal: i32) {
    // SAFETY: plain syscall on the rqmd child this test spawned.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
}

/// Kills the child if a test fails while it is still running or stopped.
struct Reap(Child);

impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Interrupted {
    code: Option<i32>,
    stderr: String,
    /// Documents in the index when the update was frozen (the first note included).
    frozen_at: i64,
}

struct Setup {
    index_dir: TempDir,
    _collection: super::helpers::Fixture,
}

/// A collection added with one note, then `FILES` more notes on disk for the
/// update to index.
fn setup() -> Setup {
    let index_dir = TempDir::new().expect("tempdir");
    let fx = fixture_collection("c", "first body\n");
    rqmd(index_dir.path())
        .args(["collection", "add"])
        .arg(&fx.path)
        .args(["--name", "c"])
        .assert()
        .success();
    for i in 0..FILES {
        std::fs::write(
            fx.path.join(format!("note{i}.md")),
            format!("token{i} body\n"),
        )
        .expect("write note");
    }
    Setup {
        index_dir,
        _collection: fx,
    }
}

/// Start `rqmd update` with `stdin`, wait until its first document is indexed,
/// run `deliver` (given the rqmd pid), then report how it ended. With `freeze`
/// the update is stopped around `deliver`, so every signal is pending when it
/// resumes; macOS drops the sender of a signal that waits for a stopped
/// process, so a test that reads the sender must not freeze. With `tty` the
/// child gets its own session and takes `stdin` as its controlling terminal, so
/// a byte written to the other end of that pty reaches it as a
/// terminal-generated signal.
fn run_update(
    setup: &Setup,
    stdin: Stdio,
    tty: bool,
    freeze: bool,
    deliver: impl FnOnce(i32),
) -> Interrupted {
    let program = assert_cmd::Command::cargo_bin("rqmd").expect("rqmd binary");
    let stderr_path = setup.index_dir.path().join("update.stderr");
    let mut command = Command::new(program.get_program());
    command
        .arg("--index-dir")
        .arg(setup.index_dir.path())
        .arg("update")
        .stdin(stdin)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr_path).expect("stderr file"));
    // A test run started as a background job inherits SIGINT as ignored, and
    // rqmd leaves an inherited ignore alone.
    // SAFETY: only `signal`, `setsid` and `ioctl`, which are async-signal-safe,
    // run between fork and exec.
    unsafe {
        command.pre_exec(move || {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            if tty {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
            }
            Ok(())
        });
    }
    let child = command.spawn().expect("spawn rqmd");
    let pid = i32::try_from(child.id()).expect("pid");
    let mut child = Reap(child);

    assert!(
        wait_until(Duration::from_secs(60), || indexed_documents(
            setup.index_dir.path()
        )
        .is_some_and(|n| n > 1)),
        "update never indexed a new document"
    );
    if freeze {
        send(pid, libc::SIGSTOP);
    }
    assert!(
        child.0.try_wait().expect("try_wait").is_none(),
        "update finished before it could be signalled; raise FILES"
    );
    let frozen_at = indexed_documents(setup.index_dir.path()).expect("documents");
    deliver(pid);
    if freeze {
        send(pid, libc::SIGCONT);
    }

    let mut code = None;
    let exited = wait_until(Duration::from_secs(60), || {
        match child.0.try_wait().expect("try_wait") {
            Some(status) => {
                code = status.code();
                true
            }
            None => false,
        }
    });
    assert!(exited, "rqmd did not exit after the signal");
    Interrupted {
        code,
        stderr: std::fs::read_to_string(&stderr_path).unwrap_or_default(),
        frozen_at,
    }
}

/// Deliver `signals` from this test process while the update is stopped.
fn signal_update_midway(setup: &Setup, signals: &[i32]) -> Interrupted {
    run_update(setup, Stdio::null(), false, true, |pid| {
        for signal in signals {
            send(pid, *signal);
        }
    })
}

const RESUME_HINT: &str = "rerun `rqmd update` to continue";

fn assert_stopped_early(setup: &Setup, run: &Interrupted, expected_code: i32) {
    assert_eq!(run.code, Some(expected_code), "stderr: {}", run.stderr);
    assert!(
        run.stderr.contains(RESUME_HINT),
        "no resume hint in stderr: {:?}",
        run.stderr
    );
    let indexed = indexed_documents(setup.index_dir.path()).expect("documents");
    assert!(
        run.frozen_at <= indexed && indexed < i64::try_from(FILES).unwrap(),
        "expected a partial index, got {indexed} of {FILES} (frozen at {})",
        run.frozen_at
    );
}

#[test]
fn ac_1_sigterm_stops_update_at_the_next_file_and_exits_143() {
    let setup = setup();
    let run = signal_update_midway(&setup, &[libc::SIGTERM]);
    assert_stopped_early(&setup, &run, 143);
}

#[test]
fn ac_2_sigint_stops_update_at_the_next_file_and_exits_130() {
    let setup = setup();
    let run = signal_update_midway(&setup, &[libc::SIGINT]);
    assert_stopped_early(&setup, &run, 130);
}

/// What was indexed before the signal is searchable, and a rerun picks up the
/// rest instead of starting over.
#[test]
fn ac_3_files_indexed_before_the_signal_are_saved_and_a_rerun_finishes_the_rest() {
    let setup = setup();
    let run = signal_update_midway(&setup, &[libc::SIGTERM]);
    assert_stopped_early(&setup, &run, 143);

    // A note indexed before the signal is found by search.
    let saved: String = {
        let conn = Connection::open(setup.index_dir.path().join("index.sqlite")).expect("open");
        conn.query_row(
            "SELECT path FROM documents WHERE path LIKE 'note%' ORDER BY id LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("an indexed note")
    };
    let token = format!(
        "token{}",
        saved.trim_start_matches("note").trim_end_matches(".md")
    );
    let found = rqmd(setup.index_dir.path())
        .args(["search", &token])
        .assert()
        .success();
    assert!(
        String::from_utf8_lossy(&found.get_output().stdout).contains(&saved),
        "{saved} was indexed before the signal but is not searchable"
    );

    let rerun = rqmd(setup.index_dir.path())
        .arg("update")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&rerun.get_output().stdout).into_owned();
    let before_word = |word: &str| -> usize {
        let head = stdout.split(word).next().expect("summary");
        head.rsplit(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no count before {word:?} in {stdout:?}"))
    };
    let (new, unchanged) = (before_word(" new"), before_word(" unchanged"));
    assert!(unchanged > 0, "the rerun re-read everything: {stdout}");
    assert_eq!(new + unchanged, FILES + 1, "{stdout}");
    assert_eq!(
        indexed_documents(setup.index_dir.path()),
        Some(i64::try_from(FILES).unwrap() + 1)
    );
}

/// A second signal does not wait for the file in hand or print the hint.
#[test]
fn ac_4_a_second_signal_exits_at_once_without_the_resume_hint() {
    let setup = setup();
    // Both are pending when the process resumes, so the second lands before any
    // work does.
    let run = signal_update_midway(&setup, &[libc::SIGINT, libc::SIGTERM]);
    assert!(
        matches!(run.code, Some(130 | 143)),
        "code {:?}, stderr: {}",
        run.code,
        run.stderr
    );
    assert!(
        !run.stderr.contains(RESUME_HINT),
        "the run wound down instead of exiting: {:?}",
        run.stderr
    );
}

/// The `rqmd: interrupted by ...` line of a run's stderr.
fn interrupted_line(run: &Interrupted) -> &str {
    run.stderr
        .lines()
        .find(|l| l.starts_with("rqmd: interrupted by"))
        .unwrap_or_else(|| panic!("no interrupted line in stderr: {:?}", run.stderr))
}

/// A `sh` child that SIGTERMs `target` with `kill`, writes `marker`, then waits
/// on its stdin so it stays alive, as `sh`, until the guard drops.
fn spawn_sender(target: i32, marker: &Path) -> Reap {
    let child = Command::new("sh")
        .args(["-c", "kill -TERM \"$1\"; : > \"$2\"; read _", "sh"])
        .arg(target.to_string())
        .arg(marker)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn sender");
    Reap(child)
}

/// The message names the process that sent the signal, here this test.
#[test]
fn ac_5_the_interrupt_message_names_the_sending_pid() {
    let setup = setup();
    let run = run_update(&setup, Stdio::null(), false, false, |pid| {
        send(pid, libc::SIGTERM);
    });
    assert_eq!(run.code, Some(143), "stderr: {}", run.stderr);
    // The name is this test binary's (`acceptance-<hash>`); Linux keeps 15 bytes
    // of it, macOS more, so only its start is compared.
    let from = format!(
        "rqmd: interrupted by SIGTERM from pid {} (acceptance-",
        std::process::id()
    );
    assert!(
        interrupted_line(&run).starts_with(&from),
        "{:?} does not start with {from:?}",
        interrupted_line(&run)
    );
}

/// A sender that is a different process, still alive when rqmd reads its name,
/// is named by pid and command.
#[test]
fn ac_6_a_live_sender_is_named_by_pid_and_command() {
    let setup = setup();
    let marker = setup.index_dir.path().join("sender.done");
    let mut sender = None;
    let run = run_update(&setup, Stdio::null(), false, false, |pid| {
        sender = Some(spawn_sender(pid, &marker));
    });
    let sender = sender.expect("sender ran").0.id();
    assert_eq!(run.code, Some(143), "stderr: {}", run.stderr);
    // `sh` is bash on macOS, so the reported command is the shell's own name.
    let line = interrupted_line(&run);
    let named = ["sh", "bash", "dash", "busybox"]
        .iter()
        .any(|shell| *line == *format!("rqmd: interrupted by SIGTERM from pid {sender} ({shell})"));
    assert!(named, "sender {sender} not named in {line:?}");
}

/// A terminal key's signal never reports `pid 0`. The Linux kernel raises it
/// itself, with no sending process, so the message is the old one; macOS
/// reports the process that wrote the key to the pty, here this test. Ctrl-C
/// written to a pty reaches the update as a terminal-generated SIGINT.
#[test]
fn ac_7_a_terminal_signal_never_reports_pid_0() {
    let setup = setup();
    let (mut master, mut slave) = (0, 0);
    // SAFETY: the out-pointers are valid; the null name, termios and winsize
    // ask for defaults. (Apple declares the last two `*mut`, glibc `*const`.)
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(opened, 0, "openpty failed");
    // SAFETY: `openpty` returned these two descriptors and nothing else owns them.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let slave = std::fs::File::from(slave);
    // Nothing reads the master, so the echo of `^C` would sit unread in the
    // output queue and the child's exit would wait on it (it hangs on macOS).
    // SAFETY: `termios` is plain C data; `slave` is a valid terminal descriptor.
    unsafe {
        let mut attrs: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(slave.as_raw_fd(), &mut attrs), 0);
        attrs.c_lflag &= !(libc::ECHO | libc::ECHOCTL);
        assert_eq!(libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &attrs), 0);
    }

    let run = run_update(&setup, Stdio::from(slave), true, false, |_| {
        // SAFETY: writes one byte from a valid buffer to the pty master this test owns.
        let written = unsafe { libc::write(master.as_raw_fd(), b"\x03".as_ptr().cast(), 1) };
        assert_eq!(written, 1, "could not type Ctrl-C");
    });
    assert_eq!(run.code, Some(130), "stderr: {}", run.stderr);
    let line = interrupted_line(&run);
    assert!(!line.contains("from pid 0"), "{line:?}");
    if cfg!(target_os = "linux") {
        assert_eq!(line, "rqmd: interrupted by SIGINT");
    } else if cfg!(target_vendor = "apple") {
        let writer = format!("from pid {}", std::process::id());
        assert!(line.contains(&writer), "{line:?} lacks {writer:?}");
    }
}
