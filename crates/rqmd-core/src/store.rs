//! Store — the main entry point for all rqmd-core operations.
//!
//! Orchestrates rusqlite (metadata), Tantivy (BM25), usearch (HNSW), and
//! the InferenceBackend (embed/rerank) to provide a hybrid search pipeline.

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use rqmd_llm::InferenceBackend;
use sha2::{Digest, Sha256};

use crate::{
    chunking::chunk_document,
    db::{
        self, content_hash, doc_for_vid, doc_for_vid_meta, docid_from_hash, get_content,
        get_context_for_path, open_db, upsert_content, upsert_document, upsert_vector_meta,
    },
    fts::FtsIndex,
    hnsw::VectorIndex,
    query::parse_query,
    rrf::{reciprocal_rank_fusion, rrf_weights},
    types::{QueryType, RankedListMeta, RankedResult, SearchResult},
};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Floor for the internal candidate-fetch/rerank pool size. The pool scales
/// with the caller's requested `limit` (`hybrid_query_multi` computes
/// `fetch_size` as roughly `limit * 2`) but never drops below this floor, and
/// is capped at 100 — rerank builds one fresh `LlamaContext` per candidate,
/// so its cost is linear in pool size.
const RERANK_CANDIDATE_LIMIT: usize = 20;

/// Cap on how many of a source document's chunks `similar_to_hash` will search
/// against the HNSW index. A very long document would otherwise pay one HNSW
/// search per chunk; callers are warned via `tracing::warn!` when this truncates.
const SIMILAR_MAX_CHUNKS: usize = 8;

/// BM25 strong-signal threshold — if the top normalized BM25 score exceeds this
/// and the gap to second place is ≥ STRONG_SIGNAL_MIN_GAP, skip LLM query expansion.
/// Values match qmd (src/store.ts:330-331); they operate on the [0,1) normalized score
/// produced by `Fts::search_fts` (raw Tantivy BM25 squashed via s/(1+s)).
const STRONG_SIGNAL_MIN_SCORE: f32 = 0.85; // qmd STRONG_SIGNAL_MIN_SCORE
const STRONG_SIGNAL_MIN_GAP: f32 = 0.15; // qmd STRONG_SIGNAL_MIN_GAP

/// Score blend weights for the final result: rerank_score * HI + rrf_score * LO.
const BLEND_HI: f32 = 0.75;
const BLEND_LO: f32 = 0.25;

// ── Store ─────────────────────────────────────────────────────────────────────

pub struct Store {
    pub db: Connection,
    fts: FtsIndex,
    hnsw: VectorIndex,
    backend: Box<dyn InferenceBackend>,
    hnsw_path: PathBuf,
}

pub struct StoreConfig {
    pub db_path: PathBuf,
    pub tantivy_dir: PathBuf,
    pub hnsw_path: PathBuf,
    /// Open the HNSW index as a memory-mapped, read-only view instead of
    /// loading it fully into RAM. Set this for query-only callers (search,
    /// get, status); indexing callers (embed, update, collection add) need
    /// `false` since a read-only index rejects `add`/`add_with_vid`/`save`.
    pub read_only: bool,
}

/// Outcome of a BM25-only index operation, used to drive honest update summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexOutcome {
    /// No existing row for this (collection, path).
    New,
    /// Row existed but the content hash changed — document was modified.
    Updated,
    /// Row existed and the content hash is identical — nothing changed.
    Unchanged,
}

/// Per-chunk embedding metadata buffered by `embed_document_chunks`.
/// Written to `content_vectors` only after the HNSW file has been flushed to disk.
#[derive(Debug)]
pub struct PendingVectorMeta {
    pub hash: String,
    pub seq: i64,
    pub pos: i64,
    pub model: String,
    pub fingerprint: String,
    pub total_chunks: i64,
    pub vid: u64,
    pub now: String,
}

impl Store {
    /// Open or create a store at the given paths.
    pub fn open(config: StoreConfig, backend: Box<dyn InferenceBackend>) -> Result<Self> {
        let db = open_db(&config.db_path)?;
        let fts = FtsIndex::open_or_create(&config.tantivy_dir)?;

        // Load HNSW index from disk if it exists, otherwise start fresh.
        // A failed load/view (corrupt file) emits a warning and starts empty —
        // callers must run `rqmd embed` to rebuild before vector search returns
        // results. read_only callers mmap the file instead of reading it fully
        // into RAM; indexing callers need the full load since they call
        // add/add_with_vid/save.
        let mut hnsw = if config.hnsw_path.exists() {
            let opened = if config.read_only {
                VectorIndex::view(&config.hnsw_path)
            } else {
                VectorIndex::load(&config.hnsw_path)
            };
            match opened {
                Ok(idx) => idx,
                Err(e) => {
                    eprintln!(
                        "rqmd: warning: HNSW index at '{}' could not be loaded ({e:#}). \
                         Vector search will return no results until you run `rqmd embed` \
                         to rebuild it.",
                        config.hnsw_path.display()
                    );
                    VectorIndex::new()?
                }
            }
        } else {
            VectorIndex::new()?
        };

        // Reconcile the HNSW allocator's next_vid against MAX(content_vectors.vid) in SQLite.
        // This guards against the case where the HNSW file and the DB diverge (corrupt/short
        // load falls back to next_vid=0, orphan-vid drift, etc.) — without this, embed()
        // re-issues vids that existing DB rows already hold, causing a UNIQUE constraint abort.
        if let Some(max_vid) = db::max_vector_vid(&db)? {
            hnsw.ensure_next_vid_at_least(max_vid + 1);
        }

        Ok(Self {
            db,
            fts,
            hnsw,
            backend,
            hnsw_path: config.hnsw_path,
        })
    }

    // ── Indexing ──────────────────────────────────────────────────────────────

