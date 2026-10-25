use std::path::Path;

use predicates::str::contains;
use tempfile::TempDir;

use super::helpers::{fixture_collection, refute_vacuous_output, rqmd};

fn add_collection(index_dir: &Path, dir: &Path, name: &str) {
    rqmd(index_dir)
        .args(["collection", "add"])
        .arg(dir)
        .args(["--name", name])
        .assert()
        .success();
}

/// stdout of `rqmd search <args>`, which must succeed.
fn search_stdout(index_dir: &Path, args: &[&str]) -> String {
    let out = rqmd(index_dir)
        .arg("search")
        .args(args)
        .output()
        .expect("run search");
    assert!(
        out.status.success(),
        "search {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Rewrite the way an index looked after a rename made by an earlier release:
/// the SQLite rows move, the full-text entries keep the old collection name.
fn rename_rows_only(index_dir: &Path, old: &str, new: &str) {
    let conn = rusqlite::Connection::open(index_dir.join("index.sqlite")).expect("open sqlite");
    conn.execute(
        "UPDATE store_collections SET name = ?2 WHERE name = ?1",
        [old, new],
    )
    .expect("rename collection row");
    conn.execute(
        "UPDATE documents SET collection = ?2 WHERE collection = ?1",
        [old, new],
    )
    .expect("rename document rows");
}

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

/// After a rename, keyword search scoped to the new name finds the collection's
/// documents and the old name no longer matches anything.
#[test]
fn ac_3_rename_keeps_scoped_keyword_search_working() {
    let index_dir = TempDir::new().expect("tempdir");
    let fx = fixture_collection("a", "zebra crossing notes\n");
    add_collection(index_dir.path(), &fx.path, "a");
    rqmd(index_dir.path()).arg("update").assert().success();

    let renamed = rqmd(index_dir.path())
        .args(["collection", "rename", "a", "b"])
        .output()
        .expect("run collection rename");
    assert!(renamed.status.success(), "rename failed: {renamed:?}");

    let scoped_new = search_stdout(index_dir.path(), &["zebra", "-c", "b"]);
    assert!(
        scoped_new.contains("doc.md"),
        "scoped search under the new name found nothing: {scoped_new}"
    );
    let scoped_old = search_stdout(index_dir.path(), &["zebra", "-c", "a"]);
    assert!(
        !scoped_old.contains("doc.md"),
        "old name still matches: {scoped_old}"
    );

    let stdout = String::from_utf8_lossy(&renamed.stdout);
    assert!(
        stdout.contains("Renamed 'a' → 'b' (1 document"),
        "missing document count: {stdout}"
    );
}

/// A rename carries the collection's path contexts to the new name.
#[test]
fn ac_4_rename_moves_path_contexts() {
    let index_dir = TempDir::new().expect("tempdir");
    let fx = fixture_collection("a", "context carrier\n");
    add_collection(index_dir.path(), &fx.path, "a");
    rqmd(index_dir.path())
        .args(["context", "add", "rqmd://a/", "notes about a"])
        .assert()
        .success();

    rqmd(index_dir.path())
        .args(["collection", "rename", "a", "b"])
        .assert()
        .success();

    let out = rqmd(index_dir.path())
        .args(["context", "list"])
        .output()
        .expect("run context list");
    refute_vacuous_output(&out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("rqmd://b/") && stdout.contains("notes about a"),
        "context did not follow the rename: {stdout}"
    );
    assert!(
        !stdout.contains("rqmd://a/"),
        "context left under the old name: {stdout}"
    );
}

/// A rename also removes full-text entries that an earlier rename left under a
/// name that no collection uses any more, so a document is listed once.
#[test]
fn ac_5_rename_sweeps_stale_entries_from_an_earlier_rename() {
    let index_dir = TempDir::new().expect("tempdir");
    let fx = fixture_collection("a", "zebra crossing notes\n");
    add_collection(index_dir.path(), &fx.path, "a");
    rqmd(index_dir.path()).arg("update").assert().success();

    rename_rows_only(index_dir.path(), "a", "b");
    // Editing the file writes a second entry under the new name.
    std::fs::write(
        fx.path.join("doc.md"),
        "zebra crossing notes, edited and longer\n",
    )
    .expect("edit fixture doc");
    rqmd(index_dir.path()).arg("update").assert().success();
    let before = search_stdout(index_dir.path(), &["zebra"]);
    assert_eq!(
        before.matches("doc.md").count(),
        2,
        "premise: the earlier rename should leave a duplicate entry: {before}"
    );

    rqmd(index_dir.path())
        .args(["collection", "rename", "b", "c"])
        .assert()
        .success();

    let after = search_stdout(index_dir.path(), &["zebra"]);
    assert_eq!(
        after.matches("doc.md").count(),
        1,
        "stale entry survived the sweep: {after}"
    );
    let scoped = search_stdout(index_dir.path(), &["zebra", "-c", "c"]);
    assert!(scoped.contains("doc.md"), "scoped search empty: {scoped}");
}

/// Removing a collection deletes its path contexts, so a later collection with
/// the same name does not inherit them. Another collection's contexts stay.
#[test]
fn ac_6_remove_deletes_the_collections_path_contexts() {
    let index_dir = TempDir::new().expect("tempdir");
    let a = fixture_collection("a", "alpha body\n");
    let b = fixture_collection("b", "beta body\n");
    add_collection(index_dir.path(), &a.path, "a");
    add_collection(index_dir.path(), &b.path, "b");
    for (path, text) in [
        ("rqmd://a/", "root of a"),
        ("rqmd://a/sub/", "sub of a"),
        ("rqmd://b/", "root of b"),
    ] {
        rqmd(index_dir.path())
            .args(["context", "add", path, text])
            .assert()
            .success();
    }

    rqmd(index_dir.path())
        .args(["collection", "remove", "a"])
        .assert()
        .success();

    let out = rqmd(index_dir.path())
        .args(["context", "list"])
        .output()
        .expect("run context list");
    refute_vacuous_output(&out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("rqmd://a/"),
        "contexts of the removed collection survived: {stdout}"
    );
    assert!(
        stdout.contains("rqmd://b/"),
        "another collection's context was removed: {stdout}"
    );
}

/// `collection add` rejects an empty name and a name containing '/', with the
/// reason in the error, and registers nothing.
#[test]
fn ac_7_add_rejects_empty_and_slash_names() {
    let index_dir = TempDir::new().expect("tempdir");
    let fx = fixture_collection("a", "body\n");

    for (name, reason) in [("", "must not be empty"), ("x/y", "must not contain '/'")] {
        rqmd(index_dir.path())
            .args(["collection", "add"])
            .arg(&fx.path)
            .args(["--name", name])
            .assert()
            .failure()
            .stderr(contains("invalid collection name"))
            .stderr(contains(reason));
    }

    // A valid name still registers, and nothing leaked from the rejected tries.
    add_collection(index_dir.path(), &fx.path, "ok");
    let out = rqmd(index_dir.path())
        .args(["collection", "list"])
        .output()
        .expect("run collection list");
    refute_vacuous_output(&out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout
            .lines()
            .filter(|l| l.starts_with(|c: char| c.is_alphanumeric()) && !l.starts_with("NAME"))
            .count()
            == 1,
        "expected exactly one collection: {stdout}"
    );
}

/// `collection rename` rejects the same invalid names and leaves the
/// collection under its old name.
#[test]
fn ac_8_rename_rejects_empty_and_slash_names() {
    let index_dir = TempDir::new().expect("tempdir");
    let fx = fixture_collection("a", "zebra body\n");
    add_collection(index_dir.path(), &fx.path, "a");
    rqmd(index_dir.path()).arg("update").assert().success();

    for (name, reason) in [("", "must not be empty"), ("x/y", "must not contain '/'")] {
        rqmd(index_dir.path())
            .args(["collection", "rename", "a", name])
            .assert()
            .failure()
            .stderr(contains("invalid collection name"))
            .stderr(contains(reason));
    }

    rqmd(index_dir.path())
        .args(["collection", "show", "a"])
        .assert()
        .success()
        .stdout(contains("Collection: a"));
}

/// A rename into a name that is already taken fails and changes nothing.
#[test]
fn ac_9_rename_into_an_existing_name_changes_nothing() {
    let index_dir = TempDir::new().expect("tempdir");
    let a = fixture_collection("a", "zebra body\n");
    let b = fixture_collection("b", "walrus body\n");
    add_collection(index_dir.path(), &a.path, "a");
    add_collection(index_dir.path(), &b.path, "b");
    rqmd(index_dir.path()).arg("update").assert().success();

    rqmd(index_dir.path())
        .args(["collection", "rename", "a", "b"])
        .assert()
        .failure()
        .stderr(contains("already exists"));

    assert!(search_stdout(index_dir.path(), &["zebra", "-c", "a"]).contains("doc.md"));
    assert!(search_stdout(index_dir.path(), &["walrus", "-c", "b"]).contains("doc.md"));
}

/// Re-adding an existing collection keeps its update hook and its
/// include-by-default setting while taking the walk settings from the new flags.
#[test]
fn ac_10_readd_keeps_hook_and_include_setting() {
    let index_dir = TempDir::new().expect("tempdir");
    let fx = fixture_collection("a", "body\n");
    add_collection(index_dir.path(), &fx.path, "a");
    rqmd(index_dir.path())
        .args(["collection", "update-cmd", "a", "echo refreshed"])
        .assert()
        .success();
    rqmd(index_dir.path())
        .args(["collection", "exclude", "a"])
        .assert()
        .success();

    rqmd(index_dir.path())
        .args(["collection", "add"])
        .arg(&fx.path)
        .args(["--name", "a", "--mask", "**/*.txt"])
        .assert()
        .success()
        .stderr(contains("Updating existing collection 'a'"));

    rqmd(index_dir.path())
        .args(["collection", "show", "a"])
        .assert()
        .success()
        .stdout(contains("Hook:     echo refreshed"))
        .stdout(contains("Included: false"))
        .stdout(contains("Pattern:  **/*.txt"));
}
