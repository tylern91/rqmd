//! rusqlite schema and CRUD layer.
//!
//! Schema mirrors qmd's TypeScript store exactly (same table/column names) so
//! existing indexes remain readable. The FTS5 virtual table and vectors_vec
//! extension are NOT created here — Tantivy and usearch replace them.

use anyhow::{Context, Result};
use hex;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;

use crate::types::{Collection, Document};

// ── Schema init ───────────────────────────────────────────────────────────────

/// Open (or create) the SQLite database and ensure schema is current.
pub fn open_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).context("open sqlite db")?;
    conn.execute_batch(
        // busy_timeout of 30s tolerates a long `rqmd embed` batch holding a write
        // lock at a commit boundary while a concurrent MCP/CLI reader is waiting,
        // without wedging indefinitely on a genuine deadlock.
        "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 30000;",
    )?;
    init_schema(&conn)?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS content (
            hash TEXT PRIMARY KEY,
            doc  TEXT NOT NULL,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS documents (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            collection  TEXT NOT NULL,
            path        TEXT NOT NULL,
            title       TEXT NOT NULL,
            hash        TEXT NOT NULL REFERENCES content(hash) ON DELETE CASCADE,
            created_at  TEXT NOT NULL,
            modified_at TEXT NOT NULL,
            active      INTEGER NOT NULL DEFAULT 1,
            UNIQUE(collection, path)
        );

        CREATE INDEX IF NOT EXISTS idx_documents_collection
            ON documents(collection, active);
        CREATE INDEX IF NOT EXISTS idx_documents_hash
            ON documents(hash);
        CREATE INDEX IF NOT EXISTS idx_documents_path
            ON documents(path, active);

        -- content_vectors tracks per-chunk embedding metadata.
        -- vid is the usearch key (auto-assigned, stable across restarts).
        CREATE TABLE IF NOT EXISTS content_vectors (
            hash             TEXT NOT NULL,
            seq              INTEGER NOT NULL DEFAULT 0,
            pos              INTEGER NOT NULL DEFAULT 0,
            model            TEXT NOT NULL,
            embed_fingerprint TEXT NOT NULL DEFAULT '',
            total_chunks     INTEGER NOT NULL DEFAULT 1,
            embedded_at      TEXT NOT NULL,
            vid              INTEGER UNIQUE,
            PRIMARY KEY (hash, seq)
        );

        CREATE TABLE IF NOT EXISTS store_collections (
            name               TEXT PRIMARY KEY,
            path               TEXT NOT NULL,
            pattern            TEXT NOT NULL DEFAULT '**/*.md',
            ignore_patterns    TEXT,
            include_by_default INTEGER DEFAULT 1,
            update_command     TEXT,
            context            TEXT,
            allow_hidden       INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS store_config (
            key   TEXT PRIMARY KEY,
            value TEXT
        );
    "#,
    )?;
    // `allow_hidden` is this codebase's first schema migration: the CREATE TABLE
    // above is a no-op against a database that already has `store_collections`
    // from before this column existed, so existing installs need an explicit
    // ALTER TABLE to catch up.
    ensure_column(
        conn,
        "store_collections",
        "allow_hidden",
        "allow_hidden INTEGER NOT NULL DEFAULT 0",
    )?;
    Ok(())
}

/// Add `column` to `table` if it isn't already present, by running
/// `ALTER TABLE <table> ADD COLUMN <ddl>`. Idempotent — safe to call on every
/// startup regardless of whether the column was just added to the `CREATE
/// TABLE` DDL above or already exists from a prior run.
///
/// Deliberately minimal: a single guarded ALTER, not a general migration
/// framework. `table` and `column` are always internal constants (never user
/// input), so building the PRAGMA/ALTER statements with `format!` carries no
/// injection risk.
fn ensure_column(conn: &Connection, table: &str, column: &str, ddl: &str) -> Result<()> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .context("prepare table_info")?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .context("query table_info")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("read table_info rows")?
        .iter()
        .any(|name| name == column);
    if !exists {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {ddl}"), [])
            .with_context(|| format!("add column {table}.{column}"))?;
    }
    Ok(())
}

// ── Docid ─────────────────────────────────────────────────────────────────────

/// First 6 hex chars of SHA-256(content) — matches qmd's docid format.
pub fn content_hash(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    hex::encode(h.finalize())
}

pub fn docid_from_hash(hash: &str) -> &str {
    &hash[..6.min(hash.len())]
}

