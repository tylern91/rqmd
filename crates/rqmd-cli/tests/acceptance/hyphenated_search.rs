//! A hyphenated term in a document is matched as the adjacent phrase of its
//! parts, so the whole term and each part find the document.

use std::fs;

use predicates::str::contains;
use tempfile::TempDir;

use super::helpers::rqmd;

/// Searching `foo-bar` and `foo` both find a document that contains `foo-bar`.
#[test]
fn ac_2_whole_hyphenated_term_and_its_first_part_find_the_document() {
    let index_dir = TempDir::new().expect("tempdir");
    let docs = TempDir::new().expect("tempdir");
    fs::write(
        docs.path().join("hyphen.md"),
        "# Notes\n\na foo-bar widget\n",
    )
    .expect("write");
    rqmd(index_dir.path())
        .args(["collection", "add"])
        .arg(docs.path())
        .args(["--name", "notes"])
        .assert()
        .success();
    rqmd(index_dir.path()).arg("update").assert().success();

    for term in ["foo-bar", "foo"] {
        rqmd(index_dir.path())
            .args(["search", term])
            .assert()
            .success()
            .stdout(contains("hyphen.md"));
    }
}
