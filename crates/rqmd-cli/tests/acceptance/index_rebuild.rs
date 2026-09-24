use predicates::prelude::*;
use predicates::str::contains;
use tempfile::TempDir;

use super::helpers::{fixture_collection, refute_vacuous_output, rqmd};

/// `embed -c <collection> --rebuild` must scope its eviction to the named
/// collection — the bug fixed by #66. Rebuilding `a` must not corrupt vector
/// search for `b`.
#[test]
fn ac_1_scoped_rebuild_preserves_other_collections_vectors() {
    let index_dir = TempDir::new().expect("tempdir");
    let a = fixture_collection("a", "the quick brown fox jumps\n");
    let b = fixture_collection("b", "a distinctive term xylophone appears here\n");

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
    rqmd(index_dir.path()).arg("embed").assert().success();

    let baseline = rqmd(index_dir.path())
        .args(["query", "xylophone"])
        .output()
        .expect("run baseline query");
    refute_vacuous_output(&baseline);
    let baseline_stdout = String::from_utf8_lossy(&baseline.stdout);
    assert!(
        baseline_stdout.contains("xylophone"),
        "baseline query for 'xylophone' should hit collection b: {baseline_stdout}"
    );

    rqmd(index_dir.path())
        .args(["embed", "-c", "a", "--rebuild"])
        .assert()
        .success();

    let after = rqmd(index_dir.path())
        .args(["query", "xylophone"])
        .output()
        .expect("run post-rebuild query");
    refute_vacuous_output(&after);
    let after_stdout = String::from_utf8_lossy(&after.stdout);
    assert!(
        after_stdout.contains("xylophone"),
        "collection b must still be searchable after 'a' is rebuilt-scoped: {after_stdout}"
    );

    // `query` is hybrid (BM25 + vector), so a keyword hit alone can't tell us
    // the vector index specifically survived — BM25 would find "xylophone"
    // even with a fully corrupted HNSW file. `vsearch` is vector-only, so
    // this is the assertion that actually pins the #66 regression.
    let after_vsearch = rqmd(index_dir.path())
        .args(["vsearch", "xylophone"])
        .output()
        .expect("run post-rebuild vsearch");
    refute_vacuous_output(&after_vsearch);
    let after_vsearch_stdout = String::from_utf8_lossy(&after_vsearch.stdout);
    assert!(
        after_vsearch_stdout.contains("xylophone"),
        "collection b's vectors must survive a scoped rebuild of 'a': {after_vsearch_stdout}"
    );

    rqmd(index_dir.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(contains("orphaned vector").not());
}
