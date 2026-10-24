//! Run a collection's `update_command` under a wall-clock timeout, forwarding
//! SIGINT, SIGTERM and SIGHUP to it while it runs.

use std::io::IsTerminal;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const TIMEOUT_ENV: &str = "RQMD_HOOK_TIMEOUT_SECS";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How long a hook gets to exit after a forwarded signal before it is killed.
const FORWARD_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub struct HookConfig {
    pub timeout: Duration,
    /// Run the hook in its own process group so a timeout kills the whole
    /// tree. Off when stdin is a terminal: a background group would lose
    /// Ctrl-C and credential prompts (SIGTTIN).
    pub own_process_group: bool,
}

impl HookConfig {
    pub fn from_env() -> Self {
        Self {
            timeout: parse_timeout(std::env::var(TIMEOUT_ENV).ok().as_deref()),
            own_process_group: !std::io::stdin().is_terminal(),
        }
    }
}

/// A positive whole number of seconds; anything else falls back to the default.
fn parse_timeout(value: Option<&str>) -> Duration {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map_or(DEFAULT_TIMEOUT, Duration::from_secs)
}

#[derive(Debug, PartialEq, Eq)]
pub enum HookOutcome {
    Succeeded,
    Failed(String),
    TimedOut(Duration),
    /// rqmd received this signal while the hook ran and stopped the hook.
    Interrupted(i32),
}

pub fn run_hook(cmd: &str, dir: &Path, config: HookConfig) -> HookOutcome {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(cmd).current_dir(dir);
    #[cfg(unix)]
    if config.own_process_group {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    // Installed before the spawn so a signal that lands right after it is kept.
    let _signals = signals::Forwarding::install();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return HookOutcome::Failed(format!("failed to run: {e}")),
    };

    let deadline = Instant::now() + config.timeout;
    loop {
        if let Some(signal) = signals::pending() {
            signals::forward(&child, signal, config.own_process_group);
            stop_after_grace(&mut child, config.own_process_group);
            return HookOutcome::Interrupted(signal);
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return HookOutcome::Succeeded,
            Ok(Some(status)) => return HookOutcome::Failed(format!("exited with {status}")),
            Ok(None) => {}
            Err(e) => return HookOutcome::Failed(format!("failed to wait: {e}")),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            kill(&mut child, config.own_process_group);
            return HookOutcome::TimedOut(config.timeout);
        }
        std::thread::sleep(remaining.min(POLL_INTERVAL));
    }
}

