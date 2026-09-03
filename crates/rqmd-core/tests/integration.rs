//! Integration tests for rqmd-core: chunking, RRF, and DB layer.
//! Does NOT require inference backend (no model downloads).

use rqmd_core::{
    Store, StoreConfig,
    chunking::chunk_document,
    db::{
        collection_context_key, content_hash, count_docs_needing_embed, count_orphaned_vectors,
        deactivate_missing_documents, docid_from_hash, find_documents_by_needles, get_config,
        get_context_for_path, get_document_by_docid_prefix, get_document_by_filepath,
        list_documents, open_db, purge_collection, set_config, upsert_content, upsert_document,
        upsert_vector_meta,
    },
    resolve::resolve_multi_get,
    rrf::{reciprocal_rank_fusion, rrf_weights},
    types::{QueryType, RankedListMeta, RankedResult},
};
use rusqlite::params;
use sha2::Digest;
use std::collections::HashSet;
use tempfile::TempDir;

// ── Chunking ──────────────────────────────────────────────────────────────────

#[test]
fn chunk_short_doc() {
    let chunks = chunk_document("hello world, this is a short document.");
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].pos, 0);
}

#[test]
fn chunk_long_doc_produces_multiple_chunks() {
    let body = "word ".repeat(1000); // ~5000 chars > CHUNK_SIZE_CHARS
    let chunks = chunk_document(&body);
    assert!(
        chunks.len() >= 2,
        "expected ≥2 chunks, got {}",
        chunks.len()
    );
    // Chunks should overlap
    for w in chunks.windows(2) {
        assert!(
            w[0].pos < w[1].pos,
            "chunk positions should be strictly increasing"
        );
    }
}

#[test]
fn chunk_heading_split_preferred() {
    // Two clearly separated sections
    let section_a = "# Section A\n".to_string() + &"alpha ".repeat(900);
    let section_b = "\n# Section B\n".to_string() + &"beta ".repeat(900);
    let text = section_a + &section_b;
    let chunks = chunk_document(&text);
    // Second chunk should start at (or near) the "# Section B" heading
    assert!(chunks.len() >= 2);
}

// ── Docid ─────────────────────────────────────────────────────────────────────

#[test]
fn docid_is_6_hex_chars() {
    let hash = content_hash("hello world");
    let docid = docid_from_hash(&hash);
    assert_eq!(docid.len(), 6);
    assert!(docid.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn same_content_same_hash() {
    assert_eq!(content_hash("abc"), content_hash("abc"));
}

#[test]
fn different_content_different_hash() {
    assert_ne!(content_hash("abc"), content_hash("def"));
}

// ── RRF fusion ────────────────────────────────────────────────────────────────

fn ranked(file: &str, score: f32) -> RankedResult {
    RankedResult {
        filepath: file.to_string(),
        title: file.to_string(),
        backend_score: score,
    }
}

#[test]
fn rrf_single_list_preserves_order() {
    let list = vec![ranked("a", 10.0), ranked("b", 5.0), ranked("c", 1.0)];
    let fused = reciprocal_rank_fusion(&[list], &[1.0]);
    assert_eq!(fused[0].filepath, "a");
    assert_eq!(fused[1].filepath, "b");
    assert_eq!(fused[2].filepath, "c");
}

#[test]
fn rrf_top_rank_bonus_applied() {
    let list = vec![ranked("top", 10.0), ranked("mid", 5.0)];
    let fused = reciprocal_rank_fusion(&[list], &[1.0]);
    // "top" is at rank 0 → gets +0.05 bonus. Check it's still first.
    assert_eq!(fused[0].filepath, "top");
    // The top-rank bonus means "top"'s score > 1/(60+1+1) = 0.0164
    assert!(fused[0].backend_score > 0.05);
}

#[test]
fn rrf_original_query_weight_2x() {
    let meta = vec![
        RankedListMeta {
            source: "fts",
            query_type: QueryType::Original,
        },
        RankedListMeta {
            source: "fts",
            query_type: QueryType::Lex,
        },
    ];
    let weights = rrf_weights(&meta);
    assert_eq!(weights[0], 2.0);
    assert_eq!(weights[1], 1.0);
}

#[test]
fn rrf_k60_formula() {
    // Rank 0 in a single list with weight=1.0 → 1/(60+0+1) = 1/61 ≈ 0.0164
    // Plus top-rank bonus +0.05 → ≈ 0.0664
    let list = vec![ranked("a", 1.0)];
    let fused = reciprocal_rank_fusion(&[list], &[1.0]);
    let expected = 1.0 / 61.0 + 0.05;
    assert!((fused[0].backend_score - expected).abs() < 1e-6);
}

// ── SQLite DB layer ───────────────────────────────────────────────────────────

#[test]
fn db_upsert_and_retrieve() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir.path().join("test.sqlite")).unwrap();

    let body = "Hello, this is a test document.";
    let hash = content_hash(body);
    upsert_content(&db, &hash, body, "2024-01-01").unwrap();
    upsert_document(
        &db,
        "testcoll",
        "docs/hello.md",
        "Hello",
        &hash,
        "2024-01-01",
    )
    .unwrap();

    let doc = rqmd_core::db::get_document_by_filepath(&db, "testcoll", "docs/hello.md")
        .unwrap()
        .expect("document should exist");
    assert_eq!(doc.title, "Hello");
    assert_eq!(doc.collection, "testcoll");
    assert_eq!(doc.hash, hash);

    let content = rqmd_core::db::get_content(&db, &hash).unwrap().unwrap();
    assert_eq!(content, body);
}

// ── Context key round-trip ────────────────────────────────────────────────────

#[test]
fn context_check_key_matches_add_key() {
    // Regression guard: `rqmd context add rqmd://vault/ "..."` stores under the
    // key `context:rqmd://vault/`.  `context check` MUST query the same key or
    // it reports false MISSING (the rrqmd:// double-r typo, context.rs:71).
    let tmp = TempDir::new().unwrap();
    let conn = open_db(&tmp.path().join("store.db")).unwrap();

    // Simulate `context add rqmd://vault/ "..."` (verbatim key, no parsing).
    set_config(&conn, "context:rqmd://vault/", "Tyler's vault").unwrap();

    // The shared key builder must produce the exact same string.
    assert_eq!(collection_context_key("vault"), "context:rqmd://vault/");

    // And looking up via collection_context_key must find the stored value.
    assert!(
        get_config(&conn, &collection_context_key("vault"))
            .unwrap()
            .is_some(),
        "context_check key did not match the key written by context_add"
    );
}

#[test]
fn context_for_path_prefers_deepest_ancestor() {
    let tmp = TempDir::new().unwrap();
    let conn = open_db(&tmp.path().join("store.db")).unwrap();

    set_config(&conn, "context:rqmd://vault/", "root context").unwrap();
    set_config(
        &conn,
        "context:rqmd://vault/Cloud Engineering/",
        "cloud eng context",
    )
    .unwrap();
    set_config(
        &conn,
        "context:rqmd://vault/Cloud Engineering/Kubernetes/",
        "kubernetes context",
    )
    .unwrap();

    let ctx = get_context_for_path(&conn, "vault", "Cloud Engineering/Kubernetes/foo.md")
        .unwrap()
        .unwrap();
    assert_eq!(ctx, "kubernetes context");
}