// ── Content CRUD ──────────────────────────────────────────────────────────────

pub fn upsert_content(conn: &Connection, hash: &str, doc: &str, now: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO content(hash, doc, created_at) VALUES (?1, ?2, ?3)",
        params![hash, doc, now],
    )?;
    Ok(())
}

pub fn get_content(conn: &Connection, hash: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT doc FROM content WHERE hash = ?1",
        params![hash],
        |row| row.get(0),
    )
    .optional()
    .context("get content")
}

// ── Document CRUD ─────────────────────────────────────────────────────────────

/// Column list shared by every query that hydrates a full `Document` — keeps
/// column order in sync with `map_document`'s field order across call sites.
const DOC_COLUMNS: &str = "id, collection, path, title, hash, active";

/// Row mapper shared by every query selecting `DOC_COLUMNS` (optionally with
/// a table alias, and optionally with trailing columns beyond index 5 — extra
/// columns are simply ignored).
fn map_document(row: &rusqlite::Row) -> rusqlite::Result<Document> {
    Ok(Document {
        id: row.get(0)?,
        collection: row.get(1)?,
        path: row.get(2)?,
        title: row.get(3)?,
        hash: row.get(4)?,
        active: row.get::<_, i64>(5)? != 0,
    })
}

/// Insert or update a document record. Returns the document's stable row id.
///
/// Uses `RETURNING id` rather than `last_insert_rowid()`: on the `ON CONFLICT
/// DO UPDATE` arm, SQLite leaves `last_insert_rowid()` untouched (it only
/// advances on an actual `INSERT`), so it would otherwise reflect whichever
/// unrelated row — e.g. the `content` row `upsert_content` just inserted for
/// the new hash — was last inserted on the connection, not this document's
/// own id. Every caller re-indexing an existing (collection, path) hit that
/// exact sequence, so the id fed into Tantivy's `doc_id` field was wrong on
/// every content update.
pub fn upsert_document(
    conn: &Connection,
    collection: &str,
    path: &str,
    title: &str,
    hash: &str,
    now: &str,
) -> Result<i64> {
    conn.query_row(
        r#"
        INSERT INTO documents(collection, path, title, hash, created_at, modified_at, active)
        VALUES (?1, ?2, ?3, ?4, ?5, ?5, 1)
        ON CONFLICT(collection, path) DO UPDATE SET
            title       = excluded.title,
            hash        = excluded.hash,
            modified_at = excluded.modified_at,
            active      = 1
        RETURNING id
        "#,
        params![collection, path, title, hash, now],
        |row| row.get(0),
    )
    .context("upsert document")
}

pub fn get_document_by_filepath(
    conn: &Connection,
    collection: &str,
    path: &str,
) -> Result<Option<Document>> {
    conn.query_row(
        &format!("SELECT {DOC_COLUMNS} FROM documents WHERE collection=?1 AND path=?2"),
        params![collection, path],
        map_document,
    )
    .optional()
    .context("get document")
}

pub fn get_document_by_id(conn: &Connection, id: i64) -> Result<Option<Document>> {
    conn.query_row(
        &format!("SELECT {DOC_COLUMNS} FROM documents WHERE id=?1"),
        params![id],
        map_document,
    )
    .optional()
    .context("get document by id")
}

