//! Opt-in query cache: query-expansion output and rerank scores from earlier
//! `rqmd query` runs, kept in `model_cache.sqlite` next to the index so a
//! repeated query does not pay for the same model calls again.
//!
//! The cache is a separate SQLite file. `index.sqlite` is never read or
//! written by this module, an `update` or `embed` holding the index lock is
//! never contended by a cache write, and deleting the file is always safe:
//! the next query recomputes what it needs.
//!
//! Only real model output is stored: an expansion the generation model
//! returned, and the scores of a rerank that finished with
//! [`crate::RerankStatus::Reranked`]. A failed or skipped expansion, a rerank
//! that failed, timed out or was skipped, and non-finite scores are never
//! written. Keys are SHA-256 digests; see `docs/ARCHITECTURE.md` for what each
//! one covers.

use anyhow::{Context, Result};
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

/// File name of the cache, inside the index directory.
pub const FILE_NAME: &str = "model_cache.sqlite";

/// Most rows the cache keeps, expansion and rerank rows counted together. A
/// write that goes over it evicts the least recently used rows.
pub const MAX_ROWS: u64 = 10_000;

/// Schema of this file. A file with any other `user_version` is dropped and
/// recreated on open; it only holds recomputable data.
const SCHEMA_VERSION: i64 = 1;

/// Version of the expansion prompt and of how its output is used. The cache
/// key already hashes the full prompt, so editing the template misses on its
/// own; bump this when the meaning of a cached output changes without the
/// prompt text changing.
const EXPANSION_PROMPT_VERSION: &str = "expansion-1";

/// Version of how the reranker is asked to score a pair. The template lives in
/// `rqmd-llm`, so a change there must bump this.
const RERANK_PROMPT_VERSION: &str = "rerank-1";

/// How long a cache read or write waits for another process's write.
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);

/// The cache file inside `index_dir`.
pub fn path_in(index_dir: &Path) -> PathBuf {
    index_dir.join(FILE_NAME)
}

/// The prompt version stored with an expansion row. Includes the rqmd version,
/// so an upgrade never reuses output produced under an older pipeline.
pub fn expansion_prompt_version() -> String {
    format!("{EXPANSION_PROMPT_VERSION}+{}", env!("CARGO_PKG_VERSION"))
}

/// The prompt version stored with a rerank row; see [`expansion_prompt_version`].
pub fn rerank_prompt_version() -> String {
    format!("{RERANK_PROMPT_VERSION}+{}", env!("CARGO_PKG_VERSION"))
}

/// The identity of a model for the cache key: its name, plus the size and
/// modification time of its weights file when it has one. `None` when that
/// file cannot be read, and the caller then neither reads nor writes the cache
/// for that model.
pub fn model_identity(name: &str, file: Option<&Path>) -> Option<String> {
    let Some(file) = file else {
        return Some(name.to_string());
    };
    let meta = std::fs::metadata(file).ok()?;
    let mtime_ns = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!("{name}|{}|{mtime_ns}", meta.len()))
}

/// SHA-256 over `kind` and each field, every field length-prefixed so no two
/// field lists hash the same.
fn digest(kind: &str, fields: &[&str]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    for part in std::iter::once(kind).chain(fields.iter().copied()) {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.finalize().to_vec()
}

/// Key of an expansion row: the full prompt, which holds the query, the intent
/// and the template.
fn expansion_key(prompt: &str) -> Vec<u8> {
    digest("expansion", &[prompt])
}

/// Key of a rerank row: the rerank query (intent included) and the exact text
/// of the chunk the reranker scored, so an edited chunk gets a new key.
fn rerank_key(query: &str, chunk: &str) -> Vec<u8> {
    digest(
        "rerank",
        &[query, &hex::encode(Sha256::digest(chunk.as_bytes()))],
    )
}

/// Whether `err` is SQLite reporting another connection's lock, a condition
/// that passes on its own rather than a broken cache file.
pub fn is_busy(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(e, _))
                if matches!(e.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
        )
    })
}

/// An open query cache.
pub struct ModelCache {
    conn: Connection,
    max_rows: u64,
}

