//! `rqmd watch`: keep the full-text index current while collection files change.
//!
//! Each tick walks every collection and compares the files' size and mtime with
//! what the previous tick saw; the index is only opened for a collection whose
//! files differ. A pass is `update_one_collection`, so masks, ignore patterns,
//! hidden files, size limits and symlink containment behave as in `rqmd update`.
//! Watch indexes for keyword search only: it loads no model, never runs an
//! update hook and never rebuilds the full-text index, and it holds the index
//! lock only while a pass runs.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

use rqmd_core::{Collection, IndexLock, db};

use super::index::{UpdateRun, update_one_collection};
use crate::exit::Interrupted;
use crate::hook::HookConfig;
use crate::{document, exclusions, format as fmt, signals, store};

/// Longest wait between attempts while another process holds the lock or the
/// full-text index waits for a rebuild (never less than the interval).
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How often a sleeping watch looks for a signal.
const SLICE: Duration = Duration::from_millis(100);

/// What the previous tick saw in a collection: the walked files and their stats.
type Fingerprint = u64;

/// Why a pass did not run, so a streak is announced once and not every tick.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Deferral {
    Locked,
    RebuildPending,
}

pub fn run_watch(index_dir: &Path, collection: Option<&str>, interval: Duration) -> Result<()> {
    let cols = {
        let s = store::open_store_no_backend(index_dir, true)?;
        select(db::list_collections(&s.db)?, collection)?
    };
    if cols.is_empty() {
        println!("No collections to watch.");
        return Ok(());
    }
    // A signal ends the loop at the next check; a second one ends the process.
    let _catch = signals::Catch::install_exiting_on_repeat();
    println!(
        "{}",
        fmt::Paint::stdout().bold(format!(
            "Watching {} collection(s) every {}s (Ctrl-C to stop)",
            cols.len(),
            interval.as_secs()
        ))
    );

    let mut watcher = Watcher {
        index_dir,
        collection,
        seen: HashMap::new(),
        deferred: None,
        deferrals: 0,
    };
    loop {
        let wait = if watcher.tick()? {
            interval
        } else {
            backoff(interval, watcher.deferrals)
        };
        sleep_until_signal(wait)?;
    }
}

struct Watcher<'a> {
    index_dir: &'a Path,
    collection: Option<&'a str>,
    /// Per collection, the fingerprint a completed pass was based on.
    seen: HashMap<String, Fingerprint>,
    /// The reason the last pass was deferred, until one completes.
    deferred: Option<Deferral>,
    /// Consecutive deferred passes, for the backoff.
    deferrals: u32,
}

impl Watcher<'_> {
    /// Check the collections once and index the ones that changed. `false`
    /// means a pass was due but deferred, so the next try should back off.
    fn tick(&mut self) -> Result<bool> {
        if let Some(signal) = signals::pending() {
            return Err(Interrupted(signal).into());
        }
        let cols = self.current_collections()?;
        self.seen
            .retain(|name, _| cols.iter().any(|c| &c.name == name));

        let mut due = Vec::new();
        for col in &cols {
            let (fingerprint, settled) = fingerprint(col);
            if self.seen.get(&col.name) != Some(&fingerprint) {
                due.push((col, fingerprint, settled));
            }
        }
        if due.is_empty() {
            return Ok(true);
        }

        // Checked before taking the lock, so the `rqmd update` that clears it is
        // never made to wait for a watch that is only looking.
        if store::open_store_no_backend(self.index_dir, true)?.fts_rebuild_pending()? {
            self.defer(
                Deferral::RebuildPending,
                "the full-text index needs a rebuild; run `rqmd update` to rebuild it (watch is paused until then)".into(),
            );
            return Ok(false);
        }
        let lock = match IndexLock::try_acquire(self.index_dir)? {
            Ok(lock) => lock,
            Err(holder) => {
                let who = holder
                    .pid
                    .map(|pid| format!(" (held by pid {pid})"))
                    .unwrap_or_default();
                self.defer(Deferral::Locked, format!("waiting for the index lock{who}"));
                return Ok(false);
            }
        };
        let mut s = store::open_store_no_backend(self.index_dir, false)?;
        let progress_mode = fmt::ProgressMode::detect();
        let p = fmt::Paint::stdout();
        for (col, fingerprint, settled) in due {
            println!(
                "{} {}",
                p.bold(&col.name),
                p.dim(format!("({})", col.pattern))
            );
            // No hook ever runs from a watch: a command that fires on every file
            // change is not something a collection's author agreed to.
            let mut col = col.clone();
            col.update_command = None;
            let outcome = update_one_collection(
                &mut s,
                &col,
                UpdateRun {
                    progress_mode,
                    hooks_allowed: false,
                    full: false,
                    hook_config: HookConfig::from_env(),
                },
                &|| lock.heartbeat(),
                &signals::pending,
            )?;
            if let Some(report) = &outcome.report {
                println!("{report}");
            }
            if let Some(signal) = outcome.interrupted {
                return Err(Interrupted(signal).into());
            }
            // A file stamped within the last moments is not trusted by the
            // stat-skip yet, so it is looked at again next tick.
            if settled {
                self.seen.insert(col.name, fingerprint);
            }
        }
        s.reclaim_orphaned_vectors()?;
        self.deferred = None;
        self.deferrals = 0;
        Ok(true)
    }

    fn defer(&mut self, why: Deferral, message: String) {
        if self.deferred != Some(why) {
            eprintln!("rqmd watch: {message}");
        }
        self.deferred = Some(why);
        self.deferrals = self.deferrals.saturating_add(1);
    }

    /// The collections to watch, read fresh so one added or removed while
    /// watching is picked up. A read-only connection: no lock, no schema work.
    fn current_collections(&self) -> Result<Vec<Collection>> {
        let path = self.index_dir.join("index.sqlite");
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(30))?;
        let mut cols = db::list_collections(&conn)?;
        if let Some(name) = self.collection {
            cols.retain(|c| c.name == name);
        }
        Ok(cols)
    }
}

