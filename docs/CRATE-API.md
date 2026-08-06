# Crate API

[← README](../README.md)

## `rqmd-core`

The search engine. Key public types:

```rust
use rqmd_core::{Store, StoreConfig, SearchResult};

let store = Store::open(config, backend)?;

// Index a document
store.index_document("collection", "rel/path.md", "Title", &body)?;

// Hybrid search: BM25 + vector + RRF + rerank
let results: Vec<SearchResult> = store.hybrid_query("search terms", 10, None, false)?;

// BM25 keyword search
let results = store.search_fts("keyword", 10, None)?;

// Vector similarity search
let results = store.search_vec("semantic query", 10, None)?;
```

## `rqmd-llm`

Inference backend abstraction. Implement `InferenceBackend` to add a new backend:

```rust
use rqmd_llm::{InferenceBackend, BackendKind, create_backend};

pub trait InferenceBackend: Send {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>>;
    fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>>;
    fn generate(&mut self, prompt: &str) -> Result<String>;
    fn embed_model_name(&self) -> &str;
    fn rerank_model_name(&self) -> &str;
}

// Factory: reads RRQMD_INFERENCE_BACKEND + RRQMD_ORT_EP from env
let backend: Box<dyn InferenceBackend> = create_backend(&BackendKind::from_env())?;
```

All embeddings are returned as **unit-normalized f32 vectors** (L2 norm = 1.0).
Cosine similarity is therefore equivalent to dot product.

## `rqmd-mcp`

MCP server with five tools:

| Tool | Description |
|------|-------------|
| `query` | Hybrid search (recommended) |
| `search` | BM25 keyword search |
| `get` | Retrieve document by path or docid |
| `multi_get` | Retrieve multiple documents by glob |
| `status` | Index health summary |

Start modes:

```sh
rqmd mcp                        # stdio (for Claude Desktop, Cursor, etc.)
rqmd mcp --http                 # Streamable HTTP on port 8181
rqmd mcp --http --port 9000     # custom port
```
