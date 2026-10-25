//! Full-text deletes match one exact path. Two collections can hold paths where
//! one contains the other (`docs/readme.md` in collection `a` and `a/docs/readme.md`
//! in collection `b` are different files), and changing or removing the shorter
//! one must leave the other searchable.

use std::fs;
use std::path::Path;

use predicates::str::contains;
use tempfile::TempDir;

use super::helpers::rqmd;

/// Collection `proj` holds `docs/readme.md` ("proj/docs/readme.md") and
/// collection `docs` holds `readme.md` ("docs/readme.md"): the second path is
/// contained in the first. The second `update` settles the one-time full-text
/// rebuild that follows the first.
fn indexed_pair(index_dir: &Path) -> (TempDir, TempDir) {
    let nested = TempDir::new().expect("tempdir");
    fs::create_dir_all(nested.path().join("docs")).expect("mkdir");
    fs::write(
        nested.path().join("docs/readme.md"),
        "# N\n\nzebra nested\n",
    )
    .expect("write");
    let short = TempDir::new().expect("tempdir");
    fs::write(short.path().join("readme.md"), "# S\n\nzebra short\n").expect("write");
    for (dir, name) in [(&nested, "proj"), (&short, "docs")] {
        rqmd(index_dir)
            .args(["collection", "add"])
            .arg(dir.path())
            .args(["--name", name])
            .assert()
            .success();
    }
    rqmd(index_dir).arg("update").assert().success();
    rqmd(index_dir).arg("update").assert().success();
    (nested, short)
}

/// stdout of `rqmd search <term> -c <collection>`, which must succeed.
fn search(index_dir: &Path, term: &str, collection: &str) -> String {
    let out = rqmd(index_dir)
        .args(["search", term, "-c", collection])
        .output()
        .expect("run search");
    assert!(
        out.status.success(),
        "search {term:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Editing the shorter path re-indexes it and leaves the longer one searchable.
#[test]
fn ac_1_editing_a_path_keeps_a_longer_path_that_contains_it() {
    let index_dir = TempDir::new().expect("tempdir");
    let (_nested, short) = indexed_pair(index_dir.path());
    fs::write(
        short.path().join("readme.md"),
        "# S\n\nzebra short edited\n",
    )
    .expect("edit");

    rqmd(index_dir.path())
        .arg("update")
        .assert()
        .success()
        .stdout(contains("1 updated"));

    let nested = search(index_dir.path(), "nested", "proj");
    assert!(nested.contains("readme.md"), "{nested}");
    let edited = search(index_dir.path(), "edited", "docs");
    assert!(edited.contains("readme.md"), "{edited}");
}

/// Removing the shorter file from disk drops its entry only.
#[test]
fn ac_2_removing_a_path_keeps_a_longer_path_that_contains_it() {
    let index_dir = TempDir::new().expect("tempdir");
    let (_nested, short) = indexed_pair(index_dir.path());
    fs::write(short.path().join("other.md"), "# O\n\nquokka\n").expect("write");
    fs::remove_file(short.path().join("readme.md")).expect("remove");

    rqmd(index_dir.path()).arg("update").assert().success();

    let nested = search(index_dir.path(), "nested", "proj");
    assert!(nested.contains("readme.md"), "{nested}");
    let gone = search(index_dir.path(), "short", "docs");
    assert!(!gone.contains("readme.md"), "{gone}");
}

/// Removing the collection that owns the shorter path leaves the other
/// collection searchable.
#[test]
fn ac_3_removing_a_collection_keeps_a_longer_path_in_another() {
    let index_dir = TempDir::new().expect("tempdir");
    let (_nested, _short) = indexed_pair(index_dir.path());

    rqmd(index_dir.path())
        .args(["collection", "remove", "docs"])
        .assert()
        .success();

    let nested = search(index_dir.path(), "nested", "proj");
    assert!(nested.contains("readme.md"), "{nested}");
}

/// An edit still replaces the path's own entry: the old text stops matching
/// and the new text matches.
#[test]
fn ac_4_editing_a_path_replaces_its_own_entry() {
    let index_dir = TempDir::new().expect("tempdir");
    let (_nested, short) = indexed_pair(index_dir.path());
    fs::write(short.path().join("readme.md"), "# S\n\nquokka replaced\n").expect("edit");

    rqmd(index_dir.path()).arg("update").assert().success();

    let old = search(index_dir.path(), "short", "docs");
    assert!(!old.contains("readme.md"), "{old}");
    let new = search(index_dir.path(), "quokka", "docs");
    assert!(new.contains("readme.md"), "{new}");
}