#[test]
fn context_for_path_falls_back_to_shallower_ancestor() {
    let tmp = TempDir::new().unwrap();
    let conn = open_db(&tmp.path().join("store.db")).unwrap();

    set_config(
        &conn,
        "context:rqmd://vault/Cloud Engineering/",
        "cloud eng context",
    )
    .unwrap();

    // No context set for the exact "Kubernetes" ancestor — should fall back
    // to the shallower "Cloud Engineering" ancestor, not skip straight to root.
    let ctx = get_context_for_path(&conn, "vault", "Cloud Engineering/Kubernetes/foo.md")
        .unwrap()
        .unwrap();
    assert_eq!(ctx, "cloud eng context");
}

#[test]
fn context_for_path_falls_back_to_collection_root() {
    let tmp = TempDir::new().unwrap();
    let conn = open_db(&tmp.path().join("store.db")).unwrap();

    set_config(&conn, "context:rqmd://vault/", "root context").unwrap();

    let ctx = get_context_for_path(&conn, "vault", "Unmapped Area/foo.md")
        .unwrap()
        .unwrap();
    assert_eq!(ctx, "root context");
}

#[test]
fn context_for_path_falls_back_to_legacy_global() {
    let tmp = TempDir::new().unwrap();
    let conn = open_db(&tmp.path().join("store.db")).unwrap();

    set_config(&conn, "context:/", "legacy global context").unwrap();

    let ctx = get_context_for_path(&conn, "vault", "Unmapped Area/foo.md")
        .unwrap()
        .unwrap();
    assert_eq!(ctx, "legacy global context");
}

#[test]
fn context_for_path_none_when_nothing_configured() {
    let tmp = TempDir::new().unwrap();
    let conn = open_db(&tmp.path().join("store.db")).unwrap();

    let ctx = get_context_for_path(&conn, "vault", "Unmapped Area/foo.md").unwrap();
    assert!(ctx.is_none());
}

#[test]
fn db_upsert_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir.path().join("test.sqlite")).unwrap();

    let hash = content_hash("body text");
    upsert_content(&db, &hash, "body text", "t").unwrap();
    upsert_document(&db, "c", "p.md", "Title v1", &hash, "t").unwrap();
    upsert_document(&db, "c", "p.md", "Title v2", &hash, "t").unwrap(); // update

    let doc = rqmd_core::db::get_document_by_filepath(&db, "c", "p.md")
        .unwrap()
        .unwrap();
    assert_eq!(doc.title, "Title v2");
}

// ── Path / tokenization round-trip (qmd 2.6.3 parity check) ─────────────────────
//
// rqmd uses Tantivy (not SQLite FTS5, unlike qmd) and already normalizes paths
// through a single "collection/path" filepath string, so these bug classes are
// unlikely to reproduce here — these tests lock that in rather than fix a
// known defect.

fn test_store(dir: &TempDir) -> Store {
    let config = StoreConfig {
        db_path: dir.path().join("test.sqlite"),
        tantivy_dir: dir.path().join("tantivy"),
        hnsw_path: dir.path().join("hnsw.usearch"),
        read_only: false,
    };
    Store::open(config, rqmd_llm::no_backend()).unwrap()
}

#[test]
fn special_char_paths_round_trip() {
    let dir = TempDir::new().unwrap();
    let mut store = test_store(&dir);
    let collection = "coll";

    // Paths containing characters that are meaningful in URLs, globs, or shells.
    let cases = [
        ("notes/a#b.md", "Hash Path"),
        ("notes/a&b.md", "Ampersand Path"),
        ("notes/a b.md", "Space Path"),
        ("notes/a[b].md", "Bracket Path"),
        ("notes/a(b).md", "Paren Path"),
    ];

    for (path, title) in cases {
        let body = format!("Body for {title}");
        store
            .index_document_fts_only(collection, path, title, &body)
            .unwrap();
        store.flush().unwrap();

        // Round-trip by "collection/path" — same lookup `get` uses for non-docid input.
        let doc = get_document_by_filepath(&store.db, collection, path)
            .unwrap()
            .unwrap_or_else(|| panic!("document not found by path: {collection}/{path}"));
        assert_eq!(doc.path, path);
        assert_eq!(doc.title, title);

        // Round-trip by docid — same lookup `get` uses for "#abc123" input.
        let docid = docid_from_hash(&doc.hash);
        let by_id = get_document_by_docid_prefix(&store.db, docid)
            .unwrap()
            .unwrap_or_else(|| panic!("document not found by docid: {docid}"));
        assert_eq!(by_id.path, path);

        // Round-trip via BM25 search on the title term.
        let hits = store.search_fts(title, 5, None).unwrap();
        assert!(
            hits.iter().any(|h| h.path == path),
            "search for {title:?} did not return {path:?}: {hits:?}"
        );
    }
}

#[test]
fn search_fts_result_carries_nearest_ancestor_context() {
    // End-to-end regression guard for the root-only-context defect: a query
    // hit under a sub-directory must carry that sub-directory's context, not
    // just the collection-root context, once one is configured.
    let dir = TempDir::new().unwrap();
    let mut store = test_store(&dir);
    let collection = "vault";

    set_config(&store.db, "context:rqmd://vault/", "root context").unwrap();
    set_config(
        &store.db,
        "context:rqmd://vault/Cloud Engineering/Kubernetes/",
        "kubernetes context",
    )
    .unwrap();

    store
        .index_document_fts_only(
            collection,
            "Cloud Engineering/Kubernetes/pods.md",
            "Kubernetes Pods",
            "Pods are the smallest deployable units in Kubernetes.",
        )
        .unwrap();
    store
        .index_document_fts_only(
            collection,
            "Databases/postgres.md",
            "Postgres Notes",
            "Postgres is a relational database.",
        )
        .unwrap();
    store.flush().unwrap();

    let kube_hits = store.search_fts("Kubernetes", 5, None).unwrap();
    let kube_hit = kube_hits
        .iter()
        .find(|h| h.path == "Cloud Engineering/Kubernetes/pods.md")
        .expect("kubernetes doc should be found");
    assert_eq!(kube_hit.context.as_deref(), Some("kubernetes context"));

    // A hit in an area with no configured sub-dir context still falls back
    // to the collection root, matching pre-patch behavior for unmapped areas.
    let db_hits = store.search_fts("Postgres", 5, None).unwrap();
    let db_hit = db_hits
        .iter()
        .find(|h| h.path == "Databases/postgres.md")
        .expect("postgres doc should be found");
    assert_eq!(db_hit.context.as_deref(), Some("root context"));
}

#[test]
fn dotted_version_tokenizes_and_matches_bm25() {
    let dir = TempDir::new().unwrap();
    let mut store = test_store(&dir);

    store
        .index_document_fts_only(
            "coll",
            "releases/notes.md",
            "Release Notes",
            "Released version 2026.4.10 with bug fixes.",
        )
        .unwrap();
    store.flush().unwrap();

    let hits = store.search_fts("2026.4.10", 5, None).unwrap();
    assert!(
        hits.iter().any(|h| h.path == "releases/notes.md"),
        "BM25 search for dotted version '2026.4.10' returned no match: {hits:?}"
    );
}

// ── MCP multi-collection filter (`collection` → `collections`) ──────────────