/// Wait up to [`FORWARD_GRACE`] for the hook to exit on the forwarded signal,
/// then kill it.
fn stop_after_grace(child: &mut Child, whole_group: bool) {
    let deadline = Instant::now() + FORWARD_GRACE;
    while Instant::now() < deadline {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    kill(child, whole_group);
}

fn kill(child: &mut Child, whole_group: bool) {
    #[cfg(unix)]
    if whole_group && let Ok(pgid) = i32::try_from(child.id()) {
        // SAFETY: plain syscall; the child leads its own group (`process_group(0)`),
        // so `-pgid` addresses exactly the hook's tree.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = whole_group;
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
mod signals {
    use std::process::Child;
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::sync::{Mutex, MutexGuard};

    const FORWARDED: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

    /// The last forwarded signal received, or 0. Set by the handler.
    static RECEIVED: AtomicI32 = AtomicI32::new(0);
    /// One hook at a time owns the process-wide handlers.
    static OWNER: Mutex<()> = Mutex::new(());

    extern "C" fn on_signal(signal: libc::c_int) {
        RECEIVED.store(signal, Ordering::SeqCst);
    }

    /// Catches [`FORWARDED`] until dropped, then restores the previous handlers.
    pub struct Forwarding {
        previous: Vec<(libc::c_int, libc::sigaction)>,
        _owner: MutexGuard<'static, ()>,
    }

    impl Forwarding {
        pub fn install() -> Self {
            let owner = OWNER.lock().unwrap_or_else(|e| e.into_inner());
            RECEIVED.store(0, Ordering::SeqCst);
            let mut previous = Vec::new();
            for signal in FORWARDED {
                // SAFETY: `sigaction` is plain C data; all-zero is a valid empty value.
                let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
                // SAFETY: a null new action only reads the current one into `old`.
                if unsafe { libc::sigaction(signal, std::ptr::null(), &mut old) } != 0 {
                    continue;
                }
                // A signal the parent ignored (`nohup`, a background job) stays ignored.
                if old.sa_sigaction == libc::SIG_IGN {
                    continue;
                }
                // SAFETY: as above.
                let mut new: libc::sigaction = unsafe { std::mem::zeroed() };
                new.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
                // SAFETY: `new.sa_mask` is a valid sigset to initialise.
                unsafe { libc::sigemptyset(&mut new.sa_mask) };
                // SAFETY: the handler only stores into an atomic, which is async-signal-safe.
                if unsafe { libc::sigaction(signal, &new, std::ptr::null_mut()) } == 0 {
                    previous.push((signal, old));
                }
            }
            Self {
                previous,
                _owner: owner,
            }
        }
    }

    impl Drop for Forwarding {
        fn drop(&mut self) {
            for (signal, old) in &self.previous {
                // SAFETY: `old` is the action `sigaction` reported for this signal.
                unsafe { libc::sigaction(*signal, old, std::ptr::null_mut()) };
            }
        }
    }

    pub fn pending() -> Option<i32> {
        match RECEIVED.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        }
    }

    /// Send `signal` to the hook: its whole group when it leads one, else just
    /// the hook. A hook in rqmd's own foreground group already got a terminal's
    /// Ctrl-C, so SIGINT is not sent twice.
    pub fn forward(child: &Child, signal: i32, own_group: bool) {
        let Ok(pid) = i32::try_from(child.id()) else {
            return;
        };
        if own_group {
            // SAFETY: the child leads its own group (`process_group(0)`), so
            // `-pid` addresses exactly the hook's tree.
            unsafe { libc::kill(-pid, signal) };
        } else if signal != libc::SIGINT {
            // SAFETY: plain syscall on the hook's pid, still unreaped.
            unsafe { libc::kill(pid, signal) };
        }
    }
}

#[cfg(not(unix))]
mod signals {
    use std::process::Child;

    pub struct Forwarding;

    impl Forwarding {
        pub fn install() -> Self {
            Self
        }
    }

    pub fn pending() -> Option<i32> {
        None
    }

    pub fn forward(_child: &Child, _signal: i32, _own_group: bool) {}
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn config(timeout_ms: u64, own_process_group: bool) -> HookConfig {
        HookConfig {
            timeout: Duration::from_millis(timeout_ms),
            own_process_group,
        }
    }

    fn pid_alive(pid: i32) -> bool {
        // SAFETY: signal 0 only probes for existence.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn successful_hook_reports_success() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            run_hook("exit 0", dir.path(), config(5_000, true)),
            HookOutcome::Succeeded
        );
    }

    #[test]
    fn failing_hook_reports_its_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        match run_hook("exit 3", dir.path(), config(5_000, true)) {
            HookOutcome::Failed(reason) => assert!(reason.contains('3'), "{reason}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn hanging_hook_is_killed_at_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        for own_group in [true, false] {
            let started = Instant::now();
            let outcome = run_hook("sleep 20", dir.path(), config(300, own_group));
            assert_eq!(outcome, HookOutcome::TimedOut(Duration::from_millis(300)));
            assert!(started.elapsed() < Duration::from_secs(10));
        }
    }

    #[test]
    fn timeout_kills_the_hooks_child_processes_in_a_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild.pid");
        let cmd = format!("sleep 20 & echo $! > {}; wait", pidfile.display());
        let outcome = run_hook(&cmd, dir.path(), config(500, true));
        assert!(matches!(outcome, HookOutcome::TimedOut(_)));

        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while pid_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!pid_alive(pid), "grandchild {pid} survived the timeout");
    }

    #[test]
    fn timeout_must_be_a_positive_whole_number_of_seconds() {
        assert_eq!(parse_timeout(Some("30")), Duration::from_secs(30));
        assert_eq!(parse_timeout(Some(" 7 ")), Duration::from_secs(7));
        assert_eq!(parse_timeout(Some("0")), DEFAULT_TIMEOUT);
        assert_eq!(parse_timeout(Some("abc")), DEFAULT_TIMEOUT);
        assert_eq!(parse_timeout(None), DEFAULT_TIMEOUT);
    }
}
