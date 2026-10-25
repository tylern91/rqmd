use anyhow::Context as _;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, TryLockError,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use rmcp::{
    ServerHandler,
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerConfig},
    schemars, serde, tool, tool_handler, tool_router,
};

use rqmd_core::{
    Deadline, Document, Store, StoreConfig, db, resolve, snap_char_boundary_backward,
    store as core_store,
};
use rqmd_llm::{BackendKind, create_backend, no_backend};

/// Hard cap on documents returned by a single `multi_get` call. Without this,
/// an unauthenticated caller could pass a bare `*` glob with no collection
/// filter and pull the entire corpus in one request — this bounds that to a
/// generous but finite batch size.
const MULTI_GET_MAX_DOCS: usize = 200;

/// Hard cap on `search`/`query`'s `limit`. An MCP client fully controls this
/// value; left unclamped, a huge `limit` on a collection-scoped search hits
/// `rqmd_core::fts`'s overscan calculation, or on any search allocates a
/// tantivy `TopDocs` collector sized to it — either way this is worth
/// bounding at the boundary rather than trusting every downstream caller to
/// do it (github.com/tylern91/rqmd#86, AC-2).
const MAX_SEARCH_LIMIT: usize = 1000;

/// Line cap applied to `get`/`multi_get` when the caller gives no `max_lines`.
const DEFAULT_MAX_LINES: usize = 2000;

/// Byte cap on one document body in a response, applied even when the caller
/// passes `max_lines` — a single huge line defeats any line cap.
const MAX_DOC_BYTES: usize = 256 * 1024;

/// Byte budget across all documents in one `multi_get` response.
const MULTI_GET_MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;

/// Clamp a client-supplied `limit` to `1..=MAX_SEARCH_LIMIT`, defaulting to
/// `default` when omitted.
fn clamp_limit(limit: Option<usize>, default: usize) -> usize {
    limit.unwrap_or(default).clamp(1, MAX_SEARCH_LIMIT)
}

/// Clamp a client-supplied `offset` to `0..=MAX_SEARCH_LIMIT`, so a huge
/// value cannot size a retrieval pool past the same cap `limit` obeys.
fn clamp_offset(offset: Option<usize>) -> usize {
    offset.unwrap_or(0).min(MAX_SEARCH_LIMIT)
}

/// Number of ranked results to fetch so that the page `offset..offset+limit`
/// can be cut out and one extra result proves whether a further page exists.
fn fetch_len(offset: usize, limit: usize) -> usize {
    (offset + limit + 1).min(MAX_SEARCH_LIMIT)
}

/// Cut one page out of a ranked list. Returns the page and whether more
/// results follow it.
fn page_of<T>(mut ranked: Vec<T>, offset: usize, limit: usize) -> (Vec<T>, bool) {
    let has_more = ranked.len() > offset + limit;
    let page = if offset >= ranked.len() {
        Vec::new()
    } else {
        ranked.drain(offset..).take(limit).collect()
    };
    (page, has_more)
}

/// Default number of read-only FTS store handles when `RQMD_MCP_FTS_READERS`
/// is unset: enough to overlap a few concurrent clients without opening a
/// handle per core on large machines.
const DEFAULT_FTS_READERS: usize = 4;
const MAX_FTS_READERS: usize = 16;

fn fts_readers_from_env() -> usize {
    std::env::var("RQMD_MCP_FTS_READERS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map_or(DEFAULT_FTS_READERS, |n| n.get().min(DEFAULT_FTS_READERS))
        })
        .clamp(1, MAX_FTS_READERS)
}

/// Default ceiling on one `query` call, from the moment it holds the model
/// store. Cold model loading before that point is not charged.
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(120);

/// A positive whole number of seconds; anything else falls back to the default.
fn parse_tool_timeout(value: Option<&str>) -> Duration {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map_or(DEFAULT_TOOL_TIMEOUT, Duration::from_secs)
}

fn tool_timeout_from_env() -> Duration {
    parse_tool_timeout(std::env::var("RQMD_MCP_TOOL_TIMEOUT_SECS").ok().as_deref())
}

// ── FTS reader pool ───────────────────────────────────────────────────────────

/// Several read-only FTS stores. `Store` owns a `rusqlite::Connection`, which
/// is `Send` but not `Sync`, and `reload_if_stale` needs `&mut self`, so one
/// shared store cannot serve concurrent readers — separate handles can.
struct FtsPool {
    handles: Vec<Mutex<Store>>,
    next: AtomicUsize,
}

impl FtsPool {
    fn open(index_dir: &Path, size: usize) -> anyhow::Result<Self> {
        let handles = (0..size.max(1))
            .map(|_| {
                Ok(Mutex::new(Store::open(
                    make_config(index_dir),
                    no_backend(),
                )?))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            handles,
            next: AtomicUsize::new(0),
        })
    }

    /// An idle handle if there is one, otherwise wait on the next in
    /// round-robin order. A handle poisoned by a panicking request is reused:
    /// these stores are read-only, so the panic cannot have left shared state
    /// half-written.
    fn checkout(&self) -> anyhow::Result<MutexGuard<'_, Store>> {
        for handle in &self.handles {
            match handle.try_lock() {
                Ok(guard) => return Self::fresh(guard),
                Err(TryLockError::Poisoned(p)) => return Self::fresh(p.into_inner()),
                Err(TryLockError::WouldBlock) => {}
            }
        }
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.handles.len();
        let guard = self.handles[i]
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Self::fresh(guard)
    }

    /// Same staleness handling as [`RqmdServer::ml`] — see its doc comment.
    fn fresh(mut guard: MutexGuard<'_, Store>) -> anyhow::Result<MutexGuard<'_, Store>> {
        guard.reload_if_stale().context("reload fts store")?;
        Ok(guard)
    }
}

// ── Server struct ─────────────────────────────────────────────────────────────

/// Shared MCP server; Clone is cheap (all fields are Arc).
#[derive(Clone)]
pub struct RqmdServer {
    index_dir: Arc<PathBuf>,
    /// Read-only FTS stores for search/get/status (no ML model loaded).
    fts_pool: Arc<FtsPool>,
    /// ML store for hybrid query (lazily initialised on first `query` call).
    ml_store: Arc<once_cell::sync::OnceCell<Arc<std::sync::Mutex<Store>>>>,
    /// Ceiling on one `query` call once it holds the model store.
    tool_timeout: Duration,
}