#[test]
fn search_fts_multi_filters_to_requested_collections() {
    let dir = TempDir::new().unwrap();
    let mut store = test_store(&dir);

    for collection in ["alpha", "beta", "gamma"] {
        store
            .index_document_fts_only(
                collection,
                "doc.md",
                "Shared Term",
                "Every document mentions widget somewhere in its body.",
            )
            .unwrap();
    }
    store.flush().unwrap();

    // Omitted / None → searches every collection.
    let all = store.search_fts_multi("widget", 10, None).unwrap();
    assert_eq!(all.len(), 3, "expected all 3 collections, got {all:?}");

    // Multiple named collections → only those match.
    let two = ["alpha".to_string(), "beta".to_string()];
    let subset = store.search_fts_multi("widget", 10, Some(&two)).unwrap();
    assert_eq!(subset.len(), 2, "expected 2 collections, got {subset:?}");
    assert!(
        subset
            .iter()
            .all(|h| h.collection == "alpha" || h.collection == "beta")
    );
    assert!(!subset.iter().any(|h| h.collection == "gamma"));
}

#[test]
fn search_fts_multi_finds_minority_collection_despite_bulk_corpus() {
    // Regression guard: collection scoping used to truncate-then-filter — the
    // top-`limit` BM25 hits were collected globally, THEN filtered down to the
    // requested collection. A small target collection buried in a much larger
    // corpus could be squeezed entirely out of that global top-`limit` before
    // the filter ever ran, producing a false-empty result despite matching
    // documents existing.
    let dir = TempDir::new().unwrap();
    let mut store = test_store(&dir);

    for i in 0..50 {
        store
            .index_document_fts_only(
                "bulk",
                &format!("doc{i}.md"),
                "Bulk Doc",
                "widget widget widget",
            )
            .unwrap();
    }
    for i in 0..2 {
        store
            .index_document_fts_only("target", &format!("doc{i}.md"), "Target Doc", "widget")
            .unwrap();
    }
    store.flush().unwrap();

    let target = vec!["target".to_string()];
    let hits = store.search_fts_multi("widget", 5, Some(&target)).unwrap();
    assert_eq!(
        hits.len(),
        2,
        "expected both target-collection docs despite a 50-doc bulk collection: {hits:?}"
    );
    assert!(hits.iter().all(|h| h.collection == "target"));
}

#[test]
fn search_fts_lenient_parse_recovers_from_colon_syntax() {
    // Regression guard: a query fragment tantivy's default parser reads as a
    // field specifier (the colon in "error: connection refused" looks like
    // `field:value`) used to fail parsing outright, and that parse error
    // degraded to a silent empty `Ok(vec![])` with no diagnostic. Lenient
    // parsing degrades only the unparseable fragment instead of the whole
    // query.
    let dir = TempDir::new().unwrap();
    let mut store = test_store(&dir);

    store
        .index_document_fts_only(
            "coll",
            "log.md",
            "Log Entry",
            "error: connection refused while dialing upstream",
        )
        .unwrap();
    store.flush().unwrap();

    let hits = store
        .search_fts("error: connection refused", 5, None)
        .unwrap();
    assert!(
        !hits.is_empty(),
        "lenient parse should still surface a match for a colon-bearing query"
    );
}

#[test]
fn search_fts_multi_none_resolves_to_include_by_default_collections() {
    // Regression guard: `include_by_default = 0` was written and displayed by
    // collection management commands but never consulted by any query path —
    // `collection exclude` was a no-op that reported success while every
    // search continued to cover the "excluded" collection anyway.
    let dir = TempDir::new().unwrap();
    let mut store = test_store(&dir);

    store
        .index_document_fts_only("visible", "doc.md", "Visible Doc", "widget")
        .unwrap();
    store
        .index_document_fts_only("hidden", "doc.md", "Hidden Doc", "widget")
        .unwrap();
    store.flush().unwrap();

    rqmd_core::db::upsert_collection(
        &store.db,
        &rqmd_core::types::Collection {
            name: "visible".to_string(),
            path: "/tmp/visible".to_string(),
            pattern: "**/*.md".to_string(),
            ignore: vec![],
            include_by_default: true,
            update_command: None,
            allow_hidden: false,
        },
    )
    .unwrap();
    rqmd_core::db::upsert_collection(
        &store.db,
        &rqmd_core::types::Collection {
            name: "hidden".to_string(),
            path: "/tmp/hidden".to_string(),
            pattern: "**/*.md".to_string(),
            ignore: vec![],
            include_by_default: false,
            update_command: None,
            allow_hidden: false,
        },
    )
    .unwrap();

    // No explicit filter → only the default-included collection.
    let default_scope = store.search_fts_multi("widget", 10, None).unwrap();
    assert_eq!(
        default_scope.len(),
        1,
        "expected only the default-included collection: {default_scope:?}"
    );
    assert_eq!(default_scope[0].collection, "visible");

    // Explicit filter still reaches the excluded collection.
    let explicit = vec!["hidden".to_string()];
    let hits = store
        .search_fts_multi("widget", 10, Some(&explicit))
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].collection, "hidden");
}

#[test]
fn list_documents_multi_filters_to_requested_collections() {
    let dir = TempDir::new().unwrap();
    let mut store = test_store(&dir);

    for collection in ["alpha", "beta", "gamma"] {
        store
            .index_document_fts_only(collection, "doc.md", "Title", "body")
            .unwrap();
    }

    // Omitted / None → every collection.
    let all = rqmd_core::db::list_documents_multi(&store.db, None).unwrap();
    assert_eq!(all.len(), 3);

    // Named subset → only those collections.
    let two = ["alpha".to_string(), "gamma".to_string()];
    let subset = rqmd_core::db::list_documents_multi(&store.db, Some(&two)).unwrap();
    assert_eq!(subset.len(), 2);
    assert!(
        subset
            .iter()
            .all(|d| d.collection == "alpha" || d.collection == "gamma")
    );
}

// ── Repeatable `-c`/`--collection` CLI flag (Vec<String> instead of Option<String>) ──
//
// `rqmd query`/`search`/`vsearch`/`multi-get` used to declare `-c` as a
// single-valued `Option<String>`; clap silently kept only the *last* `-c`
// passed on the command line with no error, so `-c a -c b` searched only `b`.
// The FTS-only path already had coverage above (`search_fts_multi_*`); these
// tests give the vector-only (`search_vec`/`search_vec_multi`) and hybrid
// (`hybrid_query`/`hybrid_query_multi`) paths the same coverage, using a
// deterministic fake embedding backend so no GGUF model download is required.

/// Test-only `InferenceBackend`: embeds any text into a one-hot vector keyed
/// by a cheap byte-sum hash of the text's first whitespace-delimited token.
/// Documents sharing a leading "topic word" collide onto the same dimension
/// (cosine similarity 1.0 — "found it") even when the rest of their body
/// differs; distinct topic words land on different dimensions almost all the
/// time (near-orthogonal — "did not match"). Hashing only the first token
/// (rather than the whole text) lets test documents carry distinct bodies —
/// and thus distinct content hashes — while still matching a query on their
/// shared topic word; giving several documents byte-identical bodies would
/// dedupe them onto one `content_vectors` row (keyed on content hash) and
/// mask the multi-collection behavior these tests exist to check. This says
/// nothing about embedding quality and must never be used outside tests.
struct FakeEmbedBackend;

impl rqmd_llm::InferenceBackend for FakeEmbedBackend {
    fn embed(&mut self, text: &str) -> anyhow::Result<Vec<f32>> {
        let key = text.split_whitespace().next().unwrap_or(text);
        let mut v = vec![0.0_f32; rqmd_llm::EMBED_DIM];
        let idx = key
            .bytes()
            .fold(0usize, |acc, b| acc.wrapping_add(b as usize))
            % rqmd_llm::EMBED_DIM;
        v[idx] = 1.0;
        Ok(v)
    }

