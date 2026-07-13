use anyhow::Context as _;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerInfo},
    schemars, serde, tool, tool_handler, tool_router, ServerHandler,
};

use rqmd_core::{db, resolve, Store, StoreConfig};
use rqmd_llm::{no_backend, LlamaCppBackend, LlamaCppConfig};

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

    /// Return the ML store, initialising it (loading models) on first call.
    fn ml(&self) -> anyhow::Result<std::sync::MutexGuard<'_, Store>> {
        let store = self.ml_store.get_or_try_init(|| {
            eprintln!("[rqmd-mcp] Loading inference backend (models download on first run)...");
            let backend = LlamaCppBackend::new(LlamaCppConfig::default())
                .context("failed to init LlamaCpp backend")?;
            eprintln!("[rqmd-mcp] Backend ready.");
            let config = make_config(&self.index_dir);
            let s = Store::open(config, Box::new(backend))?;
            Ok::<_, anyhow::Error>(Arc::new(std::sync::Mutex::new(s)))
        })?;
        store
            .lock()
            .map_err(|e| anyhow::anyhow!("ml store lock poisoned: {e}"))
    }

    fn fts(&self) -> anyhow::Result<std::sync::MutexGuard<'_, Store>> {
        self.fts_store
            .lock()
            .map_err(|e| anyhow::anyhow!("fts store lock poisoned: {e}"))
    }
}

fn make_config(index_dir: &Path) -> StoreConfig {
    StoreConfig {
        db_path: index_dir.join("index.sqlite"),
        tantivy_dir: index_dir.join("tantivy"),
        hnsw_path: index_dir.join("hnsw.usearch"),
    }
}

// ── Tool parameter types ──────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct QueryInput {
    /// Search query. Supports plain text (auto-expanded via generation model),
    /// `expand: text`, or a multi-line typed document with `lex:`, `vec:`,
    /// `hyde:`, and optional `intent:` lines per the QMD query syntax.
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
    fn query(&self, Parameters(p): Parameters<QueryInput>) -> String {
        let no_rerank = !p.rerank.unwrap_or(true);
        let no_expand = !p.expand.unwrap_or(true);
        let limit = p.limit.unwrap_or(10);
        let cols = p.collections.as_deref();
        let intent = p.intent.as_deref();
        match self.ml() {
            Ok(mut store) => {
                match store.hybrid_query_multi(&p.query, intent, limit, cols, no_rerank, no_expand)
                {
                    Ok(results) => format_results(&results, &p.query),
                    Err(e) => format!("Error running query: {e:#}"),
                }
            }
            Err(e) => format!("Error loading inference backend: {e:#}"),
        }
    }

    /// BM25 full-text keyword search. No LLM required — instant results.
    #[tool(
        description = "BM25 keyword search. Fast, no model required. Supports \"quoted phrases\" and -negation. Use for known terms or exact phrases."
    )]
    fn search(&self, Parameters(p): Parameters<SearchInput>) -> String {
        let limit = p.limit.unwrap_or(10);
        let cols = p.collections.as_deref();
        match self.fts() {
            Ok(store) => match store.search_fts_multi(&p.query, limit, cols) {
                Ok(results) => format_results(&results, &p.query),
                Err(e) => format!("Error running search: {e:#}"),
            },
            Err(e) => format!("Error opening store: {e:#}"),
        }
    }

    /// Retrieve full document content by file path or docid.
    #[tool(
        description = "Retrieve a document by file path or docid (#abc123) from search results. Supports line range: 'file.md:100:40' reads 40 lines from line 100."
    )]
    fn get(&self, Parameters(p): Parameters<GetInput>) -> String {
        let (lookup, from_line, max_lines) = parse_file_spec(&p.file, p.from_line, p.max_lines);
        match self.fts() {
            Ok(store) => get_document(&store, &lookup, from_line, max_lines),
            Err(e) => format!("Error opening store: {e:#}"),
        }
    }

    /// Retrieve multiple documents by glob pattern or comma-separated list.
    #[tool(
        description = "Retrieve multiple documents matching a glob pattern (e.g. 'journals/2025-05*.md') or a comma-separated list of paths/docids."
    )]
    fn multi_get(&self, Parameters(p): Parameters<MultiGetInput>) -> String {
        match self.fts() {
            Ok(store) => {
                multi_get_documents(&store, &p.pattern, p.collections.as_deref(), p.max_lines)
            }
            Err(e) => format!("Error opening store: {e:#}"),
        }
    }

    /// Show index status: collections, document counts, and storage sizes.
    #[tool(
        description = "Show the RQMD index status: collections, document counts, and index health."
    )]
    fn status(&self) -> String {
        match self.fts() {
            Ok(store) => build_status(&store, &self.index_dir),
            Err(e) => format!("Error opening store: {e:#}"),
        }
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
) -> String {
    let result = if lookup.starts_with('#') {
        let hex = lookup.trim_start_matches('#');
        db::get_document_by_docid_prefix(&store.db, hex)
    } else {
        // Try "collection/path" split
        match lookup.split_once('/') {
            Some((col, path)) => db::get_document_by_filepath(&store.db, col, path),
            None => return format!("Cannot parse path: {lookup}"),
        }
    };

    let doc = match result {
        Ok(Some(d)) => d,
        Ok(None) => return format!("Document not found: {lookup}"),
        Err(e) => return format!("DB error: {e:#}"),
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

    format!(
        "# {}\n── rqmd://{}/{} ──\n\n{text}",
        doc.title, doc.collection, doc.path
    )
}

fn multi_get_documents(
    store: &Store,
    pattern: &str,
    collections: Option<&[String]>,
    max_lines: Option<usize>,
) -> String {
    let docs = match resolve::resolve_multi_get(&store.db, collections, pattern) {
        Ok(d) => d,
        Err(e) => return format!("DB error: {e:#}"),
    };
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
        format!("No documents matched: {pattern}")
    } else {
        out
    }
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