impl RqmdServer {
    pub fn new(index_dir: PathBuf) -> anyhow::Result<Self> {
        Self::with_fts_readers(index_dir, fts_readers_from_env())
    }

    /// Like [`Self::new`] with an explicit number of concurrent FTS readers.
    pub fn with_fts_readers(index_dir: PathBuf, readers: usize) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&index_dir)?;
        let fts_pool = FtsPool::open(&index_dir, readers)?;
        Ok(Self {
            index_dir: Arc::new(index_dir),
            fts_pool: Arc::new(fts_pool),
            ml_store: Arc::new(once_cell::sync::OnceCell::new()),
            tool_timeout: tool_timeout_from_env(),
        })
    }

    /// Override the `query` ceiling (default `RQMD_MCP_TOOL_TIMEOUT_SECS`, else 120 s).
    pub fn with_tool_timeout(mut self, timeout: Duration) -> Self {
        self.tool_timeout = timeout;
        self
    }

    /// The index directory this server was opened against — used by the HTTP
    /// `/health` endpoint so a caller can confirm it reached the daemon it expects.
    pub fn index_dir(&self) -> &Path {
        &self.index_dir
    }

    /// Return the ML store, initialising it (loading models) on first call.
    ///
    /// Reloads the store if the on-disk index has changed since it was last
    /// checked — see `Store::reload_if_stale` for why this is necessary: a
    /// long-lived MCP daemon never indexes through its own stores, so
    /// without this it would serve the snapshot it saw at startup forever,
    /// even after a separate `rqmd index`/`update`/`embed` run.
    fn ml(&self) -> anyhow::Result<std::sync::MutexGuard<'_, Store>> {
        let store = self.ml_store.get_or_try_init(|| {
            let kind = BackendKind::from_env();
            eprintln!(
                "[rqmd-mcp] Loading inference backend (kind={kind:?}, models download on first run)..."
            );
            let backend = create_backend(&kind).context("failed to init inference backend")?;
            eprintln!("[rqmd-mcp] Backend ready.");
            let config = make_config(&self.index_dir);
            let s = Store::open(config, backend)?;
            Ok::<_, anyhow::Error>(Arc::new(std::sync::Mutex::new(s)))
        })?;
        let mut guard = store
            .lock()
            .map_err(|e| anyhow::anyhow!("ml store lock poisoned: {e}"))?;
        guard.reload_if_stale().context("reload ml store")?;
        Ok(guard)
    }

    fn fts(&self) -> anyhow::Result<MutexGuard<'_, Store>> {
        self.fts_pool.checkout()
    }

    /// Release any GGUF model idle for at least `ttl`. Returns how many were
    /// released, or `0` if the ML store was never initialised or is currently
    /// busy. Uses `try_lock` (never `lock`) so a periodic sweep can never block
    /// an in-flight query.
    pub fn release_idle_models(&self, ttl: Duration) -> usize {
        let Some(store) = self.ml_store.get() else {
            return 0;
        };
        let Ok(mut guard) = store.try_lock() else {
            return 0;
        };
        guard.release_idle_models(ttl)
    }
}

fn make_config(index_dir: &Path) -> StoreConfig {
    StoreConfig {
        db_path: index_dir.join("index.sqlite"),
        tantivy_dir: index_dir.join("tantivy"),
        hnsw_path: index_dir.join("hnsw.usearch"),
        // The MCP server only ever queries (search/get/status) or runs hybrid
        // query — it never indexes, so the HNSW index can be mmap'd read-only.
        read_only: true,
    }
}

// ── Tool parameter types ──────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct QueryInput {
    /// Search query. Supports plain text (auto-expanded via generation model),
    /// `expand: text`, or a multi-line typed document with `lex:`, `vec:`,
    /// `hyde:`, and optional `intent:` lines per the rqmd query syntax.
    pub query: String,
    /// Optional context or intent to steer query expansion, reranking, and
    /// snippet selection. Equivalent to an `intent:` line inside the query.
    pub intent: Option<String>,
    /// Filter to one or more collections by name. Omit to search all collections.
    pub collections: Option<Vec<String>>,
    /// Maximum results to return (default: 10).
    pub limit: Option<usize>,
    /// Number of ranked results to skip, to fetch the next page (default: 0).
    pub offset: Option<usize>,
    /// Set to false to skip LLM reranking (faster, lower quality). Default: true.
    pub rerank: Option<bool>,
    /// Set to false to skip the LLM query-expansion / HyDE round-trip (faster;
    /// pure hybrid retrieval). Default: true.
    pub expand: Option<bool>,
    /// Set to true to fail with a timeout error when the tool timeout passes before
    /// reranking finishes, instead of returning the results in fusion order with a
    /// note. Default: false.
    pub strict_timeout: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchInput {
    /// BM25 keyword query. Supports "quoted phrases" and -negation.
    pub query: String,
    /// Filter to one or more collections by name. Omit to search all collections.
    pub collections: Option<Vec<String>>,
    /// Maximum results to return (default: 10).
    pub limit: Option<usize>,
    /// Number of ranked results to skip, to fetch the next page (default: 0).
    pub offset: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetInput {
    /// File path (e.g. "collection/path/to/file.md") or docid (e.g. "#abc123").
    /// Supports a line-range suffix: "file.md:100" (start at line 100) or
    /// "file.md:100:40" (40 lines from line 100).
    pub file: String,
    /// Start from this line number (1-indexed). Overrides suffix.
    pub from_line: Option<usize>,
    /// Maximum lines to return.
    pub max_lines: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct MultiGetInput {
    /// Glob pattern (e.g. "collection/2025-05*.md") or comma-separated list of
    /// paths/docids to retrieve.
    pub pattern: String,
    /// Filter to one or more collections by name.
    pub collections: Option<Vec<String>>,
    /// Maximum lines per document.
    pub max_lines: Option<usize>,
}

// ── Tool implementations ──────────────────────────────────────────────────────

#[tool_router]
impl RqmdServer {
    /// Hybrid semantic search: BM25 + vector retrieval fused with RRF and
    /// reranked by a cross-encoder. Best for most queries.
    #[tool(
        description = "Hybrid search (BM25 + vector + rerank). Best for most queries. Provide a natural-language question or keyword phrase. Set expand:false to skip LLM query-expansion for lower latency.",
        annotations(read_only_hint = true)
    )]
    async fn query(&self, Parameters(p): Parameters<QueryInput>) -> Result<String, String> {
        let this = self.clone();
        run_blocking(move || this.query_blocking(p)).await
    }

    /// BM25 full-text keyword search. No LLM required — instant results.
    #[tool(
        description = "BM25 keyword search. Fast, no model required. Supports \"quoted phrases\" and -negation. Use for known terms or exact phrases.",
        annotations(read_only_hint = true)
    )]
    async fn search(&self, Parameters(p): Parameters<SearchInput>) -> Result<String, String> {
        let this = self.clone();
        run_blocking(move || this.search_blocking(p)).await
    }

    /// Retrieve full document content by file path or docid.
    #[tool(
        description = "Retrieve a document by file path or docid (#abc123) from search results. Supports line range: 'file.md:100:40' reads 40 lines from line 100.",
        annotations(read_only_hint = true)
    )]
    async fn get(&self, Parameters(p): Parameters<GetInput>) -> Result<String, String> {
        let this = self.clone();
        run_blocking(move || this.get_blocking(p)).await
    }

    /// Retrieve multiple documents by glob pattern or comma-separated list.
    #[tool(
        description = "Retrieve multiple documents matching a glob pattern (e.g. 'journals/2025-05*.md') or a comma-separated list of paths/docids.",
        annotations(read_only_hint = true)
    )]
    async fn multi_get(&self, Parameters(p): Parameters<MultiGetInput>) -> Result<String, String> {
        let this = self.clone();
        run_blocking(move || this.multi_get_blocking(p)).await
    }

    /// Show index status: collections, document counts, and storage sizes.
    #[tool(
        description = "Show the RQMD index status: collections, document counts, and index health.",
        annotations(read_only_hint = true)
    )]
    async fn status(&self) -> Result<String, String> {
        let this = self.clone();
        run_blocking(move || this.status_blocking()).await
    }
}

