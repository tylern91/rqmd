use std::fs;
use std::path::Path;

use predicates::prelude::*;
use predicates::str::contains;
use rusqlite::Connection;
use tempfile::TempDir;

use super::helpers::rqmd;

/// A tree with two kept notes, one note under `build/` and one under
/// `node_modules/pkg/`, both left out by the built-in folder rule.
fn tree_with_built_in_folders() -> TempDir {
    let root = TempDir::new().expect("tempdir");
    let p = root.path();
    fs::create_dir_all(p.join("notes")).expect("mkdir notes");
    fs::create_dir_all(p.join("build/summaries")).expect("mkdir build");
    fs::create_dir_all(p.join("node_modules/pkg")).expect("mkdir node_modules");
    fs::write(p.join("notes/a.md"), "first note\n").expect("write a");
    fs::write(p.join("b.md"), "second note\n").expect("write b");
    fs::write(p.join("build/summaries/g.md"), "left out\n").expect("write g");
    fs::write(p.join("node_modules/pkg/readme.md"), "left out\n").expect("write readme");
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

/// `collection add` says how many folders the built-in rule left out and names
/// them, relative to the collection root, in the same summary line.
#[test]
fn ac_1_add_reports_the_folders_the_built_in_rule_left_out() {
    let tree = tree_with_built_in_folders();
    let index_dir = TempDir::new().expect("tempdir");

    add(index_dir.path(), tree.path(), &[])
        .stdout(contains("Indexed 2 document(s)"))
        .stdout(contains(
            "left out 2 folder(s) by the built-in rule (build/, node_modules/)",
        ));
}

/// `update` reports the same clause, after the counts the line already had.
#[test]
fn ac_2_update_reports_the_folders_the_built_in_rule_left_out() {
    let tree = tree_with_built_in_folders();
    let index_dir = TempDir::new().expect("tempdir");
    add(index_dir.path(), tree.path(), &[]);

    rqmd(index_dir.path())
        .arg("update")
        .assert()
        .success()
        .stdout(contains(
            "Indexed: 0 new, 0 updated, 2 unchanged, 0 removed",
        ))
        .stdout(contains(
            "left out 2 folder(s) by the built-in rule (build/, node_modules/)",
        ));
}

/// A tree with no built-in folder keeps its summary exactly as before.
#[test]
fn ac_3_default_summary_is_unchanged_when_nothing_was_left_out() {
    let tree = TempDir::new().expect("tempdir");
    fs::write(tree.path().join("a.md"), "only note\n").expect("write a");
    let index_dir = TempDir::new().expect("tempdir");

    add(index_dir.path(), tree.path(), &[])
        .stdout(contains("Indexed 1 document(s)."))
        .stdout(contains("left out").not());

    rqmd(index_dir.path())
        .arg("update")
        .assert()
        .success()
        .stdout(contains("left out").not());
}

/// The set of indexed documents does not depend on whether the walk descends
/// into the folders the rule leaves out: nested ones, a hidden folder and
/// `--hidden` all give the counts they gave before.
#[test]
fn ac_4_indexed_documents_are_the_same_with_nested_and_hidden_folders() {
    let tree = tree_with_built_in_folders();
    let p = tree.path();
    fs::create_dir_all(p.join("a/build")).expect("mkdir nested build");
    fs::create_dir_all(p.join(".obsidian")).expect("mkdir hidden");
    fs::write(p.join("a/keep.md"), "kept\n").expect("write keep");
    fs::write(p.join("a/build/hidden-by-rule.md"), "left out\n").expect("write nested");
    fs::write(p.join(".obsidian/cfg.md"), "hidden\n").expect("write hidden");

    let default = TempDir::new().expect("tempdir");
    add(default.path(), p, &[]).stdout(contains("Indexed 3 document(s)"));

    let hidden = TempDir::new().expect("tempdir");
    add(hidden.path(), p, &["--hidden"]).stdout(contains("Indexed 4 document(s)"));
}

/// `collection show` lists the stored ignore patterns, or says there are none.
#[test]
fn ac_5_show_lists_the_stored_ignore_patterns() {
    let tree = TempDir::new().expect("tempdir");
    fs::write(tree.path().join("a.md"), "note\n").expect("write a");
    let with = TempDir::new().expect("tempdir");
    add(
        with.path(),
        tree.path(),
        &["--ignore", "*.log", "--ignore", "tmp/"],
    );
    rqmd(with.path())
        .args(["collection", "show", "t"])
        .assert()
        .success()
        .stdout(contains("  Ignore:   *.log, tmp/"));

    let without = TempDir::new().expect("tempdir");
    add(without.path(), tree.path(), &[]);
    rqmd(without.path())
        .args(["collection", "show", "t"])
        .assert()
        .success()
        .stdout(contains("  Ignore:   (none)"));
}

/// A malformed `--ignore` pattern fails `collection add`, names the pattern,
/// and stores nothing.
#[test]
fn ac_6_add_rejects_a_malformed_ignore_pattern_and_stores_nothing() {
    let tree = TempDir::new().expect("tempdir");
    fs::write(tree.path().join("a.md"), "note\n").expect("write a");
    let index_dir = TempDir::new().expect("tempdir");

    rqmd(index_dir.path())
        .args(["collection", "add"])
        .arg(tree.path())
        .args(["--name", "t", "--ignore", "docs/{a"])
        .assert()
        .failure()
        .stderr(contains("invalid ignore pattern 'docs/{a'"));

    // The rejected add leaves no index behind, and `collection list` no
    // longer creates one (#156), so it reports that rather than an empty list.
    rqmd(index_dir.path())
        .args(["collection", "list"])
        .assert()
        .success()
        .stdout(contains("no index at"));
}

/// An already-stored malformed pattern does not stop `update`: it warns once
/// per pattern and still applies the valid ones.
#[test]
fn ac_7_update_warns_about_a_stored_malformed_ignore_pattern_and_carries_on() {
    let tree = TempDir::new().expect("tempdir");
    fs::create_dir_all(tree.path().join("skip")).expect("mkdir skip");
    fs::write(tree.path().join("a.md"), "note\n").expect("write a");
    fs::write(tree.path().join("skip/s.md"), "skipped\n").expect("write s");
    let index_dir = TempDir::new().expect("tempdir");
    add(index_dir.path(), tree.path(), &["--ignore", "skip/**"]).stdout(contains("Indexed 1"));

    let conn = Connection::open(index_dir.path().join("index.sqlite")).expect("open index");
    conn.execute(
        "UPDATE store_collections SET ignore_patterns = ?1 WHERE name = 't'",
        [r#"["docs/{a","skip/**"]"#],
    )
    .expect("store malformed pattern");
    drop(conn);

    rqmd(index_dir.path())
        .arg("update")
        .assert()
        .success()
        .stderr(contains(
            "WARN: collection 't': ignoring invalid ignore pattern 'docs/{a'",
        ))
        .stdout(contains(
            "Indexed: 0 new, 0 updated, 1 unchanged, 0 removed",
        ));
}
