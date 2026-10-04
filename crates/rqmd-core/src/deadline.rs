//! Cooperative wall-clock deadlines for long-running operations.
//!
//! A deadline is only observed where the operation calls [`Deadline::check`],
//! between stages. A single in-flight model call (llama.cpp inference) cannot
//! be interrupted, so the overrun is bounded by the longest stage, not zero.

use std::fmt;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    limit: Option<(Instant, Duration)>,
}

impl Deadline {
    pub fn none() -> Self {
        Self { limit: None }
    }

    pub fn after(limit: Duration) -> Self {
        Self {
            limit: Some((Instant::now() + limit, limit)),
        }
    }

    /// `Ok` while time remains; otherwise [`TimedOut`] naming `stage`, the
    /// work that was about to start.
    pub fn check(&self, stage: &'static str) -> Result<(), TimedOut> {
        match self.limit {
            Some((at, limit)) if Instant::now() >= at => Err(TimedOut { stage, limit }),
            _ => Ok(()),
        }
    }
}

/// The deadline passed before `stage` could start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedOut {
    pub stage: &'static str,
    pub limit: Duration,
}

impl fmt::Display for TimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.limit < Duration::from_secs(1) {
            write!(
                f,
                "timed out after {}ms before {}",
                self.limit.as_millis(),
                self.stage
            )
        } else {
            write!(
                f,
                "timed out after {}s before {}",
                self.limit.as_secs(),
                self.stage
            )
        }
    }
}

impl std::error::Error for TimedOut {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_deadline_never_expires() {
        assert!(Deadline::none().check("anything").is_ok());
    }

    #[test]
    fn expired_deadline_reports_the_stage_and_limit() {
        let d = Deadline::after(Duration::ZERO);
        let err = d.check("rerank").unwrap_err();
        assert_eq!(err.stage, "rerank");
        assert_eq!(err.limit, Duration::ZERO);
        assert!(err.to_string().contains("before rerank"));
    }

    #[test]
    fn unexpired_deadline_passes() {
        assert!(Deadline::after(Duration::from_secs(60)).check("x").is_ok());
    }
}