/// Tool bodies run on the blocking pool: they take std mutexes and call into
/// SQLite, Tantivy and llama.cpp, any of which would otherwise pin a tokio
/// worker — starving other requests and `/health` while a slow call waits.
impl RqmdServer {
    fn query_blocking(&self, p: QueryInput) -> Result<String, String> {
        let no_rerank = !p.rerank.unwrap_or(true);
        let no_expand = !p.expand.unwrap_or(true);
        let limit = clamp_limit(p.limit, 10);
        let offset = clamp_offset(p.offset);
        let cols = p.collections.as_deref();
        let intent = p.intent.as_deref();
        let mut store = self
            .ml()
            .map_err(|e| format!("Error loading inference backend: {e:#}"))?;
        let outcome = store
            .hybrid_query_outcome(
                &p.query,
                intent,
                fetch_len(offset, limit),
                cols,
                no_rerank,
                no_expand,
                Deadline::after(self.tool_timeout),
                p.strict_timeout.unwrap_or(false),
            )
            .map_err(|e| format!("Error running query: {e:#}"))?;
        let (page, has_more) = page_of(outcome.results, offset, limit);
        Ok(with_rerank_note(
            format_results(&page, &p.query, offset, has_more),
            outcome.rerank,
        ))
    }

    fn search_blocking(&self, p: SearchInput) -> Result<String, String> {
        let limit = clamp_limit(p.limit, 10);
        let offset = clamp_offset(p.offset);
        let cols = p.collections.as_deref();
        let store = self
            .fts()
            .map_err(|e| format!("Error opening store: {e:#}"))?;
        let results = store
            .search_fts_multi(&p.query, fetch_len(offset, limit), cols)
            .map_err(|e| format!("Error running search: {e:#}"))?;
        let (page, has_more) = page_of(results, offset, limit);
        Ok(format_results(&page, &p.query, offset, has_more))
    }

    fn get_blocking(&self, p: GetInput) -> Result<String, String> {
        let (lookup, from_line, max_lines) = parse_file_spec(&p.file, p.from_line, p.max_lines);
        let store = self
            .fts()
            .map_err(|e| format!("Error opening store: {e:#}"))?;
        get_document(&store, &lookup, from_line, max_lines)
    }

    fn multi_get_blocking(&self, p: MultiGetInput) -> Result<String, String> {
        let store = self
            .fts()
            .map_err(|e| format!("Error opening store: {e:#}"))?;
        multi_get_documents(&store, &p.pattern, p.collections.as_deref(), p.max_lines)
    }

    fn status_blocking(&self) -> Result<String, String> {
        let store = self
            .fts()
            .map_err(|e| format!("Error opening store: {e:#}"))?;
        Ok(build_status(&store))
    }
}

async fn run_blocking<F>(f: F) -> Result<String, String>
where
    F: FnOnce() -> Result<String, String> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("Tool task failed: {e}"))?
}

#[tool_handler]
impl ServerHandler for RqmdServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rqmd", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "RQMD knowledge base search. \
                Use `query` for semantic/hybrid search (recommended), \
                `search` for exact keyword search, \
                `get` to retrieve a document by path or docid, \
                `multi_get` to batch-retrieve documents, \
                `status` to see index health.",
            )
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn format_results(
    results: &[rqmd_core::SearchResult],
    query: &str,
    offset: usize,
    has_more: bool,
) -> String {
    if results.is_empty() {
        return if offset == 0 {
            format!("No results found for: {query}")
        } else {
            format!("No results found for: {query} (offset {offset} is past the last result)")
        };
    }
    let mut out = if offset == 0 {
        format!("Found {} result(s) for \"{query}\":\n\n", results.len())
    } else {
        format!(
            "Found {} result(s) for \"{query}\" (offset {offset}):\n\n",
            results.len()
        )
    };
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!(
            "[{}] {} #{}\n  rqmd://{}/{} · score {:.3}\n",
            offset + i + 1,
            r.title,
            r.docid,
            r.collection,
            r.path,
            r.score
        ));
        let snippet = r.best_chunk.trim();
        if !snippet.is_empty() {
            for line in snippet.lines().take(4) {
                out.push_str(&format!("  {line}\n"));
            }
        }
        out.push('\n');
    }
    if has_more {
        out.push_str(&format!(
            "More results are available: pass offset={}.\n",
            offset + results.len()
        ));
    }
    out
}

