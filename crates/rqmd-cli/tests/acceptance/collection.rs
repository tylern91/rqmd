use predicates::str::contains;
use tempfile::TempDir;

use super::helpers::{fixture_collection, refute_vacuous_output, rqmd};

/// Adding a collection and listing it back proves the round trip through
/// `collection add` -> sqlite -> `collection list` works end-to-end.
#[test]
fn ac_1_collection_add_then_list_shows_the_new_collection() {
    let index_dir = TempDir::new().expect("tempdir");
    let a = fixture_collection("a", "hello from collection a\n");

    rqmd(index_dir.path())
        .args(["collection", "add"])
        .arg(&a.path)
        .args(["--name", "a"])
        .assert()
        .success();

    let out = rqmd(index_dir.path())
        .args(["collection", "list"])
        .output()
        .expect("run collection list");
    refute_vacuous_output(&out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("NAME"), "missing header row: {stdout}");
    assert!(
        stdout.lines().any(|l| l.starts_with('a')),
        "missing collection 'a': {stdout}"
    );
}

/// Removing a collection must not delete a content hash still actively
/// referenced by another collection — the SQL-layer bug fixed by #66.
#[test]
fn ac_2_collection_remove_keeps_a_hash_shared_with_another_collection() {
    let index_dir = TempDir::new().expect("tempdir");
    let shared_body = "identical content shared by two collections\n";
    let a = fixture_collection("a", shared_body);
    let b = fixture_collection("b", shared_body);

    rqmd(index_dir.path())
        .args(["collection", "add"])
        .arg(&a.path)
        .args(["--name", "a"])
        .assert()
        .success();
    rqmd(index_dir.path())
        .args(["collection", "add"])
        .arg(&b.path)
        .args(["--name", "b"])
        .assert()
        .success();

    rqmd(index_dir.path()).arg("update").assert().success();

    rqmd(index_dir.path())
        .args(["collection", "remove", "a"])
        .assert()
        .success()
        .stdout(contains("removed"));

    let out = rqmd(index_dir.path())
        .args(["collection", "list"])
        .output()
        .expect("run collection list");
    refute_vacuous_output(&out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.lines().any(|l| l.starts_with('a')),
        "collection 'a' should be gone: {stdout}"
    );
    assert!(
        stdout.lines().any(|l| l.starts_with('b')),
        "collection 'b' should survive: {stdout}"
    );
    // 'b' still has its document — the shared hash's row was not purged.
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with('b') && !l.contains("  0  ")),
        "collection 'b' lost its document count: {stdout}"
    );
}