impl ModelCache {
    /// Open the cache at `path`, creating the file (mode `0600` on Unix) and
    /// its tables when missing. Fails on a file that is not a usable SQLite
    /// database; the caller then runs without the cache.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_limit(path, MAX_ROWS)
    }

    /// [`Self::open`] with a row limit other than [`MAX_ROWS`], for tests.
    pub fn open_with_limit(path: &Path, max_rows: u64) -> Result<Self> {
        create_private(path).context("create the query cache file")?;
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .context("open the query cache")?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let mut cache = Self { conn, max_rows };
        cache.ensure_schema().context("read the query cache")?;
        Ok(cache)
    }

    fn ensure_schema(&mut self) -> Result<()> {
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == SCHEMA_VERSION {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "DROP TABLE IF EXISTS expansion;
             DROP TABLE IF EXISTS rerank;
             CREATE TABLE expansion (
                 key BLOB PRIMARY KEY,
                 model TEXT NOT NULL,
                 prompt_version TEXT NOT NULL,
                 output TEXT NOT NULL,
                 last_used INTEGER NOT NULL
             );
             CREATE INDEX expansion_last_used ON expansion(last_used);
             CREATE TABLE rerank (
                 key BLOB PRIMARY KEY,
                 model TEXT NOT NULL,
                 prompt_version TEXT NOT NULL,
                 score REAL NOT NULL,
                 last_used INTEGER NOT NULL
             );
             CREATE INDEX rerank_last_used ON rerank(last_used);",
        )?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(())
    }

    /// Rows held, expansion and rerank together.
    pub fn row_count(&self) -> Result<u64> {
        count_rows(&self.conn)
    }

    /// The cached output of the generation model `model` for `prompt`.
    pub fn get_expansion(&self, prompt: &str, model: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT output FROM expansion WHERE key = ?1 AND model = ?2 \
                 AND prompt_version = ?3",
                params![expansion_key(prompt), model, expansion_prompt_version()],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Store the output the generation model `model` returned for `prompt`,
    /// replacing any row for the same prompt, and mark it most recently used.
    pub fn put_expansion(&mut self, prompt: &str, model: &str, output: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let tick = next_tick(&tx)?;
        tx.execute(
            "INSERT OR REPLACE INTO expansion(key, model, prompt_version, output, last_used) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                expansion_key(prompt),
                model,
                expansion_prompt_version(),
                output,
                tick
            ],
        )?;
        evict(&tx, self.max_rows)?;
        tx.commit()?;
        Ok(())
    }

    /// The cached score of each of `chunks` for `query` under the rerank model
    /// `model`, in order; `None` where there is none.
    pub fn get_rerank_scores(
        &self,
        query: &str,
        model: &str,
        chunks: &[&str],
    ) -> Result<Vec<Option<f32>>> {
        let version = rerank_prompt_version();
        let mut stmt = self.conn.prepare_cached(
            "SELECT score FROM rerank WHERE key = ?1 AND model = ?2 AND prompt_version = ?3",
        )?;
        chunks
            .iter()
            .map(|chunk| {
                let score: Option<f64> = stmt
                    .query_row(params![rerank_key(query, chunk), model, version], |r| {
                        r.get(0)
                    })
                    .optional()?;
                Ok(score.map(|s| s as f32))
            })
            .collect()
    }

    /// Store the score the rerank model `model` gave each chunk for `query`,
    /// replacing older rows for the same pair and marking them most recently
    /// used. Non-finite scores are not real relevance scores and are skipped.
    pub fn put_rerank_scores(
        &mut self,
        query: &str,
        model: &str,
        scored: &[(&str, f32)],
    ) -> Result<()> {
        let version = rerank_prompt_version();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let tick = next_tick(&tx)?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO rerank(key, model, prompt_version, score, last_used) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (chunk, score) in scored.iter().filter(|(_, s)| s.is_finite()) {
                stmt.execute(params![
                    rerank_key(query, chunk),
                    model,
                    version,
                    f64::from(*score),
                    tick
                ])?;
            }
        }
        evict(&tx, self.max_rows)?;
        tx.commit()?;
        Ok(())
    }
}

