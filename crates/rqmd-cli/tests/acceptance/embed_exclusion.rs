//! Per-collection embed-exclusion rules and embed size cap (#147).
//!
//! `collection set --embed-ignore` / `--embed-max-bytes` keep documents out of
//! `rqmd embed` while they stay in the full-text index.
//!
//! Every test here is model-free, so it runs on CI legs with no model cache.
//! Vectors are seeded straight into `content_vectors` at the fingerprint the
//! binary expects (as `embed_reuse` seeds rows), which makes `rqmd embed`
//! take its no-model fast path when nothing but excluded documents lacks a
//! vector. AC-6 builds its index through `rqmd_core::Store` with a
//! deterministic stand-in embedder, then reads it with `rqmd similar`, which
//! needs no model. The embed loop itself (what gets encoded), the size cap on
//! indexed text and the vector-search filter for `vsearch` and `query` are
//! covered with fake embedders in `commands::index::tests` and
//! `rqmd_core::store::tests`.

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

/// Give each listed document one vector row at the fingerprint `rqmd embed`
/// expects for its path, as if it had been embedded. No HNSW entry is made;
/// `status`, `doctor`, the embed fast path and `embed --cleanup` read rows only.
fn seed_vectors(index: &Path, docs: &[(&str, &str)]) {
    let conn = Connection::open(index.join("index.sqlite")).expect("open index");
    for (i, (collection, path)) in docs.iter().enumerate() {
        let hash: String = conn
            .query_row(
                "SELECT hash FROM documents WHERE active = 1 AND collection = ?1 AND path = ?2",
                params![collection, path],
                |r| r.get(0),
            )
            .expect("document hash");
        conn.execute(
            "INSERT INTO content_vectors(hash, seq, pos, model, embed_fingerprint, \
                 total_chunks, embedded_at, vid) \
             VALUES (?1, 0, 0, 'seeded', ?2, 1, '2026-01-01T00:00:00Z', ?3)",
            params![
                hash,
                rqmd_core::store::expected_fingerprint_for_path(path),
                9000 + i as i64
            ],
        )
        .expect("seed vector row");
    }
}

/// With no rule and no cap, every active non-empty document is pending, as
/// before; once each has a vector `embed` takes its fast path; and `status`,
/// `doctor` and `collection show` print nothing new.
#[test]
fn ac_1_without_rules_every_document_is_embedded_as_before() {
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
    assert!(!status.contains("Excluded"), "{status}");
    let doctor = run_ok(index.path(), &["doctor"]);
    assert!(doctor.contains("(4 hashes pending)"), "{doctor}");
    assert!(!doctor.contains("Embed exclusions"), "{doctor}");
    let show = run_ok(index.path(), &["collection", "show", "a"]);
    assert!(!show.contains("Embed"), "{show}");

    seed_vectors(
        index.path(),
        &[
            ("a", "notes/plain.md"),
            ("a", "gen/output.md"),
            ("a", "big.md"),
            ("b", "other.md"),
        ],
    );
    let status = run_ok(index.path(), &["status"]);
    assert!(!status.contains("Pending"), "{status}");
    assert!(!status.contains("Excluded"), "{status}");
    rqmd(index.path())
        .arg("embed")
        .assert()
        .success()
        .stdout(contains(FAST_PATH));
}

/// A document matching an `--embed-ignore` glob is found by `rqmd search`, is
/// not pending, and `rqmd embed` has nothing to encode once its neighbour has
/// a vector (the fast path, which loads no model).
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

    let status = run_ok(index.path(), &["status"]);
    assert!(status.contains("Pending:  1 need embedding"), "{status}");
    assert!(
        status.contains("Excluded: 1 from embedding (1 by rule, 0 over the size cap)"),
        "{status}"
    );
    let search = run_ok(index.path(), &["search", "zanzibarquux"]);
    assert!(search.contains("rqmd://a/gen/skip.md"), "{search}");

    seed_vectors(index.path(), &[("a", "notes/keep.md")]);
    rqmd(index.path())
        .arg("embed")
        .assert()
        .success()
        .stdout(contains(FAST_PATH));
    assert_eq!(vectors_for(index.path(), "a", "gen/skip.md"), 0);
}

