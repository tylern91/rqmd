//! Process exit codes for runs that finish but did not fully succeed.

use std::fmt;
use std::process::ExitCode;

/// Exit code for a run that completed but skipped or failed part of its work
/// (an update hook that failed, a document that could not be embedded).
pub const EXIT_PARTIAL: u8 = 2;

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
/// [`EXIT_PARTIAL`] for a [`PartialFailure`], 1 for any other error.
pub fn report(result: anyhow::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => match e.downcast_ref::<PartialFailure>() {
            Some(partial) => {
                eprintln!("rqmd: {partial}");
                ExitCode::from(EXIT_PARTIAL)
            }
            None => {
                eprintln!("Error: {e:?}");
                ExitCode::FAILURE
            }
        },
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
        assert_eq!(report(Err(anyhow::anyhow!("boom"))), ExitCode::FAILURE);
    }
}
