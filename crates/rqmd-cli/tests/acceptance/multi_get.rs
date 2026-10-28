//! `rqmd multi-get` entry precedence, reporting and large patterns
//! (github.com/tylern91/rqmd#154).

use std::fs;
use std::path::Path;

use predicates::prelude::*;
use predicates::str::contains;
use tempfile::TempDir;

use super::helpers::{Fixture, fixture_collection, refute_vacuous_output, rqmd};

/// Collection `a` holding `doc.md`, `guide/doc.md` and `other.md`, indexed.
fn indexed_collection(index_dir: &Path) -> Fixture {
    let fx = fixture_collection("a", "top level doc\n");
    fs::create_dir_all(fx.path.join("guide")).expect("mkdir guide");
    fs::write(fx.path.join("guide/doc.md"), "nested doc\n").expect("write nested");
    fs::write(fx.path.join("other.md"), "other doc\n").expect("write other");
    rqmd(index_dir)
        .args(["collection", "add"])
        .arg(&fx.path)
        .args(["--name", "a"])
        .assert()
        .success();
    rqmd(index_dir)
        .arg("update")
        .assert()
        .success()
        .stdout(contains("Indexed: 0 new, 0 updated, 3 unchanged"));
    fx
}

/// A pattern of more than 1,000 path entries succeeds and prints the same
/// documents as the entries that match, issued on their own.
#[test]
fn ac_2_more_than_a_thousand_path_entries_succeed() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fx = indexed_collection(index_dir.path());

    let mut entries: Vec<String> = (0..1_100).map(|i| format!("missing-{i}.md")).collect();
    entries.push("other.md".into());
    entries.push("a/guide/doc.md".into());

    let big = rqmd(index_dir.path())
        .args(["multi-get", &entries.join(","), "--format", "files"])
        .assert()
        .success()
        .get_output()
        .clone();
    refute_vacuous_output(&big);

    let small = rqmd(index_dir.path())
        .args(["multi-get", "other.md,a/guide/doc.md", "--format", "files"])
        .assert()
        .success()
        .get_output()
        .clone();
    assert_eq!(
        String::from_utf8_lossy(&big.stdout),
        String::from_utf8_lossy(&small.stdout)
    );
    assert_eq!(String::from_utf8_lossy(&big.stdout).lines().count(), 2);
}

/// An unmatched entry and a path entry matching several documents are each
/// reported on stderr; every match is still printed and the exit code is 0.
#[test]
fn ac_4_unmatched_and_ambiguous_entries_are_reported_on_stderr() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fx = indexed_collection(index_dir.path());

    let out = rqmd(index_dir.path())
        .args(["multi-get", "doc.md, missing.md, nope/*.md"])
        .assert()
        .success()
        .stdout(contains("rqmd://a/doc.md"))
        .stdout(contains("rqmd://a/guide/doc.md"))
        .stderr(contains(
            r#"rqmd multi-get: no document matched "missing.md""#,
        ))
        .stderr(contains(
            r#"rqmd multi-get: no document matched "nope/*.md""#,
        ))
        .stderr(contains(
            r#"rqmd multi-get: "doc.md" matched 2 documents; all are returned"#,
        ))
        .get_output()
        .clone();
    refute_vacuous_output(&out);
}

/// stdout for a pattern that works today is unchanged by unmatched entries
/// mixed into it, and a pattern with nothing to report prints nothing to
/// stderr.
#[test]
fn ac_5_stdout_is_unchanged_by_reported_entries() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fx = indexed_collection(index_dir.path());

    let clean = rqmd(index_dir.path())
        .args(["multi-get", "other.md,a/guide/*"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .clone();
    refute_vacuous_output(&clean);

    let noisy = rqmd(index_dir.path())
        .args([
            "multi-get",
            "missing.md,other.md,#zzzzzz,a/guide/*,guide/doc.md",
        ])
        .assert()
        .success()
        .stderr(contains("missing.md"))
        .get_output()
        .clone();
    assert_eq!(
        String::from_utf8_lossy(&clean.stdout),
        String::from_utf8_lossy(&noisy.stdout)
    );
}