    fn rerank(&mut self, _query: &str, docs: &[&str]) -> anyhow::Result<Vec<f32>> {
        Ok(vec![0.0; docs.len()])
    }

    fn generate(&mut self, _prompt: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }

    fn embed_model_name(&self) -> &str {
        "fake"
    }

    fn rerank_model_name(&self) -> &str {
        "fake"
    }

    fn generate_model_name(&self) -> &str {
        "fake"
    }
}

fn test_store_with_vectors(dir: &TempDir) -> Store {
    let config = StoreConfig {
        db_path: dir.path().join("test.sqlite"),
        tantivy_dir: dir.path().join("tantivy"),
        hnsw_path: dir.path().join("hnsw.usearch"),
        read_only: false,
    };
    Store::open(config, Box::new(FakeEmbedBackend)).unwrap()
}

#[test]
fn search_vec_multi_filters_to_requested_collections() {
    let dir = TempDir::new().unwrap();
    let mut store = test_store_with_vectors(&dir);

    for collection in ["alpha", "beta", "gamma"] {
        store
            .index_document(
                collection,
                "doc.md",
                "Shared Term",
                &format!("widgetopic distinguishing content for {collection}"),
            )
            .unwrap();
    }
    store.flush().unwrap();

    // Omitted / None → searches every collection.
    let all = store.search_vec_multi("widgetopic", 10, None).unwrap();
    assert_eq!(all.len(), 3, "expected all 3 collections, got {all:?}");

    // Multiple named collections (`-c alpha -c beta`) → OR-matched, gamma excluded.
    let two = ["alpha".to_string(), "beta".to_string()];
    let subset = store
        .search_vec_multi("widgetopic", 10, Some(&two))
        .unwrap();
    assert_eq!(subset.len(), 2, "expected 2 collections, got {subset:?}");
    assert!(
        subset
            .iter()
            .all(|h| h.collection == "alpha" || h.collection == "beta")
    );
    assert!(!subset.iter().any(|h| h.collection == "gamma"));
}

#[test]
fn search_vec_single_collection_still_scopes_correctly() {
    // Regression guard: the singular `search_vec` wrapper (still used
    // internally, and the shape every prior caller relied on) must keep
    // filtering to exactly the requested collection now that it's a thin
    // wrapper over `search_vec_multi`.
    let dir = TempDir::new().unwrap();
    let mut store = test_store_with_vectors(&dir);

    store
        .index_document(
            "alpha",
            "doc.md",
            "Alpha Doc",
            "widgetopic content unique to alpha",
        )
        .unwrap();
    store
        .index_document(
            "beta",
            "doc.md",
            "Beta Doc",
            "widgetopic content unique to beta",
        )
        .unwrap();
    store.flush().unwrap();

    let hits = store.search_vec("widgetopic", 10, Some("alpha")).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].collection, "alpha");
}

#[test]
fn search_vec_multi_none_resolves_to_include_by_default_collections() {
    let dir = TempDir::new().unwrap();
    let mut store = test_store_with_vectors(&dir);

    store
        .index_document(
            "visible",
            "doc.md",
            "Visible Doc",
            "widgetopic content unique to visible",
        )
        .unwrap();
    store
        .index_document(
            "hidden",
            "doc.md",
            "Hidden Doc",
            "widgetopic content unique to hidden",
        )
        .unwrap();
    store.flush().unwrap();

    rqmd_core::db::upsert_collection(
        &store.db,
        &rqmd_core::types::Collection {
            name: "visible".to_string(),
            path: "/tmp/visible".to_string(),
            pattern: "**/*.md".to_string(),
            ignore: vec![],
            include_by_default: true,
            update_command: None,
            allow_hidden: false,
        },
    )
    .unwrap();
    rqmd_core::db::upsert_collection(
        &store.db,
        &rqmd_core::types::Collection {
            name: "hidden".to_string(),
            path: "/tmp/hidden".to_string(),
            pattern: "**/*.md".to_string(),
            ignore: vec![],
            include_by_default: false,
            update_command: None,
            allow_hidden: false,
        },
    )
    .unwrap();

    // No explicit filter (no `-c` at all) → only the default-included collection.
    let default_scope = store.search_vec_multi("widgetopic", 10, None).unwrap();
    assert_eq!(
        default_scope.len(),
        1,
        "expected only the default-included collection: {default_scope:?}"
    );
    assert_eq!(default_scope[0].collection, "visible");

    // Explicit filter still reaches the excluded collection.
    let explicit = vec!["hidden".to_string()];
    let hits = store
        .search_vec_multi("widgetopic", 10, Some(&explicit))
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].collection, "hidden");
}

#[test]
fn hybrid_query_multi_filters_to_requested_collections() {
    let dir = TempDir::new().unwrap();
    let mut store = test_store_with_vectors(&dir);

    for collection in ["alpha", "beta", "gamma"] {
        store
            .index_document(
                collection,
                "doc.md",
                "Shared Term",
                &format!("widgetopic distinguishing content for {collection}"),
            )
            .unwrap();
    }
    store.flush().unwrap();

    // `no_expand=true, skip_rerank=true` avoids the generate/rerank models this
    // fake backend doesn't meaningfully implement, exercising FTS + vector
    // retrieval and RRF fusion with the collection filter threaded through
    // both legs.
    let all = store
        .hybrid_query_multi("widgetopic", None, 10, None, true, true)
        .unwrap();
    assert_eq!(all.len(), 3, "expected all 3 collections, got {all:?}");

    let two = ["alpha".to_string(), "beta".to_string()];
    let subset = store
        .hybrid_query_multi("widgetopic", None, 10, Some(&two), true, true)
        .unwrap();
    assert_eq!(subset.len(), 2, "expected 2 collections, got {subset:?}");
    assert!(
        subset
            .iter()
            .all(|h| h.collection == "alpha" || h.collection == "beta")
    );
    assert!(!subset.iter().any(|h| h.collection == "gamma"));
}

#[test]
fn hybrid_query_multi_finds_minority_collection_despite_bulk_corpus() {
    // Vector-side counterpart to `search_fts_multi_finds_minority_collection_
    // despite_bulk_corpus`: the vector leg used to run `hnsw.search(emb,
    // fetch_size)` globally, THEN filter by collection — a large bulk
    // collection could fill the entire raw top-`fetch_size` before the
    // filter ever ran, starving a small scoped collection out of the vector
    // ranked list entirely. `search_vec_scoped` fixes this by widening the
    // raw ANN pool until enough in-scope hits are found.
    let dir = TempDir::new().unwrap();
    let mut store = test_store_with_vectors(&dir);

    for i in 0..40 {
        store
            .index_document(
                "bulk",
                &format!("doc{i}.md"),
                "Bulk Doc",
                "widgetopic bulk filler content",
            )
            .unwrap();
    }
    for i in 0..2 {
        store
            .index_document(
                "target",
                &format!("doc{i}.md"),
                "Target Doc",
                "widgetopic target distinguishing content",
            )
            .unwrap();
    }
    store.flush().unwrap();

    // no_expand=true, skip_rerank=true isolates the FTS+vector retrieval and
    // RRF fusion legs from the generate/rerank models this fake backend
    // doesn't meaningfully implement.
    let target = vec!["target".to_string()];
    let hits = store
        .hybrid_query_multi("widgetopic", None, 2, Some(&target), true, true)
        .unwrap();
    assert_eq!(
        hits.len(),
        2,
        "expected both target-collection docs despite a 40-doc bulk collection sharing the \
         same embedding: {hits:?}"
    );
    assert!(hits.iter().all(|h| h.collection == "target"));
}

