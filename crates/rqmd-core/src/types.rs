use serde::{Deserialize, Serialize};

// ── Document / collection types ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    pub name: String,
    pub path: String,
    pub pattern: String,
    pub ignore: Vec<String>,
    pub include_by_default: bool,
    pub update_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: i64,
    pub collection: String,
    pub path: String,
    pub title: String,
    pub hash: String,
    pub active: bool,
}

// ── Chunking types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Chunk {
    pub text: String,
    /// Character offset of chunk start in the original document.
    pub pos: usize,
}

// ── Search types ──────────────────────────────────────────────────────────────

/// A result in one of the N ranked lists fed into RRF fusion.
#[derive(Debug, Clone)]
pub struct RankedResult {
    /// `collection/path` — the join key across all results.
    pub filepath: String,
    pub title: String,
    /// Raw score from the backend (BM25 or cosine similarity).
    pub backend_score: f32,
}

/// Source of a ranked list, used to compute RRF weights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryType {
    /// Original user query (FTS or vector). Gets weight 2.0 in RRF.
    Original,
    /// Lex expansion — FTS only. Gets weight 1.0.
    Lex,
    /// Vec expansion — vector only. Gets weight 1.0.
    Vec,
    /// HyDE expansion — vector only. Gets weight 1.0.
    Hyde,
}

/// Metadata for one ranked list, used by `rrf_weight()`.
#[derive(Debug, Clone)]
pub struct RankedListMeta {
    pub source: &'static str, // "fts" or "vec"
    pub query_type: QueryType,
}

/// Final search result returned to callers.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    /// `rqmd://collection/path`
    pub file: String,
    pub title: String,
    /// Full document body (from rusqlite content table).
    pub body: String,
    /// Best matching chunk text.
    pub best_chunk: String,
    pub best_chunk_pos: usize,
    /// Blended final score (RRF for vsearch/search, rerank-blended for query).
    pub score: f32,
    /// First 6 hex chars of SHA-256(content) — matches qmd's docid format.
    pub docid: String,
    pub collection: String,
    pub path: String,
    /// Collection-level context string (from `qmd context add`), if set.
    pub context: Option<String>,
}