/// The cap is measured on the indexed text: a file whose frontmatter makes the
/// raw file larger than the cap, but whose indexed text is under it, stays
/// pending (it will be embedded); a document whose indexed text is over the
/// cap is searchable and not pending.
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

    let status = run_ok(index.path(), &["status"]);
    assert!(
        status.contains("Excluded: 1 from embedding (0 by rule, 1 over the size cap)"),
        "{status}"
    );
    assert!(
        status.contains("Pending:  1 need embedding"),
        "fm.md is still pending: {status}"
    );
    let search = run_ok(index.path(), &["search", "quuxbarnacle"]);
    assert!(search.contains("rqmd://a/big.md"), "{search}");

    seed_vectors(index.path(), &[("a", "fm.md")]);
    rqmd(index.path())
        .arg("embed")
        .assert()
        .success()
        .stdout(contains(FAST_PATH));
    assert_eq!(vectors_for(index.path(), "a", "big.md"), 0);
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
/// plain `rqmd embed`, and `rqmd embed --cleanup` drops them while keeping
/// every other document's vectors.
#[test]
fn ac_5_newly_excluded_document_keeps_its_vectors_until_cleanup() {
    let index = TempDir::new().expect("tempdir");
    let a = tree(&[
        ("keep.md", "# Keep\nNotes about mountain goats.\n"),
        ("later.md", "# Later\nThe lighthouse keeper at dusk.\n"),
    ]);
    add(index.path(), a.path(), "a");
    rqmd(index.path()).arg("update").assert().success();
    seed_vectors(index.path(), &[("a", "keep.md"), ("a", "later.md")]);

    rqmd(index.path())
        .args(["collection", "set", "a", "--embed-ignore", "later.md"])
        .assert()
        .success();
    rqmd(index.path())
        .arg("embed")
        .assert()
        .success()
        .stdout(contains(FAST_PATH));
    assert_eq!(
        vectors_for(index.path(), "a", "later.md"),
        1,
        "a plain embed keeps the vectors"
    );
    let doctor = run_ok(index.path(), &["doctor"]);
    assert!(
        doctor.contains("1 vector(s) kept for excluded documents"),
        "{doctor}"
    );

    rqmd(index.path())
        .args(["embed", "--cleanup"])
        .assert()
        .success()
        .stdout(contains(
            "Swept 1 vector(s) kept for documents excluded from embedding.",
        ));
    assert_eq!(vectors_for(index.path(), "a", "later.md"), 0);
    assert_eq!(vectors_for(index.path(), "a", "keep.md"), 1);
    let doctor = run_ok(index.path(), &["doctor"]);
    assert!(!doctor.contains("kept for excluded documents"), "{doctor}");
}

/// Deterministic stand-in embedder: every text gets the same vector, so any
/// stored vector is the nearest neighbour of any other.
struct ConstantEmbedder;

impl rqmd_llm::InferenceBackend for ConstantEmbedder {
    fn embed(&mut self, _text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(vec![0.1; rqmd_llm::EMBED_DIM])
    }
    fn rerank(&mut self, _query: &str, _docs: &[&str]) -> anyhow::Result<Vec<f32>> {
        unreachable!("not used to build the index")
    }
    fn generate(&mut self, _prompt: &str) -> anyhow::Result<String> {
        unreachable!("not used to build the index")
    }
    fn embed_model_name(&self) -> &str {
        "test-org/constant-embed.gguf"
    }
    fn rerank_model_name(&self) -> &str {
        "test-rerank"
    }
    fn generate_model_name(&self) -> &str {
        "test-generate"
    }
}

/// Two collections hold identical text, so they share one content hash and
/// one stored vector. Once collection `a` excludes its copy, `rqmd similar`
/// (vector search from a stored document, no model) returns `b`'s copy, never
/// `a`'s, and `embed --cleanup` keeps the vector `b` still uses. The same
/// filter for scoped and unscoped `vsearch` and `query` is covered by
/// `rqmd_core::store::tests::excluded_document_is_never_returned_through_a_shared_vector`.
#[test]
fn ac_6_vector_search_never_returns_an_excluded_document_through_a_shared_vector() {
    let index = TempDir::new().expect("tempdir");
    let same = "The lighthouse keeper watched ships cross the harbor at dusk.";
    {
        let mut store = rqmd_core::Store::open(
            rqmd_core::StoreConfig {
                db_path: index.path().join("index.sqlite"),
                tantivy_dir: index.path().join("tantivy"),
                hnsw_path: index.path().join("hnsw.usearch"),
                read_only: false,
            },
            Box::new(ConstantEmbedder),
        )
        .expect("open store");
        for (collection, path, body) in [
            ("a", "gen/same.md", same),
            ("b", "same.md", same),
            ("a", "other.md", "Bread recipes with sourdough starter."),
        ] {
            store
                .index_document(collection, path, "T", body)
                .expect("index document");
        }
        store.flush().expect("flush");
        for name in ["a", "b"] {
            rqmd_core::db::upsert_collection(
                &store.db,
                &rqmd_core::Collection {
                    name: name.into(),
                    path: index.path().join(name).to_string_lossy().into_owned(),
                    pattern: "**/*.md".into(),
                    ignore: vec![],
                    include_by_default: true,
                    update_command: None,
                    allow_hidden: false,
                    respect_gitignore: false,
                    max_file_bytes: None,
                    embed_ignore: vec![],
                    embed_max_bytes: None,
                },
            )
            .expect("register collection");
        }
    }

    let similar = ["similar", "a/other.md"];
    let before = run_ok(index.path(), &similar);
    assert!(before.contains("rqmd://a/gen/same.md"), "premise: {before}");

    rqmd(index.path())
        .args(["collection", "set", "a", "--embed-ignore", "gen/**"])
        .assert()
        .success();
    let after = run_ok(index.path(), &similar);
    assert!(!after.contains("rqmd://a/gen/same.md"), "{after}");
    assert!(after.contains("rqmd://b/same.md"), "{after}");

    rqmd(index.path())
        .args(["embed", "--cleanup"])
        .assert()
        .success()
        .stdout(contains(
            "Swept 0 vector(s) kept for documents excluded from embedding.",
        ));
    assert!(
        vectors_for(index.path(), "b", "same.md") > 0,
        "the shared vector is kept"
    );
    let after = run_ok(index.path(), &similar);
    assert!(after.contains("rqmd://b/same.md"), "{after}");
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