// ── multi-get resolution hardening ────────────────────────────────────────────
//
// Regression guard for the previous unanchored `contains()` matcher: a bare
// fragment like "SYNTAX.md" used to also match "OLD-SYNTAX.md" and return the
// wrong document with no error. `find_documents_by_needles` / `resolve_multi_get`
// anchor at a `/` path-segment boundary instead.

#[test]
fn find_documents_by_needles_is_anchored_at_path_boundary() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir.path().join("test.sqlite")).unwrap();

    for (collection, path, title) in [
        ("docs", "SYNTAX.md", "Syntax"),
        ("docs", "OLD-SYNTAX.md", "Old Syntax"),
        ("docs", "guide/SYNTAX.md", "Nested Syntax"),
    ] {
        let hash = content_hash(path);
        upsert_content(&db, &hash, "body", "t").unwrap();
        upsert_document(&db, collection, path, title, &hash, "t").unwrap();
    }

    // "SYNTAX.md" must match the two paths ending in "/SYNTAX.md" or exactly
    // "SYNTAX.md" — but never "OLD-SYNTAX.md" (that's a substring, not a
    // segment-boundary suffix).
    let hits = find_documents_by_needles(&db, None, &["SYNTAX.md"]).unwrap();
    let paths: Vec<&str> = hits.iter().map(|d| d.path.as_str()).collect();
    assert!(paths.contains(&"SYNTAX.md"));
    assert!(paths.contains(&"guide/SYNTAX.md"));
    assert!(
        !paths.contains(&"OLD-SYNTAX.md"),
        "unanchored match regression: {paths:?}"
    );
}

/// Regression test: a needle containing a LIKE metacharacter (`%` or `_`)
/// must be treated as a literal, not a wildcard. Before this fix, a needle of
/// "%" widened `path LIKE '%/' || ?` into `LIKE '%/%'`, which matches any
/// `collection/path` string (every row has a `/`) — turning a targeted
/// `multi_get` lookup into a full corpus dump.
#[test]
fn find_documents_by_needles_treats_percent_and_underscore_as_literal() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir.path().join("test.sqlite")).unwrap();

    for (collection, path) in [("docs", "SYNTAX.md"), ("docs", "guide/SYNTAX.md")] {
        let hash = content_hash(path);
        upsert_content(&db, &hash, "body", "t").unwrap();
        upsert_document(&db, collection, path, "Title", &hash, "t").unwrap();
    }

    let hits = find_documents_by_needles(&db, None, &["%"]).unwrap();
    assert_eq!(
        hits.len(),
        0,
        "a bare '%' needle must not match every document: {hits:?}"
    );

    let hits = find_documents_by_needles(&db, None, &["_YNTAX.md"]).unwrap();
    assert_eq!(
        hits.len(),
        0,
        "a bare '_' must not act as a single-char wildcard: {hits:?}"
    );
}

#[test]
fn find_documents_by_needles_respects_collection_filter() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir.path().join("test.sqlite")).unwrap();

    for collection in ["alpha", "beta"] {
        let hash = content_hash(collection);
        upsert_content(&db, &hash, "body", "t").unwrap();
        upsert_document(&db, collection, "README.md", "Readme", &hash, "t").unwrap();
    }

    let only_alpha = ["alpha".to_string()];
    let hits = find_documents_by_needles(&db, Some(&only_alpha), &["README.md"]).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].collection, "alpha");
}

#[test]
fn resolve_multi_get_ors_across_multiple_collections() {
    // Regression guard for the CLI's `-c`/`--collection` flag becoming
    // repeatable: `rqmd multi-get -c alpha -c beta <pattern>` must OR-match
    // documents from both named collections while excluding a third,
    // unrequested collection — and passing no `-c` at all must still return
    // every collection (multi-get's `None` semantics are "no filter", unlike
    // the search paths' `include_by_default` fallback).
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir.path().join("test.sqlite")).unwrap();

    for collection in ["alpha", "beta", "gamma"] {
        let hash = content_hash(collection);
        upsert_content(&db, &hash, "body", "t").unwrap();
        upsert_document(&db, collection, "README.md", "Readme", &hash, "t").unwrap();
    }

    let two = ["alpha".to_string(), "beta".to_string()];
    let hits = resolve_multi_get(&db, Some(&two), "README.md").unwrap();
    let cols: HashSet<&str> = hits.iter().map(|d| d.collection.as_str()).collect();
    assert_eq!(cols.len(), 2, "expected 2 collections, got {cols:?}");
    assert!(cols.contains("alpha") && cols.contains("beta"));
    assert!(!cols.contains("gamma"));

    // No filter (no `-c` at all) → every collection.
    let all = resolve_multi_get(&db, None, "README.md").unwrap();
    assert_eq!(all.len(), 3);
}

#[test]
fn get_document_by_docid_prefix_is_deterministic_on_collision() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir.path().join("test.sqlite")).unwrap();

    // Two documents deliberately share a 6-hex-char hash prefix.
    let hash_a = "abcdef1111111111111111111111111111111111111111111111111111";
    let hash_z = "abcdef2222222222222222222222222222222222222222222222222222";
    upsert_content(&db, hash_a, "body a", "t").unwrap();
    upsert_content(&db, hash_z, "body z", "t").unwrap();
    upsert_document(&db, "zeta", "z.md", "Z", hash_z, "t").unwrap();
    upsert_document(&db, "alpha", "a.md", "A", hash_a, "t").unwrap();

    let first = get_document_by_docid_prefix(&db, "abcdef")
        .unwrap()
        .unwrap();
    let second = get_document_by_docid_prefix(&db, "abcdef")
        .unwrap()
        .unwrap();
    assert_eq!(first.collection, second.collection);
    assert_eq!(first.path, second.path);
    // Deterministic choice: lowest (collection, path) — "alpha/a.md" sorts first.
    assert_eq!(first.collection, "alpha");
    assert_eq!(first.path, "a.md");
}

#[test]
fn resolve_multi_get_combines_docid_glob_and_plain_patterns() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir.path().join("test.sqlite")).unwrap();

    let hash_a = content_hash("doc a");
    let hash_b = content_hash("doc b");
    let hash_c = content_hash("doc c");
    upsert_content(&db, &hash_a, "doc a", "t").unwrap();
    upsert_content(&db, &hash_b, "doc b", "t").unwrap();
    upsert_content(&db, &hash_c, "doc c", "t").unwrap();
    upsert_document(&db, "notes", "SYNTAX.md", "Syntax", &hash_a, "t").unwrap();
    upsert_document(&db, "notes", "OLD-SYNTAX.md", "Old Syntax", &hash_b, "t").unwrap();
    upsert_document(&db, "journal", "2025-05-01.md", "Journal", &hash_c, "t").unwrap();

    let docid_a = docid_from_hash(&hash_a);
    let pattern = format!("#{docid_a}, journal/2025-05*.md");
    let docs = resolve_multi_get(&db, None, &pattern).unwrap();
    let paths: Vec<&str> = docs.iter().map(|d| d.path.as_str()).collect();

    assert!(paths.contains(&"SYNTAX.md"));
    assert!(paths.contains(&"2025-05-01.md"));
    assert!(
        !paths.contains(&"OLD-SYNTAX.md"),
        "docid pattern must not pull in unrelated docs: {paths:?}"
    );
    assert_eq!(docs.len(), 2, "expected no duplicates: {docs:?}");
}

