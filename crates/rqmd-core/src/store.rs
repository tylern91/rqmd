//! Store — the main entry point for all rqmd-core operations.
//!
//! Orchestrates rusqlite (metadata), Tantivy (BM25), usearch (HNSW), and
//! the InferenceBackend (embed/rerank) to provide a hybrid search pipeline.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::PathBuf;

use rqmd_llm::InferenceBackend;
use sha2::{Digest, Sha256};

use crate::{
    chunking::chunk_document,
    db::{
        self, content_hash, doc_for_vid, docid_from_hash, get_content, get_context_for_collection,
        open_db, upsert_content, upsert_document, upsert_vector_meta,
    },
    fts::FtsIndex,
    hnsw::VectorIndex,
    rrf::{reciprocal_rank_fusion, rrf_weights},
    types::{QueryType, RankedListMeta, RankedResult, SearchResult},
};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Candidate pool size for reranking.
const RERANK_CANDIDATE_LIMIT: usize = 20;

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
        // A failed load (corrupt file) emits a warning and starts empty — callers
        // must run `rqmd embed` to rebuild before vector search returns results.
        let hnsw = if config.hnsw_path.exists() {
            match VectorIndex::load(&config.hnsw_path) {
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
        let embeddings = self.backend.embed_batch(&texts).context("embed batch")?;

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
    pub fn index_document_fts_only(
        &mut self,
        collection: &str,
        rel_path: &str,
        title: &str,
        body: &str,
    ) -> Result<()> {
        let now = rfc3339_now();
        let hash = content_hash(body);
        upsert_content(&self.db, &hash, body, &now).context("upsert content")?;
        let doc_id = upsert_document(&self.db, collection, rel_path, title, &hash, &now)
            .context("upsert document")?;
        let filepath = format!("{collection}/{rel_path}");
        self.fts
            .add_document(&filepath, title, body, doc_id)
            .context("add to tantivy")?;
        Ok(())
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
        let embeddings = self.backend.embed_batch(&texts).context("embed batch")?;
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

    /// Commit FTS writes and persist the HNSW index to disk.
    pub fn flush(&mut self) -> Result<()> {
        self.fts.commit().context("fts commit")?;
        self.hnsw.save(&self.hnsw_path).context("hnsw save")?;
        Ok(())
    }

    // ── Search ────────────────────────────────────────────────────────────────

    /// BM25 full-text search only (no vector, no rerank).
    pub fn search_fts(
        &self,
        query: &str,
        limit: usize,
        collection: Option<&str>,
    ) -> Result<Vec<SearchResult>> {
        let hits = self.fts.search_fts(query, limit, collection)?;
        self.hits_to_results(hits, limit)
    }

    /// Vector similarity search only (no BM25, no rerank).
    pub fn search_vec(
        &mut self,
        query: &str,
        limit: usize,
        collection: Option<&str>,
    ) -> Result<Vec<SearchResult>> {
        let embedding = self.backend.embed(query).context("embed query")?;
        let raw = self.hnsw.search(&embedding, limit * 4)?; // over-fetch, then filter

        let mut results = Vec::new();
        for (vid, sim) in raw {
            if let Some((doc, body)) = doc_for_vid(&self.db, vid)? {
                if let Some(cf) = collection {
                    if doc.collection != cf {
                        continue;
                    }
                }
                let filepath = format!("{}/{}", doc.collection, doc.path);
                // Pick the first chunk as the snippet — no re-chunking needed.
                let chunks = chunk_document(&body);
                let best = chunks
                    .into_iter()
                    .next()
                    .map(|c| c.text)
                    .unwrap_or_default();
                let docid = docid_from_hash(&doc.hash).to_string();
                let ctx = get_context_for_collection(&self.db, &doc.collection)
                    .ok()
                    .flatten();
                results.push(SearchResult {
                    file: format!("rqmd://{filepath}"),
                    title: doc.title.clone(),
                    body: body.clone(),
                    best_chunk: best,
                    best_chunk_pos: 0,
                    score: sim,
                    docid,
                    collection: doc.collection,
                    path: doc.path,
                    context: ctx,
                });
                if results.len() >= limit {
                    break;
                }
            }
        }
        Ok(results)
    }

    /// Full hybrid search: BM25 + vector → RRF → chunk selection → rerank.
    pub fn hybrid_query(
        &mut self,
        query: &str,
        limit: usize,
        collection: Option<&str>,
        skip_rerank: bool,
    ) -> Result<Vec<SearchResult>> {
        let mut ranked_lists: Vec<Vec<RankedResult>> = Vec::new();
        let mut list_meta: Vec<RankedListMeta> = Vec::new();

        // Step 1: BM25 probe on the raw query.
        let initial_fts = self.fts.search_fts(query, 20, collection)?;
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
        let query_embedding = self.backend.embed(query).context("embed query")?;
        let vec_hits = self.hnsw.search(&query_embedding, 20)?;
        let vec_results = self.vec_hits_to_ranked(vec_hits, collection)?;
        if !vec_results.is_empty() {
            ranked_lists.push(vec_results);
            list_meta.push(RankedListMeta {
                source: "vec",
                query_type: QueryType::Original,
            });
        }

        // Step 3: Query expansion (skipped on strong BM25 signal).
        // TODO: wire generate model for lex/vec/hyde expansion (future phase).
        let _ = strong_signal;

        // Step 4: RRF fusion.
        if ranked_lists.is_empty() {
            return Ok(vec![]);
        }
        let weights = rrf_weights(&list_meta);
        let fused = reciprocal_rank_fusion(&ranked_lists, &weights);
        let candidates = &fused[..RERANK_CANDIDATE_LIMIT.min(fused.len())];

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
        let query_terms: Vec<String> = query
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
        } else {
            self.backend.rerank(query, &chunk_refs).ok()
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
            let ctx = get_context_for_collection(&self.db, collection_name)
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
            let ctx = get_context_for_collection(&self.db, coll).ok().flatten();
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
        collection: Option<&str>,
    ) -> Result<Vec<RankedResult>> {
        let mut results = Vec::new();
        for (vid, sim) in hits {
            if let Some((doc, _body)) = doc_for_vid(&self.db, vid)? {
                if let Some(cf) = collection {
                    if doc.collection != cf {
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
}

// ── Module-level helpers ──────────────────────────────────────────────────────

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
fn rfc3339_now() -> String {
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
fn embed_fingerprint(model: &str) -> String {
    let sig = format!("model:{model}\nchunk_tokens:900\nchunk_overlap_tokens:135");
    let hash = Sha256::digest(sig.as_bytes());
    hex::encode(&hash[..3]) // 6 hex chars
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
}
