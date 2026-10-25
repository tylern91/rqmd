//! The `refresh` option of the `search` and `query` tools: bring the full-text
//! index up to date before searching.
//!
//! The pass itself walks collection directories and parses files, which lives
//! in the CLI crate (this crate cannot depend on it), so the CLI hands the
//! server a [`Refresher`]. A server built without one answers `refresh: true`
//! with a note instead of failing the search.

use std::path::Path;
use std::sync::Arc;

use rqmd_core::Deadline;

/// How one refresh pass ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// Every requested collection was walked and the index is current.
    Refreshed,
    /// Another process holds the index lock; nothing was indexed.
    Locked,
    /// The full-text index waits for a rebuild, which only `rqmd update` does;
    /// nothing was indexed.
    RebuildPending,
    /// The deadline passed mid-pass. What was indexed is saved; nothing was pruned.
    TimedOut,
}

impl RefreshOutcome {
    /// The one-line note for a pass that left the results possibly behind the
    /// files, `None` for one that completed. Never carries paths or error detail.
    pub fn notice(self) -> Option<&'static str> {
        match self {
            Self::Refreshed => None,
            Self::Locked => Some(
                "refresh skipped because another process is updating the index; results may not include recent edits.",
            ),
            Self::RebuildPending => Some(
                "refresh skipped because the full-text index needs a rebuild (run `rqmd update`); results may not include recent edits.",
            ),
            Self::TimedOut => Some(
                "refresh stopped at the tool timeout; results may not include all recent edits.",
            ),
        }
    }
}

/// Note for a pass that failed outright.
pub(crate) const FAILED_NOTICE: &str = "refresh failed; results may not include recent edits.";

/// Note for a server that has no refresher.
pub(crate) const UNAVAILABLE_NOTICE: &str =
    "refresh is not available in this server; results may not include recent edits.";

/// Runs one refresh pass over `index_dir` for the named collections (all when
/// `None`; names that match no collection are skipped), stopping when the
/// deadline passes. It indexes for keyword search only, runs no update hook and
/// leaves vectors to `rqmd embed`.
pub type Refresher =
    Arc<dyn Fn(&Path, Option<&[String]>, Deadline) -> anyhow::Result<RefreshOutcome> + Send + Sync>;