#[test]
fn reindex_same_filepath_deletes_old_fts_entry() {
    let dir = TempDir::new().unwrap();
    let mut store = test_store(&dir);
    let collection = "coll";
    let path = "notes/a.md";

    store
        .index_document_fts_only(collection, path, "Title", "uniquealphatoken")
        .unwrap();
    store.flush().unwrap();

    let before = store.search_fts("uniquealphatoken", 10, None).unwrap();
    assert_eq!(before.len(), 1, "sanity: first index should be findable");

    store
        .index_document_fts_only(collection, path, "Title", "uniquebetatoken")
        .unwrap();
    store.flush().unwrap();

    let old_hits = store.search_fts("uniquealphatoken", 10, None).unwrap();
    let new_hits = store.search_fts("uniquebetatoken", 10, None).unwrap();
    assert_eq!(
        old_hits.len(),
        0,
        "stale ghost entry for old body must not remain searchable after re-indexing the same filepath"
    );
    assert_eq!(new_hits.len(), 1);
}

// ── Deletions actually delete ──────────────────────────────────────────────────
//
// `update` used to hardcode "0 removed" — a deleted or renamed file's document
// row was never deactivated, so it stayed active, searchable, and pointing at a
// path that no longer existed on disk. `collection remove` only dropped the
// store_collections row, leaving documents/content/content_vectors/Tantivy
// entries fully intact. These tests lock in the fix at the db + Store level —
// the CLI commands (`run_update`, `collection remove`) are thin sequencing
// wrappers over exactly these calls.

#[test]
fn deactivate_missing_documents_soft_deletes_removed_and_keeps_present() {
    let dir = TempDir::new().unwrap();
    let mut store = test_store(&dir);
    let collection = "coll";

    store
        .index_document_fts_only(collection, "a.md", "A", "alpha body")
        .unwrap();
    store
        .index_document_fts_only(collection, "b.md", "B", "beta body")
        .unwrap();
    store.flush().unwrap();

    // Simulate a disk walk that no longer sees a.md (deleted or renamed away).
    let present: HashSet<String> = ["b.md".to_string()].into_iter().collect();
    let removed = deactivate_missing_documents(&store.db, collection, &present).unwrap();
    assert_eq!(removed, vec!["a.md".to_string()]);

    // Soft-deleted: gone from the active-only read path...
    let active = list_documents(&store.db, Some(collection)).unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].path, "b.md");

    // ...but the row itself still exists (recoverable, not hard-deleted).
    let still_present: i64 = store
        .db
        .query_row(
            "SELECT COUNT(*) FROM documents WHERE collection = ?1 AND path = 'a.md'",
            params![collection],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(still_present, 1, "soft delete must not drop the row");

    // Re-running with the same present set is a no-op — nothing left to deactivate.
    let removed_again = deactivate_missing_documents(&store.db, collection, &present).unwrap();
    assert!(removed_again.is_empty());
}

#[test]
fn orphan_vector_cleanup_evicts_only_hashes_with_no_remaining_active_reference() {
    // Regression for the gap where `deactivate_missing_documents` soft-deleted a
    // document but left its vectors in `content_vectors`/HNSW forever (only
    // reclaimed by a full `embed --rebuild`). Content is deduplicated globally by
    // hash, so a hash shared with another still-active document must survive.
    let dir = TempDir::new().unwrap();
    let mut store = test_store_with_vectors(&dir);

    let shared_body = "shared orphan-check content";
    let shared_hash = content_hash(shared_body);
    let unique_body = "unique orphan-check content";
    let unique_hash = content_hash(unique_body);

    upsert_content(&store.db, &shared_hash, shared_body, "2024-01-01T00:00:00Z").unwrap();
    upsert_content(&store.db, &unique_hash, unique_body, "2024-01-01T00:00:00Z").unwrap();

    // "keep" collection references shared_hash and stays untouched throughout.
    upsert_document(
        &store.db,
        "keep",
        "shared.md",
        "Shared",
        &shared_hash,
        "2024-01-01T00:00:00Z",
    )
    .unwrap();
    // "coll" collection also references shared_hash (will be deactivated below)
    // plus a unique_hash document with no other reference.
    upsert_document(
        &store.db,
        "coll",
        "shared.md",
        "Shared",
        &shared_hash,
        "2024-01-01T00:00:00Z",
    )
    .unwrap();
    upsert_document(
        &store.db,
        "coll",
        "unique.md",
        "Unique",
        &unique_hash,
        "2024-01-01T00:00:00Z",
    )
    .unwrap();

    let shared_vid = store.hnsw_size() as u64;
    upsert_vector_meta(
        &store.db,
        &shared_hash,
        0,
        0,
        "fake",
        "fp",
        1,
        shared_vid,
        "2024-01-01T00:00:00Z",
    )
    .unwrap();
    let unique_vid = shared_vid + 1;
    upsert_vector_meta(
        &store.db,
        &unique_hash,
        0,
        0,
        "fake",
        "fp",
        1,
        unique_vid,
        "2024-01-01T00:00:00Z",
    )
    .unwrap();

    // Simulate a walk of "coll" that no longer sees either file on disk.
    let present: HashSet<String> = HashSet::new();
    let removed = deactivate_missing_documents(&store.db, "coll", &present).unwrap();
    assert_eq!(removed.len(), 2);

    let candidate_hashes = rqmd_core::db::hashes_for_paths(&store.db, "coll", &removed).unwrap();
    let mut orphaned = Vec::new();
    for hash in candidate_hashes {
        if !rqmd_core::db::hash_referenced_by_active_document(&store.db, &hash).unwrap() {
            let vids = rqmd_core::db::vids_for_hash(&store.db, &hash).unwrap();
            store.evict_hnsw_vectors(&vids).unwrap();
            orphaned.push(hash);
        }
    }
    assert_eq!(
        orphaned,
        vec![unique_hash.clone()],
        "shared_hash must not be evicted — `keep` still actively references it"
    );
    for hash in &orphaned {
        rqmd_core::db::delete_vectors_for_hash(&store.db, hash).unwrap();
    }
    store.flush().unwrap();

    assert!(
        rqmd_core::db::hash_has_any_vector(&store.db, &shared_hash),
        "shared_hash's vector must survive"
    );
    assert!(
        !rqmd_core::db::hash_has_any_vector(&store.db, &unique_hash),
        "unique_hash's vector must be reclaimed"
    );
    assert_eq!(count_orphaned_vectors(&store.db).unwrap(), 0);
}

