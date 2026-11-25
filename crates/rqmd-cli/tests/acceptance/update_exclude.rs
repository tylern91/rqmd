use std::fs;

use predicates::prelude::*;
use predicates::str::contains;
use tempfile::TempDir;

use super::helpers::{Fixture, fixture_collection, rqmd};

fn add(index_dir: &std::path::Path, name: &str, fx: &Fixture) {
    rqmd(index_dir)
        .args(["collection", "add"])
        .arg(&fx.path)
        .args(["--name", name])
        .assert()
        .success();
}

/// Collections `a` and `b`, both indexed once.
fn two_indexed(index_dir: &std::path::Path) -> (Fixture, Fixture) {
    let a = fixture_collection("a", "alpha original\n");
    let b = fixture_collection("b", "bravo original\n");
    add(index_dir, "a", &a);
    add(index_dir, "b", &b);
    rqmd(index_dir).arg("update").assert().success();
    (a, b)
}

/// An excluded collection is not walked, so a change or deletion in it is
/// neither indexed nor pruned, while the other collection is re-indexed.
#[test]
fn ac_1_excluded_collection_is_left_untouched() {
    let index_dir = TempDir::new().expect("tempdir");
    let (a, b) = two_indexed(index_dir.path());
    fs::remove_file(a.path.join("doc.md")).expect("delete a's file");
    fs::write(b.path.join("doc.md"), "bravo edited, now longer\n").expect("edit b");

    rqmd(index_dir.path())
        .args(["update", "--exclude", "a"])
        .assert()
        .success()
        .stdout(contains("Updating 1 collection(s)"))
        .stdout(contains("1 updated"));

    rqmd(index_dir.path())
        .args(["get", "a/doc.md"])
        .assert()
        .success()
        .stdout(contains("alpha original"));
    rqmd(index_dir.path())
        .args(["get", "b/doc.md"])
        .assert()
        .success()
        .stdout(contains("bravo edited"));
}

/// `-c` and `--exclude` name opposite scopes; clap rejects the pair.
#[test]
fn ac_2_collection_and_exclude_conflict() {
    let index_dir = TempDir::new().expect("tempdir");
    two_indexed(index_dir.path());

    rqmd(index_dir.path())
        .args(["update", "-c", "a", "--exclude", "b"])
        .assert()
        .failure()
        .stderr(contains("cannot be used with"));
}

/// A name that matches no collection is an error that names it.
#[test]
fn ac_3_unknown_excluded_collection_is_an_error() {
    let index_dir = TempDir::new().expect("tempdir");
    two_indexed(index_dir.path());

    rqmd(index_dir.path())
        .args(["update", "--exclude", "nosuch"])
        .assert()
        .failure()
        .stderr(contains("nosuch"));
}

/// A fresh index has its full-text rebuild pending; an excluding run, being
/// partial, leaves it pending (the warning stays) and only a full run clears it.
#[test]
fn ac_4_exclude_leaves_the_full_text_rebuild_pending() {
    let index_dir = TempDir::new().expect("tempdir");
    let a = fixture_collection("a", "alpha\n");
    let b = fixture_collection("b", "bravo\n");
    add(index_dir.path(), "a", &a);
    add(index_dir.path(), "b", &b);

    rqmd(index_dir.path())
        .args(["update", "--exclude", "a"])
        .assert()
        .success()
        .stderr(contains("full-text index predates"));

    rqmd(index_dir.path())
        .arg("update")
        .assert()
        .success()
        .stderr(contains("full-text index predates").not());
}
