//! Process exit codes for runs that finish but did not fully succeed, or that
//! were stopped by a deadline.

use std::fmt;
use std::process::ExitCode;

use rqmd_core::TimedOut;

use crate::signals;
use crate::store::NoIndex;

/// Exit code for a run that completed but skipped or failed part of its work
/// (an update hook that failed, a document that could not be embedded).
pub const EXIT_PARTIAL: u8 = 2;

/// Exit code for a run stopped by its `--timeout` deadline (the `timeout(1)` convention).
pub const EXIT_TIMEOUT: u8 = 124;

/// Added to a signal number for the exit code of a run it stopped (`128 + n`,
/// as a shell reports it: 130 for SIGINT, 143 for SIGTERM, 129 for SIGHUP).
pub const EXIT_SIGNAL_BASE: u8 = 128;

/// The run was stopped by a signal it caught.
#[derive(Debug)]
pub struct Interrupted {
    pub signal: i32,
    /// The pid that sent the signal, when the kernel reported one.
    pub sender: Option<i32>,
}

impl Interrupted {
    /// The run was stopped by `signal`, which [`signals::sender`] may attribute.
    pub fn caught(signal: i32) -> Self {
        Self {
            signal,
            sender: signals::sender()
                .filter(|&pid| !(signal == libc::SIGINT && sent_by_tty_driver(pid))),
        }
    }
}

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Numbering is the same on Linux and macOS.
        match self.signal {
            1 => f.write_str("interrupted by SIGHUP")?,
            2 => f.write_str("interrupted by SIGINT")?,
            15 => f.write_str("interrupted by SIGTERM")?,
            n => write!(f, "interrupted by signal {n}")?,
        }
        if let Some(pid) = self.sender {
            write!(f, " from pid {pid}")?;
            // The sender may already be gone; the pid alone is still useful.
            if let Some(name) = process_name(pid) {
                write!(f, " ({name})")?;
            }
        }
        Ok(())
    }
}

/// Whether `pid` is the terminal emulator that turned a Ctrl-C into this
/// SIGINT. macOS reports the process that wrote the key to the pty as the
/// sender; it owns the pty master and has no controlling terminal itself,
/// while a `kill` run from a shell has one. A sender that cannot be looked up
/// (it already exited) is kept, so it is still named by pid.
#[cfg(target_vendor = "apple")]
fn sent_by_tty_driver(pid: i32) -> bool {
    fn tty_device(pid: i32) -> Option<u32> {
        // SAFETY: `info` is plain C data and is valid for the size passed;
        // `proc_pidinfo` writes at most that many bytes and returns the number
        // written, or 0 on failure.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                std::ptr::addr_of_mut!(info).cast(),
                size,
            )
        };
        (written == size).then_some(info.e_tdev)
    }
    let own = i32::try_from(std::process::id()).ok();
    match (tty_device(pid), own.and_then(tty_device)) {
        (Some(sender), Some(ours)) => sender == u32::MAX && ours != u32::MAX,
        _ => false,
    }
}

#[cfg(not(target_vendor = "apple"))]
fn sent_by_tty_driver(_pid: i32) -> bool {
    false
}

/// The command name of `pid`, or `None` when it has exited or cannot be read.
#[cfg(target_os = "linux")]
fn process_name(pid: i32) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    printable_name(comm.trim_end_matches('\n'))
}

#[cfg(target_vendor = "apple")]
fn process_name(pid: i32) -> Option<String> {
    let mut buf = [0u8; 64];
    // SAFETY: `buf` is valid for the 64 bytes passed; `proc_name` writes at most
    // that many and returns the length written, or 0 on failure.
    let len = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), 64) };
    let len = usize::try_from(len).ok().filter(|n| *n > 0)?;
    printable_name(&String::from_utf8_lossy(&buf[..len.min(buf.len())]))
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
fn process_name(_pid: i32) -> Option<String> {
    None
}

/// A process name is another process's data: keep it to one printable line.
#[cfg(any(target_os = "linux", target_vendor = "apple", test))]
fn printable_name(raw: &str) -> Option<String> {
    let name: String = raw.chars().filter(|c| !c.is_control()).collect();
    (!name.is_empty()).then_some(name)
}