/// Create `path` if it does not exist, readable and writable by its owner
/// only, without truncating an existing file.
fn create_private(path: &Path) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map(drop)
}

fn count_rows(conn: &Connection) -> Result<u64> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        return Ok(0);
    }
    let n: i64 = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM expansion) + (SELECT COUNT(*) FROM rerank)",
        [],
        |r| r.get(0),
    )?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// One more than the newest `last_used` in either table.
fn next_tick(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT MAX(COALESCE((SELECT MAX(last_used) FROM expansion), 0), \
                    COALESCE((SELECT MAX(last_used) FROM rerank), 0)) + 1",
        [],
        |r| r.get(0),
    )?)
}

/// Delete the least recently used rows, across both tables, until at most
/// `max_rows` remain.
fn evict(conn: &Connection, max_rows: u64) -> Result<()> {
    let rows = count_rows(conn)?;
    let Some(excess) = rows.checked_sub(max_rows).filter(|n| *n > 0) else {
        return Ok(());
    };
    let victims: Vec<(i64, i64)> = conn
        .prepare(
            "SELECT 0 AS t, rowid, last_used FROM expansion \
             UNION ALL SELECT 1 AS t, rowid, last_used FROM rerank \
             ORDER BY last_used, t, rowid LIMIT ?1",
        )?
        .query_map([i64::try_from(excess).unwrap_or(i64::MAX)], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (table, rowid) in victims {
        let sql = if table == 0 {
            "DELETE FROM expansion WHERE rowid = ?1"
        } else {
            "DELETE FROM rerank WHERE rowid = ?1"
        };
        conn.execute(sql, [rowid])?;
    }
    Ok(())
}

/// Rows in the cache at `path`, read without creating or changing anything:
/// `Ok(None)` when there is no cache file, an error when it cannot be read.
pub fn row_count_at(path: &Path) -> Result<Option<u64>> {
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    count_rows(&conn).map(Some)
}

/// What [`clear_at`] removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleared {
    /// There was no cache file.
    Nothing,
    /// The cache file was deleted; it held this many rows, or `None` when it
    /// could not be read first.
    Removed(Option<u64>),
}