    /// Index a single document: store content, chunk, embed, add to FTS + HNSW.
    pub fn index_document(
        &mut self,
        collection: &str,
        rel_path: &str,
        title: &str,
        body: &str,
    ) -> Result<()> {
        let now = rfc3339_now();
        let hash = content_hash(body);

        // 1. Upsert content + document record in rusqlite.
        upsert_content(&self.db, &hash, body, &now).context("upsert content")?;
        let doc_id = upsert_document(&self.db, collection, rel_path, title, &hash, &now)
            .context("upsert document")?;

        // 2. Add to Tantivy FTS. filepath = "collection/path".
        let filepath = format!("{collection}/{rel_path}");
        self.fts
            .add_document(&filepath, title, body, doc_id)
            .context("add to tantivy")?;

        // 3. Chunk + embed.
        let chunks = chunk_document(body);
        let total = chunks.len();
        let embed_model = self.backend.embed_model_name().to_string();
        let fingerprint = embed_fingerprint(&embed_model);
        let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
        let embeddings = self
            .backend
            .embed_batch_passage(&texts)
            .context("embed batch")?;

        for (seq, (chunk, embedding)) in chunks.iter().zip(embeddings.iter()).enumerate() {
            let vid = self.hnsw.add(embedding).context("hnsw add")?;
            upsert_vector_meta(
                &self.db,
                &hash,
                seq as i64,
                chunk.pos as i64,
                &embed_model,
                &fingerprint,
                total as i64,
                vid,
                &now,
            )
            .context("upsert vector meta")?;
        }

        Ok(())
    }

    /// Index a document for BM25 only — skips embedding. Useful for offline eval
    /// and commands that never run vector search (e.g. `rqmd eval --mode bm25`).
    ///
    /// Thin wrapper over [`Self::index_document_fts_only_with_raw`] that hashes
    /// and indexes the same text it stores — the historical behavior, preserved
    /// for callers (eval harness, existing tests) that have no raw/indexed split.
    pub fn index_document_fts_only(
        &mut self,
        collection: &str,
        rel_path: &str,
        title: &str,
        body: &str,
    ) -> Result<IndexOutcome> {
        self.index_document_fts_only_with_raw(collection, rel_path, title, body, body)
    }

    /// Index a document for BM25 only, hashing/searching `indexed_text` while
    /// storing `raw` as the retrievable content (`rqmd get` returns `raw`
    /// verbatim).
    ///
    /// Deliberately hashes `indexed_text`, not `raw`: a metadata-only edit to a
    /// document's frontmatter (e.g. an `updated:` timestamp bumped with no
    /// change to the body, tags, or aliases) does NOT change the content hash,
    /// so it does not force a full re-embed. The tradeoff is that the stored
    /// raw content — and, if only the frontmatter `title:` scalar changed, the
    /// stored title — can lag behind the file on disk by exactly the
    /// frontmatter block, until a change to the indexed text forces a fresh
    /// hash.
    ///
    /// Returns [`IndexOutcome`] so callers (e.g. `rqmd update`) can report
    /// accurate new / updated / unchanged counts rather than claiming
    /// everything as "updated".
    pub fn index_document_fts_only_with_raw(
        &mut self,
        collection: &str,
        rel_path: &str,
        title: &str,
        indexed_text: &str,
        raw: &str,
    ) -> Result<IndexOutcome> {
        let now = rfc3339_now();
        let hash = content_hash(indexed_text);

        // Classify the change before upserting so we can return an honest outcome.
        let outcome = match db::get_document_by_filepath(&self.db, collection, rel_path)
            .context("get document for classification")?
        {
            None => IndexOutcome::New,
            Some(existing) => {
                if existing.hash == hash {
                    IndexOutcome::Unchanged
                } else {
                    IndexOutcome::Updated
                }
            }
        };

        // Unchanged: content and Tantivy index are already correct — skip all writes.
        if outcome == IndexOutcome::Unchanged {
            return Ok(IndexOutcome::Unchanged);
        }

        upsert_content(&self.db, &hash, raw, &now).context("upsert content")?;
        let doc_id = upsert_document(&self.db, collection, rel_path, title, &hash, &now)
            .context("upsert document")?;
        let filepath = format!("{collection}/{rel_path}");
        self.fts
            .add_document(&filepath, title, indexed_text, doc_id)
            .context("add to tantivy")?;
        Ok(outcome)
    }

    /// Chunk and embed a document's body, add vectors to the in-memory HNSW index,
    /// and return the metadata needed to persist them (but do NOT write to the DB).
    ///
    /// Used by `rqmd embed` for incremental, resumable embedding:
    ///   1. Call this for each un-embedded doc — accumulates vids in HNSW memory.
    ///   2. Every N docs (and at the end) call `flush()` to persist HNSW to disk.
    ///   3. Only after flush succeeds, write the returned `PendingVectorMeta` rows to
    ///      content_vectors in one transaction.
    ///
    /// This ordering guarantees that an interrupt either leaves both the HNSW entry
    /// and the DB row present (safe to skip on resume), or neither (re-embed on next
    /// run).  It prevents the orphaned-vid problem that previously forced a full clear.
    pub fn embed_document_chunks(
        &mut self,
        hash: &str,
        body: &str,
    ) -> Result<Vec<PendingVectorMeta>> {
        let embed_model = self.backend.embed_model_name().to_string();
        let fingerprint = embed_fingerprint(&embed_model);
        let chunks = chunk_document(body);
        let total = chunks.len();
        let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
        let embeddings = self
            .backend
            .embed_batch_passage(&texts)
            .context("embed batch")?;
        let now = rfc3339_now();

        let mut pending = Vec::with_capacity(total);
        for (seq, (chunk, embedding)) in chunks.iter().zip(embeddings.iter()).enumerate() {
            let vid = self.hnsw.add(embedding).context("hnsw add")?;
            pending.push(PendingVectorMeta {
                hash: hash.to_string(),
                seq: seq as i64,
                pos: chunk.pos as i64,
                model: embed_model.clone(),
                fingerprint: fingerprint.clone(),
                total_chunks: total as i64,
                vid,
                now: now.clone(),
            });
        }
        Ok(pending)
    }

    /// Number of vectors currently in the HNSW index (mirrors the usearch file's entry count).
    /// Used by `rqmd embed` to detect HNSW/DB divergence.
    pub fn hnsw_size(&self) -> usize {
        self.hnsw.size()
    }

    /// Drop any GGUF model idle for at least `ttl`. Returns how many were released.
    /// `backend` is private, so this is the only way callers (e.g. `rqmd-mcp`'s
    /// idle-eviction sweep) can trigger it.
    pub fn release_idle_models(&mut self, ttl: Duration) -> usize {
        self.backend.release_idle(ttl)
    }

