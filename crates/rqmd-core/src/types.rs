use serde::{Deserialize, Serialize};
use std::time::Duration;

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
    /// The deadline passed before or during reranking; results are in fusion order.
    DeadlineExpired,
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
            Self::DeadlineExpired => Some(
                "results were not reranked: the time limit was reached, so they are in keyword \
                 and vector fusion order",
            ),
            Self::Reranked | Self::NotRun => None,
        }
    }
}

/// A stage of a hybrid query, reported to the observer set with
/// `Store::set_query_observer` as the query reaches it. A stage that needs a
/// model includes loading it when the model is not resident yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryStage {
    /// Keyword search: the first probe, or a `lex:` sub-query.
    Searching,
    /// Embedding query text for vector search.
    Embedding,
    /// Generating query expansions and searching with them.
    Expanding,
    /// Reranking: `done` of `total` candidates are scored.
    Reranking { done: usize, total: usize },
}

/// Most sections of one file a search returns. Every extra section is one more
/// reranker input, so the cap bounds what a single request can cost.
pub const MAX_PER_FILE: usize = 5;

/// Bring a requested sections-per-file count into `1..=MAX_PER_FILE`.
pub fn clamp_per_file(per_file: usize) -> usize {
    per_file.clamp(1, MAX_PER_FILE)
}

/// What a file keeps of its score however old it is, when none is chosen.
pub const DEFAULT_RECENCY_FLOOR: f32 = 0.5;

/// Longest recency half-life accepted: ten years.
pub const MAX_HALF_LIFE: Duration = Duration::from_secs(3650 * 86_400);

/// Weights a result's score by the age of its file: the share of the score
/// above `floor` halves every `half_life`, so old files rank lower but are
/// never dropped and never lose more than `1 - floor` of their score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Recency {
    pub half_life: Duration,
    /// Share of its score a file keeps however old it is, within `0.0..=1.0`.
    pub floor: f32,
}

impl Recency {
    pub fn new(half_life: Duration) -> Self {
        Self {
            half_life,
            floor: DEFAULT_RECENCY_FLOOR,
        }
    }

    /// Keep `floor` of the score instead of the default. Clamped into
    /// `0.0..=1.0`; a value that is not a number leaves the default.
    pub fn with_floor(mut self, floor: f32) -> Self {
        if !floor.is_nan() {
            self.floor = floor.clamp(0.0, 1.0);
        }
        self
    }

    /// The factor a file last modified `age` ago multiplies its score by:
    /// 1.0 for a file just modified, falling to `floor`.
    pub fn factor(&self, age: Duration) -> f32 {
        let half_lives = age.as_secs_f64() / self.half_life.as_secs_f64().max(1.0);
        let decay = (-half_lives).exp2();
        (f64::from(self.floor) + f64::from(1.0 - self.floor) * decay) as f32
    }

    /// One plain line saying why results are ordered as they are, for stderr
    /// and MCP notes.
    pub fn notice(&self) -> String {
        let secs = self.half_life.as_secs();
        let half_life = if secs.is_multiple_of(86_400) {
            format!("{}d", secs / 86_400)
        } else {
            format!("{}h", secs / 3_600)
        };
        format!(
            "results are weighted by file age: half-life {half_life}, no file below {}% of its score",
            (self.floor * 100.0).round()
        )
    }
}

/// Parse a recency half-life: a whole number of days, weeks or hours (`30d`,
/// `2w`, `12h`), above zero and at most [`MAX_HALF_LIFE`].
pub fn parse_half_life(spec: &str) -> Result<Duration, String> {
    let bad = |why: &str| {
        format!(
            "invalid half-life {spec:?}: {why}; use a whole number with a unit, such as 30d, 2w or 12h"
        )
    };
    let spec = spec.trim();
    let unit_secs: u64 = match spec.chars().last() {
        Some('d') => 86_400,
        Some('w') => 7 * 86_400,
        Some('h') => 3_600,
        _ => return Err(bad("no unit")),
    };
    let digits = &spec[..spec.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad("not a whole number"));
    }
    let count: u64 = digits.parse().map_err(|_| bad("too long"))?;
    let secs = count
        .checked_mul(unit_secs)
        .ok_or_else(|| bad("too long"))?;
    if secs == 0 {
        return Err(bad("zero"));
    }
    if secs > MAX_HALF_LIFE.as_secs() {
        return Err(bad("longer than 3650d"));
    }
    Ok(Duration::from_secs(secs))
}

/// Options that shape which results a search returns, beyond its query,
/// limit and collection scope.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SearchOptions {
    /// Sections returned per file, clamped to `1..=MAX_PER_FILE` when used. The
    /// default of 1 is one result per file.
    pub per_file: usize,
    /// Weight scores by file age. `None`, the default, leaves scores and order
    /// untouched.
    pub recency: Option<Recency>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            per_file: 1,
            recency: None,
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

/// For each result, `Some((n, of))` when its file occurs `of` > 1 times in
/// `results` and this is the `n`-th of them (1-based), else `None`. Output uses
/// it to tell the sections of one file apart in words.
pub fn section_numbers(results: &[SearchResult]) -> Vec<Option<(usize, usize)>> {
    let mut totals: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for r in results {
        *totals.entry(r.file.as_str()).or_default() += 1;
    }
    let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    results
        .iter()
        .map(|r| {
            let n = seen.entry(r.file.as_str()).or_default();
            *n += 1;
            let of = totals[r.file.as_str()];
            (of > 1).then_some((*n, of))
        })
        .collect()
}
