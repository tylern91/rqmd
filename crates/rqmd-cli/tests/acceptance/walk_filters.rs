use std::fs;
use std::path::Path;

use predicates::prelude::*;
use predicates::str::contains;
use tempfile::TempDir;

use super::helpers::rqmd;

/// A tree with one kept note, one note under an ignored directory and one
/// note matching an ignored file pattern.
fn gitignored_tree() -> TempDir {
    let root = TempDir::new().expect("tempdir");
    fs::write(root.path().join(".gitignore"), "generated/\n*.tmp.md\n").expect("write .gitignore");
    fs::write(root.path().join("keep.md"), "kept note\n").expect("write keep");
    fs::write(root.path().join("scratch.tmp.md"), "scratch note\n").expect("write scratch");
    fs::create_dir_all(root.path().join("generated")).expect("mkdir generated");
    fs::write(root.path().join("generated/out.md"), "generated note\n").expect("write out");
    root
}

fn add(index_dir: &Path, tree: &Path, extra: &[&str]) -> assert_cmd::assert::Assert {
    rqmd(index_dir)
        .args(["collection", "add"])
        .arg(tree)
        .args(["--name", "t"])
        .args(extra)
        .assert()
        .success()
}

/// With `--respect-gitignore` the files a `.gitignore` in the tree matches are
/// not indexed; without it the walk is unchanged and indexes all three.
#[test]
fn ac_1_gitignore_is_honoured_only_when_enabled() {
    let tree = gitignored_tree();

    let on = TempDir::new().expect("tempdir");
    add(on.path(), tree.path(), &["--respect-gitignore"]).stdout(contains("Indexed 1 document(s)"));

    let off = TempDir::new().expect("tempdir");
    add(off.path(), tree.path(), &[]).stdout(contains("Indexed 3 document(s)"));
}

/// A file over `--max-file-bytes` is left out and counted in the summary of
/// both `collection add` and the later `update`.
#[test]
fn ac_2_file_over_the_size_limit_is_skipped_and_reported() {
    let tree = TempDir::new().expect("tempdir");
    fs::write(tree.path().join("small.md"), "tiny\n").expect("write small");
    fs::write(tree.path().join("big.md"), "x".repeat(500)).expect("write big");
    let index_dir = TempDir::new().expect("tempdir");

    add(index_dir.path(), tree.path(), &["--max-file-bytes", "100"])
        .stdout(contains("Indexed 1 document(s)"))
        .stdout(contains("1 over the size limit"));

    rqmd(index_dir.path())
        .arg("update")
        .assert()
        .success()
        .stdout(contains("skipped 1 (1 over the size limit)"));
}

/// A symlink that resolves outside the collection root is still excluded when
/// `.gitignore` handling is on.
#[cfg(unix)]
#[test]
fn ac_3_symlink_escaping_the_root_stays_excluded() {
    let outside = TempDir::new().expect("tempdir");
    fs::write(outside.path().join("secret.md"), "outside the root\n").expect("write secret");
    let tree = TempDir::new().expect("tempdir");
    fs::write(tree.path().join("keep.md"), "kept note\n").expect("write keep");
    std::os::unix::fs::symlink(
        outside.path().join("secret.md"),
        tree.path().join("leak.md"),
    )
    .expect("symlink");
    let index_dir = TempDir::new().expect("tempdir");

    add(index_dir.path(), tree.path(), &["--respect-gitignore"])
        .stdout(contains("Indexed 1 document(s)"));
}

/// A collection added with no new flags indexes everything it did before:
/// gitignored files, and a file larger than the flag-less default of any
/// small fixture, with nothing reported as skipped.
#[test]
fn ac_4_default_collection_indexes_the_same_files_as_before() {
    let tree = gitignored_tree();
    fs::write(tree.path().join("long.md"), "word ".repeat(2000)).expect("write long");
    let index_dir = TempDir::new().expect("tempdir");

    add(index_dir.path(), tree.path(), &[])
        .stdout(contains("Indexed 4 document(s)"))
        .stdout(contains("skipped").not());

    rqmd(index_dir.path())
        .arg("update")
        .assert()
        .success()
        .stdout(contains("skipped").not());
}