    /// Commit FTS writes and persist the HNSW index to disk.
    pub fn flush(&mut self) -> Result<()> {
        self.fts.commit().context("fts commit")?;
        self.hnsw.save(&self.hnsw_path).context("hnsw save")?;
        Ok(())
    }

    /// Remove a single filepath's entry from the Tantivy index (no-op if
    /// absent). `fts` is a private field, so this is the only way callers in
    /// other crates (`rqmd update`'s stale-document sweep, `collection
    /// remove`'s full purge) can reach [`FtsIndex::delete_by_filepath`].
    /// Callers must still call [`Self::flush`] afterward — this only stages
    /// the delete in the writer, it does not commit.
    pub fn remove_from_fts(&mut self, filepath: &str) -> Result<()> {
        self.fts.delete_by_filepath(filepath)
    }

    // ── Search ────────────────────────────────────────────────────────────────

    /// BM25 full-text search only (no vector, no rerank).
    ///
    /// Thin wrapper over `search_fts_multi` — a one-element slice reproduces this
    /// exact behavior.
    pub fn search_fts(
        &self,
        query: &str,
        limit: usize,
        collection: Option<&str>,
    ) -> Result<Vec<SearchResult>> {
        let owned = collection.map(|c| [c.to_string()]);
        self.search_fts_multi(query, limit, owned.as_ref().map(|a| a.as_slice()))
    }

    /// Same as `search_fts`, but matches any of several collections. An absent or
    /// empty filter resolves to the collections with `include_by_default = 1`
    /// (see `effective_collections`) rather than literally "every collection" —
    /// pass an explicit list to search collections regardless of that flag.
    pub fn search_fts_multi(
        &self,
        query: &str,
        limit: usize,
        collections: Option<&[String]>,
    ) -> Result<Vec<SearchResult>> {
        let effective = self.effective_collections(collections)?;
        if matches!(effective, Some(ref cols) if cols.is_empty()) {
            // No collection is default-included and none was requested explicitly.
            return Ok(vec![]);
        }
        let hits = self
            .fts
            .search_fts_multi(query, limit, effective.as_deref())?;
        self.hits_to_results(hits, limit)
    }

    /// Vector similarity search only (no BM25, no rerank).
    ///
    /// Thin wrapper over `search_vec_multi` — a one-element slice reproduces this
    /// exact behavior.
    pub fn search_vec(
        &mut self,
        query: &str,
        limit: usize,
        collection: Option<&str>,
    ) -> Result<Vec<SearchResult>> {
        let owned = collection.map(|c| [c.to_string()]);
        self.search_vec_multi(query, limit, owned.as_ref().map(|a| a.as_slice()))
    }

    /// Same as `search_vec`, but matches any of several collections. An absent or
    /// empty filter resolves to the collections with `include_by_default = 1`
    /// (see `effective_collections`) rather than literally "every collection" —
    /// pass an explicit list to search collections regardless of that flag.
    pub fn search_vec_multi(
        &mut self,
        query: &str,
        limit: usize,
        collections: Option<&[String]>,
    ) -> Result<Vec<SearchResult>> {
        let effective = self.effective_collections(collections)?;
        if matches!(effective, Some(ref cols) if cols.is_empty()) {
            return Ok(vec![]);
        }

        let embedding = self.backend.embed_query(query).context("embed query")?;
        let total_vectors = self.hnsw.size();

        // Widen the ANN candidate pool instead of relying on a fixed multiplier:
        // a fixed `limit * 4` over-fetch can still come back with too few
        // in-scope documents once filtered by collection and deduped down to
        // one row per document (a small minority collection can easily lose
        // out to a fixed-size raw top-k). Double `k` until enough in-scope
        // documents are found or the whole index has been searched.
        let mut k = limit.saturating_mul(4).max(1);
        let mut by_doc: HashMap<String, (f32, crate::types::Document, String)> = HashMap::new();

        loop {
            let raw = self.hnsw.search(&embedding, k)?;
            by_doc.clear();
            for (vid, sim) in raw {
                let Some((doc, body)) = doc_for_vid(&self.db, vid)? else {
                    continue;
                };
                if let Some(cols) = &effective {
                    if !cols.iter().any(|c| c == &doc.collection) {
                        continue;
                    }
                }
                let filepath = format!("{}/{}", doc.collection, doc.path);
                // Keep only the best-scoring chunk per document — a multi-chunk
                // document must appear once in results, not once per chunk.
                let keep = match by_doc.get(&filepath) {
                    Some((existing_sim, _, _)) => sim > *existing_sim,
                    None => true,
                };
                if keep {
                    by_doc.insert(filepath, (sim, doc, body));
                }
            }

            if by_doc.len() >= limit || k >= total_vectors {
                break;
            }
            k = (k * 2).min(total_vectors);
        }

        let mut ranked: Vec<(f32, crate::types::Document, String)> = by_doc.into_values().collect();
        ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(limit);

        let mut results = Vec::with_capacity(ranked.len());
        for (sim, doc, body) in ranked {
            let filepath = format!("{}/{}", doc.collection, doc.path);
            // Pick the first chunk as the snippet — no re-chunking needed.
            let chunks = chunk_document(&body);
            let best = chunks
                .into_iter()
                .next()
                .map(|c| c.text)
                .unwrap_or_default();
            let docid = docid_from_hash(&doc.hash).to_string();
            let ctx = get_context_for_path(&self.db, &doc.collection, &doc.path)
                .ok()
                .flatten();
            results.push(SearchResult {
                file: format!("rqmd://{filepath}"),
                title: doc.title.clone(),
                body,
                best_chunk: best,
                best_chunk_pos: 0,
                score: sim,
                docid,
                collection: doc.collection,
                path: doc.path,
                context: ctx,
            });
        }
        Ok(results)
    }