#[test]
fn update_prune_removes_stale_document_from_fts_search() {
    // End-to-end regression for the rename/delete ghost-path bug: after a file
    // disappears from the walked candidate set, it must both disappear from
    // SQLite's active view AND stop being returned by BM25 search — sweeping
    // only one of the two stores was the original defect.
    let dir = TempDir::new().unwrap();
    let mut store = test_store(&dir);
    let collection = "coll";

    store
        .index_document_fts_only(collection, "a.md", "A", "uniquegammatoken")
        .unwrap();
    store
        .index_document_fts_only(collection, "b.md", "B", "uniquedeltatoken")
        .unwrap();
    store.flush().unwrap();

    assert_eq!(
        store
            .search_fts("uniquegammatoken", 10, None)
            .unwrap()
            .len(),
        1
    );

    // a.md vanished from disk; b.md is still there.
    let present: HashSet<String> = ["b.md".to_string()].into_iter().collect();
    let removed = deactivate_missing_documents(&store.db, collection, &present).unwrap();
    assert_eq!(removed, vec!["a.md".to_string()]);
    for path in &removed {
        store
            .remove_from_fts(&format!("{collection}/{path}"))
            .unwrap();
    }
    store.flush().unwrap();

    let stale_hits = store.search_fts("uniquegammatoken", 10, None).unwrap();
    let live_hits = store.search_fts("uniquedeltatoken", 10, None).unwrap();
    assert_eq!(
        stale_hits.len(),
        0,
        "removed document must not remain searchable via Tantivy"
    );
    assert_eq!(
        live_hits.len(),
        1,
        "the still-present document must be unaffected"
    );
}

#[test]
fn purge_collection_hard_deletes_and_preserves_shared_content() {
    // Content is deduplicated globally by hash, so purging one collection must
    // not delete a `content` row that another collection's document still
    // references — only orphaned rows (referenced by no remaining document)
    // may be removed.
    let dir = TempDir::new().unwrap();
    let mut store = test_store(&dir);

    store
        .index_document_fts_only("keep", "shared.md", "Shared", "shared body text")
        .unwrap();
    store
        .index_document_fts_only("drop", "shared.md", "Shared", "shared body text")
        .unwrap();
    store
        .index_document_fts_only("drop", "unique.md", "Unique", "uniqueepsilontoken")
        .unwrap();
    store.flush().unwrap();

    let shared_hash = get_document_by_filepath(&store.db, "keep", "shared.md")
        .unwrap()
        .unwrap()
        .hash;
    let unique_hash = get_document_by_filepath(&store.db, "drop", "unique.md")
        .unwrap()
        .unwrap()
        .hash;

    let filepaths = purge_collection(&store.db, "drop").unwrap();
    assert_eq!(
        filepaths.iter().collect::<HashSet<_>>(),
        ["drop/shared.md".to_string(), "drop/unique.md".to_string()]
            .iter()
            .collect::<HashSet<_>>()
    );
    for filepath in &filepaths {
        store.remove_from_fts(filepath).unwrap();
    }
    store.flush().unwrap();

    // `drop` is fully gone: no documents, and its Tantivy entries are unreachable.
    assert!(list_documents(&store.db, Some("drop")).unwrap().is_empty());
    assert_eq!(
        store
            .search_fts("uniqueepsilontoken", 10, None)
            .unwrap()
            .len(),
        0
    );

    // `keep`'s copy of the shared content survives untouched.
    assert!(
        get_document_by_filepath(&store.db, "keep", "shared.md")
            .unwrap()
            .is_some()
    );
    let shared_content_count: i64 = store
        .db
        .query_row(
            "SELECT COUNT(*) FROM content WHERE hash = ?1",
            params![shared_hash],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        shared_content_count, 1,
        "content still referenced by `keep` must survive the purge of `drop`"
    );

    // The content unique to the purged collection is actually gone, not orphaned.
    let unique_content_count: i64 = store
        .db
        .query_row(
            "SELECT COUNT(*) FROM content WHERE hash = ?1",
            params![unique_hash],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        unique_content_count, 0,
        "content with no remaining document reference must be purged"
    );
}

#[test]
fn count_orphaned_vectors_counts_vectors_for_inactive_documents() {
    let dir = TempDir::new().unwrap();
    let mut store = test_store(&dir);
    let collection = "coll";

    store
        .index_document_fts_only(collection, "a.md", "A", "alpha body")
        .unwrap();
    store.flush().unwrap();
    let hash = get_document_by_filepath(&store.db, collection, "a.md")
        .unwrap()
        .unwrap()
        .hash;

    // Fake a previously-embedded vector for this document (no live backend needed
    // for this check — it exercises the counting query, not the embed pipeline).
    upsert_vector_meta(
        &store.db,
        &hash,
        0,
        0,
        "model",
        "fp",
        1,
        999,
        "2024-01-01T00:00:00Z",
    )
    .unwrap();
    assert_eq!(count_orphaned_vectors(&store.db).unwrap(), 0);

    // Once the only document referencing this hash is soft-deleted, its vector
    // becomes unreachable (every vector->document join requires active=1) but is
    // not physically removed until `embed --rebuild`.
    let present: HashSet<String> = HashSet::new();
    deactivate_missing_documents(&store.db, collection, &present).unwrap();
    assert_eq!(count_orphaned_vectors(&store.db).unwrap(), 1);
}

// ── Embed invalidation (chunk-strategy-version fingerprinting + supersede) ────

#[test]
fn embed_fingerprint_incorporates_chunk_strategy_version() {
    // `store::expected_embed_fingerprint` must differ from the pre-Phase-1 formula
    // (model + chunk size + overlap only) — otherwise a `CHUNK_STRATEGY_VERSION`
    // bump would silently fail to invalidate anything, reintroducing the bug this
    // phase fixes.
    let model = "fake-model";
    let actual = rqmd_core::store::expected_embed_fingerprint(model);

    let old_sig = format!(
        "model:{model}\nchunk_size_chars:{}\nchunk_overlap_chars:{}",
        rqmd_core::chunking::CHUNK_SIZE_CHARS,
        rqmd_core::chunking::CHUNK_OVERLAP_CHARS,
    );
    let old_hash = sha2::Sha256::digest(old_sig.as_bytes());
    let old_fingerprint = hex::encode(&old_hash[..3]);

    assert_ne!(
        actual, old_fingerprint,
        "fingerprint must change once chunk_strategy_version is folded in"
    );
}

#[test]
fn expected_embed_fingerprint_is_stable_across_calls() {
    let a = rqmd_core::store::expected_embed_fingerprint("fake-model");
    let b = rqmd_core::store::expected_embed_fingerprint("fake-model");
    assert_eq!(a, b);
}

#[test]
fn count_docs_needing_embed_counts_stale_fingerprint_as_pending() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);
    let collection = "coll";

    upsert_content(&store.db, "hash1", "some body", "2024-01-01T00:00:00Z").unwrap();
    upsert_document(
        &store.db,
        collection,
        "a.md",
        "A",
        "hash1",
        "2024-01-01T00:00:00Z",
    )
    .unwrap();

    let current = rqmd_core::store::expected_embed_fingerprint("fake");

    // No vectors at all yet — pending.
    assert_eq!(count_docs_needing_embed(&store.db, &current).unwrap(), 1);

    // A vector exists, but under a stale fingerprint — still pending.
    upsert_vector_meta(
        &store.db,
        "hash1",
        0,
        0,
        "fake",
        "stale-fp",
        1,
        1,
        "2024-01-01T00:00:00Z",
    )
    .unwrap();
    assert_eq!(
        count_docs_needing_embed(&store.db, &current).unwrap(),
        1,
        "a hash whose only vectors are stale must still count as pending"
    );

    // A vector exists under the current fingerprint — no longer pending.
    upsert_vector_meta(
        &store.db,
        "hash1",
        0,
        0,
        "fake",
        &current,
        1,
        2,
        "2024-01-01T00:00:00Z",
    )
    .unwrap();
    assert_eq!(count_docs_needing_embed(&store.db, &current).unwrap(), 0);
}

