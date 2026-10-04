//! Process exit codes for runs that finish but did not fully succeed, or that
//! were stopped by a deadline.

use std::fmt;
use std::process::ExitCode;

use rqmd_core::TimedOut;

/// Exit code for a run that completed but skipped or failed part of its work
/// (an update hook that failed, a document that could not be embedded).
pub const EXIT_PARTIAL: u8 = 2;

/// Exit code for a run stopped by its `--timeout` deadline (the `timeout(1)` convention).
pub const EXIT_TIMEOUT: u8 = 124;

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
/// [`TimedOut`], 1 for any other error.
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
        assert_eq!(report(Err(anyhow::anyhow!("boom"))), ExitCode::FAILURE);
    }
}