    /// Find documents most similar to an already-indexed document, identified by its
    /// content hash. Reads previously computed chunk vectors straight out of the HNSW
    /// index — no embedding model is invoked, so this works with a backend-less store
    /// (`open_store_no_backend`) and never loads a model.
    ///
    /// Searches once per source chunk (capped at `SIMILAR_MAX_CHUNKS`; a document with
    /// more chunks logs a warning naming the cap), keeping the best similarity per
    /// neighbour document — mirroring `search_vec`'s per-document dedup — and never
    /// returning the source document itself.
    pub fn similar_to_hash(
        &self,
        hash: &str,
        source_collection: &str,
        source_path: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let mut vids = db::vids_for_hash(&self.db, hash)?;
        if vids.is_empty() {
            bail!("no stored vectors for this document — run `rqmd embed` first");
        }
        if vids.len() > SIMILAR_MAX_CHUNKS {
            tracing::warn!(
                total_chunks = vids.len(),
                used = SIMILAR_MAX_CHUNKS,
                "rqmd similar: document has more chunks than the cap — using only the first {SIMILAR_MAX_CHUNKS}"
            );
            vids.truncate(SIMILAR_MAX_CHUNKS);
        }

        let source_filepath = format!("{source_collection}/{source_path}");
        let total_vectors = self.hnsw.size();
        let k = limit
            .saturating_mul(4)
            .saturating_add(vids.len())
            .max(limit + 1)
            .min(total_vectors.max(1));

        let mut by_doc: HashMap<String, (f32, crate::types::Document, String)> = HashMap::new();
        for vid in vids {
            let embedding = self.hnsw.get_vector(vid)?;
            let hits = self.hnsw.search(&embedding, k)?;
            for (hit_vid, sim) in hits {
                let Some((doc, body)) = doc_for_vid(&self.db, hit_vid)? else {
                    continue;
                };
                let filepath = format!("{}/{}", doc.collection, doc.path);
                if filepath == source_filepath {
                    continue;
                }
                let keep = match by_doc.get(&filepath) {
                    Some((existing_sim, _, _)) => sim > *existing_sim,
                    None => true,
                };
                if keep {
                    by_doc.insert(filepath, (sim, doc, body));
                }
            }
        }

        let mut ranked: Vec<(f32, crate::types::Document, String)> = by_doc.into_values().collect();
        ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(limit);

        let mut results = Vec::with_capacity(ranked.len());
        for (sim, doc, body) in ranked {
            let filepath = format!("{}/{}", doc.collection, doc.path);
            let chunks = chunk_document(&body);
            let best = chunks
                .into_iter()
                .next()
                .map(|c| c.text)
                .unwrap_or_default();
            let docid = docid_from_hash(&doc.hash).to_string();
            let ctx = get_context_for_path(&self.db, &doc.collection, &doc.path)
                .ok()
                .flatten();
            results.push(SearchResult {
                file: format!("rqmd://{filepath}"),
                title: doc.title.clone(),
                body,
                best_chunk: best,
                best_chunk_pos: 0,
                score: sim,
                docid,
                collection: doc.collection,
                path: doc.path,
                context: ctx,
            });
        }
        Ok(results)
    }

    /// Full hybrid search: BM25 + vector → optional HyDE expansion → RRF → chunk selection → rerank.
    ///
    /// `intent` provides optional context for expansion, reranking, and snippet selection.
    /// Pass `None` when no intent is available; any `intent:` line in a typed query document
    /// overrides this parameter.
    ///
    /// Thin wrapper over `hybrid_query_multi` — a one-element slice reproduces this
    /// exact behavior.
    pub fn hybrid_query(
        &mut self,
        query: &str,
        intent: Option<&str>,
        limit: usize,
        collection: Option<&str>,
        skip_rerank: bool,
        no_expand: bool,
    ) -> Result<Vec<SearchResult>> {
        let owned = collection.map(|c| [c.to_string()]);
        self.hybrid_query_multi(
            query,
            intent,
            limit,
            owned.as_ref().map(|a| a.as_slice()),
            skip_rerank,
            no_expand,
        )
    }

