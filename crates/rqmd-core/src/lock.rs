//! Advisory, self-healing exclusive lock for index-mutating commands (`embed`, `update`).
//!
//! Nothing else in this codebase serializes concurrent writers: `Store::open`
//! computes `next_vid` deterministically from on-disk state, so two processes
//! opening the same index at once allocate the same vids and one of them dies
//! with `UNIQUE constraint failed: content_vectors.vid` (or, before it gets
//! that far, a `usearch add: Duplicate keys` panic). See the rqmd plan's
//! "concurrent rqmd embed writers" root-cause writeup for the full trace.
//!
//! The lock is a directory (`mkdir` is atomic on every platform we ship for)
//! holding the owner's `pid`, `host` and a `heartbeat` file. A lock whose
//! owner is provably gone is stale and reclaimed rather than wedging every
//! future `embed`/`update`.
//!
//! Every acquire, reclaim, release and forced removal runs inside a short
//! `flock` critical section on a sibling guard file. The kernel drops that
//! guard when its holder dies, so unlike the lock directory it can never go
//! stale, and it is what makes "check the owner, then remove and recreate"
//! a single winner instead of a race.

use std::cell::Cell;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};

const LOCK_DIR_NAME: &str = ".rqmd-write.lock";
const GUARD_FILE_NAME: &str = ".rqmd-write.lock.guard";
const PID_FILE_NAME: &str = "pid";
const HOST_FILE_NAME: &str = "host";
const HEARTBEAT_FILE_NAME: &str = "heartbeat";

/// A lock directory with no readable pid is mid-creation (or written by an
/// older binary that does not take the guard) for this long before it counts
/// as abandoned.
const UNREADABLE_PID_GRACE: Duration = Duration::from_secs(5);

/// Minimum spacing between heartbeat file writes.
const HEARTBEAT_MIN_INTERVAL: Duration = Duration::from_secs(10);

/// How long a holder may go without a heartbeat before `rqmd unlock --force`
/// is allowed to consider it hung.
pub const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(600);

/// Held for the lifetime of an index-mutating command; released on drop
/// (including on panic-unwind), so a crash cannot leave a *live* lock behind
/// — only a stale one, which the next acquire reclaims automatically.
#[derive(Debug)]
pub struct IndexLock {
    index_dir: PathBuf,
    dir: PathBuf,
    last_beat: Cell<Instant>,
}

/// What is known about the process holding a lock we could not prove dead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub pid: Option<u32>,
    pub host: Option<String>,
    /// The lock was written on a different machine (shared/network index), so
    /// its pid says nothing about liveness here.
    pub foreign_host: bool,
    /// Time since the holder last made progress.
    pub heartbeat_age: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockState {
    Absent,
    /// The holder is provably gone: same host and the pid does not exist, or
    /// the lock never received a readable pid.
    Dead {
        pid: Option<u32>,
    },
    /// Alive, on another host, or too young to judge.
    Held(Holder),
}