/// Append one note line when results are in fusion order because reranking
/// failed or was unavailable. The note never carries paths or error detail.
fn with_rerank_note(text: String, status: rqmd_core::RerankStatus) -> String {
    match status.notice() {
        Some(notice) => format!("{}\nNote: {notice}\n", text.trim_end()),
        None => text,
    }
}

/// Parse "file.md:100:40" → (path, Some(100), Some(40))
fn parse_file_spec(
    s: &str,
    from_line: Option<usize>,
    max_lines: Option<usize>,
) -> (String, Option<usize>, Option<usize>) {
    let mut lookup = s.to_string();
    let mut fl = from_line;
    let mut ml = max_lines;

    if let Some(caps) = s
        .rsplit_once(':')
        .and_then(|(rest, last)| last.parse::<usize>().ok().map(|n| (rest.to_string(), n)))
    {
        let (rest, n2) = caps;
        if let Some((pre, n1_str)) = rest.rsplit_once(':') {
            if let Ok(n1) = n1_str.parse::<usize>() {
                if fl.is_none() {
                    fl = Some(n1);
                }
                if ml.is_none() {
                    ml = Some(n2);
                }
                lookup = pre.to_string();
            } else {
                if fl.is_none() {
                    fl = Some(n2);
                }
                lookup = rest;
            }
        } else {
            if fl.is_none() {
                fl = Some(n2);
            }
            lookup = rest;
        }
    }

    (lookup, fl, ml)
}

fn get_document(
    store: &Store,
    lookup: &str,
    from_line: Option<usize>,
    max_lines: Option<usize>,
) -> Result<String, String> {
    let result = if lookup.starts_with('#') {
        let hex = lookup.trim_start_matches('#');
        db::get_document_by_docid_prefix(&store.db, hex)
    } else {
        // Try "collection/path" split
        match lookup.split_once('/') {
            Some((col, path)) => db::get_active_document_by_filepath(&store.db, col, path),
            None => return Err(format!("Cannot parse path: {lookup}")),
        }
    };

    let doc = match result {
        Ok(Some(d)) => d,
        Ok(None) => return Err(format!("Document not found: {lookup}")),
        Err(e) => return Err(format!("DB error: {e:#}")),
    };

    let body = db::get_document_raw(&store.db, doc.id)
        .unwrap_or_default()
        .unwrap_or_default();

    let window = cap_body(&body, from_line, max_lines, MAX_DOC_BYTES);
    let mut text: String = window
        .lines
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{:>4}: {l}\n", window.start + i + 1))
        .collect();
    text.push_str(&window.truncation_note());

    Ok(format!(
        "# {}\n── rqmd://{}/{} ──\n\n{text}",
        doc.title, doc.collection, doc.path
    ))
}

/// The slice of a document body a response will carry.
struct BodyWindow<'a> {
    lines: Vec<&'a str>,
    /// Zero-based index of the first returned line.
    start: usize,
    total_lines: usize,
    /// Stopped early because of a server-side cap, not the caller's `max_lines`.
    capped: bool,
}

impl BodyWindow<'_> {
    fn bytes(&self) -> usize {
        self.lines.iter().map(|l| l.len() + 1).sum()
    }

    fn truncation_note(&self) -> String {
        if !self.capped {
            return String::new();
        }
        let first = self.start + 1;
        let last = self.start + self.lines.len();
        format!(
            "[truncated: lines {first}–{last} of {}; pass from_line/max_lines to read more]\n",
            self.total_lines
        )
    }
}

/// Select the lines of `body` to return: from `from_line`, at most
/// `max_lines` (or `DEFAULT_MAX_LINES` when unset), and never more than
/// `byte_cap` bytes. A line that alone exceeds the remaining bytes is cut on a
/// char boundary.
fn cap_body(
    body: &str,
    from_line: Option<usize>,
    max_lines: Option<usize>,
    byte_cap: usize,
) -> BodyWindow<'_> {
    let start = from_line.map_or(0, |n| n.saturating_sub(1));
    let line_limit = max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let total_lines = body.lines().count();

    let mut lines = Vec::new();
    let mut used = 0usize;
    let mut capped = false;
    for line in body.lines().skip(start) {
        if lines.len() >= line_limit {
            capped = max_lines.is_none();
            break;
        }
        let room = byte_cap.saturating_sub(used);
        if line.len() < room {
            used += line.len() + 1;
            lines.push(line);
            continue;
        }
        capped = true;
        let cut = snap_char_boundary_backward(line, room.saturating_sub(1));
        if cut > 0 {
            lines.push(&line[..cut]);
        }
        break;
    }
    BodyWindow {
        lines,
        start,
        total_lines,
        capped,
    }
}

/// Truncate `docs` to at most `max` entries, reporting the original count and
/// whether truncation happened — split out from `multi_get_documents` so the
/// capping logic is testable without a real `Store`.
fn cap_multi_get_docs(mut docs: Vec<Document>, max: usize) -> (Vec<Document>, usize, bool) {
    let total = docs.len();
    let truncated = total > max;
    docs.truncate(max);
    (docs, total, truncated)
}

fn multi_get_documents(
    store: &Store,
    pattern: &str,
    collections: Option<&[String]>,
    max_lines: Option<usize>,
) -> Result<String, String> {
    let docs = resolve::resolve_multi_get(&store.db, collections, pattern)
        .map_err(|e| format!("DB error: {e:#}"))?;
    let (docs, total, truncated) = cap_multi_get_docs(docs, MULTI_GET_MAX_DOCS);

    let mut out = String::new();
    let mut count = 0usize;
    let mut budget = MULTI_GET_MAX_TOTAL_BYTES;

    for doc in &docs {
        if budget == 0 {
            break;
        }
        let filepath = format!("{}/{}", doc.collection, doc.path);
        let body = db::get_document_raw(&store.db, doc.id)
            .unwrap_or_default()
            .unwrap_or_default();
        let window = cap_body(&body, None, max_lines, MAX_DOC_BYTES.min(budget));
        budget = budget.saturating_sub(window.bytes());
        let mut text = window.lines.join("\n");
        if window.capped {
            text.push('\n');
            text.push_str(window.truncation_note().trim_end());
        }

        if count > 0 {
            out.push_str("\n────────────────────────\n\n");
        }
        out.push_str(&format!(
            "# {}\n── rqmd://{filepath} ──\n\n{text}\n",
            doc.title
        ));
        count += 1;
    }

    if count == 0 {
        return Ok(format!("No documents matched: {pattern}"));
    }
    if truncated {
        out.push_str(&format!(
            "\n[Showing {count} of {total} matched documents — multi_get is capped at \
             {MULTI_GET_MAX_DOCS} per call.]\n"
        ));
    }
    if count < docs.len() {
        out.push_str(&format!(
            "\n[Output budget of {MULTI_GET_MAX_TOTAL_BYTES} bytes reached after {count} \
             documents; {} more matched and were omitted — narrow the pattern or use get.]\n",
            docs.len() - count
        ));
    }
    Ok(out)
}

