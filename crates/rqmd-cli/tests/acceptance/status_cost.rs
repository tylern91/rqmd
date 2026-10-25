use std::fs;
use std::path::Path;

use predicates::str::{contains, is_match};
use tempfile::TempDir;

use super::helpers::{fixture_collection, refute_vacuous_output, rqmd};

/// A collection holding an empty file, a multi-byte file and a body that spans
/// many database pages, indexed without any embedding.
fn indexed_mixed_bodies(index_dir: &Path) -> super::helpers::Fixture {
    let fx = fixture_collection("mixed", "# Plain\n\nshort body\n");
    fs::write(fx.path.join("empty.md"), "").expect("write empty doc");
    fs::write(fx.path.join("accent.md"), "\u{e9}\n").expect("write multibyte doc");
    fs::write(fx.path.join("big.md"), "word ".repeat(60_000)).expect("write large doc");
    rqmd(index_dir)
        .args(["collection", "add"])
        .arg(&fx.path)
        .args(["--name", "mixed"])
        .assert()
        .success();
    rqmd(index_dir).arg("update").assert().success();
    fx
}

/// `status` counts a document as pending embed by its stored body size: an
/// empty file never is, a one-character, multi-byte or multi-page body is.
#[test]
fn ac_1_status_pending_ignores_empty_bodies_and_counts_the_rest() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fx = indexed_mixed_bodies(index_dir.path());

    let out = rqmd(index_dir.path())
        .arg("status")
        .assert()
        .success()
        .stdout(contains("Pending:  3 need embedding"))
        .get_output()
        .clone();
    refute_vacuous_output(&out);
}

/// `doctor` and `collection list` report every active document of a collection,
/// whatever the size of its body.
#[test]
fn ac_2_per_collection_document_counts_do_not_depend_on_body_size() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fx = indexed_mixed_bodies(index_dir.path());

    rqmd(index_dir.path())
        .args(["collection", "list"])
        .assert()
        .success()
        .stdout(is_match(r"mixed\s+4\s").expect("regex"));
    rqmd(index_dir.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(contains("mixed — 4 docs at"));
    rqmd(index_dir.path())
        .args(["collection", "show", "mixed"])
        .assert()
        .success()
        .stdout(contains("  Docs:     4"));
}