impl IndexLock {
    /// Acquire the exclusive write lock for `index_dir`.
    ///
    /// Fails fast with a message naming the holder when another
    /// `embed`/`update` is genuinely running; silently reclaims the lock
    /// when its owner is provably gone.
    pub fn acquire(index_dir: &Path) -> Result<Self> {
        let _guard = Guard::lock(index_dir)?;
        let dir = index_dir.join(LOCK_DIR_NAME);
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if let LockState::Held(holder) = inspect(&dir) {
                    bail!("{}", busy_message(&holder));
                }
                // Owner is gone (or never identified itself) — reclaim.
                fs::remove_dir_all(&dir).ok();
                fs::create_dir(&dir).with_context(|| {
                    format!("re-acquiring stale index lock at {}", dir.display())
                })?;
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("acquiring index lock at {}", dir.display()));
            }
        }
        write_metadata(&dir)?;
        Ok(Self {
            index_dir: index_dir.to_path_buf(),
            dir,
            last_beat: Cell::new(Instant::now()),
        })
    }

    /// Record progress so `rqmd unlock` can tell a slow holder from a hung
    /// one. Cheap to call per document: writes are throttled.
    pub fn heartbeat(&self) {
        if self.last_beat.get().elapsed() >= HEARTBEAT_MIN_INTERVAL {
            self.beat();
        }
    }

    fn beat(&self) {
        let _ = write_heartbeat(&self.dir);
        self.last_beat.set(Instant::now());
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        // Take the guard so a concurrent acquirer never observes a
        // half-removed directory and reclaims it while we are still deleting.
        let _guard = Guard::lock(&self.index_dir).ok();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Inspect the lock for `index_dir` without modifying it.
pub fn lock_state(index_dir: &Path) -> LockState {
    inspect(&index_dir.join(LOCK_DIR_NAME))
}

/// Remove the lock for `index_dir` regardless of its owner. The caller is
/// responsible for having decided that is safe (see `rqmd unlock`).
pub fn remove_lock(index_dir: &Path) -> Result<()> {
    let _guard = Guard::lock(index_dir)?;
    match fs::remove_dir_all(index_dir.join(LOCK_DIR_NAME)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("removing index lock"),
    }
}

fn inspect(lock_dir: &Path) -> LockState {
    if !lock_dir.is_dir() {
        return LockState::Absent;
    }
    let host = fs::read_to_string(lock_dir.join(HOST_FILE_NAME))
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty());
    let foreign_host = match (&host, local_hostname()) {
        (Some(theirs), Some(ours)) => *theirs != ours,
        _ => false,
    };
    let heartbeat_age = heartbeat_age(lock_dir);
    let pid = read_pid(lock_dir);

    let holder = || Holder {
        pid,
        host: host.clone(),
        foreign_host,
        heartbeat_age,
    };
    match pid {
        None if modified_age(lock_dir) < UNREADABLE_PID_GRACE => LockState::Held(holder()),
        None => LockState::Dead { pid: None },
        Some(_) if foreign_host => LockState::Held(holder()),
        Some(p) if pid_is_alive(p) => LockState::Held(holder()),
        Some(p) => LockState::Dead { pid: Some(p) },
    }
}

fn busy_message(h: &Holder) -> String {
    let who = match (h.pid, &h.host) {
        (Some(pid), Some(host)) if h.foreign_host => format!("pid {pid} on host {host}"),
        (Some(pid), _) => format!("pid {pid}"),
        (None, _) => "a process that has not identified itself yet".to_string(),
    };
    format!(
        "another rqmd embed/update ({who}) is already writing to this index — wait for it \
         to finish. If it is confirmed dead or hung, run `rqmd unlock`."
    )
}

fn write_metadata(dir: &Path) -> Result<()> {
    // Rename so a reader never sees a half-written pid.
    let tmp = dir.join(format!("{PID_FILE_NAME}.tmp"));
    let mut f = fs::File::create(&tmp)
        .with_context(|| format!("writing pid file under {}", dir.display()))?;
    write!(f, "{}", std::process::id())?;
    drop(f);
    fs::rename(&tmp, dir.join(PID_FILE_NAME))
        .with_context(|| format!("publishing pid file under {}", dir.display()))?;
    if let Some(host) = local_hostname() {
        fs::write(dir.join(HOST_FILE_NAME), host)?;
    }
    write_heartbeat(dir)?;
    Ok(())
}

fn write_heartbeat(dir: &Path) -> Result<()> {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    fs::write(dir.join(HEARTBEAT_FILE_NAME), secs.to_string())?;
    Ok(())
}

/// Parse the owner pid, rejecting values `kill(2)` would interpret as a
/// process group (`0`) or that overflow `pid_t` — a corrupted file must read
/// as "unidentified", not as an alive process that wedges the lock forever.
fn read_pid(dir: &Path) -> Option<u32> {
    let pid: u32 = fs::read_to_string(dir.join(PID_FILE_NAME))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (pid != 0 && pid <= i32::MAX as u32).then_some(pid)
}