fn build_status(store: &Store) -> String {
    let total_docs: i64 = store
        .db
        .query_row("SELECT COUNT(*) FROM documents WHERE active=1", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    let total_vecs: i64 = store
        .db
        .query_row("SELECT COUNT(*) FROM content_vectors", [], |r| r.get(0))
        .unwrap_or(0);

    // Counted for the backend this server is configured with, so a client that
    // reads it knows the number can differ from a CLI run under another backend.
    let pending: i64 = db::count_docs_needing_embed(
        &store.db,
        &core_store::expected_fingerprint(),
        &core_store::expected_ast_fingerprints(),
    )
    .unwrap_or(0);
    let backend = format!("{:?}", BackendKind::from_env()).to_ascii_lowercase();

    let mut out = format!(
        "RQMD Index Status\n  Docs:     {total_docs}\n  Vectors:  {total_vecs}\n  \
         Pending embed: {pending} (backend: {backend})\n\n"
    );

    let cols = db::list_collections(&store.db).unwrap_or_default();
    if cols.is_empty() {
        out.push_str("  No collections.\n");
    } else {
        out.push_str(&format!("  {:<28}  {:>6}\n", "COLLECTION", "DOCS"));
        out.push_str(&format!("  {}\n", "─".repeat(36)));
        for col in &cols {
            let count = db::collection_doc_stats(&store.db, &col.name)
                .map(|(n, _)| n)
                .unwrap_or(0);
            out.push_str(&format!("  {:<28}  {:>6}\n", col.name, count));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: i64) -> Document {
        Document {
            id,
            collection: "col".to_string(),
            path: format!("doc{id}.md"),
            title: format!("Doc {id}"),
            hash: format!("hash{id}"),
            active: true,
        }
    }

    /// AC-2 (github.com/tylern91/rqmd#86): a client-supplied `limit` far above
    /// the cap must be clamped, not passed through — this is what stops a
    /// large `limit` from ever reaching `rqmd_core::fts`'s overscan math.
    #[test]
    fn clamp_limit_caps_a_large_client_supplied_value() {
        assert_eq!(clamp_limit(Some(5001), 10), MAX_SEARCH_LIMIT);
        assert_eq!(clamp_limit(Some(usize::MAX), 10), MAX_SEARCH_LIMIT);
    }

    #[test]
    fn ac_1_offset_pages_are_disjoint_slices_of_one_ranking() {
        let ranked: Vec<u32> = (1..=25).collect();
        let (first, more1) = page_of(ranked.clone(), 0, 10);
        let (second, more2) = page_of(ranked.clone(), 10, 10);
        let (third, more3) = page_of(ranked, 20, 10);
        assert_eq!(first, (1..=10).collect::<Vec<_>>());
        assert_eq!(second, (11..=20).collect::<Vec<_>>());
        assert_eq!(third, (21..=25).collect::<Vec<_>>());
        assert_eq!((more1, more2, more3), (true, true, false));
    }

    #[test]
    fn ac_2_omitted_offset_is_zero_and_changes_no_results() {
        assert_eq!(clamp_offset(None), 0);
        let ranked: Vec<u32> = (1..=5).collect();
        let (page, more) = page_of(ranked.clone(), clamp_offset(None), 10);
        assert_eq!(page, ranked);
        assert!(!more);
        assert_eq!(fetch_len(0, 10), 11);
    }

    #[test]
    fn ac_3_offset_past_the_end_is_an_empty_page_and_huge_values_are_clamped() {
        let (page, more) = page_of(vec![1, 2, 3], 50, 10);
        assert!(page.is_empty());
        assert!(!more);
        assert_eq!(clamp_offset(Some(usize::MAX)), MAX_SEARCH_LIMIT);
        assert_eq!(fetch_len(MAX_SEARCH_LIMIT, 1000), MAX_SEARCH_LIMIT);
        assert_eq!(
            format_results(&[], "q", 50, false),
            "No results found for: q (offset 50 is past the last result)"
        );
        assert_eq!(
            format_results(&[], "q", 0, false),
            "No results found for: q"
        );
    }

    #[test]
    fn rerank_note_is_one_extra_line_only_for_degraded_runs() {
        use rqmd_core::RerankStatus;
        let body =
            "Found 1 result(s) for \"q\":\n\n[1] T #abc123\n  rqmd://col/a.md\n\n".to_string();
        for status in [RerankStatus::Reranked, RerankStatus::NotRun] {
            assert_eq!(with_rerank_note(body.clone(), status), body);
        }
        for status in [RerankStatus::Unsupported, RerankStatus::Failed] {
            let out = with_rerank_note(body.clone(), status);
            let note: Vec<&str> = out.lines().filter(|l| l.starts_with("Note: ")).collect();
            assert_eq!(note.len(), 1, "{out}");
            assert!(note[0].contains("not reranked"), "{out}");
            assert!(!note[0].contains('/'), "note must not carry a path: {out}");
            assert!(out.starts_with(body.trim_end()), "{out}");
        }
    }

    #[test]
    fn clamp_limit_defaults_when_omitted_and_floors_zero() {
        assert_eq!(clamp_limit(None, 10), 10);
        assert_eq!(clamp_limit(Some(0), 10), 1);
    }

    #[test]
    fn cap_multi_get_docs_under_limit_is_unchanged() {
        let docs = vec![doc(1), doc(2), doc(3)];
        let (capped, total, truncated) = cap_multi_get_docs(docs, MULTI_GET_MAX_DOCS);
        assert_eq!(capped.len(), 3);
        assert_eq!(total, 3);
        assert!(!truncated);
    }

    #[test]
    fn cap_multi_get_docs_over_limit_truncates_and_reports_original_total() {
        let docs: Vec<Document> = (0..10).map(doc).collect();
        let (capped, total, truncated) = cap_multi_get_docs(docs, 4);
        assert_eq!(capped.len(), 4);
        assert_eq!(total, 10);
        assert!(truncated);
        assert_eq!(capped[0].id, 0);
        assert_eq!(capped[3].id, 3);
    }

    #[test]
    fn cap_multi_get_docs_exactly_at_limit_is_not_truncated() {
        let docs: Vec<Document> = (0..5).map(doc).collect();
        let (capped, total, truncated) = cap_multi_get_docs(docs, 5);
        assert_eq!(capped.len(), 5);
        assert_eq!(total, 5);
        assert!(!truncated);
    }

    fn test_server_with_doc() -> (tempfile::TempDir, RqmdServer) {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = RqmdServer::new(dir.path().join("index")).expect("server");
        {
            let mut store = server.fts().expect("fts store");
            store
                .index_document_fts_only("col", "doc1.md", "Doc 1", "hello world")
                .expect("index doc");
        }
        (dir, server)
    }

    #[test]
    fn get_document_not_found_is_err() {
        let (_dir, server) = test_server_with_doc();
        let store = server.fts().expect("fts store");
        let err = get_document(&store, "col/nope.md", None, None).unwrap_err();
        assert!(err.contains("Document not found"), "got: {err}");
    }

    #[test]
    fn get_document_malformed_lookup_is_err() {
        let (_dir, server) = test_server_with_doc();
        let store = server.fts().expect("fts store");
        let err = get_document(&store, "no-slash-no-hash", None, None).unwrap_err();
        assert!(err.contains("Cannot parse path"), "got: {err}");
    }

    #[test]
    fn get_document_found_is_ok() {
        let (_dir, server) = test_server_with_doc();
        let store = server.fts().expect("fts store");
        let text = get_document(&store, "col/doc1.md", None, None).unwrap();
        assert!(text.contains("hello world"), "got: {text}");
    }

    #[test]
    fn multi_get_documents_no_match_is_ok_not_err() {
        let (_dir, server) = test_server_with_doc();
        let store = server.fts().expect("fts store");
        let text = multi_get_documents(&store, "does-not-exist-*", None, None).unwrap();
        assert!(text.contains("No documents matched"), "got: {text}");
    }
    fn index_numbered_doc(server: &RqmdServer, path: &str, body: &str) {
        let mut store = server.fts().expect("fts store");
        store
            .index_document_fts_only("col", path, "Big", body)
            .expect("index doc");
    }

    fn numbered_body(lines: usize) -> String {
        (1..=lines)
            .map(|n| format!("line{n}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn get_without_max_lines_is_capped_and_says_so() {
        let (_dir, server) = test_server_with_doc();
        index_numbered_doc(&server, "big.md", &numbered_body(5000));
        let store = server.fts().expect("fts store");
        let text = get_document(&store, "col/big.md", None, None).unwrap();
        assert!(text.contains("line2000"), "last capped line missing");
        assert!(!text.contains("line2001"), "cap not applied");
        assert!(
            text.contains("[truncated: lines 1–2000 of 5000;"),
            "got tail: {}",
            &text[text.len().saturating_sub(120)..]
        );
    }

    #[test]
    fn get_with_explicit_max_lines_is_respected_without_a_marker() {
        let (_dir, server) = test_server_with_doc();
        index_numbered_doc(&server, "big.md", &numbered_body(5000));
        let store = server.fts().expect("fts store");
        let text = get_document(&store, "col/big.md", Some(10), Some(3)).unwrap();
        assert!(text.contains("line10") && text.contains("line12"));
        assert!(!text.contains("line13") && !text.contains("line9\n"));
        assert!(!text.contains("[truncated"));
    }

    #[test]
    fn get_caps_a_single_huge_line_by_bytes_on_a_char_boundary() {
        let (_dir, server) = test_server_with_doc();
        index_numbered_doc(&server, "minified.md", &"é".repeat(MAX_DOC_BYTES));
        let store = server.fts().expect("fts store");
        let text = get_document(&store, "col/minified.md", None, None).unwrap();
        assert!(text.len() < MAX_DOC_BYTES + 512, "len {}", text.len());
        assert!(text.contains("[truncated"));
    }

    #[test]
    fn cap_body_exact_default_limit_is_not_truncated() {
        let body = numbered_body(DEFAULT_MAX_LINES);
        let w = cap_body(&body, None, None, MAX_DOC_BYTES);
        assert_eq!(w.lines.len(), DEFAULT_MAX_LINES);
        assert!(!w.capped);
    }

    #[test]
    fn multi_get_caps_each_document_and_the_whole_response() {
        let (_dir, server) = test_server_with_doc();
        let big = "x".repeat(100) + "\n";
        let body = big.repeat(MAX_DOC_BYTES / 101 + 10);
        for i in 0..30 {
            index_numbered_doc(&server, &format!("m{i:02}.md"), &body);
        }
        let store = server.fts().expect("fts store");
        let text = multi_get_documents(&store, "col/m*.md", None, None).unwrap();
        assert!(text.contains("[truncated"), "per-doc cap not reported");
        assert!(
            text.len() <= MULTI_GET_MAX_TOTAL_BYTES + 16 * 1024,
            "response {} bytes",
            text.len()
        );
        assert!(text.contains("Output budget"), "total budget not reported");
    }

    #[test]
    fn status_does_not_disclose_filesystem_paths() {
        let (dir, server) = test_server_with_doc();
        let secret = dir.path().join("secret-project-dir");
        {
            let store = server.fts().expect("fts store");
            db::upsert_collection(
                &store.db,
                &rqmd_core::Collection {
                    name: "col".to_string(),
                    path: secret.to_string_lossy().to_string(),
                    pattern: "**/*.md".to_string(),
                    ignore: vec![],
                    include_by_default: true,
                    update_command: None,
                    allow_hidden: false,
                    respect_gitignore: false,
                    max_file_bytes: None,
                },
            )
            .unwrap();
        }
        let store = server.fts().expect("fts store");
        let out = build_status(&store);
        assert!(out.contains("col"), "collection name still reported: {out}");
        assert!(
            !out.contains("secret-project-dir"),
            "collection path leaked: {out}"
        );
        assert!(
            !out.contains(dir.path().to_str().unwrap()),
            "index path leaked: {out}"
        );
        if let Some(home) = std::env::var_os("HOME") {
            assert!(
                !out.contains(home.to_str().unwrap()),
                "home dir leaked: {out}"
            );
        }
    }

    /// The expected fingerprint for the backend the server was started with,
    /// computed from the public core API rather than the helper under test.
    fn server_expected_fingerprint() -> String {
        let model = BackendKind::from_env().default_embed_model_name();
        rqmd_core::store::expected_embed_fingerprint(&model)
    }

    #[test]
    fn status_reports_documents_pending_embedding() {
        let (_dir, server) = test_server_with_doc();
        let store = server.fts().expect("fts store");
        let out = build_status(&store);
        assert!(
            out.contains("Pending embed: 1 (backend: "),
            "pending count missing: {out}"
        );
    }

    #[test]
    fn status_pending_embed_drops_to_zero_once_embedded() {
        let (_dir, server) = test_server_with_doc();
        let store = server.fts().expect("fts store");
        let hash: String = store
            .db
            .query_row("SELECT hash FROM documents WHERE active=1", [], |r| {
                r.get(0)
            })
            .expect("doc hash");
        db::upsert_vector_meta(
            &store.db,
            &hash,
            0,
            0,
            "model",
            &server_expected_fingerprint(),
            1,
            11,
            "2024-01-01T00:00:00Z",
        )
        .expect("vector meta");
        let out = build_status(&store);
        assert!(out.contains("Pending embed: 0 "), "got: {out}");
    }

    fn pool_server(readers: usize) -> (tempfile::TempDir, RqmdServer) {
        let dir = tempfile::tempdir().expect("tempdir");
        let server =
            RqmdServer::with_fts_readers(dir.path().join("index"), readers).expect("server");
        {
            let mut store = server.fts().expect("fts store");
            store
                .index_document_fts_only("col", "doc1.md", "Doc 1", "hello world")
                .expect("index doc");
            store.flush().expect("commit");
        }
        (dir, server)
    }

    /// Backend whose query expansion outlasts any short deadline; `rerank`
    /// must never be reached.
    struct SlowGenerateBackend;

    impl rqmd_llm::InferenceBackend for SlowGenerateBackend {
        fn embed(&mut self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.1; rqmd_llm::EMBED_DIM])
        }
        fn rerank(&mut self, _query: &str, _docs: &[&str]) -> anyhow::Result<Vec<f32>> {
            panic!("rerank must not run after the deadline")
        }
        fn generate(&mut self, _prompt: &str) -> anyhow::Result<String> {
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

    fn server_with_slow_ml_store(timeout: Duration) -> (tempfile::TempDir, RqmdServer) {
        let (dir, server) = pool_server(1);
        let store = Store::open(
            make_config(&dir.path().join("index")),
            Box::new(SlowGenerateBackend),
        )
        .expect("ml store");
        assert!(
            server
                .ml_store
                .set(Arc::new(std::sync::Mutex::new(store)))
                .is_ok()
        );
        (dir, server.with_tool_timeout(timeout))
    }

    fn unmatched_query() -> QueryInput {
        QueryInput {
            query: "unmatched words".to_string(),
            intent: None,
            collections: None,
            limit: None,
            offset: None,
            rerank: None,
            expand: None,
            strict_timeout: None,
        }
    }

    #[test]
    fn query_past_the_tool_timeout_returns_a_timeout_error() {
        let (_dir, server) = server_with_slow_ml_store(Duration::from_millis(50));

        let err = server.query_blocking(unmatched_query()).unwrap_err();

        assert!(err.contains("timed out"), "{err}");
    }

    /// Backend whose query embedding outlasts a short tool timeout and whose
    /// reranker must never run once the deadline has passed.
    struct SlowEmbedBackend;

    impl rqmd_llm::InferenceBackend for SlowEmbedBackend {
        fn embed(&mut self, _text: &str) -> anyhow::Result<Vec<f32>> {
            std::thread::sleep(Duration::from_millis(200));
            Ok(vec![0.1; rqmd_llm::EMBED_DIM])
        }
        fn rerank(&mut self, _query: &str, _docs: &[&str]) -> anyhow::Result<Vec<f32>> {
            panic!("rerank must not run after the deadline")
        }
        fn generate(&mut self, _prompt: &str) -> anyhow::Result<String> {
            panic!("generate must not run with expand off")
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

    fn matching_query(strict_timeout: Option<bool>) -> QueryInput {
        QueryInput {
            query: "hello".to_string(),
            expand: Some(false),
            strict_timeout,
            ..unmatched_query()
        }
    }

    fn server_with_slow_embed_store(timeout: Duration) -> (tempfile::TempDir, RqmdServer) {
        let (dir, server) = pool_server(1);
        let store = Store::open(
            make_config(&dir.path().join("index")),
            Box::new(SlowEmbedBackend),
        )
        .expect("ml store");
        assert!(
            server
                .ml_store
                .set(Arc::new(std::sync::Mutex::new(store)))
                .is_ok()
        );
        (dir, server.with_tool_timeout(timeout))
    }

    #[test]
    fn query_past_the_tool_timeout_before_rerank_returns_results_with_a_note() {
        let (_dir, server) = server_with_slow_embed_store(Duration::from_millis(50));

        let out = server.query_blocking(matching_query(None)).unwrap();

        assert!(out.contains("doc1.md"), "{out}");
        let note: Vec<&str> = out.lines().filter(|l| l.starts_with("Note: ")).collect();
        assert_eq!(note.len(), 1, "{out}");
        assert!(note[0].contains("not reranked"), "{out}");
    }

    #[test]
    fn query_with_strict_timeout_past_the_tool_timeout_before_rerank_errors() {
        let (_dir, server) = server_with_slow_embed_store(Duration::from_millis(50));

        let err = server
            .query_blocking(matching_query(Some(true)))
            .unwrap_err();

        assert!(err.contains("timed out"), "{err}");
        assert!(err.contains("rerank"), "{err}");
    }

    #[test]
    fn query_within_the_tool_timeout_completes() {
        let (_dir, server) = server_with_slow_ml_store(Duration::from_secs(60));

        assert!(server.query_blocking(unmatched_query()).is_ok());
    }

    #[test]
    fn tool_timeout_must_be_a_positive_whole_number_of_seconds() {
        assert_eq!(parse_tool_timeout(Some("30")), Duration::from_secs(30));
        assert_eq!(parse_tool_timeout(Some("0")), DEFAULT_TOOL_TIMEOUT);
        assert_eq!(parse_tool_timeout(Some("soon")), DEFAULT_TOOL_TIMEOUT);
        assert_eq!(parse_tool_timeout(None), DEFAULT_TOOL_TIMEOUT);
    }

    #[test]
    fn concurrent_readers_overlap_instead_of_queueing() {
        let (_dir, server) = pool_server(2);
        let held = server.fts().expect("first handle");

        let (tx, rx) = std::sync::mpsc::channel();
        let other = server.clone();
        let worker = std::thread::spawn(move || {
            let store = other.fts().expect("second handle");
            let hits = store.search_fts_multi("hello", 5, None).expect("search");
            tx.send(hits.len()).unwrap();
        });
        let hits = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("second reader must not wait for the first");
        assert_eq!(hits, 1);
        drop(held);
        worker.join().unwrap();
    }

    #[test]
    fn single_reader_pool_still_serializes() {
        let (_dir, server) = pool_server(1);
        let held = server.fts().expect("only handle");

        let (tx, rx) = std::sync::mpsc::channel();
        let other = server.clone();
        let worker = std::thread::spawn(move || {
            drop(other.fts().expect("handle after release"));
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "second caller got the handle while it was held"
        );
        drop(held);
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn poisoned_reader_is_reused_not_fatal() {
        let (_dir, server) = pool_server(1);
        let poisoner = server.clone();
        let crashed = std::thread::spawn(move || {
            let _guard = poisoner.fts().expect("handle");
            panic!("request handler panicked");
        })
        .join();
        assert!(crashed.is_err());

        let store = server.fts().expect("poisoned handle must still be served");
        assert_eq!(store.search_fts_multi("hello", 5, None).unwrap().len(), 1);
    }

    /// Every tool must declare `readOnlyHint: true`, or be listed here with the
    /// reason it is not read-only. Claude Code honours a hook `allow` in plan
    /// mode only for MCP tools that carry the hint.
    const NOT_READ_ONLY: &[(&str, &str)] = &[];

    /// (name, description, input property names, required property names)
    type ToolShape = (
        &'static str,
        &'static str,
        &'static [&'static str],
        &'static [&'static str],
    );

    const EXPECTED_TOOLS: &[ToolShape] = &[
        (
            "get",
            "Retrieve a document by file path or docid (#abc123) from search results. Supports line range: 'file.md:100:40' reads 40 lines from line 100.",
            &["file", "from_line", "max_lines"],
            &["file"],
        ),
        (
            "multi_get",
            "Retrieve multiple documents matching a glob pattern (e.g. 'journals/2025-05*.md') or a comma-separated list of paths/docids.",
            &["collections", "max_lines", "pattern"],
            &["pattern"],
        ),
        (
            "query",
            "Hybrid search (BM25 + vector + rerank). Best for most queries. Provide a natural-language question or keyword phrase. Set expand:false to skip LLM query-expansion for lower latency.",
            &[
                "collections",
                "expand",
                "intent",
                "limit",
                "offset",
                "query",
                "rerank",
                "strict_timeout",
            ],
            &["query"],
        ),
        (
            "search",
            "BM25 keyword search. Fast, no model required. Supports \"quoted phrases\" and -negation. Use for known terms or exact phrases.",
            &["collections", "limit", "offset", "query"],
            &["query"],
        ),
        (
            "status",
            "Show the RQMD index status: collections, document counts, and index health.",
            &[],
            &[],
        ),
    ];

    #[test]
    fn every_tool_declares_read_only_hint_and_nothing_else_changes() {
        let tools = RqmdServer::tool_router().list_all();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        let expected_names: Vec<&str> = EXPECTED_TOOLS.iter().map(|t| t.0).collect();
        assert_eq!(
            names, expected_names,
            "tool set changed: update EXPECTED_TOOLS"
        );

        for (tool, (name, description, props, required)) in tools.iter().zip(EXPECTED_TOOLS) {
            assert_eq!(tool.description.as_deref(), Some(*description), "{name}");

            let schema = serde_json::Value::Object((*tool.input_schema).clone());
            let mut got_props: Vec<&str> = schema
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|p| p.keys().map(String::as_str).collect())
                .unwrap_or_default();
            got_props.sort_unstable();
            assert_eq!(&got_props, props, "{name}: input properties");
            let mut got_required: Vec<&str> = schema
                .get("required")
                .and_then(|r| r.as_array())
                .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            got_required.sort_unstable();
            assert_eq!(&got_required, required, "{name}: required properties");

            let read_only = tool.annotations.as_ref().and_then(|a| a.read_only_hint);
            if NOT_READ_ONLY.iter().any(|(n, _)| n == name) {
                assert_ne!(read_only, Some(true), "{name} is listed as not read-only");
            } else {
                assert_eq!(
                    read_only,
                    Some(true),
                    "{name} must declare readOnlyHint: true"
                );
            }
            // `query` may download models on first use, so it must not claim a closed world.
            assert_eq!(
                tool.annotations.as_ref().and_then(|a| a.open_world_hint),
                None,
                "{name}: openWorldHint must stay unset"
            );
        }
    }

    // The test holds the pool's only handle across awaits on purpose: that is
    // what parks the search call under test.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn blocked_tool_call_does_not_stall_the_async_runtime() {
        let (_dir, server) = pool_server(1);
        let held = server.fts().expect("only handle");

        let caller = server.clone();
        let pending = tokio::spawn(async move {
            caller
                .search(Parameters(SearchInput {
                    query: "hello".to_string(),
                    collections: None,
                    limit: None,
                    offset: None,
                }))
                .await
        });

        // The search is parked on the busy handle. With an inline handler it
        // would hold this single runtime thread and the timer below could
        // never fire.
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::time::sleep(Duration::from_millis(50)),
        )
        .await
        .expect("runtime thread was blocked by a tool call");
        assert!(!pending.is_finished());

        drop(held);
        let out = pending.await.unwrap().unwrap();
        assert!(out.contains("Doc 1"), "got: {out}");
    }
}
