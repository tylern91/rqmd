//! The pass behind `refresh: true` on the MCP `search` and `query` tools.
//!
//! It is `update_one_collection`, so masks, ignore patterns, hidden files, size
//! limits and symlink containment behave as in `rqmd update`. It indexes for
//! keyword search only: no model, no vectors, no update hook and no full-text
//! rebuild. The lock is held for the pass only, and a pass that cannot run is
//! reported, never an error. Nothing is printed to stdout, which is the
//! protocol stream of a stdio server.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use rqmd_core::{Collection, Deadline, IndexLock, db};
use rqmd_mcp::{RefreshOutcome, Refresher};

use super::index::{UpdateRun, update_one_collection};
use crate::format::ProgressMode;
use crate::hook::HookConfig;
use crate::store;

/// The refresher to give the MCP server.
pub fn refresher() -> Refresher {
    Arc::new(refresh_index)
}

/// Bring the full-text index up to date for `requested` collections (all when
/// `None`; a name that matches no collection is skipped), stopping between
/// files once `deadline` passes.
///
/// The collection walk is not interrupted, only the indexing between files, so
/// a very large collection can overrun the deadline by one walk.
pub fn refresh_index(
    index_dir: &Path,
    requested: Option<&[String]>,
    deadline: Deadline,
) -> Result<RefreshOutcome> {
    let cols: Vec<Collection> = {
        let s = store::open_store_no_backend(index_dir, true)?;
        // Checked before taking the lock, so the `rqmd update` that clears it is
        // never made to wait for a refresh that is only looking.
        if s.fts_rebuild_pending()? {
            return Ok(RefreshOutcome::RebuildPending);
        }
        let all = db::list_collections(&s.db)?;
        match requested {
            Some(names) => all
                .into_iter()
                .filter(|c| names.contains(&c.name))
                .collect(),
            None => all,
        }
    };
    if cols.is_empty() {
        return Ok(RefreshOutcome::Refreshed);
    }
    let lock = match IndexLock::try_acquire(index_dir)? {
        Ok(lock) => lock,
        Err(_holder) => return Ok(RefreshOutcome::Locked),
    };
    // A view of the vector index is enough: this pass writes no vectors, and the
    // orphaned ones a deletion leaves behind are reached by no query and swept
    // by the next `rqmd update`.
    let mut s = store::open_store_no_backend(index_dir, true)?;
    for mut col in cols {
        // No hook ever runs from a search tool: the command is a collection
        // author's, not something a query agreed to.
        col.update_command = None;
        let outcome = update_one_collection(
            &mut s,
            &col,
            UpdateRun {
                progress_mode: ProgressMode::Off,
                hooks_allowed: false,
                full: false,
                hook_config: HookConfig::from_env(),
            },
            &|| lock.heartbeat(),
            // The walk stops on any `Some`; the value is only a stand-in for a signal.
            &|| deadline.is_expired().then_some(0),
        )?;
        if outcome.interrupted.is_some() {
            return Ok(RefreshOutcome::TimedOut);
        }
    }
    Ok(RefreshOutcome::Refreshed)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rqmd_core::store::{FTS_TOKENIZER_CONFIG_KEY, FTS_TOKENIZER_VERSION};

    use super::*;

    fn collection(name: &str, dir: &Path, update_command: Option<String>) -> Collection {
        Collection {
            name: name.into(),
            path: dir.to_string_lossy().to_string(),
            pattern: "**/*.md".into(),
            ignore: vec![],
            include_by_default: true,
            update_command,
            allow_hidden: false,
            respect_gitignore: false,
            max_file_bytes: None,
        }
    }

    /// An index with the given collections, marked as built by the current
    /// tokenizer so a refresh is allowed to run.
    fn index_with(cols: &[Collection]) -> tempfile::TempDir {
        let index_dir = tempfile::tempdir().unwrap();
        let s = store::open_store_no_backend(index_dir.path(), false).unwrap();
        for col in cols {
            db::upsert_collection(&s.db, col).unwrap();
        }
        db::set_config(&s.db, FTS_TOKENIZER_CONFIG_KEY, FTS_TOKENIZER_VERSION).unwrap();
        index_dir
    }

    fn active_docs(index_dir: &Path) -> i64 {
        let s = store::open_store_no_backend(index_dir, true).unwrap();
        s.db.query_row("SELECT COUNT(*) FROM documents WHERE active = 1", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    fn notes(dir: &Path, files: &[&str]) {
        for f in files {
            std::fs::write(dir.join(f), format!("body of {f}\n")).unwrap();
        }
    }

    #[test]
    fn indexes_the_requested_collections_only() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        notes(a.path(), &["one.md", "two.md"]);
        notes(b.path(), &["three.md"]);
        let index = index_with(&[
            collection("a", a.path(), None),
            collection("b", b.path(), None),
        ]);

        let names = ["a".to_string(), "nope".to_string()];
        let outcome = refresh_index(index.path(), Some(&names), Deadline::none()).unwrap();

        assert_eq!(outcome, RefreshOutcome::Refreshed);
        assert_eq!(active_docs(index.path()), 2, "only collection a is walked");
        let outcome = refresh_index(index.path(), None, Deadline::none()).unwrap();
        assert_eq!(outcome, RefreshOutcome::Refreshed);
        assert_eq!(active_docs(index.path()), 3);
    }

    #[test]
    fn a_passed_deadline_stops_the_pass_before_it_indexes() {
        let src = tempfile::tempdir().unwrap();
        notes(src.path(), &["one.md", "two.md"]);
        let index = index_with(&[collection("a", src.path(), None)]);

        let late = Deadline::after(Duration::ZERO);
        let outcome = refresh_index(index.path(), None, late).unwrap();

        assert_eq!(outcome, RefreshOutcome::TimedOut);
        assert_eq!(active_docs(index.path()), 0);
        let outcome = refresh_index(index.path(), None, Deadline::none()).unwrap();
        assert_eq!(outcome, RefreshOutcome::Refreshed);
        assert_eq!(active_docs(index.path()), 2);
    }

    #[test]
    fn a_held_lock_skips_the_pass_and_a_free_one_is_released() {
        let src = tempfile::tempdir().unwrap();
        notes(src.path(), &["one.md"]);
        let index = index_with(&[collection("a", src.path(), None)]);

        let held = IndexLock::acquire(index.path()).unwrap();
        let outcome = refresh_index(index.path(), None, Deadline::none()).unwrap();
        assert_eq!(outcome, RefreshOutcome::Locked);
        assert_eq!(active_docs(index.path()), 0);
        drop(held);

        refresh_index(index.path(), None, Deadline::none()).unwrap();
        assert!(IndexLock::try_acquire(index.path()).unwrap().is_ok());
    }

    #[test]
    fn a_pending_full_text_rebuild_skips_the_pass() {
        let src = tempfile::tempdir().unwrap();
        notes(src.path(), &["one.md"]);
        let index = index_with(&[collection("a", src.path(), None)]);
        refresh_index(index.path(), None, Deadline::none()).unwrap();
        {
            let s = store::open_store_no_backend(index.path(), false).unwrap();
            db::set_config(&s.db, FTS_TOKENIZER_CONFIG_KEY, "0").unwrap();
        }
        notes(src.path(), &["two.md"]);

        let outcome = refresh_index(index.path(), None, Deadline::none()).unwrap();

        assert_eq!(outcome, RefreshOutcome::RebuildPending);
        assert_eq!(active_docs(index.path()), 1);
    }

    #[test]
    fn never_runs_a_collections_update_command() {
        let src = tempfile::tempdir().unwrap();
        notes(src.path(), &["one.md"]);
        let marker = src.path().join("hook-ran");
        let hook = format!("touch {}", marker.display());
        let index = index_with(&[collection("a", src.path(), Some(hook))]);

        let outcome = refresh_index(index.path(), None, Deadline::none()).unwrap();

        assert_eq!(outcome, RefreshOutcome::Refreshed);
        assert_eq!(active_docs(index.path()), 1);
        assert!(!marker.exists(), "the update command must not run");
    }
}
