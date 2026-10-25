use predicates::str::contains;
use rusqlite::Connection;
use tempfile::TempDir;

use super::helpers::{fixture_collection, rqmd};

fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("table_info");
    stmt.query_map([], |row| row.get::<_, String>(1))
        .expect("query columns")
        .any(|name| name.expect("column name") == column)
}

fn has_prev_hash_column(conn: &Connection) -> bool {
    has_column(conn, "documents", "prev_hash")
}

/// An index written before `documents.prev_hash` existed opens, updates and
/// reports on the new binary, and gains the column in place: its document rows
/// keep their ids instead of being rebuilt.
#[test]
fn ac_1_index_from_the_previous_release_gains_prev_hash_in_place() {
    let index_dir = TempDir::new().expect("tempdir");
    let fx = fixture_collection("a", "first body\n");
    rqmd(index_dir.path())
        .args(["collection", "add"])
        .arg(&fx.path)
        .args(["--name", "a"])
        .assert()
        .success();
    rqmd(index_dir.path()).arg("update").assert().success();

    let db_path = index_dir.path().join("index.sqlite");
    let id_before: i64 = {
        let conn = Connection::open(&db_path).expect("open index");
        assert!(has_prev_hash_column(&conn), "fresh index has the column");
        conn.execute("ALTER TABLE documents DROP COLUMN prev_hash", [])
            .expect("downgrade schema");
        assert!(!has_prev_hash_column(&conn));
        conn.query_row("SELECT id FROM documents", [], |r| r.get(0))
            .expect("document id")
    };

    std::fs::write(fx.path.join("doc.md"), "second body, edited\n").expect("edit note");
    rqmd(index_dir.path())
        .arg("update")
        .assert()
        .success()
        .stdout(contains("1 updated"));
    rqmd(index_dir.path()).arg("doctor").assert().success();

    let conn = Connection::open(&db_path).expect("reopen index");
    assert!(has_prev_hash_column(&conn), "the column is added on open");
    let (id_after, rows): (i64, i64) = conn
        .query_row("SELECT MIN(id), COUNT(*) FROM documents", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .expect("documents");
    assert_eq!(
        (id_after, rows),
        (id_before, 1),
        "rows are kept, not rebuilt"
    );
}

/// An index written before `content_vectors.vector_key` existed opens, updates
/// and reports on the new binary, and gains the column in place: its vector
/// rows are kept as they were, with no key until `rqmd embed` fills it in.
#[test]
fn ac_2_index_from_the_previous_release_gains_vector_key_in_place() {
    let index_dir = TempDir::new().expect("tempdir");
    let fx = fixture_collection("a", "first body\n");
    rqmd(index_dir.path())
        .args(["collection", "add"])
        .arg(&fx.path)
        .args(["--name", "a"])
        .assert()
        .success();
    rqmd(index_dir.path()).arg("update").assert().success();

    let db_path = index_dir.path().join("index.sqlite");
    {
        let conn = Connection::open(&db_path).expect("open index");
        assert!(has_column(&conn, "content_vectors", "vector_key"));
        conn.execute(
            "INSERT INTO content_vectors(hash, seq, pos, model, embed_fingerprint, \
                 total_chunks, embedded_at, vid, chunk_hash) \
             SELECT hash, 0, 0, 'old-model', 'old-fingerprint', 1, \
                 '2026-01-01T00:00:00Z', 0, 'old-chunk-hash' FROM documents",
            [],
        )
        .expect("seed a vector row");
        conn.execute("DROP INDEX idx_content_vectors_vector_key", [])
            .expect("drop index");
        conn.execute("ALTER TABLE content_vectors DROP COLUMN vector_key", [])
            .expect("downgrade schema");
        assert!(!has_column(&conn, "content_vectors", "vector_key"));
    }

    rqmd(index_dir.path())
        .arg("update")
        .assert()
        .success()
        .stdout(contains("Updating 1 collection(s)"));
    rqmd(index_dir.path()).arg("status").assert().success();

    let conn = Connection::open(&db_path).expect("reopen index");
    assert!(
        has_column(&conn, "content_vectors", "vector_key"),
        "the column is added on open"
    );
    let (rows, chunk_hash, key): (i64, String, Option<String>) = conn
        .query_row(
            "SELECT COUNT(*), MIN(chunk_hash), MIN(vector_key) FROM content_vectors",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("vector rows");
    assert_eq!(
        (rows, chunk_hash.as_str(), key),
        (1, "old-chunk-hash", None)
    );
}
