use anyhow::Context as _;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerInfo},
    schemars, serde, tool, tool_handler, tool_router, ServerHandler,
};

use rqmd_core::{db, resolve, Document, Store, StoreConfig};
use rqmd_llm::{create_backend, no_backend, BackendKind};

/// Hard cap on documents returned by a single `multi_get` call. Without this,
/// an unauthenticated caller could pass a bare `*` glob with no collection
/// filter and pull the entire corpus in one request — this bounds that to a
/// generous but finite batch size.
const MULTI_GET_MAX_DOCS: usize = 200;

// ── Server struct ─────────────────────────────────────────────────────────────

/// Shared MCP server; Clone is cheap (all fields are Arc).
#[derive(Clone)]
pub struct RqmdServer {
    index_dir: Arc<PathBuf>,
    /// FTS store for search/get/status (no ML model loaded).
    fts_store: Arc<std::sync::Mutex<Store>>,
    /// ML store for hybrid query (lazily initialised on first `query` call).
    ml_store: Arc<once_cell::sync::OnceCell<Arc<std::sync::Mutex<Store>>>>,
}

impl RqmdServer {
    pub fn new(index_dir: PathBuf) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&index_dir)?;
        let config = make_config(&index_dir);
        let fts = Store::open(config, no_backend())?;
        Ok(Self {
            index_dir: Arc::new(index_dir),
            fts_store: Arc::new(std::sync::Mutex::new(fts)),
            ml_store: Arc::new(once_cell::sync::OnceCell::new()),
        })
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

    /// Same staleness handling as [`Self::ml`] — see its doc comment.
    fn fts(&self) -> anyhow::Result<std::sync::MutexGuard<'_, Store>> {
        let mut guard = self
            .fts_store
            .lock()
            .map_err(|e| anyhow::anyhow!("fts store lock poisoned: {e}"))?;
        guard.reload_if_stale().context("reload fts store")?;
        Ok(guard)
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
    /// Set to false to skip LLM reranking (faster, lower quality). Default: true.
    pub rerank: Option<bool>,
    /// Set to false to skip the LLM query-expansion / HyDE round-trip (faster;
    /// pure hybrid retrieval). Default: true.
    pub expand: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchInput {
    /// BM25 keyword query. Supports "quoted phrases" and -negation.
    pub query: String,
    /// Filter to one or more collections by name. Omit to search all collections.
    pub collections: Option<Vec<String>>,
    /// Maximum results to return (default: 10).
    pub limit: Option<usize>,
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
        description = "Hybrid search (BM25 + vector + rerank). Best for most queries. Provide a natural-language question or keyword phrase. Set expand:false to skip LLM query-expansion for lower latency."
    )]
    fn query(&self, Parameters(p): Parameters<QueryInput>) -> Result<String, String> {
        let no_rerank = !p.rerank.unwrap_or(true);
        let no_expand = !p.expand.unwrap_or(true);
        let limit = p.limit.unwrap_or(10);
        let cols = p.collections.as_deref();
        let intent = p.intent.as_deref();
        let mut store = self
            .ml()
            .map_err(|e| format!("Error loading inference backend: {e:#}"))?;
        let results = store
            .hybrid_query_multi(&p.query, intent, limit, cols, no_rerank, no_expand)
            .map_err(|e| format!("Error running query: {e:#}"))?;
        Ok(format_results(&results, &p.query))
    }

    /// BM25 full-text keyword search. No LLM required — instant results.
    #[tool(
        description = "BM25 keyword search. Fast, no model required. Supports \"quoted phrases\" and -negation. Use for known terms or exact phrases."
    )]
    fn search(&self, Parameters(p): Parameters<SearchInput>) -> Result<String, String> {
        let limit = p.limit.unwrap_or(10);
        let cols = p.collections.as_deref();
        let store = self
            .fts()
            .map_err(|e| format!("Error opening store: {e:#}"))?;
        let results = store
            .search_fts_multi(&p.query, limit, cols)
            .map_err(|e| format!("Error running search: {e:#}"))?;
        Ok(format_results(&results, &p.query))
    }

    /// Retrieve full document content by file path or docid.
    #[tool(
        description = "Retrieve a document by file path or docid (#abc123) from search results. Supports line range: 'file.md:100:40' reads 40 lines from line 100."
    )]
    fn get(&self, Parameters(p): Parameters<GetInput>) -> Result<String, String> {
        let (lookup, from_line, max_lines) = parse_file_spec(&p.file, p.from_line, p.max_lines);
        let store = self
            .fts()
            .map_err(|e| format!("Error opening store: {e:#}"))?;
        get_document(&store, &lookup, from_line, max_lines)
    }

    /// Retrieve multiple documents by glob pattern or comma-separated list.
    #[tool(
        description = "Retrieve multiple documents matching a glob pattern (e.g. 'journals/2025-05*.md') or a comma-separated list of paths/docids."
    )]
    fn multi_get(&self, Parameters(p): Parameters<MultiGetInput>) -> Result<String, String> {
        let store = self
            .fts()
            .map_err(|e| format!("Error opening store: {e:#}"))?;
        multi_get_documents(&store, &p.pattern, p.collections.as_deref(), p.max_lines)
    }

    /// Show index status: collections, document counts, and storage sizes.
    #[tool(
        description = "Show the RQMD index status: collections, document counts, and index health."
    )]
    fn status(&self) -> Result<String, String> {
        let store = self
            .fts()
            .map_err(|e| format!("Error opening store: {e:#}"))?;
        Ok(build_status(&store, &self.index_dir))
    }
}