    /// Same as `hybrid_query`, but matches any of several collections. An absent
    /// or empty filter resolves to the collections with `include_by_default = 1`
    /// (see `effective_collections`) rather than literally "every collection" —
    /// pass an explicit list to search collections regardless of that flag.
    /// Backs the MCP server's `collections` filter.
    ///
    /// `no_expand` skips the LLM query-expansion/HyDE round-trip (step 3 below),
    /// leaving BM25 + vector retrieval and RRF fusion intact — a faster, pure
    /// hybrid-retrieval mode for callers who don't need the extra recall.
    pub fn hybrid_query_multi(
        &mut self,
        query: &str,
        intent: Option<&str>,
        limit: usize,
        collections: Option<&[String]>,
        skip_rerank: bool,
        no_expand: bool,
    ) -> Result<Vec<SearchResult>> {
        let effective = self.effective_collections(collections)?;
        if matches!(effective, Some(ref cols) if cols.is_empty()) {
            return Ok(vec![]);
        }
        let collections = effective.as_deref();

        // Internal candidate-fetch size for FTS/vector retrieval and the final
        // rerank pool: scales with the caller's requested `limit` instead of a
        // hardcoded constant, so e.g. `limit=25` isn't silently capped below
        // what was asked for. Capped at 100 — see `RERANK_CANDIDATE_LIMIT`.
        let requested_candidates = limit.saturating_mul(2);
        let fetch_size = requested_candidates.clamp(RERANK_CANDIDATE_LIMIT, 100);
        if requested_candidates > 100 {
            tracing::warn!(
                "query requested limit={limit} (wants {requested_candidates} rerank \
                 candidates); capping the candidate pool at {fetch_size} since rerank \
                 builds one LlamaContext per candidate and cost is linear in pool size"
            );
        }

        let mut ranked_lists: Vec<Vec<RankedResult>> = Vec::new();
        let mut list_meta: Vec<RankedListMeta> = Vec::new();

        // Parse the raw query per docs/SYNTAX.md.
        let parsed = parse_query(query);

        // Inline `intent:` from the query document takes precedence; fall back to the
        // parameter (from `--intent` CLI flag or MCP `intent` field).
        let effective_intent: Option<String> = parsed
            .intent
            .clone()
            .or_else(|| intent.map(|s| s.to_string()));

        // Build the rerank query: prepend intent if present.
        let rerank_query = match &effective_intent {
            Some(i) => format!("{i}\n{query}"),
            None => query.to_string(),
        };

        if !parsed.subqueries.is_empty() {
            // ── Query document mode ────────────────────────────────────────────
            // First sub-query gets weight 2.0 (Original); the rest get 1.0.
            for (idx, sub) in parsed.subqueries.iter().enumerate() {
                let qt = if idx == 0 {
                    QueryType::Original
                } else {
                    sub.qtype.clone()
                };
                match sub.qtype {
                    QueryType::Lex => {
                        let hits = self
                            .fts
                            .search_fts_multi(&sub.text, fetch_size, collections)?;
                        if !hits.is_empty() {
                            ranked_lists.push(fts_hits_to_ranked(&hits));
                            list_meta.push(RankedListMeta {
                                source: "fts",
                                query_type: qt,
                            });
                        }
                    }
                    QueryType::Vec => {
                        let emb = self
                            .backend
                            .embed_query(&sub.text)
                            .context("embed sub-query")?;
                        let hits = self.hnsw.search(&emb, fetch_size)?;
                        let results = self.vec_hits_to_ranked(hits, collections)?;
                        if !results.is_empty() {
                            ranked_lists.push(results);
                            list_meta.push(RankedListMeta {
                                source: "vec",
                                query_type: qt,
                            });
                        }
                    }
                    QueryType::Hyde => {
                        // HyDE embeds a hypothetical *document* the generation model wrote,
                        // not a query — it must go through the passage-side prompt so it
                        // lands in the same embedding subspace as the real document chunks
                        // it's meant to match, or the whole technique degrades to a
                        // worse-phrased query embedding.
                        let emb = self
                            .backend
                            .embed_passage(&sub.text)
                            .context("embed sub-query")?;
                        let hits = self.hnsw.search(&emb, fetch_size)?;
                        let results = self.vec_hits_to_ranked(hits, collections)?;
                        if !results.is_empty() {
                            ranked_lists.push(results);
                            list_meta.push(RankedListMeta {
                                source: "hyde",
                                query_type: qt,
                            });
                        }
                    }
                    QueryType::Original => {
                        // Not produced by the parser; defensive no-op.
                    }
                }
            }
        } else {
            // ── Expand mode ────────────────────────────────────────────────────
            let expand_text = parsed.expand_text.as_deref().unwrap_or(query);

            // Step 1: BM25 probe on the raw query.
            let initial_fts = self
                .fts
                .search_fts_multi(expand_text, fetch_size, collections)?;
            let top_score = initial_fts.first().map(|r| r.2).unwrap_or(0.0);
            let second_score = initial_fts.get(1).map(|r| r.2).unwrap_or(0.0);
            let strong_signal = !initial_fts.is_empty()
                && top_score >= STRONG_SIGNAL_MIN_SCORE
                && (top_score - second_score) >= STRONG_SIGNAL_MIN_GAP;

            if !initial_fts.is_empty() {
                ranked_lists.push(fts_hits_to_ranked(&initial_fts));
                list_meta.push(RankedListMeta {
                    source: "fts",
                    query_type: QueryType::Original,
                });
            }

            // Step 2: Embed original query for vector search.
            let query_embedding = self
                .backend
                .embed_query(expand_text)
                .context("embed query")?;
            let vec_hits = self.hnsw.search(&query_embedding, fetch_size)?;
            let vec_results = self.vec_hits_to_ranked(vec_hits, collections)?;
            if !vec_results.is_empty() {
                ranked_lists.push(vec_results);
                list_meta.push(RankedListMeta {
                    source: "vec",
                    query_type: QueryType::Original,
                });
            }

            // Step 3: Query expansion via generation model (skipped on strong BM25 signal,
            // or unconditionally when the caller passed `no_expand`).
            if !no_expand && !strong_signal {
                if !self.backend.capabilities().generate {
                    tracing::warn!(
                        "query expansion skipped: active inference backend does not support \
                         generation — falling back to BM25+vector fusion only"
                    );
                } else {
                    let prompt = build_expansion_prompt(expand_text, effective_intent.as_deref());
                    match self.backend.generate(&prompt) {
                        Ok(expansion) => {
                            let expansion_lists =
                                self.parse_and_run_expansion(&expansion, collections, fetch_size)?;
                            ranked_lists.extend(expansion_lists.0);
                            list_meta.extend(expansion_lists.1);
                        }
                        Err(e) => {
                            // Expansion is an enhancement — failures fall back to
                            // original BM25+vec results rather than surfacing an error.
                            tracing::warn!("query expansion skipped: {e:#}");
                        }
                    }
                }
            }
        }

        // Step 4: RRF fusion.
        if ranked_lists.is_empty() {
            return Ok(vec![]);
        }
        let weights = rrf_weights(&list_meta);
        let fused = reciprocal_rank_fusion(&ranked_lists, &weights);
        let candidates = &fused[..fetch_size.min(fused.len())];

        // Step 5: Resolve candidates to full documents.
        let mut candidate_docs: Vec<(RankedResult, String, String)> = Vec::new();
        for cand in candidates {
            if let Some((doc, body)) = self.filepath_to_doc_body(&cand.filepath)? {
                candidate_docs.push((cand.clone(), doc.hash, body));
            }
        }

        if candidate_docs.is_empty() {
            return Ok(vec![]);
        }

        // Step 6: Chunk selection — chunk each candidate once, reuse for both
        // the rerank input list and the final best_chunk / best_chunk_pos.
        // Fold intent terms into query terms for better snippet selection.
        let term_source = match &effective_intent {
            Some(i) => format!("{i} {query}"),
            None => query.to_string(),
        };
        let query_terms: Vec<String> = term_source
            .to_lowercase()
            .split_whitespace()
            .filter(|t| t.len() > 2)
            .map(|t| t.to_string())
            .collect();

        // Chunk once per candidate.
        let best_chunks: Vec<(String, usize)> = candidate_docs
            .iter()
            .map(|(_, _, body)| best_chunk(body, &query_terms))
            .collect();

        let chunk_refs: Vec<&str> = best_chunks.iter().map(|(t, _)| t.as_str()).collect();

        let rerank_scores: Option<Vec<f32>> = if skip_rerank {
            None
        } else if !self.backend.capabilities().rerank {
            tracing::warn!(
                "rerank skipped: active inference backend does not support reranking — \
                 results are BM25+vector fusion only"
            );
            None
        } else {
            // Use the intent-prepended rerank query for better cross-encoder scoring.
            self.backend.rerank(&rerank_query, &chunk_refs).ok()
        };

        let mut final_results = Vec::new();

        for (i, (cand, hash, body)) in candidate_docs.into_iter().enumerate() {
            let (chunk_text, chunk_pos) = best_chunks[i].clone();
            let rrf_score = cand.backend_score;
            let score = if let Some(ref rscores) = rerank_scores {
                let rs = rscores.get(i).copied().unwrap_or(rrf_score);
                BLEND_HI * rs + BLEND_LO * rrf_score
            } else {
                rrf_score
            };

            let (collection_name, rel_path) = split_filepath(&cand.filepath);
            let docid = docid_from_hash(&hash).to_string();
            let ctx = get_context_for_path(&self.db, collection_name, rel_path)
                .ok()
                .flatten();

            final_results.push(SearchResult {
                file: format!("rqmd://{}", cand.filepath),
                title: cand.title.clone(),
                body: body.clone(),
                best_chunk: chunk_text,
                best_chunk_pos: chunk_pos,
                score,
                docid,
                collection: collection_name.to_string(),
                path: rel_path.to_string(),
                context: ctx,
            });
        }

        final_results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        final_results.truncate(limit);
        Ok(final_results)
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn hits_to_results(
        &self,
        hits: Vec<(String, i64, f32)>,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let mut results = Vec::new();
        for (filepath, doc_id, score) in hits.into_iter().take(limit) {
            let doc = match db::get_document_by_id(&self.db, doc_id)? {
                Some(d) => d,
                None => continue,
            };
            let body = get_content(&self.db, &doc.hash)?.unwrap_or_default();
            let docid = docid_from_hash(&doc.hash).to_string();
            // For BM25-only results, use the first chunk as the snippet.
            let chunks = chunk_document(&body);
            let best = chunks
                .into_iter()
                .next()
                .map(|c| c.text)
                .unwrap_or_default();
            let (coll, path) = split_filepath(&filepath);
            let ctx = get_context_for_path(&self.db, coll, path).ok().flatten();
            results.push(SearchResult {
                file: format!("rqmd://{filepath}"),
                title: doc.title.clone(),
                body,
                best_chunk: best,
                best_chunk_pos: 0,
                score,
                docid,
                collection: coll.to_string(),
                path: path.to_string(),
                context: ctx,
            });
        }
        Ok(results)
    }

    fn vec_hits_to_ranked(
        &self,
        hits: Vec<(u64, f32)>,
        collections: Option<&[String]>,
    ) -> Result<Vec<RankedResult>> {
        let mut results = Vec::new();
        for (vid, sim) in hits {
            // Ranking only needs document identity, not the body — `doc_for_vid_meta`
            // skips the `content` join that `doc_for_vid` pays for on every candidate.
            if let Some(doc) = doc_for_vid_meta(&self.db, vid)? {
                if let Some(cols) = collections {
                    if !cols.is_empty() && !cols.iter().any(|c| c == &doc.collection) {
                        continue;
                    }
                }
                results.push(RankedResult {
                    filepath: format!("{}/{}", doc.collection, doc.path),
                    title: doc.title,
                    backend_score: sim,
                });
            }
        }
        Ok(results)
    }

    fn filepath_to_doc_body(
        &self,
        filepath: &str,
    ) -> Result<Option<(crate::types::Document, String)>> {
        let (collection, path) = split_filepath(filepath);
        let doc = match db::get_document_by_filepath(&self.db, collection, path)? {
            Some(d) => d,
            None => return Ok(None),
        };
        let body = get_content(&self.db, &doc.hash)?.unwrap_or_default();
        Ok(Some((doc, body)))
    }

    /// Parse the `lex:/vec:/hyde:` output of `generate_constrained` and run
    /// each expansion as a search, returning ranked lists + their metadata.
    ///
    /// Malformed or absent lines are silently skipped (expansion is best-effort).
    /// `fetch_size` is the caller's already-computed candidate-pool size (see
    /// `hybrid_query_multi`) — threaded through so expansion searches scale
    /// with the requested `limit` the same way the original-query searches do.
    fn parse_and_run_expansion(
        &mut self,
        expansion: &str,
        collections: Option<&[String]>,
        fetch_size: usize,
    ) -> Result<(Vec<Vec<RankedResult>>, Vec<RankedListMeta>)> {
        let mut lists: Vec<Vec<RankedResult>> = Vec::new();
        let mut metas: Vec<RankedListMeta> = Vec::new();

        for line in expansion.lines() {
            let line = line.trim();
            if let Some(text) = line.strip_prefix("lex:") {
                let text = text.trim();
                if !text.is_empty() {
                    let hits = self.fts.search_fts_multi(text, fetch_size, collections)?;
                    if !hits.is_empty() {
                        lists.push(fts_hits_to_ranked(&hits));
                        metas.push(RankedListMeta {
                            source: "expand-lex",
                            query_type: QueryType::Lex,
                        });
                    }
                }
            } else if let Some(text) = line.strip_prefix("vec:") {
                let text = text.trim();
                if !text.is_empty() {
                    let emb = self
                        .backend
                        .embed_query(text)
                        .context("embed vec expansion")?;
                    let hits = self.hnsw.search(&emb, fetch_size)?;
                    let results = self.vec_hits_to_ranked(hits, collections)?;
                    if !results.is_empty() {
                        lists.push(results);
                        metas.push(RankedListMeta {
                            source: "expand-vec",
                            query_type: QueryType::Vec,
                        });
                    }
                }
            } else if let Some(text) = line.strip_prefix("hyde:") {
                let text = text.trim();
                if !text.is_empty() {
                    // Passage-side prompt — see the QueryType::Hyde comment above for why.
                    let emb = self
                        .backend
                        .embed_passage(text)
                        .context("embed hyde expansion")?;
                    let hits = self.hnsw.search(&emb, fetch_size)?;
                    let results = self.vec_hits_to_ranked(hits, collections)?;
                    if !results.is_empty() {
                        lists.push(results);
                        metas.push(RankedListMeta {
                            source: "expand-hyde",
                            query_type: QueryType::Hyde,
                        });
                    }
                }
            }
        }

        Ok((lists, metas))
    }

    /// Resolve the collections a query should search when the caller passed no
    /// explicit filter (`None`, or an empty slice — the two are equivalent
    /// "unspecified" inputs throughout this API). An explicit, non-empty filter
    /// always wins outright, bypassing default-inclusion entirely.
    ///
    /// Otherwise, resolve to the collections with `include_by_default = 1` —
    /// this is the one place that flag is actually consulted; collection
    /// management commands previously only wrote and displayed it.
    ///
    /// Returns `Ok(None)` when every configured collection is included by
    /// default (or none are configured yet), so callers can skip scoping cost
    /// entirely. Returns `Ok(Some(list))` otherwise, where `list` may be empty
    /// if no collection is currently included by default — callers MUST treat
    /// that as "match nothing" and short-circuit, rather than forwarding the
    /// empty list on: to the FTS/vector search functions an empty slice means
    /// "no filter", which would silently widen back to "search everything".
    fn effective_collections(&self, requested: Option<&[String]>) -> Result<Option<Vec<String>>> {
        if let Some(cols) = requested {
            if !cols.is_empty() {
                return Ok(Some(cols.to_vec()));
            }
        }

        let all = db::list_collections(&self.db).context("list collections for default scope")?;
        if all.iter().all(|c| c.include_by_default) {
            return Ok(None);
        }
        Ok(Some(
            all.into_iter()
                .filter(|c| c.include_by_default)
                .map(|c| c.name)
                .collect(),
        ))
    }
}

// ── Module-level helpers ──────────────────────────────────────────────────────

/// Build a ChatML-style expansion prompt for Qwen3.
/// The system prompt requests exactly three lines: `lex:`, `vec:`, and `hyde:`.
/// Output is parsed leniently by `parse_and_run_expansion` (no grammar enforced).
fn build_expansion_prompt(query: &str, intent: Option<&str>) -> String {
    let intent_block = match intent {
        Some(i) if !i.is_empty() => format!("Context: {i}\n"),
        _ => String::new(),
    };
    format!(
        "<|im_start|>system\n\
         You are a search query expansion assistant. \
         Given a user query, emit exactly three lines:\n\
         lex: <keyword or phrase for BM25 search>\n\
         vec: <natural language question for vector search>\n\
         hyde: <a 50-100 word hypothetical passage that would answer the query>\n\
         Output only those three lines. No explanation.\
         <|im_end|>\n\
         <|im_start|>user\n\
         {intent_block}Query: {query}\
         <|im_end|>\n\
         <|im_start|>assistant\n"
    )
}

fn fts_hits_to_ranked(hits: &[(String, i64, f32)]) -> Vec<RankedResult> {
    hits.iter()
        .map(|(fp, _, score)| RankedResult {
            filepath: fp.clone(),
            title: String::new(),
            backend_score: *score,
        })
        .collect()
}

/// Split "collection/path/to/file.md" into ("collection", "path/to/file.md").
fn split_filepath(filepath: &str) -> (&str, &str) {
    filepath.split_once('/').unwrap_or((filepath, ""))
}

/// Pick the chunk with the most query-term overlap.
/// Returns `(text, char_offset)` — chunks once and returns both to avoid
/// duplicate work in the caller.
fn best_chunk(body: &str, query_terms: &[String]) -> (String, usize) {
    let chunks = chunk_document(body);
    if chunks.is_empty() {
        return (String::new(), 0);
    }
    if query_terms.is_empty() {
        let c = chunks.into_iter().next().unwrap();
        return (c.text, c.pos);
    }
    let best = chunks
        .into_iter()
        .max_by_key(|c| {
            let lower = c.text.to_lowercase();
            query_terms
                .iter()
                .filter(|t| lower.contains(t.as_str()))
                .count()
        })
        .unwrap();
    (best.text, best.pos)
}

/// Current UTC time as an RFC-3339 string (`YYYY-MM-DDTHH:MM:SSZ`).
///
/// Implemented without the `chrono` crate using civil-time arithmetic on the
/// POSIX epoch so that the `created_at`/`modified_at`/`embedded_at` columns
/// are human-readable ISO-8601.
pub fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_rfc3339(secs)
}

