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
    /// Persisted `--hidden` setting from `collection add` — `update` re-reads
    /// this so re-indexing honors the same dot-directory policy the
    /// collection was created with.
    #[serde(default)]
    pub allow_hidden: bool,
    /// Honor the collection tree's `.gitignore` files when walking. Off by
    /// default so a collection never silently loses documents.
    #[serde(default)]
    pub respect_gitignore: bool,
    /// Files larger than this many bytes are not indexed. `None` means the
    /// built-in default cap.
    #[serde(default)]
    pub max_file_bytes: Option<u64>,
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

/// How the rerank stage of a hybrid query ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerankStatus {
    /// Candidates were reordered by the rerank model.
    Reranked,
    /// Reranking did not run: it was switched off, or there was nothing to rank.
    NotRun,
    /// The active backend cannot rerank; results are in fusion order.
    Unsupported,
    /// The rerank model failed or returned unusable scores; results are in fusion order.
    Failed,
}

impl RerankStatus {
    /// One plain-text line for a caller to show when reranking was expected
    /// but did not happen, `None` otherwise. It never carries paths or error
    /// detail: the error chain goes to the log.
    pub fn notice(self) -> Option<&'static str> {
        match self {
            Self::Unsupported => Some(
                "results were not reranked: the active inference backend does not support \
                 reranking, so they are in keyword and vector fusion order",
            ),
            Self::Failed => Some(
                "results were not reranked: the rerank model failed, so they are in keyword \
                 and vector fusion order",
            ),
            Self::Reranked | Self::NotRun => None,
        }
    }
}

/// Results of a hybrid query plus how its rerank stage ended.
#[derive(Debug, Clone)]
pub struct QueryOutcome {
    pub results: Vec<SearchResult>,
    pub rerank: RerankStatus,
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
    /// Collection-level context string (from `rqmd context add`), if set.
    pub context: Option<String>,
}
