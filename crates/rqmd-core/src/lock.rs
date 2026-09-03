//! Advisory, self-healing exclusive lock for index-mutating commands (`embed`, `update`).
//!
//! Nothing else in this codebase serializes concurrent writers: `Store::open`
//! computes `next_vid` deterministically from on-disk state, so two processes
//! opening the same index at once allocate the same vids and one of them dies
//! with `UNIQUE constraint failed: content_vectors.vid` (or, before it gets
//! that far, a `usearch add: Duplicate keys` panic). See the rqmd plan's
//! "concurrent rqmd embed writers" root-cause writeup for the full trace.
//!
//! This lock closes that race for `rqmd`'s own binary. It mirrors the
//! `~/.claude/hooks/rqmd-reindex.sh` hook's own `mkdir`-based lock: a
//! directory create is atomic on every platform we ship for, and a lock
//! whose owning PID is no longer alive is stale and safely reclaimed rather
//! than wedging every future `embed`/`update` forever.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const LOCK_DIR_NAME: &str = ".rqmd-write.lock";
const PID_FILE_NAME: &str = "pid";

/// Held for the lifetime of an index-mutating command; released on drop
/// (including on panic-unwind), so a crash cannot leave a *live* lock behind
/// — only a stale one, which the next acquire reclaims automatically.
#[derive(Debug)]
pub struct IndexLock {
    dir: PathBuf,
}

impl IndexLock {
    /// Acquire the exclusive write lock for `index_dir`.
    ///
    /// Fails fast with a message naming the holding PID when another
    /// `embed`/`update` is genuinely running; silently reclaims the lock
    /// when the named PID is no longer alive.
    pub fn acquire(index_dir: &Path) -> Result<Self> {
        let dir = index_dir.join(LOCK_DIR_NAME);
        match fs::create_dir(&dir) {
            Ok(()) => {
                write_pid(&dir)?;
                Ok(Self { dir })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Some(pid) = read_pid(&dir)
                    && pid_is_alive(pid)
                {
                    bail!(
                        "another rqmd embed/update (pid {pid}) is already writing to this index \
                         — wait for it to finish. If it is confirmed dead, remove {}",
                        dir.display()
                    );
                }
                // Owning PID is gone (or unreadable) — the lock is stale. Reclaim it.
                fs::remove_dir_all(&dir).ok();
                fs::create_dir(&dir).with_context(|| {
                    format!("re-acquiring stale index lock at {}", dir.display())
                })?;
                write_pid(&dir)?;
                Ok(Self { dir })
            }
            Err(e) => Err(e).with_context(|| format!("acquiring index lock at {}", dir.display())),
        }
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn write_pid(dir: &Path) -> Result<()> {
    let mut f = fs::File::create(dir.join(PID_FILE_NAME))
        .with_context(|| format!("writing pid file under {}", dir.display()))?;
    write!(f, "{}", std::process::id())?;
    Ok(())
}

fn read_pid(dir: &Path) -> Option<u32> {
    fs::read_to_string(dir.join(PID_FILE_NAME))
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    // `kill -0` sends no signal — it only checks that the process exists and
    // is signalable by us. Shelling out avoids pulling in a libc dependency
    // for one syscall.
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(true) // can't tell — assume alive, never steal a live lock
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

    #[test]
    fn acquire_and_release_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = IndexLock::acquire(tmp.path()).unwrap();
        assert!(tmp.path().join(LOCK_DIR_NAME).is_dir());
        drop(lock);
        assert!(!tmp.path().join(LOCK_DIR_NAME).exists());
    }

    #[test]
    fn second_acquire_fails_while_first_is_held() {
        let tmp = tempfile::tempdir().unwrap();
        let _first = IndexLock::acquire(tmp.path()).unwrap();
        let err = IndexLock::acquire(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("already writing"), "{err}");
    }

    #[test]
    fn stale_lock_from_dead_pid_is_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(LOCK_DIR_NAME);
        fs::create_dir(&dir).unwrap();
        // PID 1 belongs to init/launchd on every Unix we run on and will
        // never be *this* process, but for staleness we need a PID that is
        // guaranteed not to exist. Use a very high, essentially-impossible
        // PID instead of assuming anything about PID 1's liveness.
        fs::write(dir.join(PID_FILE_NAME), "999999999").unwrap();
        let lock = IndexLock::acquire(tmp.path()).unwrap();
        assert_eq!(
            read_pid(&tmp.path().join(LOCK_DIR_NAME)),
            Some(std::process::id())
        );
        drop(lock);
    }
}
