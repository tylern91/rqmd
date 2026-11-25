use std::path::Path;

use predicates::prelude::*;
use predicates::str::contains;
use tempfile::TempDir;

use super::helpers::rqmd;

fn add(index_dir: &Path, dir: &Path, name: &str) -> assert_cmd::Command {
    let mut cmd = rqmd(index_dir);
    cmd.args(["collection", "add"])
        .arg(dir)
        .args(["--name", name]);
    cmd
}

/// Adding a root inside an existing collection still succeeds, but warns and
/// names the covering collection; a sibling directory stays silent.
#[test]
fn ac_1_collection_add_warns_about_a_nested_root_and_not_a_sibling() {
    let index_dir = TempDir::new().expect("tempdir");
    let tree = TempDir::new().expect("tempdir");
    for sub in ["docs/sub", "docs2"] {
        std::fs::create_dir_all(tree.path().join(sub)).expect("mkdir");
        std::fs::write(tree.path().join(sub).join("note.md"), "body\n").expect("write");
    }

    add(index_dir.path(), &tree.path().join("docs"), "docs")
        .assert()
        .success();
    add(index_dir.path(), &tree.path().join("docs/sub"), "inner")
        .assert()
        .success()
        .stderr(contains("WARN").and(contains("collection 'docs'")));
    add(index_dir.path(), &tree.path().join("docs2"), "sibling")
        .assert()
        .success()
        .stderr(contains("indexed twice").not());
}
