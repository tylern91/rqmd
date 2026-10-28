//! Per-collection embed-exclusion rules and embed size cap (#147).
//!
//! `collection set --embed-ignore` / `--embed-max-bytes` keep documents out of
//! `rqmd embed` while they stay in the full-text index. AC-4 and AC-7 need no
//! model; the others embed with the locally cached embedding model, like the
//! other suites that run `rqmd embed`.

use std::path::Path;

use predicates::prelude::*;
use predicates::str::contains;
use rusqlite::{Connection, params};
use tempfile::TempDir;

use super::helpers::{refute_vacuous_output, rqmd};

const FAST_PATH: &str = "All content hashes already have embeddings.";

/// A collection directory holding `files` (relative path, body).
fn tree(files: &[(&str, &str)]) -> TempDir {
    let root = TempDir::new().expect("tempdir");
    for (rel, body) in files {
        let path = root.path().join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, body).expect("write fixture");
    }
    root
}

fn add(index: &Path, dir: &Path, name: &str) {
    rqmd(index)
        .args(["collection", "add"])
        .arg(dir)
        .args(["--name", name])
        .assert()
        .success();
}

fn run_ok(index: &Path, args: &[&str]) -> String {
    let out = rqmd(index).args(args).output().expect("run rqmd");
    assert!(
        out.status.success(),
        "rqmd {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    refute_vacuous_output(&out);
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `content_vectors` rows reachable from the active document `collection/path`.
fn vectors_for(index: &Path, collection: &str, path: &str) -> i64 {
    let conn = Connection::open(index.join("index.sqlite")).expect("open index");
    conn.query_row(
        "SELECT COUNT(*) FROM content_vectors cv JOIN documents d ON d.hash = cv.hash \
         WHERE d.active = 1 AND d.collection = ?1 AND d.path = ?2",
        params![collection, path],
        |r| r.get(0),
    )
    .expect("count vectors")
}

/// With no rule and no cap, every active non-empty document is embedded, and
/// `status`, `doctor` and `collection show` print nothing new.
#[test]
fn ac_1_without_rules_embed_covers_every_document_as_before() {
    let index = TempDir::new().expect("tempdir");
    let a = tree(&[
        (
            "notes/plain.md",
            "# Plain\nA note about orchards and apples.\n",
        ),
        ("gen/output.md", "# Output\nGenerated tables of numbers.\n"),
        ("big.md", &format!("# Big\n{}\n", "long text ".repeat(400))),
    ]);
    let b = tree(&[("other.md", "# Other\nA note about rivers.\n")]);
    add(index.path(), a.path(), "a");
    add(index.path(), b.path(), "b");
    rqmd(index.path()).arg("update").assert().success();

    let status = run_ok(index.path(), &["status"]);
    assert!(status.contains("Pending:  4 need embedding"), "{status}");
    rqmd(index.path())
        .arg("embed")
        .assert()
        .success()
        .stdout(contains("from 4 documents"));

    for (col, path) in [
        ("a", "notes/plain.md"),
        ("a", "gen/output.md"),
        ("a", "big.md"),
        ("b", "other.md"),
    ] {
        assert!(
            vectors_for(index.path(), col, path) > 0,
            "{col}/{path} must be embedded"
        );
    }
    let status = run_ok(index.path(), &["status"]);
    assert!(!status.contains("Excluded"), "{status}");
    assert!(!status.contains("Pending"), "{status}");
    let doctor = run_ok(index.path(), &["doctor"]);
    assert!(!doctor.contains("Embed exclusions"), "{doctor}");
    let show = run_ok(index.path(), &["collection", "show", "a"]);
    assert!(!show.contains("Embed"), "{show}");
    rqmd(index.path())
        .arg("embed")
        .assert()
        .success()
        .stdout(contains(FAST_PATH));
}

/// A document matching an `--embed-ignore` glob is found by `rqmd search` and
/// has no vector after `rqmd embed`; its neighbour is embedded.
#[test]
fn ac_2_rule_matched_document_is_searchable_but_never_embedded() {
    let index = TempDir::new().expect("tempdir");
    let a = tree(&[
        ("notes/keep.md", "# Keep\nNotes on glacier hiking routes.\n"),
        ("gen/skip.md", "# Skip\nZanzibarquux generated listing.\n"),
    ]);
    add(index.path(), a.path(), "a");
    rqmd(index.path())
        .args(["collection", "set", "a", "--embed-ignore", "gen/**"])
        .assert()
        .success()
        .stdout(contains("embed ignore gen/**"));
    rqmd(index.path()).arg("update").assert().success();

    rqmd(index.path())
        .arg("embed")
        .assert()
        .success()
        .stdout(contains("from 1 documents"));
    assert!(vectors_for(index.path(), "a", "notes/keep.md") > 0);
    assert_eq!(vectors_for(index.path(), "a", "gen/skip.md"), 0);

    let search = run_ok(index.path(), &["search", "zanzibarquux"]);
    assert!(search.contains("rqmd://a/gen/skip.md"), "{search}");
    let status = run_ok(index.path(), &["status"]);
    assert!(
        status.contains("Excluded: 1 from embedding (1 by rule, 0 over the size cap)"),
        "{status}"
    );
}

/// The cap is measured on the indexed text: a file whose frontmatter makes the
/// raw file larger than the cap, but whose indexed text is under it, is still
/// embedded; a document whose indexed text is over the cap is searchable and
/// not embedded.
#[test]
fn ac_3_document_over_the_cap_on_indexed_text_is_searchable_not_embedded() {
    let index = TempDir::new().expect("tempdir");
    let frontmatter = format!("---\ndescription: {}\n---\n", "x".repeat(600));
    let a = tree(&[
        (
            "fm.md",
            &format!("{frontmatter}# Small\nShort body about kites.\n"),
        ),
        (
            "big.md",
            &format!("# Big\nQuuxbarnacle {}\n", "filler words ".repeat(40)),
        ),
    ]);
    add(index.path(), a.path(), "a");
    rqmd(index.path())
        .args(["collection", "set", "a", "--embed-max-bytes", "200"])
        .assert()
        .success()
        .stdout(contains("embed max 200 bytes"));
    rqmd(index.path()).arg("update").assert().success();

    rqmd(index.path()).arg("embed").assert().success();
    assert!(
        vectors_for(index.path(), "a", "fm.md") > 0,
        "raw file is over the cap but its indexed text is not"
    );
    assert_eq!(vectors_for(index.path(), "a", "big.md"), 0);

    let search = run_ok(index.path(), &["search", "quuxbarnacle"]);
    assert!(search.contains("rqmd://a/big.md"), "{search}");
    let status = run_ok(index.path(), &["status"]);
    assert!(
        status.contains("Excluded: 1 from embedding (0 by rule, 1 over the size cap)"),
        "{status}"
    );
}

/// Excluded documents are not pending: `status` drops its Pending line and
/// `rqmd embed` takes the "all content hashes already have embeddings" fast
/// path, which loads no model.
#[test]
fn ac_4_excluded_documents_are_not_pending_and_embed_takes_the_fast_path() {
    let index = TempDir::new().expect("tempdir");
    let a = tree(&[("gen/only.md", "# Only\nGenerated body text.\n")]);
    add(index.path(), a.path(), "a");
    rqmd(index.path()).arg("update").assert().success();
    let before = run_ok(index.path(), &["status"]);
    assert!(before.contains("Pending:  1 need embedding"), "{before}");

    rqmd(index.path())
        .args(["collection", "set", "a", "--embed-ignore", "gen/**"])
        .assert()
        .success();

    let after = run_ok(index.path(), &["status"]);
    assert!(!after.contains("Pending"), "{after}");
    assert!(
        after.contains("Excluded: 1 from embedding (1 by rule, 0 over the size cap)"),
        "{after}"
    );
    rqmd(index.path())
        .arg("embed")
        .assert()
        .success()
        .stdout(contains(FAST_PATH));
    rqmd(index.path())
        .arg("update")
        .assert()
        .success()
        .stdout(contains("need vectors").not());
}

/// Chosen behaviour: a document that becomes excluded keeps its vectors on a
/// plain `rqmd embed` (never served by vector search meanwhile) and loses them
/// on `rqmd embed --cleanup`, which keeps every other document's vectors.
#[test]
fn ac_5_newly_excluded_document_keeps_its_vectors_until_cleanup() {
    let index = TempDir::new().expect("tempdir");
    let a = tree(&[
        ("keep.md", "# Keep\nNotes about mountain goats.\n"),
        (
            "later.md",
            "# Later\nThe lighthouse keeper watched ships cross the harbor at dusk.\n",
        ),
    ]);
    add(index.path(), a.path(), "a");
    rqmd(index.path()).arg("update").assert().success();
    rqmd(index.path()).arg("embed").assert().success();
    assert!(vectors_for(index.path(), "a", "later.md") > 0);

    rqmd(index.path())
        .args(["collection", "set", "a", "--embed-ignore", "later.md"])
        .assert()
        .success();
    rqmd(index.path())
        .arg("embed")
        .assert()
        .success()
        .stdout(contains(FAST_PATH));
    assert!(
        vectors_for(index.path(), "a", "later.md") > 0,
        "a plain embed keeps the vectors"
    );
    let doctor = run_ok(index.path(), &["doctor"]);
    assert!(
        doctor.contains("vector(s) kept for excluded documents"),
        "{doctor}"
    );
    let vsearch = run_ok(index.path(), &["vsearch", "harbor lighthouse at dusk"]);
    assert!(!vsearch.contains("rqmd://a/later.md"), "{vsearch}");

    rqmd(index.path())
        .args(["embed", "--cleanup"])
        .assert()
        .success()
        .stdout(contains("kept for documents excluded from embedding"));
    assert_eq!(vectors_for(index.path(), "a", "later.md"), 0);
    assert!(vectors_for(index.path(), "a", "keep.md") > 0);
}

/// Two collections hold identical text, so they share one content hash and
/// one set of vectors. Once collection `a` excludes its copy, neither a
/// scoped nor an unscoped `vsearch` or `query` returns `a`'s copy through the
/// vector made for `b`'s.
#[test]
fn ac_6_vector_search_never_returns_an_excluded_document_through_a_shared_vector() {
    let index = TempDir::new().expect("tempdir");
    let same = "# Same\nThe lighthouse keeper watched ships cross the harbor at dusk.\n";
    let a = tree(&[
        ("gen/same.md", same),
        (
            "other.md",
            "# Other\nBread recipes with sourdough starter.\n",
        ),
    ]);
    let b = tree(&[("same.md", same)]);
    add(index.path(), a.path(), "a");
    add(index.path(), b.path(), "b");
    rqmd(index.path()).arg("update").assert().success();
    rqmd(index.path()).arg("embed").assert().success();

    // No word of this query is in the shared text, so only the vector route
    // can reach it.
    let q = "maritime beacon at night";
    let query = ["query", q, "--no-expand", "--no-rerank", "-c", "a"];
    let before = run_ok(index.path(), &["vsearch", q, "-c", "a"]);
    assert!(before.contains("rqmd://a/gen/same.md"), "premise: {before}");
    let before = run_ok(index.path(), &query);
    assert!(before.contains("rqmd://a/gen/same.md"), "premise: {before}");

    rqmd(index.path())
        .args(["collection", "set", "a", "--embed-ignore", "gen/**"])
        .assert()
        .success();

    let scoped = run_ok(index.path(), &["vsearch", q, "-c", "a"]);
    assert!(!scoped.contains("rqmd://a/gen/same.md"), "{scoped}");
    let unscoped = run_ok(index.path(), &["vsearch", q]);
    assert!(!unscoped.contains("rqmd://a/gen/same.md"), "{unscoped}");
    assert!(unscoped.contains("rqmd://b/same.md"), "{unscoped}");
    let scoped_query = run_ok(index.path(), &query);
    assert!(
        !scoped_query.contains("rqmd://a/gen/same.md"),
        "{scoped_query}"
    );
}

/// Rules and the cap persist with the collection: `collection show` lists
/// them, re-adding the collection keeps them, `--clear-*` removes them, a
/// malformed glob is refused, and the docs describe them.
#[test]
fn ac_7_rules_persist_with_the_collection_and_are_documented() {
    let index = TempDir::new().expect("tempdir");
    let a = tree(&[("doc.md", "# Doc\nBody.\n")]);
    add(index.path(), a.path(), "a");
    rqmd(index.path())
        .args(["collection", "set", "a"])
        .args(["--embed-ignore", "gen/**", "--embed-ignore", "*.log"])
        .args(["--embed-max-bytes", "4096"])
        .assert()
        .success();

    let shown = |index: &Path| run_ok(index, &["collection", "show", "a"]);
    let show = shown(index.path());
    assert!(show.contains("Embed ignore: gen/**, *.log"), "{show}");
    assert!(show.contains("Embed max:    4096 bytes"), "{show}");

    add(index.path(), a.path(), "a");
    let show = shown(index.path());
    assert!(
        show.contains("Embed ignore: gen/**, *.log"),
        "re-add: {show}"
    );
    assert!(show.contains("Embed max:    4096 bytes"), "re-add: {show}");

    rqmd(index.path())
        .args(["collection", "set", "a", "--embed-ignore", "["])
        .assert()
        .failure()
        .stderr(contains("invalid ignore pattern '['"));
    rqmd(index.path())
        .args(["collection", "set", "missing", "--embed-max-bytes", "10"])
        .assert()
        .failure()
        .stderr(contains("collection 'missing' not found"));

    rqmd(index.path())
        .args(["collection", "set", "a", "--clear-embed-ignore"])
        .args(["--clear-embed-max-bytes"])
        .assert()
        .success();
    let show = shown(index.path());
    assert!(!show.contains("Embed"), "{show}");

    let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
    for file in ["CONFIGURATION.md", "CLI.md"] {
        let text = std::fs::read_to_string(docs.join(file)).expect("read docs");
        for needle in [
            "collection set",
            "--embed-ignore",
            "--embed-max-bytes",
            "embed --cleanup",
        ] {
            assert!(text.contains(needle), "{file} must mention {needle}");
        }
    }
}
