use predicates::str::contains;
use rusqlite::Connection;
use tempfile::TempDir;

use super::helpers::{fixture_collection, rqmd};

fn has_prev_hash_column(conn: &Connection) -> bool {
    let mut stmt = conn
        .prepare("PRAGMA table_info(documents)")
        .expect("table_info");
    stmt.query_map([], |row| row.get::<_, String>(1))
        .expect("query columns")
        .any(|name| name.expect("column name") == "prev_hash")
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
