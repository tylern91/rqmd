//! Regression coverage for github.com/tylern91/rqmd#86 (security assessment
//! against upstream tobi/qmd). AC-2 (MCP `limit` clamp) is covered by unit
//! tests in `rqmd-core::fts` and `rqmd-mcp::server` instead of here — this
//! tier only drives the `rqmd` CLI binary, not the MCP server.

use predicates::str::contains;
use tempfile::TempDir;

use super::helpers::{rqmd, rqmd_in_cwd};

/// AC-1: `rqmd update` must not run a collection's `update_command` when the
/// index directory was picked up implicitly from a project-local `.rqmd/` —
/// that command was chosen by whoever set up the repo the current directory
/// belongs to, not necessarily the person running `rqmd update` inside a
/// fresh clone of it. `--run-hooks` is the explicit opt-in.
#[test]
fn ac_1_project_local_index_skips_update_hook_without_run_hooks_flag() {
    let project_dir = TempDir::new().expect("tempdir");
    let index_dir = project_dir.path().join(".rqmd");
    let src = TempDir::new().expect("tempdir");
    std::fs::write(src.path().join("doc.md"), "hello\n").expect("write fixture doc");
    let marker = src.path().join("pwned.txt");

    // Explicit --index-dir here is just how the fixture is built; it is not
    // the code path under test.
    rqmd(&index_dir)
        .args(["collection", "add"])
        .arg(src.path())
        .args(["--name", "c"])
        .assert()
        .success();
    rqmd(&index_dir)
        .args(["collection", "update-cmd", "c"])
        .arg(format!("touch {}", marker.display()))
        .assert()
        .success();

    // No --index-dir: `resolve_index_dir` must pick up `.rqmd/` from `cwd`
    // implicitly, and treat its hooks as untrusted by default.
    rqmd_in_cwd(project_dir.path())
        .arg("update")
        .assert()
        .success()
        .stderr(contains("skipping update hook"));
    assert!(
        !marker.exists(),
        "update hook must not run without --run-hooks"
    );

    rqmd_in_cwd(project_dir.path())
        .args(["update", "--run-hooks"])
        .assert()
        .success();
    assert!(
        marker.exists(),
        "update hook must run once --run-hooks is passed"
    );
}

/// AC-3: a symlink inside a collection whose real target resolves outside
/// the collection root must not be indexed — `rqmd get` on it must find
/// nothing, not the linked-to file's content.
#[test]
#[cfg(unix)]
fn ac_3_symlink_escaping_the_collection_root_is_not_indexed() {
    let index_dir = TempDir::new().expect("tempdir");
    let outside = TempDir::new().expect("tempdir");
    std::fs::write(outside.path().join("secret.md"), "outside the collection\n")
        .expect("write outside file");

    let coll_src = TempDir::new().expect("tempdir");
    std::fs::write(coll_src.path().join("a.md"), "inside the collection\n")
        .expect("write fixture doc");
    std::os::unix::fs::symlink(
        outside.path().join("secret.md"),
        coll_src.path().join("leak.md"),
    )
    .expect("create escaping symlink");

    rqmd(index_dir.path())
        .args(["collection", "add"])
        .arg(coll_src.path())
        .args(["--name", "c"])
        .assert()
        .success();
    rqmd(index_dir.path()).arg("update").assert().success();

    rqmd(index_dir.path())
        .args(["get", "c/leak.md"])
        .assert()
        .failure();
    rqmd(index_dir.path())
        .args(["get", "c/a.md"])
        .assert()
        .success()
        .stdout(contains("inside the collection"));
}

/// AC-4: `rqmd get "#"` (an empty docid) must fail instead of returning an
/// arbitrary document, and a document whose file was deleted on disk must
/// no longer be servable by path once `update` has deactivated it.
#[test]
fn ac_4_empty_docid_and_deactivated_document_are_not_served() {
    let index_dir = TempDir::new().expect("tempdir");
    let src = TempDir::new().expect("tempdir");
    std::fs::write(src.path().join("a.md"), "content a\n").expect("write fixture doc");

    rqmd(index_dir.path())
        .args(["collection", "add"])
        .arg(src.path())
        .args(["--name", "c"])
        .assert()
        .success();

    rqmd(index_dir.path()).args(["get", "#"]).assert().failure();

    // Deactivate a.md by deleting it on disk and re-running update.
    std::fs::remove_file(src.path().join("a.md")).expect("delete fixture doc");
    // update needs at least one matched file for the mask to keep the
    // collection from being skipped outright; write a second file first.
    std::fs::write(src.path().join("b.md"), "content b\n").expect("write second doc");
    rqmd(index_dir.path()).arg("update").assert().success();

    rqmd(index_dir.path())
        .args(["get", "c/a.md"])
        .assert()
        .failure();
}

/// A project-local `.rqmd/` index built with `collection add`, so it holds a
/// real `index.sqlite`; the caller then drops a `-wal` file beside it.
fn project_with_index() -> TempDir {
    let project_dir = TempDir::new().expect("tempdir");
    let src = TempDir::new().expect("tempdir");
    std::fs::write(src.path().join("doc.md"), "hello\n").expect("write fixture doc");
    rqmd(&project_dir.path().join(".rqmd"))
        .args(["collection", "add"])
        .arg(src.path())
        .args(["--name", "c"])
        .assert()
        .success();
    project_dir
}

/// The 32-byte header of a SQLite write-ahead log declaring `page_size`.
fn wal_header(page_size: u32) -> Vec<u8> {
    let mut h = Vec::with_capacity(32);
    h.extend_from_slice(&0x377f_0682_u32.to_be_bytes());
    h.extend_from_slice(&3_007_000_u32.to_be_bytes());
    h.extend_from_slice(&page_size.to_be_bytes());
    h.resize(32, 0);
    h
}

/// The page size recorded in the database header (bytes 16-17, big-endian).
fn db_page_size(db: &std::path::Path) -> u32 {
    let head = std::fs::read(db).expect("read index.sqlite");
    u32::from(u16::from_be_bytes([head[16], head[17]]))
}

/// AC-5: a cloned repo's `.rqmd/index.sqlite` is opened implicitly by any
/// command run inside it. A `-wal` file whose page size differs from the
/// database's is the RUSTSEC-2026-0459 trigger, so rqmd must refuse the pair
/// instead of opening it.
#[test]
fn ac_5_project_local_index_with_mismatched_wal_page_size_is_refused() {
    let project_dir = project_with_index();
    let db = project_dir.path().join(".rqmd/index.sqlite");
    let wrong = db_page_size(&db) * 2;
    std::fs::write(
        project_dir.path().join(".rqmd/index.sqlite-wal"),
        wal_header(wrong),
    )
    .expect("write crafted wal");

    rqmd_in_cwd(project_dir.path())
        .arg("status")
        .assert()
        .failure()
        .stderr(contains("write-ahead log"))
        .stderr(contains("page size"));
}

/// AC-6: a `-wal` file that agrees with the database's page size is what an
/// honest crashed or running rqmd leaves behind, so it must still open.
#[test]
fn ac_6_project_local_index_with_matching_wal_page_size_still_opens() {
    let project_dir = project_with_index();
    let db = project_dir.path().join(".rqmd/index.sqlite");
    let same = db_page_size(&db);
    std::fs::write(
        project_dir.path().join(".rqmd/index.sqlite-wal"),
        wal_header(same),
    )
    .expect("write matching wal");

    rqmd_in_cwd(project_dir.path())
        .arg("status")
        .assert()
        .success();
}