#[tool_handler]
impl ServerHandler for RqmdServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
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

fn format_results(results: &[rqmd_core::SearchResult], query: &str) -> String {
    if results.is_empty() {
        return format!("No results found for: {query}");
    }
    let mut out = format!("Found {} result(s) for \"{query}\":\n\n", results.len());
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!(
            "[{}] {} #{}\n  rqmd://{}/{} · score {:.3}\n",
            i + 1,
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
    out
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
            Some((col, path)) => db::get_document_by_filepath(&store.db, col, path),
            None => return Err(format!("Cannot parse path: {lookup}")),
        }
    };

    let doc = match result {
        Ok(Some(d)) => d,
        Ok(None) => return Err(format!("Document not found: {lookup}")),
        Err(e) => return Err(format!("DB error: {e:#}")),
    };

    let body = db::get_content(&store.db, &doc.hash)
        .unwrap_or_default()
        .unwrap_or_default();

    let start = from_line.map(|n| n.saturating_sub(1)).unwrap_or(0);
    let text: String = body
        .lines()
        .skip(start)
        .take(max_lines.unwrap_or(usize::MAX))
        .enumerate()
        .map(|(i, l)| format!("{:>4}: {l}\n", start + i + 1))
        .collect();

    Ok(format!(
        "# {}\n── rqmd://{}/{} ──\n\n{text}",
        doc.title, doc.collection, doc.path
    ))
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

    for doc in &docs {
        let filepath = format!("{}/{}", doc.collection, doc.path);
        let body = db::get_content(&store.db, &doc.hash)
            .unwrap_or_default()
            .unwrap_or_default();
        let text: String = body
            .lines()
            .take(max_lines.unwrap_or(usize::MAX))
            .collect::<Vec<_>>()
            .join("\n");

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
    Ok(out)
}

fn build_status(store: &Store, index_dir: &Path) -> String {
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

    let mut out = format!(
        "RQMD Index Status\n  Path:     {}\n  Docs:     {total_docs}\n  Vectors:  {total_vecs}\n\n",
        index_dir.display()
    );

    let cols = db::list_collections(&store.db).unwrap_or_default();
    if cols.is_empty() {
        out.push_str("  No collections.\n");
    } else {
        out.push_str(&format!("  {:<28}  {:>6}  PATH\n", "COLLECTION", "DOCS"));
        out.push_str(&format!("  {}\n", "─".repeat(70)));
        for col in &cols {
            let count = db::list_documents(&store.db, Some(&col.name))
                .map(|d| d.len())
                .unwrap_or(0);
            out.push_str(&format!("  {:<28}  {:>6}  {}\n", col.name, count, col.path));
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
}