fn modified_age(path: &Path) -> Duration {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .unwrap_or(Duration::MAX)
}

/// Heartbeat age, falling back to the lock directory's own mtime for locks
/// written by a binary that predates the heartbeat file.
fn heartbeat_age(dir: &Path) -> Duration {
    let beat = dir.join(HEARTBEAT_FILE_NAME);
    if beat.exists() {
        modified_age(&beat)
    } else {
        modified_age(dir)
    }
}

/// Exclusive `flock` on the guard file, released when dropped.
struct Guard {
    _file: fs::File,
}

impl Guard {
    fn lock(index_dir: &Path) -> Result<Self> {
        let path = index_dir.join(GUARD_FILE_NAME);
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening index lock guard at {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            loop {
                // SAFETY: `flock` on a descriptor we own for the lifetime of the call.
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                    break;
                }
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::Interrupted {
                    return Err(err).context("locking index lock guard");
                }
            }
        }
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
fn local_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is valid for `buf.len()` bytes for the duration of the call.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

#[cfg(not(unix))]
fn local_hostname() -> Option<String> {
    std::env::var("COMPUTERNAME").ok().filter(|h| !h.is_empty())
}

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    // Signal 0 only checks that the process exists and could be signalled.
    // SAFETY: no signal is delivered.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    // EPERM means it exists but belongs to another user; only ESRCH proves it
    // is gone. Any other errno is "can't tell" — never steal a live lock.
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: u32) -> bool {
    // No cheap liveness check on this platform — assume alive so a lock is
    // never silently stolen from a process that's still running.
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock_dir(tmp: &tempfile::TempDir) -> PathBuf {
        tmp.path().join(LOCK_DIR_NAME)
    }

    fn plant_lock(tmp: &tempfile::TempDir, pid: &str) -> PathBuf {
        let dir = lock_dir(tmp);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join(PID_FILE_NAME), pid).unwrap();
        dir
    }

    fn age_file(path: &Path, by: Duration) {
        let f = fs::File::open(path).unwrap();
        f.set_modified(SystemTime::now() - by).unwrap();
    }

    #[test]
    fn acquire_and_release_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = IndexLock::acquire(tmp.path()).unwrap();
        assert!(lock_dir(&tmp).is_dir());
        drop(lock);
        assert!(!lock_dir(&tmp).exists());
    }

    #[test]
    fn second_acquire_fails_while_first_is_held() {
        let tmp = tempfile::tempdir().unwrap();
        let _first = IndexLock::acquire(tmp.path()).unwrap();
        let err = IndexLock::acquire(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("already writing"), "{err}");
        assert!(err.to_string().contains("rqmd unlock"), "{err}");
    }

    #[test]
    fn stale_lock_from_dead_pid_is_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        // A very high, essentially-impossible PID: valid for `pid_t` but absent.
        plant_lock(&tmp, "999999999");
        let lock = IndexLock::acquire(tmp.path()).unwrap();
        assert_eq!(read_pid(&lock_dir(&tmp)), Some(std::process::id()));
        drop(lock);
    }

    #[cfg(unix)]
    #[test]
    fn pid_owned_by_another_user_counts_as_alive() {
        // PID 1 exists everywhere; as a non-root user `kill(1, 0)` is EPERM,
        // as root it succeeds. Both must read as alive.
        assert!(pid_is_alive(1));
    }

    #[cfg(unix)]
    #[test]
    fn lock_held_by_pid_we_cannot_signal_is_not_stolen() {
        let tmp = tempfile::tempdir().unwrap();
        plant_lock(&tmp, "1");
        let err = IndexLock::acquire(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("pid 1"), "{err}");
        assert_eq!(
            fs::read_to_string(lock_dir(&tmp).join(PID_FILE_NAME)).unwrap(),
            "1"
        );
    }

    #[test]
    fn garbage_pids_are_rejected_not_treated_as_alive() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in ["0", "4294967295", "-5", "abc", ""] {
            let dir = lock_dir(&tmp);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(PID_FILE_NAME), bad).unwrap();
            assert_eq!(read_pid(&dir), None, "{bad:?}");
        }
    }

    #[test]
    fn fresh_lock_without_pid_is_busy_old_one_is_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = lock_dir(&tmp);
        fs::create_dir(&dir).unwrap();
        let err = IndexLock::acquire(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("already writing"), "{err}");

        age_file(&dir, UNREADABLE_PID_GRACE + Duration::from_secs(60));
        let lock = IndexLock::acquire(tmp.path()).unwrap();
        drop(lock);
    }

    #[test]
    fn lock_from_another_host_is_not_auto_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = plant_lock(&tmp, "999999999");
        fs::write(dir.join(HOST_FILE_NAME), "some-other-machine.invalid").unwrap();
        let err = IndexLock::acquire(tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("some-other-machine.invalid"), "{msg}");
        match lock_state(tmp.path()) {
            LockState::Held(h) => assert!(h.foreign_host),
            other => panic!("expected Held, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn acquire_waits_for_the_guard_before_touching_a_stale_lock() {
        use std::sync::mpsc;
        let tmp = tempfile::tempdir().unwrap();
        plant_lock(&tmp, "999999999");
        let guard = Guard::lock(tmp.path()).unwrap();

        let path = tmp.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let r = IndexLock::acquire(&path);
            tx.send(()).unwrap();
            r
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "acquire must block while another process holds the guard"
        );
        drop(guard);
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn release_waits_for_the_guard_before_removing_the_lock() {
        use std::sync::mpsc;
        let tmp = tempfile::tempdir().unwrap();
        let lock = IndexLock::acquire(tmp.path()).unwrap();
        let guard = Guard::lock(tmp.path()).unwrap();

        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            drop(lock);
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        assert!(
            lock_dir(&tmp).is_dir(),
            "lock removed without holding the guard"
        );
        drop(guard);
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        assert!(!lock_dir(&tmp).exists());
    }

    #[test]
    fn heartbeat_is_throttled_and_refreshes_age() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = IndexLock::acquire(tmp.path()).unwrap();
        let beat = lock_dir(&tmp).join(HEARTBEAT_FILE_NAME);

        age_file(&beat, Duration::from_secs(300));
        lock.heartbeat(); // within the throttle window: no write
        assert!(modified_age(&beat) >= Duration::from_secs(299));

        lock.beat();
        assert!(modified_age(&beat) < Duration::from_secs(5));
    }

    #[test]
    fn lock_state_reports_each_case() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(lock_state(tmp.path()), LockState::Absent);

        plant_lock(&tmp, "999999999");
        assert_eq!(
            lock_state(tmp.path()),
            LockState::Dead {
                pid: Some(999_999_999)
            }
        );
        remove_lock(tmp.path()).unwrap();
        assert_eq!(lock_state(tmp.path()), LockState::Absent);

        let _held = IndexLock::acquire(tmp.path()).unwrap();
        match lock_state(tmp.path()) {
            LockState::Held(h) => {
                assert_eq!(h.pid, Some(std::process::id()));
                assert!(!h.foreign_host);
            }
            other => panic!("expected Held, got {other:?}"),
        }
    }

    #[test]
    fn legacy_lock_without_heartbeat_falls_back_to_dir_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = plant_lock(&tmp, "1");
        age_file(&dir, Duration::from_secs(3600));
        match lock_state(tmp.path()) {
            LockState::Held(h) => assert!(h.heartbeat_age >= Duration::from_secs(3599)),
            other => panic!("expected Held, got {other:?}"),
        }
    }
}
