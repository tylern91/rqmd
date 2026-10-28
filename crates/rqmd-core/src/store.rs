//! Store — the main entry point for all rqmd-core operations.
//!
//! Orchestrates rusqlite (metadata), Tantivy (BM25), usearch (HNSW), and
//! the InferenceBackend (embed/rerank) to provide a hybrid search pipeline.

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, hash_map::Entry};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rqmd_llm::{BackendKind, InferenceBackend};
use sha2::{Digest, Sha256};

use crate::{
    chunking::{chunk_document_for_path, first_chunk},
    db::{
        self, content_hash, doc_chunk_for_vid_in, doc_for_vid_meta, doc_for_vid_meta_in,
        docid_from_hash, get_context_for_path, open_db, upsert_content, upsert_document,
        upsert_vector_meta,
    },
    deadline::Deadline,
    fts::FtsIndex,
    hnsw::VectorIndex,
    query::parse_query,
    rrf::{reciprocal_rank_fusion, rrf_weights},
    types::{
        AnnPass, Chunk, QueryOutcome, QueryStage, QueryTimings, QueryType, RankedListMeta,
        RankedResult, Recency, RerankStatus, SearchOptions, SearchResult, clamp_per_file,
    },
};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Floor for the internal candidate-fetch/rerank pool size. The pool scales
/// with the caller's requested `limit` (`hybrid_query_multi` computes
/// `fetch_size` as roughly `limit * 2`) but never drops below this floor, and
/// is capped at 100 — rerank builds one fresh `LlamaContext` per candidate,
/// so its cost is linear in pool size.
const RERANK_CANDIDATE_LIMIT: usize = 20;

/// Most chunks one rerank pass scores: the candidate pool's own ceiling. Sections
/// beyond a file's best one are only added while the pass stays within it.
const RERANK_POOL_CEILING: usize = 100;

/// Candidates per reranker call while a query observer is set, so progress
/// can be reported between calls. Scores do not depend on the slice size:
/// the reranker scores each (query, candidate) pair on its own.
const RERANK_PROGRESS_SLICE: usize = 4;

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

/// `store_config` key under which the HNSW allocator's high-water-mark `next_vid` is
/// persisted on every checkpoint (see `Self::next_vid` and `Self::open`'s reconciliation).
pub const NEXT_VID_CONFIG_KEY: &str = "next_vid";

/// `store_config` key recording which FTS tokenizer built the Tantivy index.
pub const FTS_TOKENIZER_CONFIG_KEY: &str = "fts_tokenizer_version";

/// Bump whenever the FTS analyzer changes what a given text tokenizes to: the
/// index then no longer matches what queries produce, and
/// [`Store::rebuild_fts_if_needed`] re-indexes it. `2` = CJK bigrams,
/// `3` = weighted `frontmatter` field.
pub const FTS_TOKENIZER_VERSION: &str = "3";

/// The tags and aliases in `raw`'s frontmatter, space-separated, for the
/// weighted `frontmatter` full-text field. Empty when there is none.
fn frontmatter_terms(raw: &str) -> String {
    crate::frontmatter::split(raw)
        .0
        .map(|fm| fm.terms.join(" "))
        .unwrap_or_default()
}

// ── Store ─────────────────────────────────────────────────────────────────────

/// The hybrid search engine — opens the SQLite/Tantivy/HNSW triad and exposes
/// indexing and search over them. See [`crate`] for the full pipeline description.
///
/// ```no_run
/// use rqmd_core::{SearchResult, Store, StoreConfig};
/// use rqmd_llm::InferenceBackend;
///
/// fn example(config: StoreConfig, backend: Box<dyn InferenceBackend>) -> anyhow::Result<()> {
///     let mut store = Store::open(config, backend)?;
///
///     // Index a document
///     store.index_document("collection", "rel/path.md", "Title", "body text")?;
///
///     // Hybrid search: BM25 + vector + RRF + rerank
///     let results: Vec<SearchResult> =
///         store.hybrid_query("search terms", None, 10, None, false, false)?;
///
///     // BM25 keyword search
///     let results = store.search_fts("keyword", 10, None)?;
///
///     // Vector similarity search
///     let results = store.search_vec("semantic query", 10, None)?;
///     # let _ = results;
///
///     Ok(())
/// }
/// ```
pub struct Store {
    pub db: Connection,
    fts: FtsIndex,
    hnsw: VectorIndex,
    backend: Box<dyn InferenceBackend>,
    hnsw_path: PathBuf,
    /// Tantivy's `meta.json` — rewritten atomically on every `commit()`, so
    /// its mtime is the reliable staleness signal for the FTS side. The main
    /// `.sqlite` file is *not* a substitute: this database runs in WAL mode
    /// (`db.rs`'s `PRAGMA journal_mode = WAL`), and ordinary writes land in
    /// the `-wal` sidecar without touching the main file's mtime until a
    /// checkpoint happens — which can lag far behind the writes a resident
    /// reader needs to notice.
    tantivy_meta_path: PathBuf,
    /// mtimes observed at `open()` (or the last `reload_if_stale()`), used to
    /// detect when another process has written to the index since. `None`
    /// means the file didn't exist at that point.
    last_seen_tantivy_meta_mtime: Option<SystemTime>,
    last_seen_hnsw_mtime: Option<SystemTime>,
    /// Vectors added by `embed_document_chunks` whose rows `rqmd embed` has not
    /// committed yet, by reuse key. Lets a later document in the same batch
    /// reuse them; see [`Self::vectors_committed`].
    staged_vectors: HashMap<String, u64>,
    /// Set whenever `hnsw.add`/`add_with_vid` stages an unsaved vector; reset
    /// once `flush()` saves it (or `reload_if_stale()` replaces `self.hnsw`
    /// with a fresh view that has no unsaved work of its own). Lets `flush()`
    /// skip `hnsw.save()` — a rewrite of the full usearch file — on a pass
    /// that added no vectors.
    hnsw_dirty: bool,
    /// Told which stage a hybrid query has reached; see [`Store::set_query_observer`].
    query_observer: Option<Box<dyn FnMut(QueryStage) + Send>>,
    /// Per-stage timings of hybrid queries; `None` (the default) means nothing
    /// is measured. See [`Store::set_query_timings`].
    timings: Option<RefCell<QueryTimings>>,
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

/// What [`Store::rename_collection`] changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenameSummary {
    /// Documents of the renamed collection re-indexed for keyword search.
    pub documents: usize,
    /// Full-text entries removed that belonged to no collection's active
    /// documents and not to the old name (left by an earlier rename).
    pub stale_removed: usize,
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
    /// Hash of the chunk's text, stored so a later edit of the document can
    /// reuse this vector for chunks that did not change.
    pub chunk_hash: String,
    /// Reuse key of the vector (see [`vector_key`]), stored beside `chunk_hash`.
    pub vector_key: String,
}