#[test]
fn hash_has_vector_with_fingerprint_distinguishes_stale_from_current() {
    let dir = TempDir::new().unwrap();
    let store = test_store(&dir);

    upsert_content(&store.db, "hash1", "body", "2024-01-01T00:00:00Z").unwrap();
    upsert_vector_meta(
        &store.db,
        "hash1",
        0,
        0,
        "fake",
        "stale-fp",
        1,
        1,
        "2024-01-01T00:00:00Z",
    )
    .unwrap();

    assert!(rqmd_core::db::hash_has_any_vector(&store.db, "hash1"));
    assert!(!rqmd_core::db::hash_has_vector_with_fingerprint(
        &store.db,
        "hash1",
        "current-fp"
    ));
    assert!(rqmd_core::db::hash_has_vector_with_fingerprint(
        &store.db, "hash1", "stale-fp"
    ));
}

#[test]
fn supersede_cycle_leaves_exactly_one_fingerprint_and_no_growth() {
    // Mirrors the exact sequence `rqmd embed`'s incremental loop performs when it
    // finds a hash with stale-fingerprint vectors: evict old vids from HNSW,
    // delete the old content_vectors rows, embed fresh chunks, insert new rows.
    // Anti-orphan assertion: the live vector count must not grow across the cycle.
    let dir = TempDir::new().unwrap();
    let mut store = test_store_with_vectors(&dir);
    let collection = "coll";
    let rel_path = "a.md";
    let body = "widgetopic content for the supersede cycle test";
    let hash = content_hash(body);

    upsert_content(&store.db, &hash, body, "2024-01-01T00:00:00Z").unwrap();
    upsert_document(
        &store.db,
        collection,
        rel_path,
        "A",
        &hash,
        "2024-01-01T00:00:00Z",
    )
    .unwrap();

    // Seed one stale-fingerprint vector for this hash, as if embedded before a
    // chunker/model change.
    let stale_vid = store.hnsw_size() as u64;
    upsert_vector_meta(
        &store.db,
        &hash,
        0,
        0,
        "fake",
        "stale-fp",
        1,
        stale_vid,
        "2024-01-01T00:00:00Z",
    )
    .unwrap();

    let current_fingerprint = rqmd_core::store::expected_embed_fingerprint("fake");
    assert!(rqmd_core::db::hash_has_any_vector(&store.db, &hash));
    assert!(!rqmd_core::db::hash_has_vector_with_fingerprint(
        &store.db,
        &hash,
        &current_fingerprint
    ));

    // Supersede: evict the stale vid from HNSW, delete its DB row, then embed fresh.
    let stale_vids = rqmd_core::db::vids_for_hash(&store.db, &hash).unwrap();
    assert_eq!(stale_vids, vec![stale_vid]);
    store.evict_hnsw_vectors(&stale_vids).unwrap();
    rqmd_core::db::delete_vectors_for_hash(&store.db, &hash).unwrap();

    let pending = store.embed_document_chunks(&hash, rel_path, body).unwrap();
    for m in &pending {
        upsert_vector_meta(
            &store.db,
            &m.hash,
            m.seq,
            m.pos,
            &m.model,
            &m.fingerprint,
            m.total_chunks,
            m.vid,
            &m.now,
        )
        .unwrap();
    }
    store.flush().unwrap();

    let live_count: i64 = store
        .db
        .query_row(
            "SELECT COUNT(*) FROM content_vectors WHERE hash = ?1",
            rusqlite::params![&hash],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        live_count,
        pending.len() as i64,
        "supersede must not leave the old row alongside the new ones"
    );
    assert!(rqmd_core::db::hash_has_vector_with_fingerprint(
        &store.db,
        &hash,
        &current_fingerprint
    ));
    assert_eq!(
        count_docs_needing_embed(&store.db, &current_fingerprint).unwrap(),
        0
    );
}

#[test]
fn reopened_store_never_reissues_a_vid_hard_deleted_by_eviction() {
    // Regression test for the production bug: MAX(content_vectors.vid) only reflects
    // *active* rows, so once the highest-vid hash is evicted (superseded, or a document
    // removed entirely) and the store is reopened, the old MAX-based floor under-counts.
    // `checkpoint()` now also persists the HNSW allocator's true high-water-mark via
    // `Store::next_vid()`/`NEXT_VID_CONFIG_KEY`, and `Store::open()` reconciles against
    // it as an additional floor — this must survive the hard delete.
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.sqlite");
    let tantivy_dir = dir.path().join("tantivy");
    let hnsw_path = dir.path().join("hnsw.usearch");
    let make_config = || StoreConfig {
        db_path: db_path.clone(),
        tantivy_dir: tantivy_dir.clone(),
        hnsw_path: hnsw_path.clone(),
        read_only: false,
    };

    let mut store = Store::open(make_config(), Box::new(FakeEmbedBackend)).unwrap();
    let collection = "coll";
    let rel_path = "a.md";
    let body = "content whose vectors will be hard-deleted";
    let hash = content_hash(body);

    upsert_content(&store.db, &hash, body, "2024-01-01T00:00:00Z").unwrap();
    upsert_document(
        &store.db,
        collection,
        rel_path,
        "A",
        &hash,
        "2024-01-01T00:00:00Z",
    )
    .unwrap();

    let pending = store.embed_document_chunks(&hash, rel_path, body).unwrap();
    assert!(!pending.is_empty());
    for m in &pending {
        upsert_vector_meta(
            &store.db,
            &m.hash,
            m.seq,
            m.pos,
            &m.model,
            &m.fingerprint,
            m.total_chunks,
            m.vid,
            &m.now,
        )
        .unwrap();
    }
    store.flush().unwrap();

    let highest_vid_ever_issued = store.next_vid();

    // Hard-delete: evict from HNSW and drop the DB rows, exactly like a real supersede
    // or collection-scoped removal. Also persist the high-water-mark, mirroring what
    // `checkpoint()` does in production.
    let vids = rqmd_core::db::vids_for_hash(&store.db, &hash).unwrap();
    store.evict_hnsw_vectors(&vids).unwrap();
    rqmd_core::db::delete_vectors_for_hash(&store.db, &hash).unwrap();
    set_config(
        &store.db,
        rqmd_core::store::NEXT_VID_CONFIG_KEY,
        &store.next_vid().to_string(),
    )
    .unwrap();
    store.flush().unwrap();
    drop(store);

    // Reopen fresh, exactly like a new `rqmd embed` process starting.
    let mut reopened = Store::open(make_config(), Box::new(FakeEmbedBackend)).unwrap();
    let new_body = "brand new content added after reopen";
    let new_hash = content_hash(new_body);
    upsert_content(&reopened.db, &new_hash, new_body, "2024-01-02T00:00:00Z").unwrap();
    upsert_document(
        &reopened.db,
        collection,
        "b.md",
        "B",
        &new_hash,
        "2024-01-02T00:00:00Z",
    )
    .unwrap();

    let new_pending = reopened
        .embed_document_chunks(&new_hash, "b.md", new_body)
        .unwrap();
    assert!(
        new_pending.iter().all(|m| m.vid >= highest_vid_ever_issued),
        "reopened store must never reissue a vid that was ever handed out, even after \
         the owning content_vectors rows were hard-deleted: got vids {:?}, floor was {}",
        new_pending.iter().map(|m| m.vid).collect::<Vec<_>>(),
        highest_vid_ever_issued
    );
}
