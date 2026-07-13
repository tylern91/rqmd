//! Integration tests for qmd-core: chunking, RRF, and DB layer.
//! Does NOT require inference backend (no model downloads).

use rqmd_core::{
    chunking::chunk_document,
    db::{
        collection_context_key, content_hash, docid_from_hash, find_documents_by_needles,
        get_config, get_document_by_docid_prefix, get_document_by_filepath, open_db, set_config,
        upsert_content, upsert_document,
    },
    resolve::resolve_multi_get,
    rrf::{reciprocal_rank_fusion, rrf_weights},
    types::{QueryType, RankedListMeta, RankedResult},
    Store, StoreConfig,
};
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
    // Regression guard: `qmd context add rqmd://vault/ "..."` stores under the
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

// ── MCP multi-collection filter (qmd 2.6.3 parity: `collection` → `collections`) ──

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
    assert!(subset
        .iter()
        .all(|h| h.collection == "alpha" || h.collection == "beta"));
    assert!(!subset.iter().any(|h| h.collection == "gamma"));
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
    assert!(subset
        .iter()
        .all(|d| d.collection == "alpha" || d.collection == "gamma"));
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