fn select(cols: Vec<Collection>, collection: Option<&str>) -> Result<Vec<Collection>> {
    match collection {
        Some(name) => Ok(vec![
            cols.into_iter()
                .find(|c| c.name == name)
                .with_context(|| format!("collection '{name}' not found"))?,
        ]),
        None => Ok(cols),
    }
}

/// A fingerprint of the files a pass would index, from the same walk `update`
/// does and each file's size and mtime, and whether every stamp is old enough
/// for the stat-skip to trust. Nothing is read from the files or the index.
fn fingerprint(col: &Collection) -> (Fingerprint, bool) {
    let dir = Path::new(&col.path);
    // A mask that does not parse matches nothing; a pass reports it once.
    let Ok(include) = exclusions::build_include_set(&col.pattern) else {
        return (0, true);
    };
    let ignore = exclusions::build_ignore_set(&col.ignore);
    let found = document::collect_candidates(
        dir,
        &include,
        &ignore,
        document::WalkOptions::for_collection(col),
    );
    let mut files: Vec<_> = found
        .files
        .iter()
        .map(|path| (document::rel_path_of(path, dir), document::stat_file(path)))
        .collect();
    files.sort();
    let settled = files
        .iter()
        .all(|(_, stat)| stat.is_none_or(|s| document::trusted_stat(s).is_some()));
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    files.hash(&mut hasher);
    (hasher.finish(), settled)
}

/// The wait after `deferrals` deferred passes in a row: the interval, doubled
/// each time, up to [`MAX_BACKOFF`].
fn backoff(interval: Duration, deferrals: u32) -> Duration {
    interval
        .saturating_mul(1 << deferrals.saturating_sub(1).min(5))
        .min(MAX_BACKOFF.max(interval))
}

/// Sleep for `wait`, or return `Interrupted` as soon as a signal arrives.
fn sleep_until_signal(wait: Duration) -> Result<()> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(signal) = signals::pending() {
            return Err(Interrupted(signal).into());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        std::thread::sleep(left.min(SLICE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collection_at(dir: &Path) -> Collection {
        Collection {
            name: "c".into(),
            path: dir.to_string_lossy().to_string(),
            pattern: "**/*.md".into(),
            ignore: Vec::new(),
            include_by_default: true,
            update_command: None,
            allow_hidden: false,
            respect_gitignore: false,
            max_file_bytes: None,
            embed_ignore: vec![],
            embed_max_bytes: None,
        }
    }

    #[test]
    fn backoff_starts_at_the_interval_doubles_and_stops_at_the_cap() {
        let one = Duration::from_secs(1);
        let waits: Vec<u64> = (0..9).map(|n| backoff(one, n).as_secs()).collect();
        assert_eq!(waits, [1, 1, 2, 4, 8, 16, 30, 30, 30]);
        // The cap never undercuts a long interval.
        let long = Duration::from_secs(120);
        assert_eq!(backoff(long, 4), long);
    }

    #[test]
    fn fingerprint_changes_with_the_file_set_and_with_edits() {
        let dir = tempfile::tempdir().unwrap();
        let col = collection_at(dir.path());
        std::fs::write(dir.path().join("a.md"), "one").unwrap();
        let (before, _) = fingerprint(&col);
        assert_eq!(fingerprint(&col).0, before, "stable while nothing changes");

        std::fs::write(dir.path().join("b.md"), "two").unwrap();
        let (with_b, _) = fingerprint(&col);
        assert_ne!(with_b, before, "a new file");

        std::fs::write(dir.path().join("a.md"), "one, longer").unwrap();
        let (edited, _) = fingerprint(&col);
        assert_ne!(edited, with_b, "an edit");

        std::fs::remove_file(dir.path().join("b.md")).unwrap();
        assert_ne!(fingerprint(&col).0, edited, "a deleted file");
    }

    #[test]
    fn fresh_files_are_not_settled_and_old_ones_are() {
        let dir = tempfile::tempdir().unwrap();
        let col = collection_at(dir.path());
        let file = dir.path().join("a.md");
        std::fs::write(&file, "one").unwrap();
        assert!(!fingerprint(&col).1, "just written");

        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::open(&file)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(fingerprint(&col).1, "an hour old");
    }
}