/// Delete the cache at `path` and any SQLite journal files beside it. The
/// index is not touched.
pub fn clear_at(path: &Path) -> Result<Cleared> {
    if !path.exists() {
        return Ok(Cleared::Nothing);
    }
    let rows = row_count_at(path).ok().flatten();
    std::fs::remove_file(path).context("delete the query cache file")?;
    for suffix in ["-journal", "-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        match std::fs::remove_file(PathBuf::from(sidecar)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("delete a query cache journal file"),
        }
    }
    Ok(Cleared::Removed(rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open(dir: &TempDir, max_rows: u64) -> ModelCache {
        ModelCache::open_with_limit(&path_in(dir.path()), max_rows).unwrap()
    }

    #[test]
    fn expansion_round_trips_under_the_same_model_only() {
        let dir = TempDir::new().unwrap();
        let mut cache = open(&dir, MAX_ROWS);
        assert_eq!(cache.get_expansion("p", "gen-a").unwrap(), None);

        cache.put_expansion("p", "gen-a", "lex: x").unwrap();

        assert_eq!(
            cache.get_expansion("p", "gen-a").unwrap().as_deref(),
            Some("lex: x")
        );
        assert_eq!(cache.get_expansion("p", "gen-b").unwrap(), None);
        assert_eq!(cache.get_expansion("other", "gen-a").unwrap(), None);
    }

    #[test]
    fn ac_2_rerank_scores_are_keyed_by_query_chunk_text_and_model() {
        let dir = TempDir::new().unwrap();
        let mut cache = open(&dir, MAX_ROWS);
        cache
            .put_rerank_scores("q", "rr", &[("one", 0.25), ("two", -1.5)])
            .unwrap();

        let got = cache
            .get_rerank_scores("q", "rr", &["two", "one", "one edited"])
            .unwrap();
        assert_eq!(got, vec![Some(-1.5), Some(0.25), None]);
        assert_eq!(
            cache.get_rerank_scores("q", "rr-2", &["one"]).unwrap(),
            vec![None]
        );
        assert_eq!(
            cache.get_rerank_scores("q2", "rr", &["one"]).unwrap(),
            vec![None]
        );
    }

    #[test]
    fn ac_3_a_row_from_another_prompt_version_is_a_miss() {
        let dir = TempDir::new().unwrap();
        let mut cache = open(&dir, MAX_ROWS);
        cache.put_expansion("p", "gen", "out").unwrap();
        cache.put_rerank_scores("q", "rr", &[("c", 1.0)]).unwrap();
        cache
            .conn
            .execute_batch("UPDATE expansion SET prompt_version = 'old'; UPDATE rerank SET prompt_version = 'old';")
            .unwrap();

        assert_eq!(cache.get_expansion("p", "gen").unwrap(), None);
        assert_eq!(
            cache.get_rerank_scores("q", "rr", &["c"]).unwrap(),
            vec![None]
        );
    }

    #[test]
    fn ac_4_non_finite_scores_are_never_stored() {
        let dir = TempDir::new().unwrap();
        let mut cache = open(&dir, MAX_ROWS);
        cache
            .put_rerank_scores(
                "q",
                "rr",
                &[("a", f32::NEG_INFINITY), ("b", f32::NAN), ("c", 0.5)],
            )
            .unwrap();
        assert_eq!(cache.row_count().unwrap(), 1);
    }

    #[test]
    fn ac_5_writes_past_the_limit_evict_the_least_recently_used_rows() {
        let dir = TempDir::new().unwrap();
        let mut cache = open(&dir, 3);
        cache.put_expansion("p1", "gen", "old").unwrap();
        cache
            .put_rerank_scores("q", "rr", &[("a", 1.0), ("b", 2.0)])
            .unwrap();
        // Rewriting p1 makes it the newest, so the rerank rows go first.
        cache.put_expansion("p1", "gen", "new").unwrap();
        cache.put_expansion("p2", "gen", "x").unwrap();

        assert_eq!(cache.row_count().unwrap(), 3);
        assert_eq!(
            cache.get_expansion("p1", "gen").unwrap().as_deref(),
            Some("new")
        );
        let scores = cache.get_rerank_scores("q", "rr", &["a", "b"]).unwrap();
        assert_eq!(scores.iter().filter(|s| s.is_some()).count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn ac_6_the_cache_file_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        drop(open(&dir, MAX_ROWS));
        let mode = std::fs::metadata(path_in(dir.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn ac_7_a_corrupt_file_fails_to_open_instead_of_being_overwritten() {
        let dir = TempDir::new().unwrap();
        let path = path_in(dir.path());
        let garbage = b"not a sqlite database ".repeat(200);
        std::fs::write(&path, &garbage).unwrap();

        assert!(ModelCache::open(&path).is_err());
        assert!(row_count_at(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), garbage);
    }

    #[test]
    fn ac_5_counting_and_clearing_never_create_a_cache() {
        let dir = TempDir::new().unwrap();
        let path = path_in(dir.path());

        assert_eq!(row_count_at(&path).unwrap(), None);
        assert_eq!(clear_at(&path).unwrap(), Cleared::Nothing);
        assert!(!path.exists());

        let mut cache = ModelCache::open(&path).unwrap();
        cache.put_expansion("p", "gen", "out").unwrap();
        drop(cache);
        assert_eq!(row_count_at(&path).unwrap(), Some(1));
        assert_eq!(clear_at(&path).unwrap(), Cleared::Removed(Some(1)));
        assert!(!path.exists());
    }

    #[test]
    fn ac_3_model_identity_changes_with_the_weights_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("w.gguf");
        assert_eq!(model_identity("m", None).as_deref(), Some("m"));
        assert_eq!(model_identity("m", Some(&file)), None, "missing file");

        std::fs::write(&file, b"1234").unwrap();
        let first = model_identity("m", Some(&file)).unwrap();
        assert!(first.starts_with("m|4|"), "{first}");
        std::fs::write(&file, b"12345").unwrap();
        assert_ne!(model_identity("m", Some(&file)).unwrap(), first);
    }
}