/// Add every embedding to `index`, or none: if an add fails partway, the vids
/// already added are passed to `remove` so no vector is left in the index
/// without a `content_vectors` row pointing at it.
fn add_all_or_rollback<T>(
    index: &mut T,
    embeddings: &[Vec<f32>],
    add: impl Fn(&mut T, &[f32]) -> Result<u64>,
    remove: impl Fn(&mut T, u64),
) -> Result<Vec<u64>> {
    let mut vids = Vec::with_capacity(embeddings.len());
    for embedding in embeddings {
        match add(index, embedding) {
            Ok(vid) => vids.push(vid),
            Err(e) => {
                for vid in vids {
                    remove(index, vid);
                }
                return Err(e);
            }
        }
    }
    Ok(vids)
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

        // Also reconcile against the persisted high-water-mark counter (`db::checkpoint`
        // writes it on every checkpoint). Unlike `MAX(content_vectors.vid)`, this floor is
        // monotonic and survives hard-deletes of `content_vectors` rows, so it still holds
        // after a vid's owning document has been superseded or evicted.
        if let Some(persisted_next_vid) =
            db::get_config(&db, NEXT_VID_CONFIG_KEY)?.and_then(|s| s.parse::<u64>().ok())
        {
            hnsw.ensure_next_vid_at_least(persisted_next_vid);
        }

        let tantivy_meta_path = config.tantivy_dir.join("meta.json");
        let last_seen_tantivy_meta_mtime = mtime_of(&tantivy_meta_path);
        let last_seen_hnsw_mtime = mtime_of(&config.hnsw_path);

        Ok(Self {
            db,
            fts,
            hnsw,
            backend,
            hnsw_path: config.hnsw_path,
            tantivy_meta_path,
            last_seen_tantivy_meta_mtime,
            last_seen_hnsw_mtime,
            hnsw_dirty: false,
            staged_vectors: HashMap::new(),
            query_observer: None,
            timings: None,
        })
    }

    /// Reload the FTS reader and re-`view()` the HNSW index if either has
    /// changed on disk since `open()` (or the last call to this method).
    /// Returns whether a reload happened.
    ///
    /// This exists for long-lived, read-only callers — namely the MCP
    /// daemon — that open a [`Store`] once and keep it resident across many
    /// tool calls without ever indexing through it themselves. Without this,
    /// such a caller serves whatever snapshot existed at startup forever:
    /// the Tantivy reader was built with `ReloadPolicy::Manual` (only
    /// `commit()` reloads it, and this Store never commits), and the HNSW
    /// index was opened via `VectorIndex::view()`, a frozen mmap snapshot.
    /// A separate process running `rqmd index`/`update`/`embed` against the
    /// same paths would otherwise never become visible without restarting
    /// the daemon.
    ///
    /// Cheap to call on every tool invocation: it's two `stat()` calls in
    /// the common case where nothing changed.
    pub fn reload_if_stale(&mut self) -> Result<bool> {
        let tantivy_meta_mtime = mtime_of(&self.tantivy_meta_path);
        let hnsw_mtime = mtime_of(&self.hnsw_path);

        if tantivy_meta_mtime == self.last_seen_tantivy_meta_mtime
            && hnsw_mtime == self.last_seen_hnsw_mtime
        {
            return Ok(false);
        }

        self.fts.reader.reload().context("reload fts reader")?;
        self.hnsw = if self.hnsw_path.exists() {
            // A torn/zero-length file (another process crashed mid-flush) must not
            // wedge this reader — mirror `Store::open`'s warn-and-fall-back-to-empty
            // behavior instead of propagating the error.
            match VectorIndex::view(&self.hnsw_path) {
                Ok(idx) => idx,
                Err(e) => {
                    eprintln!(
                        "rqmd: warning: HNSW index at '{}' could not be reloaded ({e:#}). \
                         Vector search will return no results until the index is rebuilt.",
                        self.hnsw_path.display()
                    );
                    VectorIndex::new()?
                }
            }
        } else {
            VectorIndex::new()?
        };

        self.last_seen_tantivy_meta_mtime = tantivy_meta_mtime;
        self.last_seen_hnsw_mtime = hnsw_mtime;
        self.hnsw_dirty = false;
        Ok(true)
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

        // 1. Upsert content + document record in rusqlite. No indexed/raw split
        // here (unlike index_document_fts_only_with_raw) — body serves both roles.
        upsert_content(&self.db, &hash, body, &now).context("upsert content")?;
        let doc_id = upsert_document(&self.db, collection, rel_path, title, &hash, &now)
            .context("upsert document")?;
        db::set_document_raw(&self.db, doc_id, body).context("set document raw")?;

        // 2. Add to Tantivy FTS. filepath = "collection/path".
        let filepath = format!("{collection}/{rel_path}");
        self.fts
            .add_document(&filepath, title, body, doc_id)
            .context("add to tantivy")?;

        // 3. Chunk + embed.
        let chunks = chunk_document_for_path(rel_path, body);
        let total = chunks.len();
        let embed_model = self.backend.embed_model_name().to_string();
        let fingerprint = embed_fingerprint(&embed_model, rel_path);
        let chunk_hashes: Vec<String> = chunks.iter().map(|c| chunk_text_hash(&c.text)).collect();
        let vector_keys: Vec<String> = chunk_hashes.iter().map(|h| self.vector_key(h)).collect();
        let embeddings = self.embed_passages_reusing(&chunks, &vector_keys, &embed_model)?;

        for (seq, (chunk, embedding)) in chunks.iter().zip(embeddings.iter()).enumerate() {
            let vid = self.hnsw.add(embedding).context("hnsw add")?;
            self.hnsw_dirty = true;
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
            db::set_vector_identity(
                &self.db,
                &hash,
                seq as i64,
                &chunk_hashes[seq],
                &vector_keys[seq],
            )
            .context("record chunk identity")?;
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

        // `content.doc` is keyed by `hash = content_hash(indexed_text)`, so it
        // must hold `indexed_text` — the bytes the hash actually identifies —
        // not `raw`. `upsert_content` overwrites on conflict, so this also
        // self-heals rows written before the raw/indexed-text split, where a
        // shared hash's first writer could donate its raw text to every other
        // document on that hash. `raw` is stored per-document instead, via
        // `set_document_raw`, so retrieval still returns each document's own
        // verbatim file. Both writes run unconditionally — including when
        // `outcome == Unchanged` — since `raw` can legitimately go stale (a
        // frontmatter-only edit doesn't change `hash`) and pre-split indexes
        // need this pass to repair `content.doc` without a forced reindex.
        upsert_content(&self.db, &hash, indexed_text, &now).context("upsert content")?;

        if outcome == IndexOutcome::Unchanged {
            let existing = db::get_document_by_filepath(&self.db, collection, rel_path)
                .context("get document for raw backfill")?
                .context("document vanished between classification and backfill")?;
            db::set_document_raw(&self.db, existing.id, raw).context("set document raw")?;
            return Ok(IndexOutcome::Unchanged);
        }

        let doc_id = upsert_document(&self.db, collection, rel_path, title, &hash, &now)
            .context("upsert document")?;
        db::set_document_raw(&self.db, doc_id, raw).context("set document raw")?;
        let filepath = format!("{collection}/{rel_path}");
        self.fts
            .add_document_with_frontmatter(
                &filepath,
                title,
                indexed_text,
                &frontmatter_terms(raw),
                doc_id,
            )
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
        rel_path: &str,
        body: &str,
    ) -> Result<Vec<PendingVectorMeta>> {
        let embed_model = self.backend.embed_model_name().to_string();
        let fingerprint = embed_fingerprint(&embed_model, rel_path);
        let chunks = chunk_document_for_path(rel_path, body);
        let total = chunks.len();
        let chunk_hashes: Vec<String> = chunks.iter().map(|c| chunk_text_hash(&c.text)).collect();
        let vector_keys: Vec<String> = chunk_hashes.iter().map(|h| self.vector_key(h)).collect();
        let embeddings = self.embed_passages_reusing(&chunks, &vector_keys, &embed_model)?;
        let now = rfc3339_now();

        let vids = add_all_or_rollback(
            &mut self.hnsw,
            &embeddings,
            |hnsw, embedding| hnsw.add(embedding).context("hnsw add"),
            |hnsw, vid| {
                let _ = hnsw.remove(vid);
            },
        )?;
        self.hnsw_dirty = true;
        for (key, vid) in vector_keys.iter().zip(&vids) {
            self.staged_vectors.entry(key.clone()).or_insert(*vid);
        }

        Ok(chunks
            .iter()
            .zip(vids)
            .enumerate()
            .map(|(seq, (chunk, vid))| PendingVectorMeta {
                hash: hash.to_string(),
                seq: seq as i64,
                pos: chunk.pos as i64,
                model: embed_model.clone(),
                fingerprint: fingerprint.clone(),
                total_chunks: total as i64,
                vid,
                now: now.clone(),
                chunk_hash: chunk_hashes[seq].clone(),
                vector_key: vector_keys[seq].clone(),
            })
            .collect())
    }

    /// One embedding per chunk, in order. A chunk whose reuse key already has a
    /// stored or staged vector gets a copy of it; only the remaining chunks are
    /// encoded. A reusable vector the index can no longer return is treated as
    /// missing rather than as an error.
    fn embed_passages_reusing(
        &mut self,
        chunks: &[crate::types::Chunk],
        vector_keys: &[String],
        model: &str,
    ) -> Result<Vec<Vec<f32>>> {
        let stored = db::reusable_vector_vids(&self.db, vector_keys, model)
            .context("look up reusable chunk vectors")?;
        let mut slots: Vec<Option<Vec<f32>>> = vector_keys
            .iter()
            .map(|k| {
                [self.staged_vectors.get(k), stored.get(k)]
                    .into_iter()
                    .flatten()
                    .find_map(|vid| self.hnsw.get_vector(*vid).ok())
            })
            .collect();
        let missing: Vec<usize> = (0..slots.len()).filter(|i| slots[*i].is_none()).collect();
        if !missing.is_empty() {
            let texts: Vec<&str> = missing.iter().map(|i| chunks[*i].text.as_str()).collect();
            let fresh = self
                .backend
                .embed_batch_passage(&texts)
                .context("embed batch")?;
            if fresh.len() != missing.len() {
                bail!(
                    "embed backend returned {} vectors for {} chunks",
                    fresh.len(),
                    missing.len()
                );
            }
            for (i, embedding) in missing.into_iter().zip(fresh) {
                slots[i] = Some(embedding);
            }
        }
        Ok(slots.into_iter().flatten().collect())
    }

    /// [`vector_key`] for a chunk, under the configured backend.
    fn vector_key(&self, chunk_hash: &str) -> String {
        vector_key(
            self.backend.embed_backend_kind(),
            self.backend.embed_model_name(),
            self.backend.embed_context_size(),
            chunk_hash,
        )
    }

    /// Give vectors an earlier release stored a `vector_key`, from their
    /// recorded chunk hash and without encoding anything. Only vectors embedded
    /// under the configured model at a fingerprint this build would produce
    /// qualify, because that is what shows they were made with the current
    /// passage prefix; anything else stays unkeyed and is never reused.
    /// Returns the number of rows filled.
    pub fn backfill_vector_keys(&mut self) -> Result<usize> {
        let kind = self.backend.embed_backend_kind().to_string();
        let model = self.backend.embed_model_name().to_string();
        let context_size = self.backend.embed_context_size();
        let fingerprints: Vec<String> = std::iter::once(embed_fingerprint(&model, ""))
            .chain(
                expected_embed_ast_fingerprints(&model)
                    .into_iter()
                    .map(|g| g.fingerprint),
            )
            .collect();
        let fingerprints: Vec<&str> = fingerprints.iter().map(String::as_str).collect();
        db::backfill_vector_keys(&self.db, &model, &fingerprints, |chunk_hash| {
            vector_key(&kind, &model, context_size, chunk_hash)
        })
    }

    /// The rows of every vector `embed_document_chunks` returned so far are now
    /// in `content_vectors`, so lookups find them there.
    pub fn vectors_committed(&mut self) {
        self.staged_vectors.clear();
    }

    /// Number of vectors currently in the HNSW index (mirrors the usearch file's entry count).
    /// Used by `rqmd embed` to detect HNSW/DB divergence.
    pub fn hnsw_size(&self) -> usize {
        self.hnsw.size()
    }

    /// The HNSW allocator's current high-water-mark `next_vid`. `hnsw` is a private field,
    /// so this is the only way callers (`checkpoint` in `rqmd-cli`) can persist it to
    /// `store_config` as a durable floor — see [`NEXT_VID_CONFIG_KEY`].
    pub fn next_vid(&self) -> u64 {
        self.hnsw.next_vid()
    }

    /// Drop any GGUF model idle for at least `ttl`. Returns how many were released.
    /// `backend` is private, so this is the only way callers (e.g. `rqmd-mcp`'s
    /// idle-eviction sweep) can trigger it.
    pub fn release_idle_models(&mut self, ttl: Duration) -> usize {
        self.backend.release_idle(ttl)
    }

    /// Commit FTS writes and persist the HNSW index to disk.
    ///
    /// Both halves are skipped when there's nothing to persist: `fts.commit()`
    /// is a no-op when no document was staged, and `hnsw.save()` is skipped
    /// entirely when no vector was added since the last flush — avoiding a
    /// full usearch-file rewrite (and, for FTS, the exclusive Tantivy writer
    /// lock) on a pass where every file hashed `Unchanged`.
    pub fn flush(&mut self) -> Result<()> {
        self.fts.commit().context("fts commit")?;
        if self.hnsw_dirty {
            self.hnsw
                .save_atomic(&self.hnsw_path)
                .context("hnsw save")?;
            self.hnsw_dirty = false;
        }
        Ok(())
    }

    /// Evict superseded vectors from the in-memory HNSW index (e.g. a hash's
    /// old chunks under a stale `embed_fingerprint`). Marks the index dirty
    /// so the removal is persisted on the next [`Self::flush`] — `hnsw` is a
    /// private field, so this is the only way callers in other crates can
    /// reach [`crate::hnsw::VectorIndex::remove`]. Errors propagate: a failed
    /// removal here means the in-memory index and the DB's view of which
    /// vids are live have diverged, which must not be silently ignored.
    pub fn evict_hnsw_vectors(&mut self, vids: &[u64]) -> Result<()> {
        if vids.is_empty() {
            return Ok(());
        }
        for &vid in vids {
            self.hnsw.remove(vid)?;
        }
        self.hnsw_dirty = true;
        Ok(())
    }

    /// Evict every currently-orphaned vector from HNSW and delete its
    /// `content_vectors` row. Shares `db::orphaned_vector_hashes`'s predicate
    /// with `rqmd doctor`'s orphan count, so the two can never disagree on
    /// what counts as orphaned. Safe to call repeatedly (a no-op once nothing
    /// is orphaned) and safe to call from any write path — `rqmd update`'s
    /// post-collection sweep, `embed --cleanup`, and callers under test all
    /// share this instead of re-deriving the eviction logic. Returns the
    /// number of `content_vectors` rows deleted.
    pub fn reclaim_orphaned_vectors(&mut self) -> Result<usize> {
        let orphaned_hashes =
            crate::db::orphaned_vector_hashes(&self.db).context("list orphaned hashes")?;
        if orphaned_hashes.is_empty() {
            return Ok(0);
        }
        for hash in &orphaned_hashes {
            let vids = crate::db::vids_for_hash(&self.db, hash)?;
            self.evict_hnsw_vectors(&vids)?;
        }
        // Durability barrier: HNSW must be persisted before the DB rows
        // pointing at those vids are deleted.
        self.flush()?;
        let mut swept = 0usize;
        let tx = self.db.transaction()?;
        for hash in &orphaned_hashes {
            swept += crate::db::delete_vectors_for_hash(&tx, hash)?;
        }
        tx.commit().context("commit orphaned vector sweep")?;
        Ok(swept)
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

    /// Rename a collection: the SQLite rows and path contexts (see
    /// [`db::rename_collection`]) and the collection's full-text entries, which
    /// are stored under `"collection/path"`. Vectors are keyed by content hash,
    /// so nothing is re-embedded.
    ///
    /// Every full-text entry that no active document accounts for is removed
    /// first, which covers the old name's entries and any left by an earlier
    /// rename; the renamed collection's documents are then indexed again under
    /// the new name. All deletes are staged before any insert. The SQLite changes commit only after the full-text
    /// writes are staged; call this from a writer that holds the index lock.
    pub fn rename_collection(&mut self, old: &str, new: &str) -> Result<RenameSummary> {
        let tx = self.db.unchecked_transaction()?;
        db::rename_collection(&tx, old, new)?;

        const ROW_SQL: &str = "SELECT d.collection || '/' || d.path, d.title, c.doc, d.id, d.raw \
             FROM documents d JOIN content c ON c.hash = d.hash WHERE d.active = 1 AND ";
        let read_row = |r: &rusqlite::Row<'_>| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        };
        let rows = tx
            .prepare(&format!("{ROW_SQL}d.collection = ?1"))?
            .query_map([new], read_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let documents = rows.len();

        let live = db::active_filepaths(&tx)?;
        let old_prefix = format!("{old}/");
        let mut stale_removed = 0;
        let mut deletes: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
        for filepath in self.fts.indexed_filepaths()? {
            if live.contains(&filepath) {
                continue;
            }
            if !filepath.starts_with(&old_prefix) {
                stale_removed += 1;
            }
            deletes.push(filepath);
        }

        for filepath in &deletes {
            self.fts.delete_by_filepath(filepath)?;
        }

        for (filepath, title, doc, id, raw) in &rows {
            self.fts
                .insert_document(filepath, title, doc, &frontmatter_terms(raw), *id)
                .context("re-index renamed document")?;
        }

        tx.commit().context("commit collection rename")?;
        self.fts.commit().context("commit full-text index")?;
        Ok(RenameSummary {
            documents,
            stale_removed,
        })
    }

    /// Whether the Tantivy index was built by an older tokenizer than this
    /// binary's, so some text (CJK today) is indexed in a form queries no
    /// longer produce.
    pub fn fts_rebuild_pending(&self) -> Result<bool> {
        if db::get_config(&self.db, FTS_TOKENIZER_CONFIG_KEY)?.as_deref()
            == Some(FTS_TOKENIZER_VERSION)
        {
            return Ok(false);
        }
        // An index with no documents has nothing indexed under an old tokenizer.
        let any_active: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM documents WHERE active = 1)",
            [],
            |r| r.get(0),
        )?;
        Ok(any_active)
    }

    /// Re-index every active document into Tantivy when the index predates
    /// [`FTS_TOKENIZER_VERSION`]. Returns whether a rebuild ran. Reads each
    /// document's text from `content`, so call it only when `content` is
    /// trustworthy (after a full `update` pass), and only from a writer that
    /// holds the index lock — it replaces the whole index.
    pub fn rebuild_fts_if_needed(&mut self) -> Result<bool> {
        if !self.fts_rebuild_pending()? {
            return Ok(false);
        }
        let mut stmt = self.db.prepare(
            "SELECT d.collection || '/' || d.path, d.title, c.doc, d.id, d.raw \
             FROM documents d JOIN content c ON c.hash = d.hash \
             WHERE d.active = 1",
        )?;
        let rows = stmt.query_map([], |r| {
            let raw: String = r.get(4)?;
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                frontmatter_terms(&raw),
                r.get(3)?,
            ))
        })?;
        self.fts
            .rebuild(rows.map(|row| row.map_err(anyhow::Error::from)))
            .context("rebuild full-text index")?;
        drop(stmt);
        db::set_config(&self.db, FTS_TOKENIZER_CONFIG_KEY, FTS_TOKENIZER_VERSION)?;
        Ok(true)
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
        self.search_fts_multi_with(query, limit, collections, &SearchOptions::default())
    }

    /// [`Self::search_fts_multi`] with `options`. With `per_file` above 1 a file
    /// can yield several results, each a different section holding the query's
    /// words; `limit` then counts sections. The default of 1 is one result per
    /// file showing the file's opening chunk, as before. With `recency` the
    /// best [`recency_pool`] files are weighted by age before the first `limit`
    /// are kept.
    pub fn search_fts_multi_with(
        &self,
        query: &str,
        limit: usize,
        collections: Option<&[String]>,
        options: &SearchOptions,
    ) -> Result<Vec<SearchResult>> {
        let effective = self.effective_collections(collections)?;
        if matches!(effective, Some(ref cols) if cols.is_empty()) {
            // No collection is default-included and none was requested explicitly.
            return Ok(vec![]);
        }
        let hits = match &options.recency {
            None => self.fts_scoped(query, limit, effective.as_deref())?,
            Some(recency) => {
                let mut hits = self.fts_scoped(query, recency_pool(limit), effective.as_deref())?;
                self.weight_hits(recency, &mut hits)?;
                hits
            }
        };
        match clamp_per_file(options.per_file) {
            1 => self.hits_to_results(hits, limit),
            per_file => self.hits_to_sections(hits, query, limit, per_file),
        }
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
        self.search_vec_multi_with(query, limit, collections, &SearchOptions::default())
    }

    /// [`Self::search_vec_multi`] with `options`. Each result shows the chunk
    /// whose vector matched. With `per_file` above 1 a file can yield several
    /// results, one per matching chunk, best match first; `limit` then counts
    /// sections. With `recency` the best [`recency_pool`] files are weighted by
    /// age before the first `limit` are kept.
    pub fn search_vec_multi_with(
        &mut self,
        query: &str,
        limit: usize,
        collections: Option<&[String]>,
        options: &SearchOptions,
    ) -> Result<Vec<SearchResult>> {
        let per_file = clamp_per_file(options.per_file);
        let effective = self.effective_collections(collections)?;
        if matches!(effective, Some(ref cols) if cols.is_empty()) {
            return Ok(vec![]);
        }
        // Files to gather before the cut: weighting by age can lift a file
        // that similarity alone ranks below `limit`.
        let wanted = match options.recency {
            Some(_) => recency_pool(limit),
            None => limit,
        };

        let embedding = self.backend.embed_query(query).context("embed query")?;
        let total_vectors = self.hnsw.size();

        // Widen the ANN candidate pool instead of relying on a fixed multiplier:
        // a fixed `limit * 4` over-fetch can still come back with too few
        // in-scope documents once filtered by collection and deduped down to
        // one row per document (a small minority collection can easily lose
        // out to a fixed-size raw top-k). Double `k` until enough in-scope
        // documents are found or the whole index has been searched.
        let mut k = limit.saturating_mul(4).max(1);
        // `doc_chunk_for_vid_in` gives ranking/dedup document identity without
        // the body, which is only needed for the final `limit` survivors —
        // fetched once, below, instead of once per raw HNSW hit on every
        // widening pass (rqmd#86).
        let mut by_doc: HashMap<String, (crate::types::Document, Vec<(f32, usize)>)> =
            HashMap::new();

        loop {
            let raw = self.hnsw.search(&embedding, k)?;
            by_doc.clear();
            for (vid, sim) in raw {
                // Pushes the collection constraint into the lookup itself: a
                // hash shared across collections must resolve to the
                // in-scope row here, not to an arbitrary one that then gets
                // discarded by a post-hoc filter (qmd#933 parity, rqmd#86).
                let Some((doc, pos)) = doc_chunk_for_vid_in(&self.db, vid, effective.as_deref())?
                else {
                    continue;
                };
                let filepath = format!("{}/{}", doc.collection, doc.path);
                by_doc
                    .entry(filepath)
                    .or_insert_with(|| (doc, Vec::new()))
                    .1
                    .push((sim, pos));
            }

            if by_doc.len() >= wanted || k >= total_vectors {
                break;
            }
            k = (k * 2).min(total_vectors);
        }

        let now = SystemTime::now();
        // A multi-chunk document appears once per requested section, not once
        // per chunk: keep its best-matching chunks only.
        let mut ids = Vec::with_capacity(by_doc.len());
        let mut docs = Vec::with_capacity(by_doc.len());
        let mut picks = Vec::new();
        for (filepath, (doc, mut hits)) in by_doc {
            let factor = match &options.recency {
                Some(recency) => Some(self.recency_factor(recency, now, &filepath)?),
                None => None,
            };
            hits.sort_by(|a, b| {
                b.0.partial_cmp(&a.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.1.cmp(&b.1))
            });
            let mut seen = HashSet::new();
            hits.retain(|h| seen.insert(h.1));
            for (rank, (sim, pos)) in hits.into_iter().take(per_file).enumerate() {
                picks.push(Pick {
                    filepath: filepath.clone(),
                    doc: docs.len(),
                    rank,
                    score: factor.map_or(sim, |factor| sim * factor),
                    chunk: (String::new(), pos),
                });
            }
            ids.push(doc.id);
            docs.push(PickDoc {
                title: doc.title,
                hash: doc.hash,
                body: String::new(),
            });
        }
        rank_picks(&mut picks, limit);

        // Only the survivors' bodies are read. The matched chunk is found by
        // its stored offset; a body chunked differently since its vectors were
        // written falls back to the chunk nearest to it.
        let mut chunked: HashMap<usize, Vec<Chunk>> = HashMap::new();
        for pick in &mut picks {
            let chunks = match chunked.entry(pick.doc) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let body = db::get_document_raw(&self.db, ids[pick.doc])?.unwrap_or_default();
                    let chunks = entry.insert(chunk_document_for_path(&pick.filepath, &body));
                    docs[pick.doc].body = body;
                    chunks
                }
            };
            pick.chunk = nearest_chunk(chunks, pick.chunk.1);
        }
        Ok(self.picks_to_results(docs, picks))
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

        // Same rationale as `search_vec_multi`: rank by identity only
        // (`doc_for_vid_meta`), fetch bodies once for the final survivors.
        let mut by_doc: HashMap<String, (f32, crate::types::Document)> = HashMap::new();
        for vid in vids {
            let embedding = self.hnsw.get_vector(vid)?;
            let hits = self.hnsw.search(&embedding, k)?;
            for (hit_vid, sim) in hits {
                let Some(doc) = doc_for_vid_meta(&self.db, hit_vid)? else {
                    continue;
                };
                let filepath = format!("{}/{}", doc.collection, doc.path);
                if filepath == source_filepath {
                    continue;
                }
                let keep = match by_doc.get(&filepath) {
                    Some((existing_sim, _)) => sim > *existing_sim,
                    None => true,
                };
                if keep {
                    by_doc.insert(filepath, (sim, doc));
                }
            }
        }

        let mut ranked: Vec<(f32, String, crate::types::Document)> = by_doc
            .into_iter()
            .map(|(filepath, (sim, doc))| (sim, filepath, doc))
            .collect();
        ranked.sort_by(|a, b| crate::rrf::score_desc_then_path(a.0, &a.1, b.0, &b.1));
        ranked.truncate(limit);

        let mut results = Vec::with_capacity(ranked.len());
        for (sim, _, doc) in ranked {
            let body = db::get_document_raw(&self.db, doc.id)?.unwrap_or_default();
            results.push(self.result_from_doc(&doc, body, sim));
        }
        Ok(results)
    }

    /// Report each stage of the hybrid queries that follow to `observer`, or
    /// stop reporting with `None`. While one is set, reranking runs in calls of
    /// a few candidates so `Reranking` can report progress; results and scores
    /// are unchanged.
    pub fn set_query_observer(&mut self, observer: Option<Box<dyn FnMut(QueryStage) + Send>>) {
        self.query_observer = observer;
    }

    fn emit_stage(&mut self, stage: QueryStage) {
        if let Some(observer) = self.query_observer.as_mut() {
            observer(stage);
        }
    }

    /// Start (`true`) or stop (`false`) measuring where hybrid queries spend
    /// their time; read the figures with [`Self::take_query_timings`]. Off by
    /// default, and measuring changes no result: the cost is a few clock reads
    /// per stage, and with it off the store only tests an `Option`.
    pub fn set_query_timings(&mut self, on: bool) {
        self.timings = on.then(|| RefCell::new(Self::fresh_timings()));
    }

    /// The timings gathered since they were switched on or last taken, then
    /// start afresh. `None` when measuring is off.
    pub fn take_query_timings(&mut self) -> Option<QueryTimings> {
        let cell = self.timings.as_mut()?;
        Some(std::mem::replace(cell.get_mut(), Self::fresh_timings()))
    }

    fn fresh_timings() -> QueryTimings {
        QueryTimings {
            phase: "query",
            ..QueryTimings::default()
        }
    }

    /// A clock for a timed section: `None`, without reading the clock, when
    /// timings are off.
    fn tick(&self) -> Option<Instant> {
        self.timings.as_ref().map(|_| Instant::now())
    }

    /// Hand `record` the timings and the time since `start`. Does nothing when
    /// timings are off.
    fn tock(&self, start: Option<Instant>, record: impl FnOnce(&mut QueryTimings, Duration)) {
        if let (Some(start), Some(timings)) = (start, &self.timings) {
            record(&mut timings.borrow_mut(), start.elapsed());
        }
    }

    /// Tag the ANN passes recorded from here on with `phase`.
    fn timing_phase(&self, phase: &'static str) {
        if let Some(timings) = &self.timings {
            timings.borrow_mut().phase = phase;
        }
    }

    /// [`InferenceBackend::embed_query`], timed.
    fn embed_query_timed(&mut self, text: &str) -> Result<Vec<f32>> {
        let start = self.tick();
        let embedding = self.backend.embed_query(text);
        self.tock(start, |t, d| {
            t.embed_calls += 1;
            t.embed += d;
        });
        embedding
    }

    /// [`InferenceBackend::embed_passage`], timed.
    fn embed_passage_timed(&mut self, text: &str) -> Result<Vec<f32>> {
        let start = self.tick();
        let embedding = self.backend.embed_passage(text);
        self.tock(start, |t, d| {
            t.embed_calls += 1;
            t.embed += d;
        });
        embedding
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
        self.hybrid_query_multi_with_deadline(
            query,
            intent,
            limit,
            collections,
            skip_rerank,
            no_expand,
            Deadline::none(),
        )
    }

    /// [`Self::hybrid_query_multi`] that gives up with a [`crate::TimedOut`] error
    /// once `deadline` has passed. The deadline is checked before each model
    /// stage (query embedding, expansion, each expansion leg, rerank); a model
    /// call already running is not interrupted. A query only reads, so giving
    /// up leaves the index untouched.
    #[allow(clippy::too_many_arguments)]
    pub fn hybrid_query_multi_with_deadline(
        &mut self,
        query: &str,
        intent: Option<&str>,
        limit: usize,
        collections: Option<&[String]>,
        skip_rerank: bool,
        no_expand: bool,
        deadline: Deadline,
    ) -> Result<Vec<SearchResult>> {
        self.hybrid_query_outcome(
            query,
            intent,
            limit,
            collections,
            skip_rerank,
            no_expand,
            deadline,
            true,
        )
        .map(|outcome| outcome.results)
    }

    /// [`Self::hybrid_query_multi_with_deadline`] that also reports how the
    /// rerank stage ended, so a caller can tell the user when results are in
    /// fusion order because reranking failed, was unavailable or ran out of time.
    ///
    /// With `strict_timeout` unset, a `deadline` that passes before or during
    /// reranking returns the fusion results with [`RerankStatus::DeadlineExpired`];
    /// set, it fails with [`crate::TimedOut`] naming `rerank`, and a rerank already
    /// running is not interrupted. Earlier stages fail either way, as no results
    /// exist yet.
    #[allow(clippy::too_many_arguments)]
    pub fn hybrid_query_outcome(
        &mut self,
        query: &str,
        intent: Option<&str>,
        limit: usize,
        collections: Option<&[String]>,
        skip_rerank: bool,
        no_expand: bool,
        deadline: Deadline,
        strict_timeout: bool,
    ) -> Result<QueryOutcome> {
        self.hybrid_query_outcome_with(
            query,
            intent,
            limit,
            collections,
            skip_rerank,
            no_expand,
            deadline,
            strict_timeout,
            &SearchOptions::default(),
        )
    }

    /// [`Self::hybrid_query_outcome`] with `options`. With `per_file` above 1 a
    /// file can yield several results, each a different section holding the
    /// query's words; `limit` then counts sections. Every extra section is one
    /// more reranker input, so they are only added while one rerank pass stays
    /// within the candidate pool's ceiling of 100 chunks, best-ranked files first.
    #[allow(clippy::too_many_arguments)]
    pub fn hybrid_query_outcome_with(
        &mut self,
        query: &str,
        intent: Option<&str>,
        limit: usize,
        collections: Option<&[String]>,
        skip_rerank: bool,
        no_expand: bool,
        deadline: Deadline,
        strict_timeout: bool,
        options: &SearchOptions,
    ) -> Result<QueryOutcome> {
        let started = self.tick();
        let per_file = clamp_per_file(options.per_file);
        let not_run = |results| QueryOutcome {
            results,
            rerank: RerankStatus::NotRun,
        };
        let effective = self.effective_collections(collections)?;
        if matches!(effective, Some(ref cols) if cols.is_empty()) {
            return Ok(not_run(vec![]));
        }
        let collections = effective.as_deref();

        // Internal candidate-fetch size for FTS/vector retrieval and the final
        // rerank pool: scales with the caller's requested `limit` instead of a
        // hardcoded constant, so e.g. `limit=25` isn't silently capped below
        // what was asked for. Capped at 100 — see `RERANK_CANDIDATE_LIMIT`.
        let requested_candidates = limit.saturating_mul(2);
        let fetch_size = requested_candidates.clamp(RERANK_CANDIDATE_LIMIT, RERANK_POOL_CEILING);
        if requested_candidates > RERANK_POOL_CEILING {
            tracing::warn!(
                "query requested limit={limit} (wants {requested_candidates} rerank \
                 candidates); capping the candidate pool at {fetch_size} since rerank \
                 builds one LlamaContext per candidate and cost is linear in pool size"
            );
        }

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

        let (ranked_lists, list_meta) = self.retrieve_ranked_lists(
            &parsed,
            effective_intent.as_deref(),
            query,
            collections,
            fetch_size,
            no_expand,
            deadline,
        )?;

        // Step 4: RRF fusion. Step 5: resolve fused candidates to full documents.
        let candidate_docs =
            self.fuse_and_resolve_candidates(&ranked_lists, &list_meta, fetch_size)?;
        if candidate_docs.is_empty() {
            self.tock(started, |t, d| t.total = d);
            return Ok(not_run(vec![]));
        }

        if strict_timeout {
            deadline.check("rerank")?;
        }

        // Step 6: chunk selection + rerank.
        let (sections, rerank_scores, rerank) = self.select_chunks_and_rerank(
            query,
            &rerank_query,
            effective_intent.as_deref(),
            &candidate_docs,
            skip_rerank,
            (!strict_timeout).then_some(deadline),
            per_file,
        );

        let recency = match options.recency {
            Some(recency) => Some(self.recency_factors(&recency, &candidate_docs)?),
            None => None,
        };
        let assemble = self.tick();
        let results =
            self.assemble_results(candidate_docs, sections, rerank_scores, recency, limit);
        self.tock(assemble, |t, d| t.assemble = d);
        self.tock(started, |t, d| t.total = d);
        Ok(QueryOutcome { results, rerank })
    }

    /// Steps 1-3 of `hybrid_query_multi`: retrieve one ranked list per query
    /// signal — either every sub-query in a parsed query document ("query
    /// document mode"), or BM25 + vector + optional generation-based
    /// expansion on the raw query text ("expand mode") — without fusing them
    /// yet. Returns the ranked lists alongside metadata (source + query type)
    /// used by `rrf_weights` to weight each list during fusion.
    #[allow(clippy::too_many_arguments)]
    fn retrieve_ranked_lists(
        &mut self,
        parsed: &crate::query::ParsedQuery,
        effective_intent: Option<&str>,
        query: &str,
        collections: Option<&[String]>,
        fetch_size: usize,
        no_expand: bool,
        deadline: Deadline,
    ) -> Result<(Vec<Vec<RankedResult>>, Vec<RankedListMeta>)> {
        let mut ranked_lists: Vec<Vec<RankedResult>> = Vec::new();
        let mut list_meta: Vec<RankedListMeta> = Vec::new();

        if !parsed.subqueries.is_empty() {
            // ── Query document mode ────────────────────────────────────────────
            // First sub-query gets weight 2.0 (Original); the rest get 1.0.
            self.timing_phase("sub-query");
            for (idx, sub) in parsed.subqueries.iter().enumerate() {
                deadline.check("a sub-query")?;
                let qt = if idx == 0 {
                    QueryType::Original
                } else {
                    sub.qtype.clone()
                };
                match sub.qtype {
                    QueryType::Lex => {
                        self.emit_stage(QueryStage::Searching);
                        let hits = self.fts_scoped(&sub.text, fetch_size, collections)?;
                        if !hits.is_empty() {
                            ranked_lists.push(fts_hits_to_ranked(&hits));
                            list_meta.push(RankedListMeta {
                                source: "fts",
                                query_type: qt,
                            });
                        }
                    }
                    QueryType::Vec => {
                        self.emit_stage(QueryStage::Embedding);
                        let emb = self
                            .embed_query_timed(&sub.text)
                            .context("embed sub-query")?;
                        let results = self.search_vec_scoped(&emb, fetch_size, collections)?;
                        if !results.is_empty() {
                            ranked_lists.push(results);
                            list_meta.push(RankedListMeta {
                                source: "vec",
                                query_type: qt,
                            });
                        }
                    }
                    QueryType::Hyde => {
                        self.emit_stage(QueryStage::Embedding);
                        // HyDE embeds a hypothetical *document* the generation model wrote,
                        // not a query — it must go through the passage-side prompt so it
                        // lands in the same embedding subspace as the real document chunks
                        // it's meant to match, or the whole technique degrades to a
                        // worse-phrased query embedding.
                        let emb = self
                            .embed_passage_timed(&sub.text)
                            .context("embed sub-query")?;
                        let results = self.search_vec_scoped(&emb, fetch_size, collections)?;
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
            self.emit_stage(QueryStage::Searching);
            let initial_fts = self.fts_scoped(expand_text, fetch_size, collections)?;
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
            deadline.check("embedding the query")?;
            self.emit_stage(QueryStage::Embedding);
            let query_embedding = self.embed_query_timed(expand_text).context("embed query")?;
            let vec_results = self.search_vec_scoped(&query_embedding, fetch_size, collections)?;
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
                    deadline.check("query expansion")?;
                    self.emit_stage(QueryStage::Expanding);
                    let prompt = build_expansion_prompt(expand_text, effective_intent);
                    let generate = self.tick();
                    let generated = self.backend.generate(&prompt);
                    self.tock(generate, |t, d| t.expansion_generate = d);
                    match generated {
                        Ok(expansion) => {
                            self.timing_phase("expansion");
                            let search = self.tick();
                            let expansion_lists = self.parse_and_run_expansion(
                                &expansion,
                                expand_text,
                                collections,
                                fetch_size,
                                deadline,
                            )?;
                            self.tock(search, |t, d| t.expansion_search = d);
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

        Ok((ranked_lists, list_meta))
    }

    /// Step 4-5 of `hybrid_query_multi`: RRF-fuse the independently ranked
    /// lists, then resolve the top `fetch_size` fused candidates to their
    /// document bodies. Returns an empty vec if there were no ranked lists
    /// to fuse, or none of the fused candidates resolved to a document —
    /// callers treat either case as "no results."
    fn fuse_and_resolve_candidates(
        &self,
        ranked_lists: &[Vec<RankedResult>],
        list_meta: &[RankedListMeta],
        fetch_size: usize,
    ) -> Result<Vec<(RankedResult, String, String)>> {
        if ranked_lists.is_empty() {
            return Ok(vec![]);
        }
        let fuse = self.tick();
        let weights = rrf_weights(list_meta);
        let fused = reciprocal_rank_fusion(ranked_lists, &weights);
        let candidates = &fused[..fetch_size.min(fused.len())];
        self.tock(fuse, |t, d| {
            t.fuse = d;
            t.fused_from = ranked_lists.len();
            t.fused = fused.len();
        });

        let resolve = self.tick();
        let mut candidate_docs: Vec<(RankedResult, String, String)> = Vec::new();
        for cand in candidates {
            if let Some((doc, body)) = self.filepath_to_doc_body(&cand.filepath)? {
                candidate_docs.push((cand.clone(), doc.hash, body));
            }
        }
        self.tock(resolve, |t, d| {
            t.resolve = d;
            t.resolve_lookups = candidates.len();
        });
        Ok(candidate_docs)
    }

    /// Step 6 of `hybrid_query_multi`: chunk each candidate once (reused for
    /// both the rerank input list and the final `best_chunk`/`best_chunk_pos`),
    /// then rerank with the cross-encoder unless reranking was skipped or the
    /// active backend doesn't support it. Returns `(sections, rerank_scores,
    /// status)` — `sections` holds each candidate's chunks, best first, and
    /// `rerank_scores` one score per chunk in that order. `rerank_scores` is
    /// `None` unless the status is `Reranked`, in which case `assemble_results`
    /// falls back to pure RRF scoring. A reranker error, a score count that
    /// differs from the chunk count, or scores that are all non-finite count as
    /// `Failed`, so fusion scores and rerank scores are never mixed. With a
    /// `soft_deadline`, a rerank that has not finished when it passes counts as
    /// `DeadlineExpired`.
    #[allow(clippy::too_many_arguments)]
    fn select_chunks_and_rerank(
        &mut self,
        query: &str,
        rerank_query: &str,
        effective_intent: Option<&str>,
        candidate_docs: &[(RankedResult, String, String)],
        skip_rerank: bool,
        soft_deadline: Option<Deadline>,
        per_file: usize,
    ) -> (Sections, Option<Vec<f32>>, RerankStatus) {
        // Fold intent terms into query terms for better snippet selection.
        let term_source = match effective_intent {
            Some(i) => format!("{i} {query}"),
            None => query.to_string(),
        };
        let query_terms: Vec<String> = term_source
            .to_lowercase()
            .split_whitespace()
            .filter(|t| t.len() > 2)
            .map(|t| t.to_string())
            .collect();

        // Chunk once per candidate. Sections past a file's best one are handed
        // out in candidate order while the rerank pass stays within its ceiling.
        let chunk_select = self.tick();
        let mut extra_budget = RERANK_POOL_CEILING.saturating_sub(candidate_docs.len());
        let sections: Sections = candidate_docs
            .iter()
            .map(|(cand, _, body)| {
                let wanted = 1 + (per_file - 1).min(extra_budget);
                let picked = best_chunks(&cand.filepath, body, &query_terms, wanted);
                extra_budget -= picked.len() - 1;
                picked
            })
            .collect();

        let chunk_refs: Vec<&str> = sections.iter().flatten().map(|(t, _)| t.as_str()).collect();
        self.tock(chunk_select, |t, d| t.chunk_select = d);

        let rerank = self.tick();
        let (rerank_scores, status) = if skip_rerank {
            (None, RerankStatus::NotRun)
        } else if !self.backend.capabilities().rerank {
            tracing::warn!(
                "rerank skipped: active inference backend does not support reranking — \
                 results are BM25+vector fusion only"
            );
            (None, RerankStatus::Unsupported)
        } else {
            // Use the intent-prepended rerank query for better cross-encoder scoring.
            match self.rerank_candidates(rerank_query, &chunk_refs, soft_deadline) {
                Ok(Some(scores))
                    if scores.len() == chunk_refs.len() && scores.iter().any(|s| s.is_finite()) =>
                {
                    (Some(scores), RerankStatus::Reranked)
                }
                Ok(None) => (None, RerankStatus::DeadlineExpired),
                Ok(Some(scores)) => {
                    tracing::warn!(
                        "rerank returned unusable scores ({} for {} candidate(s)); \
                         results are BM25+vector fusion only",
                        scores.len(),
                        chunk_refs.len()
                    );
                    (None, RerankStatus::Failed)
                }
                Err(e) => {
                    tracing::warn!("rerank failed: {e:#}; results are BM25+vector fusion only");
                    (None, RerankStatus::Failed)
                }
            }
        };

        if !matches!(status, RerankStatus::NotRun | RerankStatus::Unsupported) {
            self.tock(rerank, |t, d| {
                t.rerank = d;
                t.rerank_chunks = chunk_refs.len();
            });
        }
        (sections, rerank_scores, status)
    }

    /// Score `chunks` with the reranker: one backend call, or calls of
    /// [`RERANK_PROGRESS_SLICE`] candidates while a query observer is set, with
    /// `Reranking` reported before each. `Ok(None)` means `soft_deadline` passed
    /// first; any slice that errors fails the whole rerank, as one call would.
    fn rerank_candidates(
        &mut self,
        rerank_query: &str,
        chunks: &[&str],
        soft_deadline: Option<Deadline>,
    ) -> Result<Option<Vec<f32>>> {
        let total = chunks.len();
        let slice = if self.query_observer.is_some() {
            RERANK_PROGRESS_SLICE
        } else {
            total.max(1)
        };
        let mut scores = Vec::with_capacity(total);
        for part in chunks.chunks(slice) {
            self.emit_stage(QueryStage::Reranking {
                done: scores.len(),
                total,
            });
            let part_scores = match soft_deadline {
                Some(deadline) => self
                    .backend
                    .rerank_until(rerank_query, part, &|| deadline.is_expired())?,
                None => Some(self.backend.rerank(rerank_query, part)?),
            };
            match part_scores {
                Some(part_scores) => scores.extend(part_scores),
                None => return Ok(None),
            }
        }
        Ok(Some(scores))
    }

    /// The recency factor of each candidate, in order.
    fn recency_factors(
        &self,
        recency: &Recency,
        candidate_docs: &[(RankedResult, String, String)],
    ) -> Result<Vec<f32>> {
        let now = SystemTime::now();
        candidate_docs
            .iter()
            .map(|(cand, _, _)| self.recency_factor(recency, now, &cand.filepath))
            .collect()
    }

    /// The factor the file `filepath` (`collection/path`) multiplies its score
    /// by at `now`. A file with no recorded mtime, or one dated in the future,
    /// counts as just modified.
    fn recency_factor(&self, recency: &Recency, now: SystemTime, filepath: &str) -> Result<f32> {
        let (collection, path) = split_filepath(filepath);
        let mtime = db::document_mtime_ns(&self.db, collection, path)?
            .and_then(|ns| u64::try_from(ns).ok())
            .map(|ns| UNIX_EPOCH + Duration::from_nanos(ns));
        let age = mtime
            .and_then(|at| now.duration_since(at).ok())
            .unwrap_or_default();
        Ok(recency.factor(age))
    }

    /// Scale each keyword hit's score by its file's recency factor, then order
    /// the hits by score descending and path.
    fn weight_hits(&self, recency: &Recency, hits: &mut [(String, i64, f32)]) -> Result<()> {
        let now = SystemTime::now();
        for hit in hits.iter_mut() {
            hit.2 *= self.recency_factor(recency, now, &hit.0)?;
        }
        hits.sort_by(|a, b| crate::rrf::score_desc_then_path(a.2, &a.0, b.2, &b.0));
        Ok(())
    }

    /// Step 7 of `hybrid_query_multi`: blend each section's rerank score
    /// with its candidate's RRF score (falling back to pure RRF when reranking
    /// didn't run), scale it by the candidate's `recency` factor when there is
    /// one, build the final `SearchResult`s, then sort by score descending and
    /// truncate to `limit`.
    fn assemble_results(
        &self,
        candidate_docs: Vec<(RankedResult, String, String)>,
        sections: Sections,
        rerank_scores: Option<Vec<f32>>,
        recency: Option<Vec<f32>>,
        limit: usize,
    ) -> Vec<SearchResult> {
        let mut picks = Vec::new();
        let mut docs = Vec::with_capacity(candidate_docs.len());
        let mut next_score = 0;

        for (i, ((cand, hash, body), chunks)) in
            candidate_docs.into_iter().zip(sections).enumerate()
        {
            let rrf_score = cand.backend_score;
            for (rank, chunk) in chunks.into_iter().enumerate() {
                let score = if let Some(ref rscores) = rerank_scores {
                    let rs = rscores.get(next_score).copied().unwrap_or(rrf_score);
                    BLEND_HI * rs + BLEND_LO * rrf_score
                } else {
                    rrf_score
                };
                next_score += 1;
                let score = score * recency.as_ref().map_or(1.0, |factors| factors[i]);
                picks.push(Pick {
                    filepath: cand.filepath.clone(),
                    doc: i,
                    rank,
                    score,
                    chunk,
                });
            }
            docs.push(PickDoc {
                title: cand.title,
                hash,
                body,
            });
        }

        rank_picks(&mut picks, limit);
        self.picks_to_results(docs, picks)
    }

    /// Build a result for each pick, in order. A document's body is moved into
    /// the last result that uses it and cloned for any earlier one.
    fn picks_to_results(&self, docs: Vec<PickDoc>, picks: Vec<Pick>) -> Vec<SearchResult> {
        let mut left: HashMap<usize, usize> = HashMap::new();
        for pick in &picks {
            *left.entry(pick.doc).or_default() += 1;
        }
        let mut docs: Vec<Option<PickDoc>> = docs.into_iter().map(Some).collect();
        let mut results = Vec::with_capacity(picks.len());
        for pick in picks {
            let remaining = left.get_mut(&pick.doc).expect("counted above");
            *remaining -= 1;
            let slot = &mut docs[pick.doc];
            let (title, hash, body) = match slot {
                Some(doc) if *remaining > 0 => {
                    (doc.title.clone(), doc.hash.clone(), doc.body.clone())
                }
                _ => {
                    let doc = slot
                        .take()
                        .expect("a pick's document is present until its last pick");
                    (doc.title, doc.hash, doc.body)
                }
            };
            let (collection, path) = split_filepath(&pick.filepath);
            results.push(SearchResult {
                file: format!("rqmd://{}", pick.filepath),
                title,
                body,
                best_chunk: pick.chunk.0,
                best_chunk_pos: pick.chunk.1,
                score: pick.score,
                docid: docid_from_hash(&hash).to_string(),
                collection: collection.to_string(),
                path: path.to_string(),
                context: get_context_for_path(&self.db, collection, path)
                    .ok()
                    .flatten(),
            });
        }
        results
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Build a [`SearchResult`] from an already-hydrated document and body,
    /// using the document's first chunk as the snippet (`best_chunk_pos` is
    /// always 0). Shared by the result-construction paths that don't need
    /// term-relevance chunk selection — only [`Self::hybrid_query_multi`]'s
    /// rerank path needs a real chunk position, via the `best_chunks` helper.
    fn result_from_doc(
        &self,
        doc: &crate::types::Document,
        body: String,
        score: f32,
    ) -> SearchResult {
        let filepath = format!("{}/{}", doc.collection, doc.path);
        let best_chunk = first_chunk(&body);
        let docid = docid_from_hash(&doc.hash).to_string();
        let context = get_context_for_path(&self.db, &doc.collection, &doc.path)
            .ok()
            .flatten();
        SearchResult {
            file: format!("rqmd://{filepath}"),
            title: doc.title.clone(),
            body,
            best_chunk,
            best_chunk_pos: 0,
            score,
            docid,
            collection: doc.collection.clone(),
            path: doc.path.clone(),
            context,
        }
    }

    /// BM25 search limited to `collections`. A scope is resolved to the exact
    /// set of active document ids first, so the full-text top-`limit` cut only
    /// counts in-scope documents; no scope searches the whole index.
    fn fts_scoped(
        &self,
        query: &str,
        limit: usize,
        collections: Option<&[String]>,
    ) -> Result<Vec<(String, i64, f32)>> {
        let started = self.tick();
        let ids = match collections.filter(|c| !c.is_empty()) {
            Some(cols) => {
                let scope = self.tick();
                let ids = db::active_document_ids_in_collections(&self.db, cols)
                    .context("resolve collection scope")?;
                self.tock(scope, |t, d| t.bm25_scope += d);
                Some(Arc::new(ids))
            }
            None => None,
        };
        let hits = self
            .fts
            .search_fts_scoped(query, limit, collections, ids.as_ref());
        self.tock(started, |t, d| {
            t.bm25_calls += 1;
            t.bm25 += d;
        });
        hits
    }

    fn hits_to_results(
        &self,
        hits: Vec<(String, i64, f32)>,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let mut results = Vec::new();
        for (_filepath, doc_id, score) in hits.into_iter().take(limit) {
            let doc = match db::get_document_by_id(&self.db, doc_id)? {
                Some(d) => d,
                None => continue,
            };
            let body = db::get_document_raw(&self.db, doc.id)?.unwrap_or_default();
            results.push(self.result_from_doc(&doc, body, score));
        }
        Ok(results)
    }

    /// [`Self::hits_to_results`] for `per_file` sections of each hit, chosen by
    /// the words of `query`. Every hit scores each of its sections alike.
    fn hits_to_sections(
        &self,
        hits: Vec<(String, i64, f32)>,
        query: &str,
        limit: usize,
        per_file: usize,
    ) -> Result<Vec<SearchResult>> {
        let terms = keyword_terms(query);
        let mut docs = Vec::new();
        let mut picks = Vec::new();
        for (_filepath, doc_id, score) in hits.into_iter().take(limit) {
            let Some(doc) = db::get_document_by_id(&self.db, doc_id)? else {
                continue;
            };
            let body = db::get_document_raw(&self.db, doc.id)?.unwrap_or_default();
            let filepath = format!("{}/{}", doc.collection, doc.path);
            for (rank, chunk) in best_chunks(&filepath, &body, &terms, per_file)
                .into_iter()
                .enumerate()
            {
                picks.push(Pick {
                    filepath: filepath.clone(),
                    doc: docs.len(),
                    rank,
                    score,
                    chunk,
                });
            }
            docs.push(PickDoc {
                title: doc.title,
                hash: doc.hash,
                body,
            });
        }
        rank_picks(&mut picks, limit);
        Ok(self.picks_to_results(docs, picks))
    }

    /// Vector search scoped to `collections`, widening the raw ANN pool until
    /// enough in-scope hits are found or the whole index has been searched.
    ///
    /// A bare `hnsw.search(embedding, fetch_size)` followed by a collection
    /// filter starves a minority collection buried in a much larger corpus —
    /// the fixed-size raw top-k can be dominated entirely by the bulk
    /// collection before the filter ever runs, the vector-side counterpart to
    /// the BM25 truncate-then-filter bug fixed for `search_fts_multi` (see
    /// `search_fts_multi_finds_minority_collection_despite_bulk_corpus`).
    /// Mirrors the doubling strategy `search_vec_multi` already uses.
    fn search_vec_scoped(
        &self,
        embedding: &[f32],
        fetch_size: usize,
        collections: Option<&[String]>,
    ) -> Result<Vec<RankedResult>> {
        let total_vectors = self.hnsw.size();
        let mut k = fetch_size;
        loop {
            let search = self.tick();
            let hits = self.hnsw.search(embedding, k)?;
            let ann = search.map(|start| start.elapsed());
            let fetched = hits.len();
            let (results, lookup) = self.vec_hits_to_ranked(hits, collections)?;
            if let (Some(ann), Some(lookup), Some(timings)) = (ann, lookup, &self.timings) {
                let mut timings = timings.borrow_mut();
                let phase = timings.phase;
                timings.ann_passes.push(AnnPass {
                    phase,
                    k,
                    fetched,
                    in_scope: results.len(),
                    ann,
                    lookup,
                });
            }
            if results.len() >= fetch_size || k >= total_vectors {
                return Ok(results);
            }
            k = (k * 2).min(total_vectors);
        }
    }

    /// Resolve each ANN hit to its document with one SQLite lookup per hit,
    /// keeping the in-scope ones. The second value is the time the lookups
    /// took, summed, and is `Some` only while timings are on.
    fn vec_hits_to_ranked(
        &self,
        hits: Vec<(u64, f32)>,
        collections: Option<&[String]>,
    ) -> Result<(Vec<RankedResult>, Option<Duration>)> {
        let timed = self.timings.is_some();
        let mut lookup = Duration::ZERO;
        let mut results = Vec::new();
        for (vid, sim) in hits {
            // Ranking only needs document identity, not the body. The
            // collection constraint is pushed into the lookup itself, not
            // applied after the fact — a hash shared across collections
            // must resolve to the in-scope row here, not to an arbitrary one
            // that then gets discarded (qmd#933 parity, rqmd#86).
            let start = timed.then(Instant::now);
            let found = doc_for_vid_meta_in(&self.db, vid, collections)?;
            if let Some(start) = start {
                lookup += start.elapsed();
            }
            if let Some(doc) = found {
                results.push(RankedResult {
                    filepath: format!("{}/{}", doc.collection, doc.path),
                    title: doc.title,
                    backend_score: sim,
                });
            }
        }
        Ok((results, timed.then_some(lookup)))
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
        let body = db::get_document_raw(&self.db, doc.id)?.unwrap_or_default();
        Ok(Some((doc, body)))
    }

    /// Parse the `lex:/vec:/hyde:` output of `generate_constrained` and run
    /// each expansion as a search, returning ranked lists + their metadata.
    ///
    /// Malformed or absent lines are silently skipped (expansion is best-effort).
    /// `fetch_size` is the caller's already-computed candidate-pool size (see
    /// `hybrid_query_multi`) — threaded through so expansion searches scale
    /// with the requested `limit` the same way the original-query searches do.
    ///
    /// Two dedup rules, applied per kind (a `lex:` line never suppresses a
    /// `vec:` line even with identical text — they search different
    /// retrieval mechanisms): a line is dropped if its trimmed text exactly
    /// repeats the original query (already covered by the original-query FTS
    /// and vector legs run before expansion) or exactly repeats an earlier
    /// line of the same kind in this same expansion output. Without this, the
    /// generation model repeating a line — observed to cache-duplicate a HyDE
    /// passage up to 12x for one query — re-runs the same FTS/vector search
    /// for zero new candidates each time (qmd#952/#1000/#921 parity, rqmd#86).
    fn parse_and_run_expansion(
        &mut self,
        expansion: &str,
        original_query: &str,
        collections: Option<&[String]>,
        fetch_size: usize,
        deadline: Deadline,
    ) -> Result<(Vec<Vec<RankedResult>>, Vec<RankedListMeta>)> {
        let mut lists: Vec<Vec<RankedResult>> = Vec::new();
        let mut metas: Vec<RankedListMeta> = Vec::new();
        let original_trimmed = original_query.trim();
        let mut seen: HashSet<(&'static str, &str)> = HashSet::new();
        let language_mismatch = |text: &str| {
            let mismatch = expansion_language_mismatch(original_query, text);
            if mismatch {
                tracing::debug!(
                    "dropped expansion line in a different script than the query: {text}"
                );
            }
            mismatch
        };

        for line in expansion.lines() {
            deadline.check("an expansion query")?;
            let line = line.trim();
            if let Some(text) = line.strip_prefix("lex:") {
                let text = text.trim();
                if !text.is_empty()
                    && text != original_trimmed
                    && !language_mismatch(text)
                    && seen.insert(("lex", text))
                {
                    let hits = self.fts_scoped(text, fetch_size, collections)?;
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
                if !text.is_empty()
                    && text != original_trimmed
                    && !language_mismatch(text)
                    && seen.insert(("vec", text))
                {
                    let emb = self
                        .embed_query_timed(text)
                        .context("embed vec expansion")?;
                    let results = self.search_vec_scoped(&emb, fetch_size, collections)?;
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
                if !text.is_empty()
                    && text != original_trimmed
                    && !language_mismatch(text)
                    && seen.insert(("hyde", text))
                {
                    // Passage-side prompt — see the QueryType::Hyde comment above for why.
                    let emb = self
                        .embed_passage_timed(text)
                        .context("embed hyde expansion")?;
                    let results = self.search_vec_scoped(&emb, fetch_size, collections)?;
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
        if let Some(cols) = requested
            && !cols.is_empty()
        {
            return Ok(Some(cols.to_vec()));
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
         Write all three lines in the same language as the query.\n\
         Output only those three lines. No explanation.\
         <|im_end|>\n\
         <|im_start|>user\n\
         {intent_block}Query: {query}\
         <|im_end|>\n\
         <|im_start|>assistant\n"
    )
}

/// Coarse script family of a piece of text, for the expansion language guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Script {
    Latin,
    /// Han, Hiragana, Katakana and Hangul.
    Cjk,
    /// Anything else, or no letters at all.
    Other,
}

fn script_of(c: char) -> Script {
    match c {
        '\u{3040}'..='\u{30FF}'
        | '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{AC00}'..='\u{D7AF}' => Script::Cjk,
        c if c.is_ascii_alphabetic() || ('\u{00C0}'..='\u{024F}').contains(&c) => Script::Latin,
        _ => Script::Other,
    }
}

/// The script most of `text`'s letters belong to. A CJK character carries
/// roughly a word of meaning, so it counts double against a Latin letter —
/// "Rust の 所有権" is Japanese, not English.
fn dominant_script(text: &str) -> Script {
    let (mut latin, mut cjk, mut other) = (0usize, 0usize, 0usize);
    for c in text.chars().filter(|c| c.is_alphabetic()) {
        match script_of(c) {
            Script::Latin => latin += 1,
            Script::Cjk => cjk += 2,
            Script::Other => other += 1,
        }
    }
    if cjk >= latin && cjk >= other && cjk > 0 {
        Script::Cjk
    } else if latin >= other && latin > 0 {
        Script::Latin
    } else {
        Script::Other
    }
}

/// An expansion line is dropped when it is clearly in a different script
/// family than the query: a Latin query with CJK lines, or the reverse. Other
/// scripts and languages sharing a script (French vs English) are not
/// detectable here and are kept.
fn expansion_language_mismatch(query: &str, line: &str) -> bool {
    use Script::{Cjk, Latin};
    let (q, l) = (dominant_script(query), dominant_script(line));
    matches!((q, l), (Latin, Cjk) | (Cjk, Latin))
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

/// `path`'s last-modified time, or `None` if it doesn't exist or the
/// platform can't report one. Used by [`Store::reload_if_stale`] to detect
/// out-of-process writes without needing an fs watcher.
fn mtime_of(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Up to `n` chunks of `body`, those with the most query-term overlap first.
/// A tie between chunks goes to the later one. Only the first may contain no
/// query term at all; every further one must contain a query term in text the
/// chunks before it do not already show, so a word in the stretch two chunks
/// share is one section. Chunks once and returns `(text, offset)` pairs, never
/// an empty list.
fn best_chunks(
    filepath: &str,
    body: &str,
    query_terms: &[String],
    n: usize,
) -> Vec<(String, usize)> {
    let chunks = chunk_document_for_path(filepath, body);
    if chunks.is_empty() {
        return vec![(String::new(), 0)];
    }
    if query_terms.is_empty() {
        let c = chunks.into_iter().next().unwrap();
        return vec![(c.text, c.pos)];
    }
    let mut scored: Vec<(usize, usize, Chunk)> = chunks
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let lower = c.text.to_lowercase();
            let hits = query_terms
                .iter()
                .filter(|t| lower.contains(t.as_str()))
                .count();
            (hits, i, c)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let mut picked: Vec<Chunk> = Vec::new();
    for (hits, _, chunk) in scored {
        if picked.len() == n || (!picked.is_empty() && hits == 0) {
            break;
        }
        let fresh = uncovered_text(&chunk, &picked).to_lowercase();
        if picked.is_empty() || query_terms.iter().any(|t| fresh.contains(t.as_str())) {
            picked.push(chunk);
        }
    }
    picked.into_iter().map(|c| (c.text, c.pos)).collect()
}

/// The part of `chunk`'s text that no chunk in `taken` already covers. A chunk
/// that starts inside another loses its head, one that ends inside another its
/// tail; offsets that do not fall on a character boundary leave the text whole.
fn uncovered_text<'a>(chunk: &'a Chunk, taken: &[Chunk]) -> &'a str {
    let (mut from, mut to) = (0, chunk.text.len());
    for other in taken {
        let other_end = other.pos + other.text.len();
        if other.pos <= chunk.pos && chunk.pos < other_end {
            from = from.max((other_end - chunk.pos).min(chunk.text.len()));
        } else if chunk.pos < other.pos && other.pos < chunk.pos + chunk.text.len() {
            to = to.min(other.pos - chunk.pos);
        }
    }
    chunk.text.get(from..to.max(from)).unwrap_or(&chunk.text)
}

/// The words of a keyword query that a section can contain: quotes stripped,
/// negated words left out, lowercased, as [`best_chunks`] expects them.
fn keyword_terms(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split_whitespace()
        .filter(|t| !t.starts_with('-'))
        .map(|t| t.trim_matches('"'))
        .filter(|t| t.len() > 2)
        .map(str::to_string)
        .collect()
}

/// The chunk that starts at `pos`, or the one starting nearest to it when the
/// body no longer chunks the way it did when `pos` was stored.
fn nearest_chunk(chunks: &[Chunk], pos: usize) -> (String, usize) {
    chunks
        .iter()
        .min_by_key(|c| c.pos.abs_diff(pos))
        .map_or((String::new(), 0), |c| (c.text.clone(), c.pos))
}

/// Each candidate's chunks as `(text, offset)` pairs, best first.
type Sections = Vec<Vec<(String, usize)>>;

/// One section chosen for a result, before the result is built.
struct Pick {
    /// `collection/path` of its document.
    filepath: String,
    /// Index of its document in the list handed to `picks_to_results`.
    doc: usize,
    /// 0 for the document's best section, then 1, 2, ... in order.
    rank: usize,
    score: f32,
    chunk: (String, usize),
}

/// A document that picks refer to.
struct PickDoc {
    title: String,
    hash: String,
    body: String,
}

/// Files gathered per result wanted when `search` or `vsearch` weights by age.
const RECENCY_POOL_FACTOR: usize = 4;

/// Most files gathered for weighting, however many results are wanted.
const RECENCY_POOL_CAP: usize = 200;

/// How many of the best-matching files a recency-weighted `search` or
/// `vsearch` considers before keeping `limit` of them: `RECENCY_POOL_FACTOR`
/// times `limit`, at most `RECENCY_POOL_CAP`, and never fewer than `limit`.
fn recency_pool(limit: usize) -> usize {
    limit.max(
        limit
            .saturating_mul(RECENCY_POOL_FACTOR)
            .min(RECENCY_POOL_CAP),
    )
}

/// Order picks by score descending, then path, then the order within their
/// document, and keep the first `limit`.
fn rank_picks(picks: &mut Vec<Pick>, limit: usize) {
    picks.sort_by(|a, b| {
        crate::rrf::score_desc_then_path(a.score, &a.filepath, b.score, &b.filepath)
            .then(a.rank.cmp(&b.rank))
    });
    picks.truncate(limit);
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

/// The fingerprint marker of the AST language `chunk_document_for_path(rel_path, _)`
/// would actually chunk `rel_path` with — `None` when no compiled grammar owns
/// its extension. `embed_fingerprint` uses this to decide whether a document's
/// fingerprint needs an AST marker: without it, a binary built without a
/// grammar silently re-chunks source files with the markdown regex chunker
/// under the *same* fingerprint as a binary that has it, so the invalidation
/// mechanism never notices the swap.
fn ast_marker_for_path(rel_path: &str) -> Option<&'static str> {
    std::path::Path::new(rel_path)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(crate::chunking::ast_fingerprint_marker)
}

/// Whether `chunk_document_for_path(rel_path, _)` would dispatch to the AST
/// chunker for `rel_path`.
pub fn embed_fingerprint_applies_ast(rel_path: &str) -> bool {
    ast_marker_for_path(rel_path).is_some()
}

/// Hash identifying one chunk's text, independent of the document it sits in.
fn chunk_text_hash(text: &str) -> String {
    hex::encode(&Sha256::digest(text.as_bytes())[..16])
}

/// What a stored vector was computed from, other than the chunking settings:
/// the chunk text plus everything that shapes the embedding of that text. Two
/// vectors with the same key are interchangeable.
fn vector_key(backend_kind: &str, model: &str, context_size: usize, chunk_hash: &str) -> String {
    let sig = format!(
        "backend:{backend_kind}\nmodel:{model}\ncontext_size:{context_size}\npassage_prefix:{}\nchunk:{chunk_hash}",
        rqmd_llm::EMBEDDINGGEMMA_PASSAGE_PREFIX
    );
    hex::encode(&Sha256::digest(sig.as_bytes())[..16])
}

/// Embedding fingerprint: 6-hex-char hash of model name + chunk constants, plus
/// the language's AST marker when [`embed_fingerprint_applies_ast`] is true for
/// `rel_path`. Used to detect stale embeddings after a model or chunking-config
/// change.
///
/// The AST marker is scoped to eligible paths rather than folded in
/// unconditionally so that enabling/disabling a grammar does not invalidate
/// every markdown/text vector in the index — only the source files the chunker
/// choice actually affects. It is also per language, so one grammar change
/// re-embeds only that language's files.
///
/// Derives from the real `chunking::CHUNK_SIZE_CHARS`/`CHUNK_OVERLAP_CHARS` constants
/// rather than hardcoded literals, so a future change to those constants actually
/// invalidates the fingerprint instead of going undetected.
fn embed_fingerprint(model: &str, rel_path: &str) -> String {
    embed_fingerprint_with_prefixes(
        model,
        rel_path,
        rqmd_llm::EMBEDDINGGEMMA_QUERY_PREFIX,
        rqmd_llm::EMBEDDINGGEMMA_PASSAGE_PREFIX,
    )
}

/// [`embed_fingerprint`] with the prompt prefixes given explicitly: a vector
/// encoded under one prefix is not comparable with a query encoded under
/// another, so the prefixes are part of what makes an embedding current.
fn embed_fingerprint_with_prefixes(
    model: &str,
    rel_path: &str,
    query_prefix: &str,
    passage_prefix: &str,
) -> String {
    marked_fingerprint(
        model,
        query_prefix,
        passage_prefix,
        ast_marker_for_path(rel_path),
    )
}

/// The fingerprint of `model` and the chunking constants with `ast_marker`
/// appended, or the markdown chunker's marker when there is none.
fn marked_fingerprint(
    model: &str,
    query_prefix: &str,
    passage_prefix: &str,
    ast_marker: Option<&str>,
) -> String {
    let mut sig = format!(
        "model:{model}\nchunk_size_chars:{}\nchunk_overlap_chars:{}\nchunk_strategy_version:{}\nquery_prefix:{query_prefix}\npassage_prefix:{passage_prefix}",
        crate::chunking::CHUNK_SIZE_CHARS,
        crate::chunking::CHUNK_OVERLAP_CHARS,
        crate::chunking::CHUNK_STRATEGY_VERSION,
    );
    if let Some(marker) = ast_marker {
        sig.push('\n');
        sig.push_str(marker);
    } else {
        // Only the markdown chunker repeats table headers; AST-chunked source
        // vectors are unaffected and keep their fingerprint.
        sig.push_str("\ntable_header_chunking:1");
    }
    let hash = Sha256::digest(sig.as_bytes());
    hex::encode(&hash[..3]) // 6 hex chars
}

/// Compute the base fingerprint a fresh `rqmd embed` run would produce for the
/// given embed-model identity on a non-AST-eligible path, without loading any
/// model weights or contacting HuggingFace. `embed_model_name` must match
/// whatever the actually-configured backend's `InferenceBackend::embed_model_name`
/// reports — e.g. `BackendKind::default_embed_model_name()` for the backend kind
/// currently selected — not a hardcoded single backend's defaults.
pub fn expected_embed_fingerprint(embed_model_name: &str) -> String {
    embed_fingerprint(embed_model_name, "")
}

/// Compute the fingerprint a fresh `rqmd embed` run would produce for
/// `rel_path` specifically — the AST-marked variant when `rel_path` is
/// AST-eligible, otherwise identical to [`expected_embed_fingerprint`]. Used by
/// `rqmd doctor`/`rqmd embed` to tell current vectors from stale ones on a
/// per-document basis.
pub fn expected_embed_fingerprint_for_path(embed_model_name: &str, rel_path: &str) -> String {
    embed_fingerprint(embed_model_name, rel_path)
}

/// The `embed_fingerprint` the currently *configured* backend (per
/// `RQMD_INFERENCE_BACKEND`) + chunking constants would produce. Compare
/// against `db::fingerprint_breakdown` rows to detect stale vectors — shared
/// by `doctor`, `embed`, `query`, `vsearch` and the MCP `status` tool so there
/// is exactly one staleness detection path.
///
/// Derived from `BackendKind::default_embed_model_name()`, not a hardcoded
/// `LlamaCppConfig::default()` — otherwise this permanently disagrees with the
/// real fingerprint whenever a non-default backend (e.g. ORT) is active.
pub fn expected_fingerprint() -> String {
    let name = BackendKind::from_env().default_embed_model_name();
    expected_embed_fingerprint(&name)
}

/// The `embed_fingerprint` a fresh `rqmd embed` run would produce for
/// `rel_path` specifically, given the currently configured backend — the
/// AST-marked variant when `rel_path` is AST-eligible, otherwise identical to
/// [`expected_fingerprint`].
pub fn expected_fingerprint_for_path(rel_path: &str) -> String {
    let name = BackendKind::from_env().default_embed_model_name();
    expected_embed_fingerprint_for_path(&name, rel_path)
}

/// Every AST fingerprint a fresh `rqmd embed` run produces for `embed_model_name`:
/// one per fingerprint marker, with the extensions that carry it. Empty when no
/// grammar is compiled in.
pub fn expected_embed_ast_fingerprints(embed_model_name: &str) -> Vec<db::AstFingerprint> {
    crate::chunking::ast_fingerprint_groups()
        .into_iter()
        .map(|(marker, extensions)| db::AstFingerprint {
            extensions,
            fingerprint: marked_fingerprint(
                embed_model_name,
                rqmd_llm::EMBEDDINGGEMMA_QUERY_PREFIX,
                rqmd_llm::EMBEDDINGGEMMA_PASSAGE_PREFIX,
                Some(marker),
            ),
        })
        .collect()
}

/// [`expected_embed_ast_fingerprints`] for the currently configured backend —
/// what the AST-eligible paths currently produce. Callers that need "the AST
/// fingerprints" (not a specific document's) use this instead of inventing a
/// placeholder path themselves.
pub fn expected_ast_fingerprints() -> Vec<db::AstFingerprint> {
    let name = BackendKind::from_env().default_embed_model_name();
    expected_embed_ast_fingerprints(&name)
}

/// Every fingerprint that is current right now: the base one and each AST one.
/// A stored vector at any other fingerprint is stale.
pub fn expected_fingerprint_set() -> Vec<String> {
    std::iter::once(expected_fingerprint())
        .chain(
            expected_ast_fingerprints()
                .into_iter()
                .map(|g| g.fingerprint),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deadline::TimedOut;

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

    #[test]
    fn add_all_or_rollback_removes_vids_added_before_the_failure() {
        let mut state = (10u64, Vec::<u64>::new());
        let err = add_all_or_rollback(
            &mut state,
            &vec![vec![0.0_f32]; 3],
            |st, _| {
                if st.0 == 12 {
                    bail!("third add fails");
                }
                st.0 += 1;
                Ok(st.0 - 1)
            },
            |st, vid| st.1.push(vid),
        )
        .unwrap_err();
        assert!(err.to_string().contains("third add fails"));
        assert_eq!(state.1, vec![10, 11]);
    }

    #[test]
    fn add_all_or_rollback_keeps_everything_when_all_adds_succeed() {
        let mut state = (0u64, Vec::<u64>::new());
        let vids = add_all_or_rollback(
            &mut state,
            &vec![vec![0.0_f32]; 3],
            |st, _| {
                st.0 += 1;
                Ok(st.0)
            },
            |st, vid| st.1.push(vid),
        )
        .unwrap();
        assert_eq!(vids, vec![1, 2, 3]);
        assert!(state.1.is_empty());
    }

    #[test]
    fn rolled_back_adds_leave_the_hnsw_size_unchanged() {
        let mut hnsw = VectorIndex::new().unwrap();
        let before = hnsw.size();
        let embeddings = vec![vec![0.1_f32; rqmd_llm::EMBED_DIM]; 3];
        let calls = std::cell::Cell::new(0);
        let result = add_all_or_rollback(
            &mut hnsw,
            &embeddings,
            |h, e| {
                calls.set(calls.get() + 1);
                if calls.get() == 3 {
                    bail!("injected");
                }
                h.add(e)
            },
            |h, vid| {
                h.remove(vid).unwrap();
            },
        );
        assert!(result.is_err());
        assert_eq!(hnsw.size(), before);
    }

    /// Backend whose `generate` is slow and whose `rerank` must never run:
    /// stands in for a model call that outlasts the query's deadline.
    struct SlowBackend {
        embed_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        embed_delay: Duration,
    }

    impl rqmd_llm::InferenceBackend for SlowBackend {
        fn embed(&mut self, _text: &str) -> Result<Vec<f32>> {
            self.embed_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(self.embed_delay);
            Ok(vec![0.1; rqmd_llm::EMBED_DIM])
        }
        fn rerank(&mut self, _query: &str, _docs: &[&str]) -> Result<Vec<f32>> {
            panic!("rerank must not run after the deadline")
        }
        fn generate(&mut self, _prompt: &str) -> Result<String> {
            std::thread::sleep(Duration::from_millis(200));
            Ok("lex: alpha beta\nvec: gamma delta\n".to_string())
        }
        fn embed_model_name(&self) -> &str {
            "slow-embed"
        }
        fn rerank_model_name(&self) -> &str {
            "slow-rerank"
        }
        fn generate_model_name(&self) -> &str {
            "slow-generate"
        }
    }

    fn slow_store(
        dir: &std::path::Path,
        embed_delay: Duration,
    ) -> (Store, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let embed_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let config = StoreConfig {
            db_path: dir.join("test.sqlite"),
            tantivy_dir: dir.join("tantivy"),
            hnsw_path: dir.join("hnsw.usearch"),
            read_only: false,
        };
        let backend = SlowBackend {
            embed_calls: embed_calls.clone(),
            embed_delay,
        };
        (Store::open(config, Box::new(backend)).unwrap(), embed_calls)
    }

    fn timed_out(err: &anyhow::Error) -> TimedOut {
        *err.downcast_ref::<TimedOut>()
            .unwrap_or_else(|| panic!("expected TimedOut, got {err:#}"))
    }

    #[test]
    fn expansion_prompt_asks_for_the_query_language() {
        let prompt = build_expansion_prompt("所有権とは", None);
        assert!(prompt.contains("in the same language as the query"));
    }

    #[test]
    fn dominant_script_classifies_by_weighted_letters() {
        assert_eq!(dominant_script("ownership rules"), Script::Latin);
        assert_eq!(dominant_script("café résumé"), Script::Latin);
        assert_eq!(dominant_script("所有権について"), Script::Cjk);
        assert_eq!(dominant_script("Rust の 所有権"), Script::Cjk);
        assert_eq!(dominant_script("abc 所有"), Script::Cjk);
        assert_eq!(dominant_script("한국어 검색"), Script::Cjk);
        assert_eq!(dominant_script("правила владения"), Script::Other);
        assert_eq!(dominant_script("2024 -- 15"), Script::Other);
    }

    #[test]
    fn language_mismatch_only_fires_between_latin_and_cjk() {
        assert!(expansion_language_mismatch(
            "ownership rules",
            "所有権 ルール"
        ));
        assert!(expansion_language_mismatch(
            "所有権について",
            "ownership rules"
        ));
        assert!(!expansion_language_mismatch("ownership", "borrowing"));
        assert!(!expansion_language_mismatch("所有権", "借用"));
        assert!(!expansion_language_mismatch("правила", "ownership"));
        assert!(!expansion_language_mismatch("2024", "所有権"));
    }

    /// Documents hit by the expansion's `lex:` legs. The Japanese word is
    /// space-delimited so the default tokenizer indexes it, which gives the
    /// Japanese line real hits to lose: only the guard decides whether it runs.
    fn expansion_hits(query: &str) -> Vec<String> {
        let dir = tempfile::TempDir::new().unwrap();
        let backend = StubBackend {
            name: "stub".to_string(),
            caps: rqmd_llm::BackendCapabilities {
                embed: false,
                rerank: false,
                generate: false,
            },
        };
        let mut store = open_stub_store(dir.path(), backend);
        store
            .index_document("coll", "japanese.md", "T", "所有権 ルール")
            .unwrap();
        store
            .index_document("coll", "english.md", "T", "ownership notes")
            .unwrap();
        store.flush().unwrap();

        let (lists, _) = store
            .parse_and_run_expansion(
                "lex: 所有権\nlex: ownership",
                query,
                None,
                10,
                Deadline::none(),
            )
            .unwrap();
        let mut hits: Vec<String> = lists.iter().flatten().map(|r| r.filepath.clone()).collect();
        hits.sort();
        hits
    }

    #[test]
    fn expansion_lines_in_another_script_than_the_query_are_dropped() {
        assert_eq!(
            expansion_hits("what is owned"),
            vec!["coll/english.md"],
            "the Japanese line must not run for an English query"
        );
        assert_eq!(
            expansion_hits("所有権について"),
            vec!["coll/japanese.md"],
            "the English line must not run for a Japanese query"
        );
    }

    #[test]
    fn query_with_an_expired_deadline_stops_before_the_first_model_call() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, embed_calls) = slow_store(dir.path(), Duration::ZERO);

        let err = store
            .hybrid_query_multi_with_deadline(
                "unmatched words",
                None,
                5,
                None,
                false,
                false,
                Deadline::after(Duration::ZERO),
            )
            .unwrap_err();

        assert_eq!(timed_out(&err).stage, "embedding the query");
        assert_eq!(embed_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn query_stops_at_the_next_stage_after_a_slow_generate_passes_the_deadline() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, _) = slow_store(dir.path(), Duration::ZERO);

        let started = std::time::Instant::now();
        let err = store
            .hybrid_query_multi_with_deadline(
                "unmatched words",
                None,
                5,
                None,
                false,
                false,
                Deadline::after(Duration::from_millis(50)),
            )
            .unwrap_err();

        assert_eq!(timed_out(&err).stage, "an expansion query");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn query_with_candidates_stops_before_rerank_when_the_deadline_passes() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, _) = slow_store(dir.path(), Duration::from_millis(200));
        store
            .index_document("coll", "a.md", "Title", "uniquerankterm body text")
            .unwrap();
        store.flush().unwrap();

        let err = store
            .hybrid_query_multi_with_deadline(
                "uniquerankterm",
                None,
                5,
                None,
                false,
                true,
                Deadline::after(Duration::from_millis(50)),
            )
            .unwrap_err();

        assert_eq!(timed_out(&err).stage, "rerank");
    }

    #[test]
    fn query_without_a_deadline_runs_the_same_pipeline_to_completion() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, _) = slow_store(dir.path(), Duration::ZERO);

        let results = store
            .hybrid_query_multi("unmatched words", None, 5, None, false, false)
            .unwrap();

        assert!(results.is_empty());
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

    /// Regression test for the MCP daemon staleness bug: a long-lived,
    /// read-only `Store` (standing in for the daemon's) must be able to see
    /// documents written by a separate `Store` handle (standing in for a
    /// concurrent `rqmd index`/`update` run) after calling
    /// `reload_if_stale()` — without being reopened.
    ///
    /// This also pins the fix for a follow-on bug in the first attempt at
    /// this method: it compared the main `.sqlite` file's mtime, which does
    /// not change under WAL-mode writes (confirmed empirically — WAL writes
    /// land in the `-wal` sidecar) until a checkpoint happens. The real
    /// staleness signal for the FTS side is Tantivy's `meta.json`, rewritten
    /// atomically on every `commit()`.
    #[test]
    fn reload_if_stale_surfaces_writes_from_a_second_store_handle() {
        let dir = tempfile::TempDir::new().unwrap();

        // Writer: simulates `rqmd index` running as a separate process.
        let writer_backend = StubBackend {
            name: "stub-org/stub-embed.onnx".to_string(),
            caps: rqmd_llm::BackendCapabilities {
                embed: false,
                rerank: false,
                generate: false,
            },
        };
        let mut writer = open_stub_store(dir.path(), writer_backend);

        // Reader: simulates the resident MCP daemon, opened read-only before
        // any documents exist.
        let reader_config = StoreConfig {
            db_path: dir.path().join("test.sqlite"),
            tantivy_dir: dir.path().join("tantivy"),
            hnsw_path: dir.path().join("hnsw.usearch"),
            read_only: true,
        };
        let mut reader = Store::open(reader_config, rqmd_llm::no_backend()).unwrap();

        assert_eq!(
            reader.search_fts("uniquestaledoc", 10, None).unwrap().len(),
            0,
            "sanity: nothing indexed yet"
        );

        writer
            .index_document_fts_only("coll", "notes/new.md", "Title", "uniquestaledoc")
            .unwrap();
        writer.flush().unwrap();

        // Without calling reload_if_stale, the reader's Manual-reload Tantivy
        // reader must still show the pre-write snapshot.
        assert_eq!(
            reader.search_fts("uniquestaledoc", 10, None).unwrap().len(),
            0,
            "reader must not see the new doc before reload_if_stale() is called"
        );

        let reloaded = reader.reload_if_stale().unwrap();
        assert!(reloaded, "reload_if_stale must detect the writer's commit");

        assert_eq!(
            reader.search_fts("uniquestaledoc", 10, None).unwrap().len(),
            1,
            "reader must see the new doc after reload_if_stale()"
        );

        // Calling again with nothing new written must be a no-op.
        assert!(!reader.reload_if_stale().unwrap());
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

    /// A non-AST-eligible path (e.g. markdown) must keep today's exact base
    /// fingerprint regardless of whether `ast-chunking` is compiled in —
    /// otherwise enabling/disabling the feature would invalidate every
    /// markdown/text vector in the index, not just the source files the
    /// chunker choice actually affects.
    #[test]
    fn non_ast_eligible_path_keeps_base_fingerprint() {
        let base = expected_embed_fingerprint("fake-model");
        let for_md = expected_embed_fingerprint_for_path("fake-model", "docs/readme.md");
        assert_eq!(
            for_md, base,
            "non-eligible paths must not carry the AST marker"
        );
    }

    /// An AST-eligible path only diverges from the base fingerprint when the
    /// grammar is actually compiled in — the marker describes what the binary
    /// will *do*, not merely what extension the file has.
    #[test]
    fn ast_eligible_path_diverges_from_base_iff_feature_compiled() {
        let base = expected_embed_fingerprint("fake-model");
        let for_py = expected_embed_fingerprint_for_path("fake-model", "src/module.py");
        if cfg!(feature = "ast-python") {
            assert_ne!(
                for_py, base,
                "AST-eligible paths must diverge once the feature is compiled in"
            );
        } else {
            assert_eq!(
                for_py, base,
                "without the feature compiled, no path is AST-eligible"
            );
        }
    }

    /// `embed_fingerprint_applies_ast` is the single eligibility predicate the
    /// CLI and SQL builder share — assert it agrees with the extension list
    /// `chunking::ast_chunking_extensions()` publishes, not a restated one.
    #[test]
    fn fts_rebuild_is_pending_only_until_it_has_run_over_existing_documents() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut store = open_stub_store(
            dir.path(),
            StubBackend {
                name: "stub".to_string(),
                caps: rqmd_llm::BackendCapabilities {
                    embed: false,
                    rerank: false,
                    generate: false,
                },
            },
        );
        assert!(!store.fts_rebuild_pending().unwrap(), "empty index");
        assert!(!store.rebuild_fts_if_needed().unwrap());

        store
            .index_document("coll", "jp.md", "T", "私は昨日東京都庁を訪れました")
            .unwrap();
        store.flush().unwrap();
        assert!(store.fts_rebuild_pending().unwrap());

        // Simulate an index built by the old tokenizer: the entry is absent.
        store.remove_from_fts("coll/jp.md").unwrap();
        store.flush().unwrap();
        assert!(store.search_fts("東京都", 10, None).unwrap().is_empty());

        assert!(store.rebuild_fts_if_needed().unwrap());
        assert_eq!(store.search_fts("東京都", 10, None).unwrap().len(), 1);
        assert!(!store.fts_rebuild_pending().unwrap());

        // A second call must not rebuild again.
        store.remove_from_fts("coll/jp.md").unwrap();
        store.flush().unwrap();
        assert!(!store.rebuild_fts_if_needed().unwrap());
        assert!(store.search_fts("東京都", 10, None).unwrap().is_empty());
    }

    /// An index written before the weighted `frontmatter` field existed is
    /// reported as needing a rebuild, keeps answering queries meanwhile, and
    /// the rebuild moves it to the current schema with each document's tags
    /// and aliases taken from its stored raw text.
    #[test]
    fn fm_ac_3_a_pre_frontmatter_index_is_pending_and_rebuilds_with_tags() {
        use tantivy::schema::{FAST, STORED, SchemaBuilder, TEXT};

        let dir = tempfile::TempDir::new().unwrap();
        let stub = || StubBackend {
            name: "stub".to_string(),
            caps: rqmd_llm::BackendCapabilities {
                embed: false,
                rerank: false,
                generate: false,
            },
        };
        {
            let mut store = open_stub_store(dir.path(), stub());
            store
                .index_document_fts_only_with_raw(
                    "coll",
                    "tagged.md",
                    "Tagged",
                    "body text\nzirconia",
                    "---\ntags: [zirconia]\n---\nbody text",
                )
                .unwrap();
            store.flush().unwrap();
        }

        // Swap the Tantivy index for one with the old field set holding a
        // different document, and mark the stored version as the old one.
        let tantivy_dir = dir.path().join("tantivy");
        std::fs::remove_dir_all(&tantivy_dir).unwrap();
        std::fs::create_dir_all(&tantivy_dir).unwrap();
        let mut builder = SchemaBuilder::new();
        let filepath = builder.add_text_field("filepath", TEXT | STORED);
        let title = builder.add_text_field("title", TEXT | STORED);
        let body = builder.add_text_field("body", TEXT);
        let doc_id = builder.add_i64_field("doc_id", STORED | FAST);
        let old = tantivy::Index::create_in_dir(&tantivy_dir, builder.build()).unwrap();
        let mut writer: tantivy::IndexWriter = old.writer(15_000_000).unwrap();
        writer
            .add_document(tantivy::doc!(
                filepath => "coll/stale.md", title => "Stale", body => "staleterm", doc_id => 99i64
            ))
            .unwrap();
        writer.commit().unwrap();
        drop(writer);
        drop(old);

        let mut store = open_stub_store(dir.path(), stub());
        db::set_config(&store.db, FTS_TOKENIZER_CONFIG_KEY, "2").unwrap();
        assert!(store.fts_rebuild_pending().unwrap());
        assert!(store.fts.is_legacy_schema());
        assert_eq!(store.search_fts("staleterm", 10, None).unwrap().len(), 0);

        assert!(store.rebuild_fts_if_needed().unwrap());
        assert!(!store.fts.is_legacy_schema());
        assert!(!store.fts_rebuild_pending().unwrap());
        let hits = store.search_fts("zirconia", 10, None).unwrap();
        assert_eq!(hits.len(), 1, "got {hits:?}");
        assert!(store.search_fts("staleterm", 10, None).unwrap().is_empty());
    }

    fn fingerprint_of(sig: &str) -> String {
        hex::encode(&Sha256::digest(sig.as_bytes())[..3])
    }

    fn base_signature(model: &str) -> String {
        format!(
            "model:{model}\nchunk_size_chars:{}\nchunk_overlap_chars:{}\nchunk_strategy_version:{}\nquery_prefix:{}\npassage_prefix:{}",
            crate::chunking::CHUNK_SIZE_CHARS,
            crate::chunking::CHUNK_OVERLAP_CHARS,
            crate::chunking::CHUNK_STRATEGY_VERSION,
            rqmd_llm::EMBEDDINGGEMMA_QUERY_PREFIX,
            rqmd_llm::EMBEDDINGGEMMA_PASSAGE_PREFIX,
        )
    }

    /// Markdown vectors carry the table-header marker, so they are re-embedded
    /// once; AST-chunked source vectors do not, so they are left alone.
    #[test]
    fn only_non_ast_paths_carry_the_table_header_marker() {
        let md = embed_fingerprint("m", "notes/a.md");
        assert_eq!(
            md,
            fingerprint_of(&format!("{}\ntable_header_chunking:1", base_signature("m")))
        );
        assert_ne!(md, fingerprint_of(&base_signature("m")));

        if cfg!(feature = "ast-python") {
            assert_eq!(
                embed_fingerprint("m", "src/a.py"),
                fingerprint_of(&format!("{}\nast_chunking:1", base_signature("m")))
            );
        }
    }

    /// The fingerprints the previous release wrote for the languages that were
    /// already AST-chunked, for the model name `fake-model`. They are literals,
    /// not recomputed, so a change to the registry that moves any of them
    /// fails here instead of silently re-embedding every source file.
    #[cfg(all(
        feature = "ast-typescript",
        feature = "ast-javascript",
        feature = "ast-java",
        feature = "ast-python"
    ))]
    #[test]
    fn existing_ast_languages_keep_their_fingerprints() {
        for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs", "java", "py"] {
            assert_eq!(
                embed_fingerprint("fake-model", &format!("src/a.{ext}")),
                "239714",
                ".{ext} fingerprint moved"
            );
        }
        assert_eq!(embed_fingerprint("fake-model", "notes/a.md"), "284aed");
    }

    /// Each language added after the registry carries its own marker: its files
    /// differ from every other language and from the base fingerprint, while
    /// the extensions of one language share it.
    #[cfg(all(
        feature = "ast-go",
        feature = "ast-rust",
        feature = "ast-csharp",
        feature = "ast-kotlin",
        feature = "ast-swift",
        feature = "ast-hcl",
        feature = "ast-sql",
        feature = "ast-python"
    ))]
    #[test]
    fn added_languages_have_their_own_fingerprints() {
        let fp = |path: &str| embed_fingerprint("fake-model", path);
        let one_per_language = [
            "a.go", "a.rs", "a.cs", "a.kt", "a.swift", "a.tf", "a.sql", "a.py",
        ];
        let mut seen: Vec<String> = one_per_language.iter().map(|p| fp(p)).collect();
        seen.push(fp("a.md"));
        let distinct: std::collections::HashSet<_> = seen.iter().collect();
        assert_eq!(
            distinct.len(),
            seen.len(),
            "two languages share a fingerprint"
        );

        assert_eq!(fp("a.kt"), fp("a.kts"));
        assert_eq!(fp("a.tf"), fp("a.tfvars"));
        assert_eq!(fp("a.tf"), fp("a.hcl"));
        assert_eq!(
            fp("a.go"),
            fingerprint_of(&format!(
                "{}\nast_chunking:1\nast_language:go:1",
                base_signature("fake-model")
            ))
        );
    }

    /// The current set is the base fingerprint plus one per marker group, and
    /// every group's extensions map to the fingerprint the document would get.
    #[test]
    fn expected_fingerprint_set_covers_base_and_every_ast_group() {
        let groups = expected_embed_ast_fingerprints("fake-model");
        let base = expected_embed_fingerprint("fake-model");
        for group in &groups {
            assert_ne!(group.fingerprint, base);
            for ext in &group.extensions {
                assert_eq!(
                    group.fingerprint,
                    embed_fingerprint("fake-model", &format!("x.{ext}")),
                    ".{ext}"
                );
            }
        }
        let covered: usize = groups.iter().map(|g| g.extensions.len()).sum();
        assert_eq!(covered, crate::chunking::ast_chunking_extensions().len());
        assert_eq!(groups.is_empty(), !crate::chunking::ast_chunking_compiled());
    }

    #[test]
    fn embed_fingerprint_applies_ast_matches_extension_list() {
        for ext in crate::chunking::ast_chunking_extensions() {
            assert!(
                embed_fingerprint_applies_ast(&format!("some/path/file.{ext}")),
                "extension {ext} is in ast_chunking_extensions() but not treated as eligible"
            );
        }
        assert!(!embed_fingerprint_applies_ast("docs/readme.md"));
        assert!(!embed_fingerprint_applies_ast("no_extension"));
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

    /// A backend whose `generate` returns a fixed, caller-supplied expansion
    /// string — `StubBackend::generate` panics unconditionally, so this
    /// exists only to give `parse_and_run_expansion` a controllable output to
    /// dedup. `embed` returns a constant vector (like `StubBackend`) so any
    /// two texts always cosine-match, making vector search deterministic
    /// without depending on real embedding math.
    struct FixedExpansionBackend {
        expansion: String,
    }

    impl rqmd_llm::InferenceBackend for FixedExpansionBackend {
        fn embed(&mut self, _text: &str) -> Result<Vec<f32>> {
            Ok(vec![0.1; rqmd_llm::EMBED_DIM])
        }
        fn rerank(&mut self, _query: &str, docs: &[&str]) -> Result<Vec<f32>> {
            Ok(vec![0.0; docs.len()])
        }
        fn generate(&mut self, _prompt: &str) -> Result<String> {
            Ok(self.expansion.clone())
        }
        fn embed_model_name(&self) -> &str {
            "fixed-expansion"
        }
        fn rerank_model_name(&self) -> &str {
            "fixed-expansion"
        }
        fn generate_model_name(&self) -> &str {
            "fixed-expansion"
        }
    }

    /// qmd#952/#1000/#921 parity (rqmd#86): a generation model repeating an
    /// identical `vec:`/`lex:`/`hyde:` line in one expansion output must only
    /// run that search once, and a line that just repeats the original query
    /// must not run at all — both re-runs the original-query legs already
    /// cover with zero new candidates.
    #[test]
    fn parse_and_run_expansion_dedups_repeated_lines_and_drops_the_original_query() {
        let dir = tempfile::TempDir::new().unwrap();
        let backend = FixedExpansionBackend {
            expansion: "lex: original query\nvec: duplicate line\nvec: duplicate line\n"
                .to_string(),
        };
        let config = StoreConfig {
            db_path: dir.path().join("test.sqlite"),
            tantivy_dir: dir.path().join("tantivy"),
            hnsw_path: dir.path().join("hnsw.usearch"),
            read_only: false,
        };
        let mut store = Store::open(config, Box::new(backend)).unwrap();

        store
            .index_document("coll", "a.md", "Title", "some content here")
            .unwrap();
        store.flush().unwrap();

        let (lists, metas) = store
            .parse_and_run_expansion(
                "lex: original query\nvec: duplicate line\nvec: duplicate line\n",
                "original query",
                None,
                10,
                Deadline::none(),
            )
            .unwrap();

        assert_eq!(
            metas.len(),
            1,
            "expected exactly one surviving expansion list, got {metas:?}"
        );
        assert_eq!(metas[0].source, "expand-vec");
        assert_eq!(lists.len(), 1);
        assert_eq!(
            lists[0].len(),
            1,
            "the deduped vec: line should still find the one indexed document"
        );
    }

    // ── Chunk-level embedding reuse ──────────────────────────────────────────

    /// Embeds deterministically from the text and records every text it is
    /// asked to encode, so a test can tell reused chunks from encoded ones.
    struct RecordingBackend {
        encoded: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        kind: &'static str,
        context_size: usize,
    }

    impl rqmd_llm::InferenceBackend for RecordingBackend {
        fn capabilities(&self) -> rqmd_llm::BackendCapabilities {
            rqmd_llm::BackendCapabilities {
                embed: true,
                rerank: false,
                generate: false,
            }
        }
        fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
            self.encoded.lock().unwrap().push(text.to_string());
            let digest = Sha256::digest(text.as_bytes());
            Ok((0..rqmd_llm::EMBED_DIM)
                .map(|i| f32::from(digest[i % digest.len()]) / 255.0 + 0.01)
                .collect())
        }
        fn rerank(&mut self, _query: &str, _docs: &[&str]) -> Result<Vec<f32>> {
            panic!("rerank is not used here")
        }
        fn generate(&mut self, _prompt: &str) -> Result<String> {
            panic!("generate is not used here")
        }
        fn embed_model_name(&self) -> &str {
            "recording-embed"
        }
        fn embed_backend_kind(&self) -> &str {
            self.kind
        }
        fn embed_context_size(&self) -> usize {
            self.context_size
        }
        fn rerank_model_name(&self) -> &str {
            "recording-rerank"
        }
        fn generate_model_name(&self) -> &str {
            "recording-generate"
        }
    }

    type EncodedLog = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    fn recording_store(dir: &std::path::Path) -> (Store, EncodedLog) {
        recording_store_as(dir, "recording", 2048)
    }

    /// Opens (or reopens) the store in `dir` with a backend that reports the
    /// given engine kind and context size.
    fn recording_store_as(
        dir: &std::path::Path,
        kind: &'static str,
        context_size: usize,
    ) -> (Store, EncodedLog) {
        let encoded = EncodedLog::default();
        let store = open_stub_store_with(
            dir,
            Box::new(RecordingBackend {
                encoded: encoded.clone(),
                kind,
                context_size,
            }),
        );
        (store, encoded)
    }

    fn open_stub_store_with(dir: &std::path::Path, backend: Box<dyn InferenceBackend>) -> Store {
        let config = StoreConfig {
            db_path: dir.join("test.sqlite"),
            tantivy_dir: dir.join("tantivy"),
            hnsw_path: dir.join("hnsw.usearch"),
            read_only: false,
        };
        Store::open(config, backend).unwrap()
    }

    /// A note of `sections` sections, each long enough to fill its own chunk.
    /// `edited` swaps one sentence of the last section for different text of
    /// the same length, so every earlier chunk boundary stays where it was.
    fn long_note(sections: usize, edited: bool) -> String {
        (0..sections)
            .map(|n| {
                let tail = if edited && n + 1 == sections {
                    "an entirely different closing remark"
                } else {
                    "the original closing remark written"
                };
                let filler: String = (0..40)
                    .map(|k| format!("Section {n} sentence {k} describes topic {n}-{k}. "))
                    .collect();
                format!("# Section {n}\n\n{filler}{tail}.\n\n")
            })
            .collect()
    }

    /// Store a document's pending vector rows the way `rqmd embed`'s
    /// checkpoint does, and make them visible to later reuse lookups.
    fn persist(store: &mut Store, hash: &str, pending: &[PendingVectorMeta]) {
        upsert_content(&store.db, hash, "body", "2026-01-01T00:00:00Z").unwrap();
        for m in pending {
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
            db::set_vector_identity(&store.db, &m.hash, m.seq, &m.chunk_hash, &m.vector_key)
                .unwrap();
        }
        store.vectors_committed();
    }

    fn embed_and_persist(store: &mut Store, hash: &str, body: &str) -> Vec<PendingVectorMeta> {
        let pending = store.embed_document_chunks(hash, "note.md", body).unwrap();
        persist(store, hash, &pending);
        pending
    }

    #[test]
    fn chunk_ac_1_only_the_changed_chunks_are_encoded_after_an_edit() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, encoded) = recording_store(dir.path());
        let before = long_note(5, false);
        let after = long_note(5, true);

        let first = embed_and_persist(&mut store, "hash-before", &before);
        assert!(first.len() >= 3, "fixture must span several chunks");
        encoded.lock().unwrap().clear();

        let second = embed_and_persist(&mut store, "hash-after", &after);

        let old_texts: HashSet<String> = chunk_document_for_path("note.md", &before)
            .into_iter()
            .map(|c| c.text)
            .collect();
        let changed: Vec<String> = chunk_document_for_path("note.md", &after)
            .into_iter()
            .map(|c| c.text)
            .filter(|t| !old_texts.contains(t))
            .collect();
        assert!(!changed.is_empty() && changed.len() < second.len());
        let mut got = encoded.lock().unwrap().clone();
        let mut want = changed.clone();
        got.sort();
        want.sort();
        assert_eq!(got, want, "only the edited chunks may be encoded");
        assert_eq!(second.len(), first.len());
    }

    #[test]
    fn chunk_ac_2_reused_vectors_equal_a_full_re_embed() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, _) = recording_store(dir.path());
        embed_and_persist(&mut store, "hash-before", &long_note(5, false));
        let reused = embed_and_persist(&mut store, "hash-after", &long_note(5, true));

        let fresh_dir = tempfile::TempDir::new().unwrap();
        let (mut fresh_store, _) = recording_store(fresh_dir.path());
        let full = embed_and_persist(&mut fresh_store, "hash-after", &long_note(5, true));

        assert_eq!(reused.len(), full.len());
        for (r, f) in reused.iter().zip(&full) {
            assert_eq!(r.chunk_hash, f.chunk_hash);
            assert_eq!(
                store.hnsw.get_vector(r.vid).unwrap(),
                fresh_store.hnsw.get_vector(f.vid).unwrap(),
                "chunk {} must hold the same vector either way",
                r.seq
            );
        }
    }

    /// A small file of a language the registry added was embedded by the
    /// previous release under the base fingerprint. It is pending once under
    /// the language's own fingerprint, and re-embedding it encodes nothing,
    /// because its single chunk is the same text either way.
    #[cfg(feature = "ast-go")]
    #[test]
    fn added_language_file_is_re_embedded_once_without_encoding() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, encoded) = recording_store(dir.path());
        let body = "package main\n\nfunc alpha() {\n\t_ = 1\n}\n";
        let hash = content_hash(body);
        upsert_content(&store.db, &hash, body, "2026-01-01T00:00:00Z").unwrap();
        upsert_document(
            &store.db,
            "c",
            "main.go",
            "main",
            &hash,
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        // What the previous release stored: the markdown chunker's fingerprint.
        let old = store.embed_document_chunks(&hash, "main.md", body).unwrap();
        persist(&mut store, &hash, &old);
        let base = expected_embed_fingerprint("recording-embed");
        let go = expected_embed_ast_fingerprints("recording-embed");
        assert_eq!(old[0].fingerprint, base);
        let pending = |store: &Store| db::count_docs_needing_embed(&store.db, &base, &go).unwrap();
        assert_eq!(
            pending(&store),
            1,
            "the new fingerprint makes the file pending"
        );

        encoded.lock().unwrap().clear();
        let new = store.embed_document_chunks(&hash, "main.go", body).unwrap();
        persist(&mut store, &hash, &new);
        assert!(
            encoded.lock().unwrap().is_empty(),
            "an unchanged chunk reuses its vector"
        );
        assert_eq!(
            new[0].fingerprint,
            embed_fingerprint("recording-embed", "main.go")
        );
        assert_ne!(new[0].fingerprint, base);
        assert_eq!(pending(&store), 0, "re-embedded once, then current");
    }

    #[test]
    fn chunk_ac_3_a_vector_recorded_under_another_model_is_never_reused() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, encoded) = recording_store(dir.path());
        let body = long_note(4, false);
        let first = embed_and_persist(&mut store, "hash-one", &body);
        store
            .db
            .execute("UPDATE content_vectors SET model = 'someone-else'", [])
            .unwrap();
        encoded.lock().unwrap().clear();

        embed_and_persist(&mut store, "hash-two", &body);

        assert_eq!(
            encoded.lock().unwrap().len(),
            first.len(),
            "a differing model must force every chunk to be encoded"
        );
    }

    #[test]
    fn chunk_ac_4_a_prompt_prefix_change_changes_the_fingerprint() {
        let base = embed_fingerprint_with_prefixes("m", "a.md", "query: ", "passage: ");
        assert_ne!(
            base,
            embed_fingerprint_with_prefixes("m", "a.md", "search: ", "passage: ")
        );
        assert_ne!(
            base,
            embed_fingerprint_with_prefixes("m", "a.md", "query: ", "document: ")
        );
        assert_eq!(
            embed_fingerprint("m", "a.md"),
            embed_fingerprint_with_prefixes(
                "m",
                "a.md",
                rqmd_llm::EMBEDDINGGEMMA_QUERY_PREFIX,
                rqmd_llm::EMBEDDINGGEMMA_PASSAGE_PREFIX
            ),
            "the live fingerprint must be built from the prefixes the backend sends"
        );
    }

    #[test]
    fn chunk_ac_5_rows_without_a_chunk_hash_still_work_and_get_filled_in() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, encoded) = recording_store(dir.path());
        let body = long_note(4, false);
        let first = embed_and_persist(&mut store, "hash-one", &body);
        store
            .db
            .execute(
                "UPDATE content_vectors SET chunk_hash = NULL, vector_key = NULL",
                [],
            )
            .unwrap();
        encoded.lock().unwrap().clear();

        let second = embed_and_persist(&mut store, "hash-two", &body);

        assert_eq!(encoded.lock().unwrap().len(), first.len());
        let filled: i64 = store
            .db
            .query_row(
                "SELECT COUNT(*) FROM content_vectors \
                 WHERE hash = 'hash-two' AND chunk_hash IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(filled as usize, second.len());

        encoded.lock().unwrap().clear();
        embed_and_persist(&mut store, "hash-three", &body);
        assert!(
            encoded.lock().unwrap().is_empty(),
            "once filled in, the chunks are reusable"
        );
    }

    #[test]
    fn chunk_ac_5_a_database_created_before_the_column_is_upgraded_on_open() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("old.sqlite");
        {
            let conn = open_db(&path).unwrap();
            conn.execute("DROP INDEX idx_content_vectors_chunk_hash", [])
                .unwrap();
            conn.execute("ALTER TABLE content_vectors DROP COLUMN chunk_hash", [])
                .unwrap();
        }
        let conn = open_db(&path).unwrap();
        let has_column: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('content_vectors') \
                 WHERE name = 'chunk_hash'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_column, 1);
    }

    #[test]
    fn key_ac_1_identical_chunks_in_two_documents_are_encoded_once() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, encoded) = recording_store(dir.path());
        let body = long_note(4, false);

        let first = store
            .embed_document_chunks("hash-a", "a.md", &body)
            .unwrap();
        let encoded_for_first = encoded.lock().unwrap().len();
        assert_eq!(encoded_for_first, first.len());

        // The first document's rows are not stored yet, as inside one embed run.
        let second = store
            .embed_document_chunks("hash-b", "b.md", &body)
            .unwrap();

        assert_eq!(second.len(), first.len());
        assert_eq!(
            encoded.lock().unwrap().len(),
            encoded_for_first,
            "the second document's chunks all match staged vectors"
        );
        for (a, b) in first.iter().zip(&second) {
            assert_eq!(a.vector_key, b.vector_key);
            assert_eq!(
                store.hnsw.get_vector(a.vid).unwrap(),
                store.hnsw.get_vector(b.vid).unwrap()
            );
        }
    }

    #[test]
    fn key_ac_2_a_vector_is_reused_under_a_different_chunking_fingerprint() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, encoded) = recording_store(dir.path());
        let body = long_note(4, false);
        embed_and_persist(&mut store, "hash-one", &body);
        store
            .db
            .execute(
                "UPDATE content_vectors SET embed_fingerprint = 'older-chunking'",
                [],
            )
            .unwrap();
        encoded.lock().unwrap().clear();

        embed_and_persist(&mut store, "hash-two", &body);

        assert!(
            encoded.lock().unwrap().is_empty(),
            "same text, backend and context size: nothing to encode"
        );
    }

    #[test]
    fn key_ac_3_another_backend_kind_or_context_size_never_reuses() {
        let body = long_note(4, false);
        for (kind, context_size) in [("other-engine", 2048), ("recording", 512)] {
            let dir = tempfile::TempDir::new().unwrap();
            let (mut first_store, first_log) = recording_store(dir.path());
            let first = embed_and_persist(&mut first_store, "hash-one", &body);
            first_store.flush().unwrap();
            let chunks = first.len();
            assert_eq!(first_log.lock().unwrap().len(), chunks);
            drop(first_store);

            let (mut same, same_log) = recording_store(dir.path());
            embed_and_persist(&mut same, "hash-two", &body);
            assert!(
                same_log.lock().unwrap().is_empty(),
                "control: an identical backend reuses every chunk"
            );
            drop(same);

            let (mut changed, changed_log) = recording_store_as(dir.path(), kind, context_size);
            embed_and_persist(&mut changed, "hash-three", &body);
            assert_eq!(
                changed_log.lock().unwrap().len(),
                chunks,
                "{kind} / {context_size}: every chunk must be encoded again"
            );
        }
    }

    #[test]
    fn key_ac_4_vectors_without_a_key_are_backfilled_without_re_encoding() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, encoded) = recording_store(dir.path());
        let body = long_note(4, false);
        let first = embed_and_persist(&mut store, "hash-one", &body);
        store
            .db
            .execute("UPDATE content_vectors SET vector_key = NULL", [])
            .unwrap();
        // One row embedded under another model must be left alone.
        store
            .db
            .execute(
                "UPDATE content_vectors SET model = 'someone-else' WHERE seq = 0",
                [],
            )
            .unwrap();
        encoded.lock().unwrap().clear();

        let filled = store.backfill_vector_keys().unwrap();

        assert_eq!(filled, first.len() - 1);
        assert!(
            encoded.lock().unwrap().is_empty(),
            "the backfill reads metadata only"
        );
        let unkeyed: i64 = store
            .db
            .query_row(
                "SELECT COUNT(*) FROM content_vectors WHERE vector_key IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unkeyed, 1);
        assert_eq!(store.backfill_vector_keys().unwrap(), 0, "idempotent");

        embed_and_persist(&mut store, "hash-two", &body);
        assert_eq!(
            encoded.lock().unwrap().len(),
            1,
            "only the chunk whose row was left alone is encoded again"
        );
    }

    #[test]
    fn key_ac_5_a_database_created_before_the_column_is_upgraded_on_open() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("old.sqlite");
        {
            let conn = open_db(&path).unwrap();
            conn.execute("DROP INDEX idx_content_vectors_vector_key", [])
                .unwrap();
            conn.execute("ALTER TABLE content_vectors DROP COLUMN vector_key", [])
                .unwrap();
        }
        let conn = open_db(&path).unwrap();
        let has_column: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('content_vectors') \
                 WHERE name = 'vector_key'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_column, 1);
    }

    #[test]
    fn key_ac_6_the_key_changes_with_each_input() {
        let base = vector_key("llama", "m", 2048, "c");
        assert_eq!(base, vector_key("llama", "m", 2048, "c"));
        assert_ne!(base, vector_key("ort", "m", 2048, "c"));
        assert_ne!(base, vector_key("llama", "n", 2048, "c"));
        assert_ne!(base, vector_key("llama", "m", 512, "c"));
        assert_ne!(base, vector_key("llama", "m", 2048, "d"));
    }

    fn tie_stub() -> StubBackend {
        StubBackend {
            name: "stub-org/stub-embed.onnx".to_string(),
            caps: rqmd_llm::BackendCapabilities {
                embed: true,
                rerank: false,
                generate: false,
            },
        }
    }

    /// The stub embeds every text to the same vector, so every document ties
    /// on similarity. Result order must not depend on indexing order.
    #[test]
    fn vector_ties_order_by_path_in_any_insertion_order() {
        let paths = ["m.md", "c.md", "x.md", "a.md"];
        for round in 0..4 {
            let dir = tempfile::TempDir::new().unwrap();
            let mut store = open_stub_store(dir.path(), tie_stub());
            let mut order = paths.to_vec();
            order.rotate_left(round);
            for p in &order {
                store
                    .index_document("coll", p, "T", &format!("body of {p}"))
                    .unwrap();
            }

            let got: Vec<String> = store
                .search_vec("anything", 10, Some("coll"))
                .unwrap()
                .into_iter()
                .map(|r| r.path)
                .collect();
            assert_eq!(
                got,
                ["a.md", "c.md", "m.md", "x.md"],
                "search_vec {order:?}"
            );

            let hash = content_hash("body of m.md");
            let got: Vec<String> = store
                .similar_to_hash(&hash, "coll", "m.md", 10)
                .unwrap()
                .into_iter()
                .map(|r| r.path)
                .collect();
            assert_eq!(got, ["a.md", "c.md", "x.md"], "similar {order:?}");
        }
    }

    /// Candidates with equal final scores keep a path order whatever order
    /// the fused list handed them over in.
    #[test]
    fn assemble_results_orders_equal_scores_by_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = open_stub_store(dir.path(), tie_stub());
        let ids = ["m", "c", "x", "a"];
        for round in 0..ids.len() {
            let mut order = ids.to_vec();
            order.rotate_left(round);
            let candidates: Vec<(RankedResult, String, String)> = order
                .iter()
                .map(|id| {
                    (
                        RankedResult {
                            filepath: format!("coll/{id}.md"),
                            title: id.to_string(),
                            backend_score: 0.5,
                        },
                        format!("{id}hash"),
                        format!("body {id}"),
                    )
                })
                .collect();
            let chunks: Vec<Vec<(String, usize)>> =
                order.iter().map(|_| vec![(String::new(), 0)]).collect();
            let got: Vec<String> = store
                .assemble_results(candidates, chunks, None, None, 10)
                .into_iter()
                .map(|r| r.path)
                .collect();
            assert_eq!(got, ["a.md", "c.md", "m.md", "x.md"], "order {order:?}");
        }
    }

    /// The pool is four times the limit, capped at 200, never below the limit
    /// and never overflowing.
    #[test]
    fn recency_pool_is_four_times_the_limit_capped_and_never_below_it() {
        assert_eq!(recency_pool(0), 0);
        assert_eq!(recency_pool(1), 4);
        assert_eq!(recency_pool(10), 40);
        assert_eq!(recency_pool(50), 200);
        assert_eq!(recency_pool(51), 200);
        assert_eq!(recency_pool(200), 200);
        assert_eq!(recency_pool(201), 201);
        assert_eq!(recency_pool(usize::MAX), usize::MAX);
    }

    /// Candidates whose recency factors are equal keep the path order; a
    /// smaller factor moves a candidate down without dropping it.
    #[test]
    fn assemble_results_with_recency_keeps_ties_in_path_order() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = open_stub_store(dir.path(), tie_stub());
        let ids = ["m", "c", "x", "a"];
        for (factors, expected) in [
            (vec![0.8; 4], vec!["a.md", "c.md", "m.md", "x.md"]),
            // Factors follow `ids`: m 1.0, c 0.5, x 1.0, a 0.5.
            (
                vec![1.0, 0.5, 1.0, 0.5],
                vec!["m.md", "x.md", "a.md", "c.md"],
            ),
        ] {
            let candidates: Vec<(RankedResult, String, String)> = ids
                .iter()
                .map(|id| {
                    (
                        RankedResult {
                            filepath: format!("coll/{id}.md"),
                            title: id.to_string(),
                            backend_score: 0.5,
                        },
                        format!("{id}hash"),
                        format!("body {id}"),
                    )
                })
                .collect();
            let chunks: Vec<Vec<(String, usize)>> =
                ids.iter().map(|_| vec![(String::new(), 0)]).collect();
            let got: Vec<String> = store
                .assemble_results(candidates, chunks, None, Some(factors), 10)
                .into_iter()
                .map(|r| r.path)
                .collect();
            assert_eq!(got, expected);
        }
    }

    /// What a [`RerankStub`] hands back from `rerank`.
    enum RerankMode {
        Fail,
        Scores(Vec<f32>),
        AllNegInfinity,
    }

    /// Embeds like [`StubBackend`], but its reranker is scripted.
    struct RerankStub {
        mode: RerankMode,
        can_rerank: bool,
    }

    impl rqmd_llm::InferenceBackend for RerankStub {
        fn capabilities(&self) -> rqmd_llm::BackendCapabilities {
            rqmd_llm::BackendCapabilities {
                embed: true,
                rerank: self.can_rerank,
                generate: false,
            }
        }
        fn embed(&mut self, _text: &str) -> Result<Vec<f32>> {
            Ok(vec![0.1; rqmd_llm::EMBED_DIM])
        }
        fn rerank(&mut self, _query: &str, docs: &[&str]) -> Result<Vec<f32>> {
            match &self.mode {
                RerankMode::Fail => bail!("rerank model blew up at /secret/models/rerank.gguf"),
                RerankMode::Scores(s) => Ok(s.clone()),
                RerankMode::AllNegInfinity => Ok(vec![f32::NEG_INFINITY; docs.len()]),
            }
        }
        fn generate(&mut self, _prompt: &str) -> Result<String> {
            panic!("generate must not be called")
        }
        fn embed_model_name(&self) -> &str {
            "rerank-stub"
        }
        fn rerank_model_name(&self) -> &str {
            "rerank-stub"
        }
        fn generate_model_name(&self) -> &str {
            "rerank-stub"
        }
    }

    fn rerank_store(dir: &std::path::Path, mode: RerankMode, can_rerank: bool) -> Store {
        let config = StoreConfig {
            db_path: dir.join("test.sqlite"),
            tantivy_dir: dir.join("tantivy"),
            hnsw_path: dir.join("hnsw.usearch"),
            read_only: false,
        };
        let mut store = Store::open(config, Box::new(RerankStub { mode, can_rerank })).unwrap();
        for (path, body) in [
            ("a.md", "uniquerankterm alpha"),
            ("b.md", "uniquerankterm beta beta"),
            ("c.md", "uniquerankterm gamma gamma gamma"),
        ] {
            store.index_document("coll", path, "T", body).unwrap();
        }
        store.flush().unwrap();
        store
    }

    fn run_outcome(store: &mut Store, skip_rerank: bool) -> QueryOutcome {
        store
            .hybrid_query_outcome(
                "uniquerankterm",
                None,
                10,
                None,
                skip_rerank,
                true,
                Deadline::none(),
                false,
            )
            .unwrap()
    }

    fn files_and_scores(outcome: &QueryOutcome) -> Vec<(String, f32)> {
        outcome
            .results
            .iter()
            .map(|r| (r.file.clone(), r.score))
            .collect()
    }

    /// A reranker that cannot rank leaves the fusion order and fusion scores
    /// exactly as a `--no-rerank` run produces them, and the outcome says so.
    fn assert_falls_back_to_fusion(mode: RerankMode) {
        let dir = tempfile::TempDir::new().unwrap();
        let mut store = rerank_store(dir.path(), mode, true);
        let fusion = run_outcome(&mut store, true);
        assert_eq!(fusion.rerank, RerankStatus::NotRun);
        assert_eq!(fusion.results.len(), 3);

        let got = run_outcome(&mut store, false);

        assert_eq!(got.rerank, RerankStatus::Failed);
        assert_eq!(files_and_scores(&got), files_and_scores(&fusion));
    }

    #[test]
    fn rerank_error_keeps_fusion_order_and_reports_failed() {
        assert_falls_back_to_fusion(RerankMode::Fail);
    }

    #[test]
    fn rerank_with_too_few_scores_is_reported_failed_instead_of_mixing_scales() {
        assert_falls_back_to_fusion(RerankMode::Scores(vec![0.9]));
    }

    #[test]
    fn rerank_with_too_many_scores_is_reported_failed() {
        assert_falls_back_to_fusion(RerankMode::Scores(vec![0.9; 4]));
    }

    #[test]
    fn rerank_with_no_finite_score_is_reported_failed() {
        assert_falls_back_to_fusion(RerankMode::AllNegInfinity);
    }

    #[test]
    fn rerank_with_usable_scores_is_reported_reranked_and_reorders() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut store = rerank_store(dir.path(), RerankMode::Scores(vec![0.0, 0.5, 1.0]), true);
        let fusion = run_outcome(&mut store, true);

        let got = run_outcome(&mut store, false);

        assert_eq!(got.rerank, RerankStatus::Reranked);
        let mut reversed: Vec<String> = fusion.results.iter().map(|r| r.file.clone()).collect();
        reversed.reverse();
        let got_files: Vec<String> = got.results.iter().map(|r| r.file.clone()).collect();
        assert_eq!(got_files, reversed);
    }

    #[test]
    fn skipping_rerank_is_reported_not_run_and_never_calls_the_reranker() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut store = rerank_store(dir.path(), RerankMode::Fail, true);
        assert_eq!(run_outcome(&mut store, true).rerank, RerankStatus::NotRun);
    }

    #[test]
    fn a_backend_without_rerank_is_reported_unsupported() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut store = rerank_store(dir.path(), RerankMode::Fail, false);
        let got = run_outcome(&mut store, false);
        assert_eq!(got.rerank, RerankStatus::Unsupported);
        assert_eq!(got.results.len(), 3);
    }

    #[test]
    fn the_rerank_notice_is_one_line_without_paths_and_only_for_degraded_runs() {
        assert_eq!(RerankStatus::Reranked.notice(), None);
        assert_eq!(RerankStatus::NotRun.notice(), None);
        for status in [
            RerankStatus::Unsupported,
            RerankStatus::Failed,
            RerankStatus::DeadlineExpired,
        ] {
            let notice = status.notice().expect("degraded run has a notice");
            assert!(!notice.contains('\n'), "{notice}");
            assert!(!notice.contains('/'), "{notice}");
            assert!(!notice.contains("rqmd://"), "{notice}");
        }
    }

    /// Embeds slowly, so a short deadline passes before the rerank stage.
    /// Counts plain `rerank` calls; with `interrupt` set, `rerank_until`
    /// reports that the deadline cut the rerank short.
    struct DeadlineStub {
        embed_delay: Duration,
        rerank_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        interrupt: bool,
    }

    impl rqmd_llm::InferenceBackend for DeadlineStub {
        fn embed(&mut self, _text: &str) -> Result<Vec<f32>> {
            std::thread::sleep(self.embed_delay);
            Ok(vec![0.1; rqmd_llm::EMBED_DIM])
        }
        fn rerank(&mut self, _query: &str, docs: &[&str]) -> Result<Vec<f32>> {
            self.rerank_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok((0..docs.len()).map(|i| i as f32).collect())
        }
        fn rerank_until(
            &mut self,
            query: &str,
            docs: &[&str],
            expired: &dyn Fn() -> bool,
        ) -> Result<Option<Vec<f32>>> {
            if self.interrupt || expired() {
                return Ok(None);
            }
            self.rerank(query, docs).map(Some)
        }
        fn generate(&mut self, _prompt: &str) -> Result<String> {
            panic!("generate must not be called")
        }
        fn embed_model_name(&self) -> &str {
            "deadline-stub"
        }
        fn rerank_model_name(&self) -> &str {
            "deadline-stub"
        }
        fn generate_model_name(&self) -> &str {
            "deadline-stub"
        }
    }

    fn deadline_store(
        dir: &std::path::Path,
        interrupt: bool,
    ) -> (Store, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let rerank_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let config = StoreConfig {
            db_path: dir.join("test.sqlite"),
            tantivy_dir: dir.join("tantivy"),
            hnsw_path: dir.join("hnsw.usearch"),
            read_only: false,
        };
        let backend = DeadlineStub {
            embed_delay: Duration::from_millis(150),
            rerank_calls: rerank_calls.clone(),
            interrupt,
        };
        let mut store = Store::open(config, Box::new(backend)).unwrap();
        for (path, body) in [
            ("a.md", "uniquerankterm alpha"),
            ("b.md", "uniquerankterm beta beta"),
            ("c.md", "uniquerankterm gamma gamma gamma"),
        ] {
            store.index_document("coll", path, "T", body).unwrap();
        }
        store.flush().unwrap();
        (store, rerank_calls)
    }

    fn run_with_deadline(
        store: &mut Store,
        skip_rerank: bool,
        deadline: Deadline,
        strict_timeout: bool,
    ) -> Result<QueryOutcome> {
        store.hybrid_query_outcome(
            "uniquerankterm",
            None,
            10,
            None,
            skip_rerank,
            true,
            deadline,
            strict_timeout,
        )
    }

    fn calls(n: &std::sync::atomic::AtomicUsize) -> usize {
        n.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[test]
    fn soft_deadline_before_rerank_returns_fusion_results_and_skips_the_reranker() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, rerank_calls) = deadline_store(dir.path(), false);
        let fusion = run_with_deadline(&mut store, true, Deadline::none(), false).unwrap();

        let got = run_with_deadline(
            &mut store,
            false,
            Deadline::after(Duration::from_millis(20)),
            false,
        )
        .unwrap();

        assert_eq!(got.rerank, RerankStatus::DeadlineExpired);
        assert_eq!(files_and_scores(&got), files_and_scores(&fusion));
        assert_eq!(got.results.len(), 3);
        assert_eq!(calls(&rerank_calls), 0);
    }

    #[test]
    fn strict_deadline_before_rerank_fails_naming_the_rerank_stage() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, rerank_calls) = deadline_store(dir.path(), false);

        let err = run_with_deadline(
            &mut store,
            false,
            Deadline::after(Duration::from_millis(20)),
            true,
        )
        .unwrap_err();

        assert_eq!(timed_out(&err).stage, "rerank");
        assert_eq!(calls(&rerank_calls), 0);
    }

    #[test]
    fn soft_deadline_reached_during_rerank_returns_fusion_results() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, _) = deadline_store(dir.path(), true);
        let fusion = run_with_deadline(&mut store, true, Deadline::none(), false).unwrap();

        let got = run_with_deadline(
            &mut store,
            false,
            Deadline::after(Duration::from_secs(60)),
            false,
        )
        .unwrap();

        assert_eq!(got.rerank, RerankStatus::DeadlineExpired);
        assert_eq!(files_and_scores(&got), files_and_scores(&fusion));
    }

    #[test]
    fn soft_deadline_with_time_left_still_reranks() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, rerank_calls) = deadline_store(dir.path(), false);

        let got = run_with_deadline(
            &mut store,
            false,
            Deadline::after(Duration::from_secs(60)),
            false,
        )
        .unwrap();

        assert_eq!(got.rerank, RerankStatus::Reranked);
        assert_eq!(calls(&rerank_calls), 1);
    }

    #[test]
    fn strict_deadline_with_time_left_still_reranks() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, rerank_calls) = deadline_store(dir.path(), true);

        let got = run_with_deadline(
            &mut store,
            false,
            Deadline::after(Duration::from_secs(60)),
            true,
        )
        .unwrap();

        assert_eq!(got.rerank, RerankStatus::Reranked);
        assert_eq!(calls(&rerank_calls), 1);
    }

    #[test]
    fn soft_deadline_with_rerank_switched_off_reports_not_run() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, rerank_calls) = deadline_store(dir.path(), false);

        let got = run_with_deadline(
            &mut store,
            true,
            Deadline::after(Duration::from_millis(20)),
            false,
        )
        .unwrap();

        assert_eq!(got.rerank, RerankStatus::NotRun);
        assert_eq!(calls(&rerank_calls), 0);
    }

    /// How a [`SliceStub`] reranker behaves on its `n`th call (1-based).
    #[derive(Clone, Copy)]
    enum SliceFault {
        None,
        FailOn(usize),
        ExpireOn(usize),
    }

    /// Scores each candidate from its own text, and records how many
    /// candidates each reranker call carried.
    struct SliceStub {
        calls: std::sync::Arc<std::sync::Mutex<Vec<usize>>>,
        fault: SliceFault,
    }

    impl rqmd_llm::InferenceBackend for SliceStub {
        fn embed(&mut self, _text: &str) -> Result<Vec<f32>> {
            Ok(vec![0.1; rqmd_llm::EMBED_DIM])
        }
        fn rerank(&mut self, _query: &str, docs: &[&str]) -> Result<Vec<f32>> {
            let mut calls = self.calls.lock().unwrap();
            calls.push(docs.len());
            if matches!(self.fault, SliceFault::FailOn(n) if n == calls.len()) {
                bail!("rerank model failed");
            }
            Ok(docs.iter().map(|d| d.len() as f32 / 100.0).collect())
        }
        fn rerank_until(
            &mut self,
            query: &str,
            docs: &[&str],
            expired: &dyn Fn() -> bool,
        ) -> Result<Option<Vec<f32>>> {
            let next = self.calls.lock().unwrap().len() + 1;
            if expired() || matches!(self.fault, SliceFault::ExpireOn(n) if n == next) {
                return Ok(None);
            }
            self.rerank(query, docs).map(Some)
        }
        fn generate(&mut self, _prompt: &str) -> Result<String> {
            Ok("lex: unrelatedterm\nvec: another phrasing\n".to_string())
        }
        fn embed_model_name(&self) -> &str {
            "slice-stub"
        }
        fn rerank_model_name(&self) -> &str {
            "slice-stub"
        }
        fn generate_model_name(&self) -> &str {
            "slice-stub"
        }
    }

    /// Nine candidates, so slices of four are 4 + 4 + 1.
    fn slice_store(
        dir: &std::path::Path,
        fault: SliceFault,
    ) -> (Store, std::sync::Arc<std::sync::Mutex<Vec<usize>>>) {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let config = StoreConfig {
            db_path: dir.join("test.sqlite"),
            tantivy_dir: dir.join("tantivy"),
            hnsw_path: dir.join("hnsw.usearch"),
            read_only: false,
        };
        let backend = SliceStub {
            calls: calls.clone(),
            fault,
        };
        let mut store = Store::open(config, Box::new(backend)).unwrap();
        for i in 0..9 {
            let body = format!("uniquerankterm {}", "filler ".repeat(i + 1));
            store
                .index_document("coll", &format!("d{i}.md"), "T", &body)
                .unwrap();
        }
        store.flush().unwrap();
        (store, calls)
    }

    /// Run `query` with a stage observer and return what it reported.
    fn observed_outcome(
        store: &mut Store,
        query: &str,
        no_expand: bool,
        strict_timeout: bool,
    ) -> (QueryOutcome, Vec<QueryStage>) {
        let stages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = stages.clone();
        store.set_query_observer(Some(Box::new(move |s| sink.lock().unwrap().push(s))));
        let outcome = store
            .hybrid_query_outcome(
                query,
                None,
                10,
                None,
                false,
                no_expand,
                Deadline::none(),
                strict_timeout,
            )
            .unwrap();
        store.set_query_observer(None);
        let stages = stages.lock().unwrap().clone();
        (outcome, stages)
    }

    fn rerank_steps(done: &[usize], total: usize) -> Vec<QueryStage> {
        done.iter()
            .map(|&done| QueryStage::Reranking { done, total })
            .collect()
    }

    /// An expand-mode query reports keyword search, query embedding,
    /// expansion and then one reranking step per slice, in that order.
    #[test]
    fn query_observer_reports_stages_in_order() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, _) = slice_store(dir.path(), SliceFault::None);

        let (outcome, stages) = observed_outcome(&mut store, "uniquerankterm", false, false);

        assert_eq!(outcome.rerank, RerankStatus::Reranked);
        let mut expected = vec![
            QueryStage::Searching,
            QueryStage::Embedding,
            QueryStage::Expanding,
        ];
        expected.extend(rerank_steps(&[0, 4, 8], 9));
        assert_eq!(stages, expected);
    }

    /// Without expansion the stage is not reported at all.
    #[test]
    fn query_observer_skips_expansion_when_disabled() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, _) = slice_store(dir.path(), SliceFault::None);

        let (_, stages) = observed_outcome(&mut store, "uniquerankterm", true, false);

        assert!(!stages.contains(&QueryStage::Expanding), "{stages:?}");
        assert_eq!(&stages[..2], [QueryStage::Searching, QueryStage::Embedding]);
    }

    /// A query document reports each sub-query's kind as it runs.
    #[test]
    fn query_observer_reports_each_sub_query_of_a_query_document() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, _) = slice_store(dir.path(), SliceFault::None);

        let (_, stages) = observed_outcome(
            &mut store,
            "lex: uniquerankterm\nvec: uniquerankterm\nhyde: uniquerankterm filler",
            true,
            false,
        );

        assert_eq!(
            &stages[..3],
            [
                QueryStage::Searching,
                QueryStage::Embedding,
                QueryStage::Embedding
            ]
        );
        assert_eq!(&stages[3..], rerank_steps(&[0, 4, 8], 9));
    }

    /// Reporting stages changes how the reranker is called, not what it
    /// returns: same files, same order, same scores, bit for bit.
    #[test]
    fn query_observer_leaves_results_and_scores_unchanged() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, calls) = slice_store(dir.path(), SliceFault::None);

        let plain = run_outcome(&mut store, false);
        assert_eq!(*calls.lock().unwrap(), [9], "one call without an observer");
        calls.lock().unwrap().clear();
        let (observed, _) = observed_outcome(&mut store, "uniquerankterm", true, false);
        assert_eq!(*calls.lock().unwrap(), [4, 4, 1], "slices of four");

        assert_eq!(plain.rerank, RerankStatus::Reranked);
        assert_eq!(observed.rerank, RerankStatus::Reranked);
        assert_eq!(files_and_scores(&observed), files_and_scores(&plain));
        let bits = |o: &QueryOutcome| {
            o.results
                .iter()
                .map(|r| r.score.to_bits())
                .collect::<Vec<_>>()
        };
        assert_eq!(bits(&observed), bits(&plain));
    }

    /// A slice that fails fails the whole rerank, exactly as one failing call
    /// would: fusion scores, never a mix.
    #[test]
    fn a_failing_slice_leaves_the_results_in_fusion_order() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, _) = slice_store(dir.path(), SliceFault::FailOn(2));
        let fusion = run_outcome(&mut store, true);

        let (observed, _) = observed_outcome(&mut store, "uniquerankterm", true, false);

        assert_eq!(observed.rerank, RerankStatus::Failed);
        assert_eq!(files_and_scores(&observed), files_and_scores(&fusion));
    }

    /// A soft deadline that passes between slices ends the rerank as
    /// `DeadlineExpired`, with fusion scores.
    #[test]
    fn a_deadline_between_slices_leaves_the_results_in_fusion_order() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut store, _) = slice_store(dir.path(), SliceFault::ExpireOn(2));
        let fusion = run_outcome(&mut store, true);

        let (observed, _) = observed_outcome(&mut store, "uniquerankterm", true, false);

        assert_eq!(observed.rerank, RerankStatus::DeadlineExpired);
        assert_eq!(files_and_scores(&observed), files_and_scores(&fusion));
    }
}