impl std::error::Error for Interrupted {}

/// The run finished; some units of work failed and were reported above.
#[derive(Debug)]
pub struct PartialFailure(pub String);

impl fmt::Display for PartialFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PartialFailure {}

/// Report `result` on stderr and map it to a process exit code: 0 on success,
/// [`EXIT_PARTIAL`] for a [`PartialFailure`], [`EXIT_TIMEOUT`] for a
/// [`TimedOut`], `128 + n` for an [`Interrupted`] by signal `n`, 1 for a
/// [`NoIndex`] or any other error.
pub fn report(result: anyhow::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if let Some(partial) = e.downcast_ref::<PartialFailure>() {
                eprintln!("rqmd: {partial}");
                ExitCode::from(EXIT_PARTIAL)
            } else if let Some(timed_out) = e.downcast_ref::<TimedOut>() {
                eprintln!("rqmd: {timed_out}");
                ExitCode::from(EXIT_TIMEOUT)
            } else if let Some(interrupted) = e.downcast_ref::<Interrupted>() {
                eprintln!("rqmd: {interrupted}");
                ExitCode::from(
                    EXIT_SIGNAL_BASE.saturating_add(u8::try_from(interrupted.signal).unwrap_or(0)),
                )
            } else if let Some(no_index) = e.downcast_ref::<NoIndex>() {
                eprintln!("rqmd: {no_index}");
                ExitCode::FAILURE
            } else {
                eprintln!("Error: {e:?}");
                ExitCode::FAILURE
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_distinguish_success_partial_and_error() {
        assert_eq!(report(Ok(())), ExitCode::SUCCESS);
        assert_eq!(
            report(Err(PartialFailure("1 hook failed".into()).into())),
            ExitCode::from(EXIT_PARTIAL)
        );
        let timed_out = TimedOut {
            stage: "rerank",
            limit: std::time::Duration::from_secs(5),
        };
        assert_eq!(
            report(Err(anyhow::Error::new(timed_out).context("running query"))),
            ExitCode::from(EXIT_TIMEOUT)
        );
        assert_eq!(
            report(Err(Interrupted::caught(15).into())),
            ExitCode::from(143)
        );
        let no_index = NoIndex {
            dir: "/nowhere".into(),
        };
        assert_eq!(report(Err(no_index.into())), ExitCode::FAILURE);
        assert_eq!(report(Err(anyhow::anyhow!("boom"))), ExitCode::FAILURE);
    }

    #[test]
    fn a_sender_that_cannot_be_looked_up_is_not_the_tty_driver() {
        // i32::MAX exceeds every kernel's pid limit, so the lookup fails and
        // the sender stays named.
        assert!(!sent_by_tty_driver(i32::MAX));
    }

    #[test]
    fn interrupted_message_has_three_shapes() {
        let none = Interrupted {
            signal: 15,
            sender: None,
        };
        assert_eq!(none.to_string(), "interrupted by SIGTERM");

        // A pid that is never alive: i32::MAX exceeds every kernel's pid limit.
        let gone = Interrupted {
            signal: 2,
            sender: Some(i32::MAX),
        };
        assert_eq!(
            gone.to_string(),
            format!("interrupted by SIGINT from pid {}", i32::MAX)
        );

        // This test process is alive, so it gets named.
        let own = i32::try_from(std::process::id()).unwrap();
        let named = Interrupted {
            signal: 1,
            sender: Some(own),
        }
        .to_string();
        let prefix = format!("interrupted by SIGHUP from pid {own}");
        assert!(named.starts_with(&prefix), "{named}");
        let name = named[prefix.len()..].trim();
        if cfg!(any(target_os = "linux", target_vendor = "apple")) {
            assert!(
                name.starts_with('(') && name.ends_with(')') && name.len() > 2,
                "no name in {named:?}"
            );
        } else {
            assert!(name.is_empty(), "{named}");
        }
    }

    #[test]
    fn a_process_name_keeps_to_one_printable_line() {
        assert_eq!(printable_name("zsh"), Some("zsh".to_string()));
        assert_eq!(printable_name("a\x1b[2Jb\nc"), Some("a[2Jbc".to_string()));
        assert_eq!(printable_name("\n"), None);
    }
}