/// Convert a Unix timestamp (UTC) to `YYYY-MM-DDTHH:MM:SSZ`.
fn format_rfc3339(secs: u64) -> String {
    // Civil-time decomposition — no external dependency.
    let time_of_day = secs % 86_400;
    let days = secs / 86_400;
    let h = time_of_day / 3600;
    let m = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;

    // Gregorian calendar from epoch day (algorithm from H. F. Verhoeff / Richards).
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };

    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Embedding fingerprint: 6-hex-char hash of model name + chunk constants.
/// Used to detect stale embeddings after a model or chunking-strategy change.
///
/// Derives from the real `chunking::CHUNK_SIZE_CHARS`/`CHUNK_OVERLAP_CHARS` constants
/// rather than hardcoded literals, so a future change to those constants actually
/// invalidates the fingerprint instead of going undetected.
fn embed_fingerprint(model: &str) -> String {
    let sig = format!(
        "model:{model}\nchunk_size_chars:{}\nchunk_overlap_chars:{}",
        crate::chunking::CHUNK_SIZE_CHARS,
        crate::chunking::CHUNK_OVERLAP_CHARS,
    );
    let hash = Sha256::digest(sig.as_bytes());
    hex::encode(&hash[..3]) // 6 hex chars
}

/// Compute the fingerprint a fresh `rqmd embed` run would produce for the given
/// embed-model identity, without loading any model weights or contacting
/// HuggingFace. `embed_model_name` must match whatever the actually-configured
/// backend's `InferenceBackend::embed_model_name` reports — e.g.
/// `BackendKind::default_embed_model_name()` for the backend kind currently
/// selected — not a hardcoded single backend's defaults. Used by `rqmd doctor`
/// to tell current vectors from stale ones.
pub fn expected_embed_fingerprint(embed_model_name: &str) -> String {
    embed_fingerprint(embed_model_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_rfc3339_epoch() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn format_rfc3339_known() {
        // 2024-03-15T12:30:45Z = 1710505845 seconds since epoch
        // Verified: python3 -c "import datetime; print(int(datetime.datetime(2024,3,15,12,30,45,tzinfo=datetime.timezone.utc).timestamp()))"
        assert_eq!(format_rfc3339(1_710_505_845), "2024-03-15T12:30:45Z");
    }

    /// Re-indexing an existing (collection, path) must keep the same
    /// `documents.id` across updates, and the Store-level join must reflect
    /// the new content, not a stale or dropped result.
    ///
    /// Regression test for a compound bug: `db::upsert_document` returned
    /// `last_insert_rowid()`, which SQLite does not advance on the
    /// `ON CONFLICT DO UPDATE` arm — so it silently returned whichever
    /// unrelated row `upsert_content`'s immediately-preceding `INSERT` had
    /// just created, feeding a wrong `doc_id` into Tantivy on every content
    /// update. Combined with the (separately fixed) stale-Tantivy-entry bug,
    /// a query for the old content used to join to a bogus id — sometimes an
    /// unrelated document, sometimes nothing at all.
    #[test]
    fn reindex_same_path_keeps_stable_doc_id_and_correct_join() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig {
            db_path: dir.path().join("test.sqlite"),
            tantivy_dir: dir.path().join("tantivy"),
            hnsw_path: dir.path().join("hnsw.usearch"),
            read_only: false,
        };
        let mut store = Store::open(config, rqmd_llm::no_backend()).unwrap();

        let outcome1 = store
            .index_document_fts_only("coll", "notes/a.md", "Title", "uniquealphatoken")
            .unwrap();
        assert_eq!(outcome1, IndexOutcome::New);
        store.flush().unwrap();

        let outcome2 = store
            .index_document_fts_only("coll", "notes/a.md", "Title", "uniquebetatoken")
            .unwrap();
        assert_eq!(outcome2, IndexOutcome::Updated);
        store.flush().unwrap();

        assert_eq!(
            store.fts.reader.searcher().num_docs(),
            1,
            "the stale entry from the first index must not linger as a ghost"
        );

        let raw_alpha = store.fts.search_fts("uniquealphatoken", 10, None).unwrap();
        let raw_beta = store.fts.search_fts("uniquebetatoken", 10, None).unwrap();
        assert_eq!(raw_alpha.len(), 0, "old content must no longer match");
        assert_eq!(raw_beta[0].1, 1, "doc_id must stay stable across re-index");

        let store_alpha = store.search_fts("uniquealphatoken", 10, None).unwrap();
        let store_beta = store.search_fts("uniquebetatoken", 10, None).unwrap();
        assert_eq!(store_alpha.len(), 0);
        assert_eq!(store_beta.len(), 1);
        assert_eq!(store_beta[0].body, "uniquebetatoken");
    }

    /// A stub `InferenceBackend` with a configurable identity and capability
    /// set. `rerank`/`generate` panic rather than error — call sites must
    /// check `capabilities()` and skip the call entirely, not attempt-and-discard.
    struct StubBackend {
        name: String,
        caps: rqmd_llm::BackendCapabilities,
    }

    impl rqmd_llm::InferenceBackend for StubBackend {
        fn capabilities(&self) -> rqmd_llm::BackendCapabilities {
            self.caps
        }
        fn embed(&mut self, _text: &str) -> Result<Vec<f32>> {
            // Constant vector: any two calls are identical, so index-time and
            // query-time embeddings always cosine-match at 1.0 regardless of text.
            Ok(vec![0.1; rqmd_llm::EMBED_DIM])
        }
        fn rerank(&mut self, _query: &str, _docs: &[&str]) -> Result<Vec<f32>> {
            panic!("rerank must not be called when capabilities().rerank is false")
        }
        fn generate(&mut self, _prompt: &str) -> Result<String> {
            panic!("generate must not be called when capabilities().generate is false")
        }
        fn embed_model_name(&self) -> &str {
            &self.name
        }
        fn rerank_model_name(&self) -> &str {
            "stub-rerank"
        }
        fn generate_model_name(&self) -> &str {
            "stub-generate"
        }
    }

    fn open_stub_store(dir: &std::path::Path, backend: StubBackend) -> Store {
        let config = StoreConfig {
            db_path: dir.join("test.sqlite"),
            tantivy_dir: dir.join("tantivy"),
            hnsw_path: dir.join("hnsw.usearch"),
            read_only: false,
        };
        Store::open(config, Box::new(backend)).unwrap()
    }

    /// Regression test for the "embeddings are stale" false-positive: the
    /// expected fingerprint must be computed from whichever backend identity
    /// actually did the embedding, not a hardcoded `LlamaCppConfig::default()`.
    #[test]
    fn expected_fingerprint_matches_whatever_backend_actually_embedded() {
        let dir = tempfile::TempDir::new().unwrap();
        let backend = StubBackend {
            name: "stub-org/stub-embed.onnx".to_string(),
            caps: rqmd_llm::BackendCapabilities {
                embed: true,
                rerank: true,
                generate: true,
            },
        };
        let mut store = open_stub_store(dir.path(), backend);

        store
            .index_document("coll", "a.md", "Title", "hello world")
            .unwrap();

        let breakdown = db::fingerprint_breakdown(&store.db).unwrap();
        assert_eq!(breakdown.len(), 1);
        let recorded_fp = &breakdown[0].0;

        // Matches when computed from the SAME identity the stub actually used.
        let expected_for_stub = expected_embed_fingerprint("stub-org/stub-embed.onnx");
        assert_eq!(recorded_fp, &expected_for_stub);

        // Must NOT match the llama.cpp default identity — otherwise this
        // assertion would be vacuously true regardless of which backend embedded.
        let llama_default = expected_embed_fingerprint(&format!(
            "{}/{}",
            rqmd_llm::DEFAULT_EMBED_REPO,
            rqmd_llm::DEFAULT_EMBED_FILE
        ));
        assert_ne!(recorded_fp, &llama_default);
    }

    /// Regression test for silent query-quality degradation: when the active
    /// backend doesn't support rerank/generate, `hybrid_query_multi` must skip
    /// those steps (not attempt-and-discard) and still return results.
    #[test]
    fn hybrid_query_skips_unsupported_capabilities_without_calling_them() {
        let dir = tempfile::TempDir::new().unwrap();
        let backend = StubBackend {
            name: "embed-only-backend".to_string(),
            caps: rqmd_llm::BackendCapabilities {
                embed: true,
                rerank: false,
                generate: false,
            },
        };
        let mut store = open_stub_store(dir.path(), backend);

        store
            .index_document("coll", "a.md", "Title", "hello world")
            .unwrap();
        store.flush().unwrap();

        // Query shares no BM25 tokens with the doc (weak/no FTS signal, so
        // `strong_signal` is false and the generate-expansion step is attempted)
        // but embeds identically via the stub, so vector search still matches.
        let results = store
            .hybrid_query_multi("zzzznonexistentqueryterm", None, 10, None, false, false)
            .unwrap();

        assert_eq!(results.len(), 1, "vector match must still surface a result");
    }
}