/// Escape `LIKE` metacharacters (`\`, `%`, `_`) so a caller-supplied prefix is
/// matched literally rather than as a wildcard pattern. Must escape `\` first
/// since it becomes the escape character for the other two.
fn escape_like_pattern(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Look up a document by the first 6 hex chars of its content hash (the docid).
///
/// Ordered by `(collection, path)` before `LIMIT 1` so a hash-prefix collision
/// resolves to the same document every time, rather than an arbitrary one
/// depending on SQLite's row order. `docid` is not validated as hex before
/// reaching this function (see `PathSpec::docid_hex` in rqmd-cli), so `%`/`_`
/// are escaped rather than trusted — otherwise a docid containing either
/// would silently widen the prefix match into a wildcard one.
pub fn get_document_by_docid_prefix(conn: &Connection, docid: &str) -> Result<Option<Document>> {
    let pattern = format!("{}%", escape_like_pattern(docid));
    conn.query_row(
        &format!(
            "SELECT {DOC_COLUMNS} FROM documents \
             WHERE hash LIKE ?1 ESCAPE '\\' AND active=1 ORDER BY collection, path LIMIT 1"
        ),
        params![pattern],
        map_document,
    )
    .optional()
    .context("get document by docid")
}

/// List active documents, optionally filtered to a single collection.
/// Thin wrapper over `list_documents_multi` — a one-element slice reproduces
/// this function's exact prior behavior.
pub fn list_documents(conn: &Connection, collection: Option<&str>) -> Result<Vec<Document>> {
    let owned = collection.map(|c| [c.to_string()]);
    list_documents_multi(conn, owned.as_ref().map(|a| a.as_slice()))
}

/// List active documents, optionally filtered to any of several collections.
/// `None` or an empty slice returns documents from every collection. Backs the
/// MCP server's `collections` filter (multi-collection parity with qmd 2.6.3).
pub fn list_documents_multi(
    conn: &Connection,
    collections: Option<&[String]>,
) -> Result<Vec<Document>> {
    match collections {
        Some(cols) if !cols.is_empty() => {
            let placeholders = vec!["?"; cols.len()].join(",");
            let sql = format!(
                "SELECT {DOC_COLUMNS} FROM documents \
                 WHERE collection IN ({placeholders}) AND active=1 ORDER BY collection, path"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params_from_iter(cols.iter()), map_document)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        }
        _ => {
            let sql = format!(
                "SELECT {DOC_COLUMNS} FROM documents WHERE active=1 ORDER BY collection, path"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map([], map_document)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        }
    }
}

/// Resolve plain (non-glob, non-docid) `multi-get` patterns directly in SQL,
/// avoiding a full-table scan of `list_documents_multi` for the common case.
///
/// Each needle matches a document if it equals — or is a `/`-anchored suffix
/// of — either the collection-relative `path` or the full `collection/path`.
/// The anchoring is what prevents a fragment like "SYNTAX.md" from matching
/// "OLD-SYNTAX.md": the suffix check requires a `/` immediately before the
/// needle, not just a raw substring match.
pub fn find_documents_by_needles(
    conn: &Connection,
    collections: Option<&[String]>,
    needles: &[&str],
) -> Result<Vec<Document>> {
    if needles.is_empty() {
        return Ok(vec![]);
    }

    // The two LIKE arms need `ESCAPE '\'` and an escaped needle — otherwise a
    // needle containing `%` or `_` (e.g. a client-supplied "%") widens into a
    // wildcard and the suffix check degrades to "any path with a `/` in it",
    // i.e. every document. The two `=` arms are exact matches and take the
    // needle raw.
    const NEEDLE_CLAUSE: &str =
        "(path = ? OR (collection || '/' || path) = ? OR path LIKE '%/' || ? ESCAPE '\\' OR (collection || '/' || path) LIKE '%/' || ? ESCAPE '\\')";
    let clauses = vec![NEEDLE_CLAUSE; needles.len()].join(" OR ");

    let mut params: Vec<String> = Vec::with_capacity(needles.len() * 4);
    for needle in needles {
        let escaped = escape_like_pattern(needle);
        params.push(needle.to_string());
        params.push(needle.to_string());
        params.push(escaped.clone());
        params.push(escaped);
    }

    let mut sql = format!("SELECT {DOC_COLUMNS} FROM documents WHERE active=1 AND ({clauses})");
    if let Some(cols) = collections.filter(|c| !c.is_empty()) {
        let placeholders = vec!["?"; cols.len()].join(",");
        sql.push_str(&format!(" AND collection IN ({placeholders})"));
        params.extend(cols.iter().cloned());
    }
    sql.push_str(" ORDER BY collection, path");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(params.iter()), map_document)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

// ── content_vectors CRUD ──────────────────────────────────────────────────────

/// Check whether a chunk already has an embedding (by embed_fingerprint).
pub fn has_vector(conn: &Connection, hash: &str, seq: i64, fingerprint: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM content_vectors WHERE hash=?1 AND seq=?2 AND embed_fingerprint=?3",
        params![hash, seq, fingerprint],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Count distinct content hashes still needing embedding: active documents with
/// NON-EMPTY content whose hash has no content_vectors row. The `length(c.doc) > 0`
/// filter mirrors run_embed's `if body.is_empty() { continue; }` skip — without it,
/// empty files (hash = SHA-256 of "") count as pending forever but never embed.
pub fn count_docs_needing_embed(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(DISTINCT d.hash) FROM documents d \
         JOIN content c ON c.hash = d.hash \
         WHERE d.active = 1 AND length(c.doc) > 0 \
         AND d.hash NOT IN (SELECT hash FROM content_vectors)",
        [],
        |r| r.get(0),
    )
}

/// Check whether a content hash has at least one vector row (any seq, any fingerprint).
/// Used by `rqmd embed` to skip already-embedded documents during an incremental run.
pub fn hash_has_any_vector(conn: &Connection, hash: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM content_vectors WHERE hash=?1",
        params![hash],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// Return the highest vid currently stored in content_vectors, or None if the table is empty.
/// Used to reconcile the HNSW allocator's `next_vid` against the DB after load so that
/// freshly-issued vids never collide with existing rows.
pub fn max_vector_vid(conn: &Connection) -> Result<Option<u64>> {
    let maybe: Option<i64> = conn
        .query_row("SELECT MAX(vid) FROM content_vectors", [], |row| row.get(0))
        .context("max_vector_vid")?;
    Ok(maybe.map(|v| v as u64))
}

/// Remove all content_vectors rows — used by `rqmd embed --rebuild` to reset the
/// entire vector index before re-embedding from scratch.
pub fn clear_all_vectors(conn: &Connection) -> Result<usize> {
    let n = conn.execute("DELETE FROM content_vectors", [])?;
    Ok(n)
}

/// Distinct `embed_fingerprint` values present in `content_vectors`, with per-fingerprint
/// chunk counts, ordered most-common first. Empty-string fingerprints (rows embedded
/// before fingerprinting existed) are excluded — `rqmd embed --rebuild` covers those too.
/// Used by `rqmd doctor` to detect an index that mixes vectors from more than one
/// embedding model or chunking configuration.
pub fn fingerprint_breakdown(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT embed_fingerprint, COUNT(*) FROM content_vectors \
         WHERE embed_fingerprint != '' GROUP BY embed_fingerprint ORDER BY COUNT(*) DESC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Insert or update a chunk's vector metadata.
/// `vid` is the usearch key (caller assigns it from the HNSW index).
#[allow(clippy::too_many_arguments)]
pub fn upsert_vector_meta(
    conn: &Connection,
    hash: &str,
    seq: i64,
    pos: i64,
    model: &str,
    fingerprint: &str,
    total_chunks: i64,
    vid: u64,
    now: &str,
) -> Result<()> {
    conn.execute(
        r#"
        INSERT INTO content_vectors(hash, seq, pos, model, embed_fingerprint, total_chunks, embedded_at, vid)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        ON CONFLICT(hash, seq) DO UPDATE SET
            pos              = excluded.pos,
            model            = excluded.model,
            embed_fingerprint = excluded.embed_fingerprint,
            total_chunks     = excluded.total_chunks,
            embedded_at      = excluded.embedded_at,
            vid              = excluded.vid
        "#,
        params![hash, seq, pos, model, fingerprint, total_chunks, now, vid as i64],
    )?;
    Ok(())
}

/// Look up (collection, path, title, hash, doc_body) for a vector ID.
/// Returns None if the vid has no matching active document.
pub fn doc_for_vid(conn: &Connection, vid: u64) -> Result<Option<(Document, String)>> {
    conn.query_row(
        r#"
        SELECT d.id, d.collection, d.path, d.title, d.hash, d.active, c.doc
        FROM content_vectors cv
        JOIN documents d ON d.hash = cv.hash AND d.active = 1
        JOIN content c ON c.hash = cv.hash
        WHERE cv.vid = ?1
        LIMIT 1
        "#,
        params![vid as i64],
        |row| Ok((map_document(row)?, row.get::<_, String>(6)?)),
    )
    .optional()
    .context("doc_for_vid")
}

/// Look up (collection, path, title, hash) for a vector ID, without the
/// document body. Callers that only need identity/metadata (e.g. ranking
/// and dedup before any chunk is selected) should prefer this over
/// `doc_for_vid` — it skips the `content` join entirely, avoiding an
/// unused body fetch on every candidate.
pub fn doc_for_vid_meta(conn: &Connection, vid: u64) -> Result<Option<Document>> {
    conn.query_row(
        r#"
        SELECT d.id, d.collection, d.path, d.title, d.hash, d.active
        FROM content_vectors cv
        JOIN documents d ON d.hash = cv.hash AND d.active = 1
        WHERE cv.vid = ?1
        LIMIT 1
        "#,
        params![vid as i64],
        map_document,
    )
    .optional()
    .context("doc_for_vid_meta")
}

/// Return all vids for a content hash's chunks, ordered by `seq` (chunk order).
/// Used by `rqmd similar` to gather every chunk vector for a resolved document.
pub fn vids_for_hash(conn: &Connection, hash: &str) -> Result<Vec<u64>> {
    let mut stmt = conn.prepare("SELECT vid FROM content_vectors WHERE hash = ?1 ORDER BY seq")?;
    let rows = stmt
        .query_map(params![hash], |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("vids_for_hash")?;
    Ok(rows.into_iter().map(|v| v as u64).collect())
}

/// Load all (vid → (hash, seq)) pairs for rebuilding the HNSW index on startup.
pub fn load_all_vid_mappings(conn: &Connection) -> Result<Vec<(u64, String, i64)>> {
    let mut stmt = conn
        .prepare("SELECT vid, hash, seq FROM content_vectors WHERE vid IS NOT NULL ORDER BY vid")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

// ── Collections ───────────────────────────────────────────────────────────────

pub fn upsert_collection(conn: &Connection, c: &Collection) -> Result<()> {
    let ignore = serde_json::to_string(&c.ignore)?;
    conn.execute(
        r#"
        INSERT INTO store_collections(name, path, pattern, ignore_patterns, include_by_default, update_command, allow_hidden)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ON CONFLICT(name) DO UPDATE SET
            path               = excluded.path,
            pattern            = excluded.pattern,
            ignore_patterns    = excluded.ignore_patterns,
            include_by_default = excluded.include_by_default,
            update_command     = excluded.update_command,
            allow_hidden       = excluded.allow_hidden
        "#,
        params![
            c.name,
            c.path,
            c.pattern,
            ignore,
            c.include_by_default as i64,
            c.update_command,
            c.allow_hidden as i64,
        ],
    )?;
    Ok(())
}

pub fn list_collections(conn: &Connection) -> Result<Vec<Collection>> {
    let mut stmt = conn.prepare(
        "SELECT name, path, pattern, ignore_patterns, include_by_default, update_command, allow_hidden FROM store_collections",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    rows.into_iter()
        .map(
            |(name, path, pattern, ignore_json, include, update, allow_hidden)| {
                let ignore: Vec<String> = ignore_json
                    .map(|j| serde_json::from_str(&j).unwrap_or_default())
                    .unwrap_or_default();
                Ok(Collection {
                    name,
                    path,
                    pattern,
                    ignore,
                    include_by_default: include != 0,
                    update_command: update,
                    allow_hidden: allow_hidden != 0,
                })
            },
        )
        .collect()
}

/// Return (doc_count, last_modified_rfc3339) for a collection's active documents.
/// `last_modified` is None when the collection has no active documents.
pub fn collection_doc_stats(conn: &Connection, collection: &str) -> Result<(i64, Option<String>)> {
    let (count, latest): (i64, Option<String>) = conn.query_row(
        "SELECT COUNT(*), MAX(modified_at) FROM documents WHERE collection=?1 AND active=1",
        params![collection],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((count, latest))
}

/// Soft-delete active documents whose `path` is no longer among `current_paths`
/// — files removed or renamed on disk since the collection was last indexed.
/// Sets `active = 0` rather than deleting the row: every read path already
/// filters `WHERE active = 1`, so this alone makes the document unreachable,
/// while leaving its `content`/`content_vectors`/HNSW vids in place (usearch
/// has no API to remove a single vector from the graph). Those vectors are
/// reclaimed on the next `embed --rebuild`.
///
/// Returns the deactivated collection-relative paths so the caller can sweep
/// the matching Tantivy entries — this module has no Tantivy handle.
pub fn deactivate_missing_documents(
    conn: &Connection,
    collection: &str,
    current_paths: &HashSet<String>,
) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT path FROM documents WHERE collection = ?1 AND active = 1")?;
    let existing: Vec<String> = stmt
        .query_map(params![collection], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    let missing: Vec<String> = existing
        .into_iter()
        .filter(|p| !current_paths.contains(p))
        .collect();

    // SQLite caps bound parameters at 999 by default; chunk so the collection
    // param plus each chunk's paths always stays comfortably under that limit.
    const CHUNK_SIZE: usize = 500;
    for chunk in missing.chunks(CHUNK_SIZE) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "UPDATE documents SET active = 0 WHERE collection = ? AND path IN ({placeholders})"
        );
        let bind_values = std::iter::once(collection).chain(chunk.iter().map(String::as_str));
        conn.execute(&sql, params_from_iter(bind_values))?;
    }

    Ok(missing)
}

/// Fully remove everything a collection owns: its `documents` rows (hard
/// delete — unlike `update`'s soft-delete prune, the collection itself is
/// gone, so there is nothing left to reactivate), any `content`/
/// `content_vectors` rows no longer referenced by a document in another
/// collection (content is deduplicated globally by hash, so a shared file
/// must survive), and the `store_collections` row itself.
///
/// Returns the deleted documents' Tantivy filepaths (`"collection/path"`) so
/// the caller can sweep the matching Tantivy entries — this module has no
/// Tantivy handle.
pub fn purge_collection(conn: &Connection, name: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT path FROM documents WHERE collection = ?1")?;
    let filepaths: Vec<String> = stmt
        .query_map(params![name], |row| {
            let path: String = row.get(0)?;
            Ok(format!("{name}/{path}"))
        })?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    conn.execute("DELETE FROM documents WHERE collection = ?1", params![name])?;

    // Content is deduplicated globally by hash, so a hash only becomes
    // reclaimable once no document anywhere (any collection, active or not
    // — soft-deleted rows still count as referencing it) points to it
    // anymore. Sweeping the whole `NOT IN` set in one statement instead of
    // re-checking each formerly-owned hash individually is both fewer round
    // trips and exactly equivalent: a hash this collection didn't touch was
    // already referenced before and stays referenced now.
    conn.execute(
        "DELETE FROM content_vectors WHERE hash NOT IN (SELECT hash FROM documents)",
        [],
    )?;
    conn.execute(
        "DELETE FROM content WHERE hash NOT IN (SELECT hash FROM documents)",
        [],
    )?;

    conn.execute(
        "DELETE FROM store_collections WHERE name = ?1",
        params![name],
    )?;

    Ok(filepaths)
}

/// Remove all content_vectors rows for a collection's documents.
///
/// Called before re-embedding a collection so that fresh HNSW vids (which
/// restart from the current index size) never conflict with stale vid values
/// left behind by a previous interrupted embed run.
pub fn clear_vectors_for_collection(conn: &Connection, collection: &str) -> Result<usize> {
    let n = conn.execute(
        "DELETE FROM content_vectors WHERE hash IN \
         (SELECT hash FROM documents WHERE collection = ?1 AND active = 1)",
        params![collection],
    )?;
    Ok(n)
}

/// Count `content_vectors` rows whose hash has no active document referencing
/// it — orphaned by [`deactivate_missing_documents`]'s soft-delete prune.
/// These vectors are unreachable (every vector→document join requires an
/// active document) but not physically freed from the HNSW file; only
/// `embed --rebuild` reclaims that space. Surfaced by `rqmd doctor` so the
/// accumulation isn't silent.
pub fn count_orphaned_vectors(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM content_vectors \
         WHERE hash NOT IN (SELECT hash FROM documents WHERE active = 1)",
        [],
        |row| row.get(0),
    )
    .context("count orphaned vectors")
}

pub fn rename_collection(conn: &Connection, old: &str, new: &str) -> Result<()> {
    conn.execute(
        "UPDATE store_collections SET name=?2 WHERE name=?1",
        params![old, new],
    )?;
    conn.execute(
        "UPDATE documents SET collection=?2 WHERE collection=?1",
        params![old, new],
    )?;
    Ok(())
}

pub fn set_collection_include(conn: &Connection, name: &str, include: bool) -> Result<()> {
    conn.execute(
        "UPDATE store_collections SET include_by_default=?2 WHERE name=?1",
        params![name, include as i64],
    )?;
    Ok(())
}

pub fn set_collection_update_cmd(conn: &Connection, name: &str, cmd: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE store_collections SET update_command=?2 WHERE name=?1",
        params![name, cmd],
    )?;
    Ok(())
}

// ── Config ────────────────────────────────────────────────────────────────────

pub fn get_config(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM store_config WHERE key=?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
    .context("get config")
}

pub fn set_config(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO store_config(key, value) VALUES (?1, ?2)",
        params![key, value],
    )?;
    Ok(())
}

pub fn delete_config(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM store_config WHERE key=?1", params![key])?;
    Ok(())
}

/// List all `store_config` rows whose key starts with `prefix`, ordered by key.
pub fn list_config_by_prefix(conn: &Connection, prefix: &str) -> Result<Vec<(String, String)>> {
    let mut stmt =
        conn.prepare("SELECT key, value FROM store_config WHERE key LIKE ?1 ORDER BY key")?;
    let like_pattern = format!("{prefix}%");
    let rows = stmt
        .query_map(params![like_pattern], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("list config by prefix")?;
    Ok(rows)
}

/// Build the `store_config` key under which a collection's context is stored.
///
/// This is the canonical format used by `rqmd context add rqmd://<collection>/`.
/// Both `context check` and `get_context_for_collection` must use this function
/// to guarantee they query the same key that `context add` writes.
pub fn collection_context_key(collection: &str) -> String {
    format!("context:rqmd://{collection}/")
}

/// Look up a collection's context string from the store_config table.
///
/// Context is stored by the `rqmd context add` command with the key
/// `context:rqmd://<collection>/`. Returns `None` if no context has been set.
pub fn get_context_for_collection(conn: &Connection, collection: &str) -> Result<Option<String>> {
    // Try the canonical `context:rqmd://<collection>/` key first, then the
    // legacy `context:/` (global context) as a fallback.
    let key = collection_context_key(collection);
    if let Some(v) = get_config(conn, &key)? {
        return Ok(Some(v));
    }
    get_config(conn, "context:/")
}

/// Look up the nearest-ancestor context for a specific document path.
///
/// Walks `rel_path`'s parent directories from deepest to shallowest, checking
/// `context:rqmd://<collection>/<ancestor>/` at each level, and returns the
/// first one found. Falls back to the collection-root context and then the
/// legacy global context via `get_context_for_collection` if no ancestor
/// directory has a context configured.
///
/// `rel_path` is the collection-relative path as stored in `documents.path`
/// (e.g. `"Cloud Engineering/Kubernetes/foo.md"`) — ancestor keys must be
/// written with this same raw casing, since `context add` does not slugify.
pub fn get_context_for_path(
    conn: &Connection,
    collection: &str,
    rel_path: &str,
) -> Result<Option<String>> {
    let mut components: Vec<&str> = rel_path.split('/').collect();
    components.pop(); // drop the filename itself

    while !components.is_empty() {
        let key = format!("context:rqmd://{collection}/{}/", components.join("/"));
        if let Some(v) = get_config(conn, &key)? {
            return Ok(Some(v));
        }
        components.pop();
    }

    get_context_for_collection(conn, collection)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn insert_doc(conn: &Connection, hash: &str, path: &str) {
        upsert_content(conn, hash, "body", "2026-01-01T00:00:00Z").unwrap();
        upsert_document(conn, "col", path, "title", hash, "2026-01-01T00:00:00Z").unwrap();
    }

    /// A docid containing a literal `_` must only match a hash with a literal
    /// `_` at that position, not (pre-fix) any single character — otherwise
    /// `rqmd get "#ab_cd"` could resolve to an unrelated document.
    #[test]
    fn docid_prefix_escapes_underscore_metacharacter() {
        let conn = test_conn();
        insert_doc(&conn, "ab_cd0000", "literal.md");
        insert_doc(&conn, "abXcd0000", "wildcard-victim.md");

        let found = get_document_by_docid_prefix(&conn, "ab_cd")
            .unwrap()
            .expect("literal underscore match");
        assert_eq!(found.path, "literal.md");
    }

    /// Same, but for `%` — pre-fix, `LIKE 'ab%cd%'` would match any hash
    /// starting with "ab" and containing "cd" anywhere after, not just a hash
    /// with a literal `%` character.
    #[test]
    fn docid_prefix_escapes_percent_metacharacter() {
        let conn = test_conn();
        insert_doc(&conn, "ab%cd0000", "literal.md");
        insert_doc(&conn, "abZZZcd00", "wildcard-victim.md");

        let found = get_document_by_docid_prefix(&conn, "ab%cd")
            .unwrap()
            .expect("literal percent match");
        assert_eq!(found.path, "literal.md");
    }

    #[test]
    fn docid_prefix_still_matches_plain_hex_prefix() {
        let conn = test_conn();
        insert_doc(&conn, "abc123deadbeef", "plain.md");

        let found = get_document_by_docid_prefix(&conn, "abc123")
            .unwrap()
            .expect("plain prefix still matches");
        assert_eq!(found.path, "plain.md");
    }
}
