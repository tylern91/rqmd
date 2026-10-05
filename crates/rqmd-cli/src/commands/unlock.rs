use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use rqmd_core::{DEFAULT_STALE_AFTER, Holder, LockState, lock_state, remove_lock};

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    NothingToDo,
    Remove,
    RemoveAfterConfirm,
    Refuse(String),
}

fn decide(state: &LockState, stale_after: Duration, force: bool) -> Decision {
    match state {
        LockState::Absent => Decision::NothingToDo,
        LockState::Dead { .. } => Decision::Remove,
        LockState::Held(h) if h.heartbeat_age < stale_after => Decision::Refuse(format!(
            "{} is alive and made progress {}s ago; refusing to remove its lock",
            describe(h),
            h.heartbeat_age.as_secs()
        )),
        LockState::Held(h) if !force => Decision::Refuse(format!(
            "{} has made no progress for {}s but may only be slow; pass --force to remove it anyway",
            describe(h),
            h.heartbeat_age.as_secs().min(u32::MAX as u64)
        )),
        LockState::Held(_) => Decision::RemoveAfterConfirm,
    }
}

fn describe(h: &Holder) -> String {
    match (h.pid, &h.host) {
        (Some(pid), Some(host)) if h.foreign_host => format!("the holder (pid {pid} on {host})"),
        (Some(pid), _) => format!("the holder (pid {pid})"),
        (None, _) => "the lock's owner".to_string(),
    }
}

fn stale_after_from_env() -> Duration {
    std::env::var("RQMD_LOCK_STALE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(DEFAULT_STALE_AFTER, Duration::from_secs)
}

fn confirmed(yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        bail!("refusing to force-remove a lock non-interactively; pass --yes to confirm");
    }
    eprint!(
        "Removing the lock while its holder may still be writing can corrupt the index. Continue? [y/N] "
    );
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().eq_ignore_ascii_case("y"))
}

pub fn run_unlock(index_dir: &Path, force: bool, yes: bool) -> Result<()> {
    let state = lock_state(index_dir);
    match decide(&state, stale_after_from_env(), force) {
        Decision::NothingToDo => println!("No lock held."),
        Decision::Remove => {
            remove_lock(index_dir)?;
            println!("Removed stale lock.");
        }
        Decision::RemoveAfterConfirm => {
            if !confirmed(yes)? {
                bail!("aborted; lock left in place");
            }
            remove_lock(index_dir)?;
            println!("Removed lock.");
        }
        Decision::Refuse(reason) => bail!("{reason}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const STALE: Duration = Duration::from_secs(600);

    fn held(age_secs: u64) -> LockState {
        LockState::Held(Holder {
            pid: Some(42),
            host: None,
            foreign_host: false,
            heartbeat_age: Duration::from_secs(age_secs),
        })
    }

    #[test]
    fn no_lock_is_a_no_op() {
        assert_eq!(
            decide(&LockState::Absent, STALE, false),
            Decision::NothingToDo
        );
    }

    #[test]
    fn dead_holder_is_removed_without_force() {
        let dead = LockState::Dead { pid: Some(7) };
        assert_eq!(decide(&dead, STALE, false), Decision::Remove);
    }

    #[test]
    fn live_holder_with_recent_progress_is_refused_even_with_force() {
        for force in [false, true] {
            assert!(matches!(
                decide(&held(30), STALE, force),
                Decision::Refuse(_)
            ));
        }
    }

    #[test]
    fn live_holder_without_progress_needs_force_then_confirmation() {
        assert!(matches!(
            decide(&held(3600), STALE, false),
            Decision::Refuse(_)
        ));
        assert_eq!(
            decide(&held(3600), STALE, true),
            Decision::RemoveAfterConfirm
        );
    }

    #[test]
    fn stale_window_is_a_parameter() {
        assert_eq!(
            decide(&held(30), Duration::from_secs(10), true),
            Decision::RemoveAfterConfirm
        );
    }
}
