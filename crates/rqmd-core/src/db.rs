//! rusqlite schema and CRUD layer.
//!
//! Schema mirrors qmd's TypeScript store exactly (same table/column names) so
//! existing indexes remain readable. The FTS5 virtual table and vectors_vec
//! extension are NOT created here — Tantivy and usearch replace them.

use anyhow::{Context, Result};
use hex;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::Path;

use crate::types::{Collection, Document};

// ── Schema init ───────────────────────────────────────────────────────────────

/// Open (or create) the SQLite database and ensure schema is current.
pub fn open_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).context("open sqlite db")?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
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
            context            TEXT
        );

        CREATE TABLE IF NOT EXISTS store_config (
            key   TEXT PRIMARY KEY,
            value TEXT
        );
    "#,
    )?;
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

/// Insert or update a document record. Returns the rowid.
pub fn upsert_document(
    conn: &Connection,
    collection: &str,
    path: &str,
    title: &str,
    hash: &str,
    now: &str,
) -> Result<i64> {
    conn.execute(
        r#"
        INSERT INTO documents(collection, path, title, hash, created_at, modified_at, active)
        VALUES (?1, ?2, ?3, ?4, ?5, ?5, 1)
        ON CONFLICT(collection, path) DO UPDATE SET
            title       = excluded.title,
            hash        = excluded.hash,
            modified_at = excluded.modified_at,
            active      = 1
        "#,
        params![collection, path, title, hash, now],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_document_by_filepath(
    conn: &Connection,
    collection: &str,
    path: &str,
) -> Result<Option<Document>> {
    conn.query_row(
        "SELECT id, collection, path, title, hash, active FROM documents WHERE collection=?1 AND path=?2",
        params![collection, path],
        |row| {
            Ok(Document {
                id: row.get(0)?,
                collection: row.get(1)?,
                path: row.get(2)?,
                title: row.get(3)?,
                hash: row.get(4)?,
                active: row.get::<_, i64>(5)? != 0,
            })
        },
    )
    .optional()
    .context("get document")
}

pub fn get_document_by_id(conn: &Connection, id: i64) -> Result<Option<Document>> {
    conn.query_row(
        "SELECT id, collection, path, title, hash, active FROM documents WHERE id=?1",
        params![id],
        |row| {
            Ok(Document {
                id: row.get(0)?,
                collection: row.get(1)?,
                path: row.get(2)?,
                title: row.get(3)?,
                hash: row.get(4)?,
                active: row.get::<_, i64>(5)? != 0,
            })
        },
    )
    .optional()
    .context("get document by id")
}

/// Look up a document by the first 6 hex chars of its content hash (the docid).
pub fn get_document_by_docid_prefix(conn: &Connection, docid: &str) -> Result<Option<Document>> {
    let pattern = format!("{docid}%");
    conn.query_row(
        "SELECT id, collection, path, title, hash, active FROM documents WHERE hash LIKE ?1 AND active=1 LIMIT 1",
        params![pattern],
        |row| {
            Ok(Document {
                id: row.get(0)?,
                collection: row.get(1)?,
                path: row.get(2)?,
                title: row.get(3)?,
                hash: row.get(4)?,
                active: row.get::<_, i64>(5)? != 0,
            })
        },
    )
    .optional()
    .context("get document by docid")
}

pub fn list_documents(conn: &Connection, collection: Option<&str>) -> Result<Vec<Document>> {
    let (sql, collection_val) = match collection {
        Some(c) => (
            "SELECT id, collection, path, title, hash, active FROM documents WHERE collection=?1 AND active=1 ORDER BY path",
            Some(c.to_string()),
        ),
        None => (
            "SELECT id, collection, path, title, hash, active FROM documents WHERE active=1 ORDER BY collection, path",
            None,
        ),
    };

    let mut stmt = conn.prepare(sql)?;
    let rows: Vec<Document> = if let Some(cname) = collection_val {
        stmt.query_map(params![cname], |row| {
            Ok(Document {
                id: row.get(0)?,
                collection: row.get(1)?,
                path: row.get(2)?,
                title: row.get(3)?,
                hash: row.get(4)?,
                active: row.get::<_, i64>(5)? != 0,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    } else {
        stmt.query_map([], |row| {
            Ok(Document {
                id: row.get(0)?,
                collection: row.get(1)?,
                path: row.get(2)?,
                title: row.get(3)?,
                hash: row.get(4)?,
                active: row.get::<_, i64>(5)? != 0,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    };
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
        |row| {
            Ok((
                Document {
                    id: row.get(0)?,
                    collection: row.get(1)?,
                    path: row.get(2)?,
                    title: row.get(3)?,
                    hash: row.get(4)?,
                    active: row.get::<_, i64>(5)? != 0,
                },
                row.get::<_, String>(6)?,
            ))
        },
    )
    .optional()
    .context("doc_for_vid")
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
        INSERT INTO store_collections(name, path, pattern, ignore_patterns, include_by_default, update_command)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ON CONFLICT(name) DO UPDATE SET
            path               = excluded.path,
            pattern            = excluded.pattern,
            ignore_patterns    = excluded.ignore_patterns,
            include_by_default = excluded.include_by_default,
            update_command     = excluded.update_command
        "#,
        params![
            c.name,
            c.path,
            c.pattern,
            ignore,
            c.include_by_default as i64,
            c.update_command,
        ],
    )?;
    Ok(())
}

pub fn list_collections(conn: &Connection) -> Result<Vec<Collection>> {
    let mut stmt = conn.prepare("SELECT name, path, pattern, ignore_patterns, include_by_default, update_command FROM store_collections")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    rows.into_iter()
        .map(|(name, path, pattern, ignore_json, include, update)| {
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
            })
        })
        .collect()
}

pub fn delete_collection(conn: &Connection, name: &str) -> Result<()> {
    conn.execute("DELETE FROM store_collections WHERE name=?1", params![name])?;
    Ok(())
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
