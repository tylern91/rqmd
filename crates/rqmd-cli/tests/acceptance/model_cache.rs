//! `rqmd query --cache` (#121): an opt-in cache of query-expansion output and
//! rerank scores in `model_cache.sqlite` next to the index.
//!
//! Model-free by design: CI has no models, so nothing here runs `query` against
//! an index. What needs a model (AC-1 to AC-4 and the bound half of AC-5) is
//! covered by stub-backend tests named `ac_N_*` in
//! `crates/rqmd-core/src/store/query_cache.rs`, `crates/rqmd-core/src/model_cache.rs`
//! and `crates/rqmd-cli/src/commands/query.rs`. This tier covers what a CLI
//! process shows without one: clearing, the status line, the opt-in switches,
//! and an unreadable cache file.

use std::fs;
use std::path::{Path, PathBuf};

use predicates::str::contains;
use tempfile::TempDir;

use super::helpers::{Fixture, fixture_collection, refute_vacuous_output, rqmd};

/// An index of one note, built without any model.
fn index(index_dir: &Path) -> Fixture {
    let fx = fixture_collection(
        "notes",
        "# Keeper\n\nThe lighthouse keeper trims the wick.\n",
    );
    rqmd(index_dir)
        .env_remove("RQMD_QUERY_CACHE")
        .args(["collection", "add"])
        .arg(&fx.path)
        .args(["--name", "notes"])
        .assert()
        .success()
        .stdout(contains("Indexed 1 document(s)"));
    fx
}

fn cache_path(index_dir: &Path) -> PathBuf {
    index_dir.join("model_cache.sqlite")
}

fn sidecar(index_dir: &Path, suffix: &str) -> PathBuf {
    index_dir.join(format!("model_cache.sqlite{suffix}"))
}

/// `rqmd status` stdout, which must succeed.
fn status(index_dir: &Path) -> String {
    let out = rqmd(index_dir)
        .env_remove("RQMD_QUERY_CACHE")
        .arg("status")
        .output()
        .expect("run status");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    refute_vacuous_output(&out);
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The cache can be cleared: `rqmd embed --clear-cache` deletes a cache file
/// and its SQLite journal files and keeps the index. `rqmd status` shows the
/// row count only while the file exists. (The bound half of AC-5 is
/// `ac_5_*` in rqmd-core.)
#[test]
fn ac_5_embed_clear_cache_removes_the_cache_and_its_journals() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fx = index(index_dir.path());
    assert!(
        !status(index_dir.path()).contains("Cache:"),
        "no cache, no line"
    );

    let path = cache_path(index_dir.path());
    rqmd_core::model_cache::ModelCache::open(&path)
        .expect("open cache")
        .put_rerank_scores("q", "m", &[("a", 0.1), ("b", 0.2), ("c", 0.3)])
        .expect("seed cache");
    assert!(
        status(index_dir.path()).contains("  Cache:    3 of 10000 row(s) (rqmd query --cache)\n"),
        "{}",
        status(index_dir.path())
    );
    for suffix in ["-journal", "-wal", "-shm"] {
        fs::write(sidecar(index_dir.path(), suffix), b"").expect("seed journal file");
    }

    rqmd(index_dir.path())
        .args(["embed", "--clear-cache"])
        .assert()
        .success()
        .stdout(contains("Query cache cleared. Removed 3 row(s)."));
    assert!(!path.exists(), "the cache file is deleted");
    for suffix in ["-journal", "-wal", "-shm"] {
        assert!(
            !sidecar(index_dir.path(), suffix).exists(),
            "{suffix} deleted"
        );
    }
    assert!(
        index_dir.path().join("index.sqlite").is_file(),
        "the index is kept"
    );
    assert!(!status(index_dir.path()).contains("Cache:"));

    rqmd(index_dir.path())
        .args(["embed", "--clear-cache"])
        .assert()
        .success()
        .stdout(contains("No query cache to clear."));
}

/// The cache is opt-in. `RQMD_QUERY_CACHE` takes the usual boolean words and
/// a bad value exits 2; `--cache` takes no value; `--no-cache` combines with
/// either. No command other than `query --cache` creates the cache file, even
/// with the variable set.
#[test]
fn ac_6_the_cache_is_opt_in_and_its_switches_parse() {
    let scratch = TempDir::new().expect("tempdir");
    let missing = scratch.path().join("missing");
    // A missing index fails before any model is loaded, after parsing.
    let parses = |env: Option<&str>, flags: &[&str]| {
        let mut cmd = rqmd(&missing);
        cmd.env_remove("RQMD_QUERY_CACHE");
        if let Some(value) = env {
            cmd.env("RQMD_QUERY_CACHE", value);
        }
        cmd.args(["query", "anything"])
            .args(flags)
            .assert()
            .failure()
            .code(1)
            .stderr(contains("no index at"));
    };
    for value in ["1", "true", "yes", "on", "0", "false", "no", "off"] {
        parses(Some(value), &[]);
        parses(Some(value), &["--no-cache"]);
    }
    parses(None, &["--cache"]);
    parses(None, &["--cache", "--no-cache"]);
    assert!(!missing.exists(), "a failed query creates nothing");

    rqmd(&missing)
        .env("RQMD_QUERY_CACHE", "maybe")
        .args(["query", "anything"])
        .assert()
        .failure()
        .code(2)
        .stderr(contains("invalid value 'maybe' for '--cache'"));
    rqmd(&missing)
        .env_remove("RQMD_QUERY_CACHE")
        .args(["query", "anything", "--cache=true"])
        .assert()
        .failure()
        .code(2);
    rqmd(&missing)
        .args(["query", "--help"])
        .assert()
        .success()
        .stdout(contains("--cache"))
        .stdout(contains("--no-cache"))
        .stdout(contains("RQMD_QUERY_CACHE"));

    let index_dir = TempDir::new().expect("tempdir");
    let _fx = index(index_dir.path());
    for args in [
        &["status"][..],
        &["search", "lighthouse"],
        &["update"],
        &["embed", "--cleanup"],
        &["collection", "list"],
    ] {
        let out = rqmd(index_dir.path())
            .env("RQMD_QUERY_CACHE", "1")
            .args(args)
            .output()
            .expect("run rqmd");
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        refute_vacuous_output(&out);
    }
    assert!(
        !cache_path(index_dir.path()).exists(),
        "only `query --cache` creates the cache"
    );
}

/// A cache file that is not a database never breaks a command: `rqmd status`
/// still succeeds, with one extra line naming the fix, and
/// `rqmd embed --clear-cache` deletes it.
#[test]
fn ac_7_an_unreadable_cache_never_breaks_status_or_clearing() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fx = index(index_dir.path());
    let baseline = status(index_dir.path());

    fs::write(
        cache_path(index_dir.path()),
        b"not a sqlite database ".repeat(200),
    )
    .expect("write corrupt cache");
    let with_corrupt = status(index_dir.path());
    let line = "  Cache:    unreadable (run 'rqmd embed --clear-cache')\n";
    assert!(with_corrupt.contains(line), "{with_corrupt}");
    assert_eq!(
        with_corrupt.replacen(line, "", 1),
        baseline,
        "only that line is added"
    );

    rqmd(index_dir.path())
        .args(["embed", "--clear-cache"])
        .assert()
        .success()
        .stdout(contains("Query cache cleared. The file was unreadable."));
    assert!(!cache_path(index_dir.path()).exists());
    assert_eq!(status(index_dir.path()), baseline);
}
