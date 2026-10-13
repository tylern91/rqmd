use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime};

use predicates::prelude::*;
use predicates::str::contains;
use tempfile::TempDir;

use super::helpers::{fixture_collection, rqmd};

fn set_mtime(path: &Path, mtime: SystemTime) {
    File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(mtime))
        .expect("set mtime");
}

fn hour_ago() -> SystemTime {
    SystemTime::now() - Duration::from_secs(3600)
}

/// Add `body` as a one-file collection whose file is already old enough to be
/// trusted, then run the first `update` that records its stat.
fn indexed_old_file(index_dir: &Path, body: &str) -> super::helpers::Fixture {
    let fx = fixture_collection("a", body);
    set_mtime(&fx.path.join("doc.md"), hour_ago());
    rqmd(index_dir)
        .args(["collection", "add"])
        .arg(&fx.path)
        .args(["--name", "a"])
        .assert()
        .success();
    rqmd(index_dir).arg("update").assert().success();
    fx
}

/// A file whose size and mtime are unchanged is not opened again: once it is
/// made unreadable, `update` still reports it unchanged instead of skipping it.
#[cfg(unix)]
#[test]
fn ac_1_unchanged_file_is_not_reread() {
    use std::os::unix::fs::PermissionsExt;

    let index_dir = TempDir::new().expect("tempdir");
    let fx = indexed_old_file(index_dir.path(), "stable content\n");
    let doc = fx.path.join("doc.md");
    fs::set_permissions(&doc, fs::Permissions::from_mode(0o000)).expect("chmod");
    if File::open(&doc).is_ok() {
        // Running as a user that ignores file modes; the premise cannot hold.
        return;
    }

    rqmd(index_dir.path())
        .arg("update")
        .assert()
        .success()
        .stdout(contains("1 unchanged"))
        .stdout(contains("skipped").not());

    fs::set_permissions(&doc, fs::Permissions::from_mode(0o644)).expect("restore chmod");
}

/// A changed size or mtime is picked up by the next plain `update`.
#[test]
fn ac_2_file_with_changed_stat_is_reindexed() {
    let index_dir = TempDir::new().expect("tempdir");
    let fx = indexed_old_file(index_dir.path(), "first version\n");
    let doc = fx.path.join("doc.md");

    fs::write(&doc, "second, longer version\n").expect("edit");
    set_mtime(&doc, hour_ago() + Duration::from_secs(60));

    rqmd(index_dir.path())
        .arg("update")
        .assert()
        .success()
        .stdout(contains("1 updated"));
}

/// `--full` re-reads a file edited without changing its size or mtime, which a
/// plain `update` trusts as unchanged.
#[test]
fn ac_3_full_update_rereads_a_same_stat_edit() {
    let index_dir = TempDir::new().expect("tempdir");
    let fx = indexed_old_file(index_dir.path(), "alpha one\n");
    let doc = fx.path.join("doc.md");
    let old_mtime = fs::metadata(&doc).unwrap().modified().unwrap();

    fs::write(&doc, "omega two\n").expect("same-size edit");
    set_mtime(&doc, old_mtime);

    rqmd(index_dir.path())
        .arg("update")
        .assert()
        .success()
        .stdout(contains("1 unchanged"));
    rqmd(index_dir.path())
        .args(["update", "--full"])
        .assert()
        .success()
        .stdout(contains("1 updated"));
}

/// A file whose mtime is too recent to trust (here: not yet in the past, so no
/// scheduling delay can age it past the window) has no stat recorded, so a
/// later same-stat edit is still caught by a plain `update`.
#[test]
fn ac_4_recently_modified_file_is_never_skipped() {
    let index_dir = TempDir::new().expect("tempdir");
    let fx = fixture_collection("a", "alpha one\n");
    let doc = fx.path.join("doc.md");
    let fresh_mtime = SystemTime::now() + Duration::from_secs(3600);
    set_mtime(&doc, fresh_mtime);
    rqmd(index_dir.path())
        .args(["collection", "add"])
        .arg(&fx.path)
        .args(["--name", "a"])
        .assert()
        .success();
    rqmd(index_dir.path()).arg("update").assert().success();

    fs::write(&doc, "omega two\n").expect("same-size edit");
    set_mtime(&doc, fresh_mtime);

    rqmd(index_dir.path())
        .arg("update")
        .assert()
        .success()
        .stdout(contains("1 updated"));
}
