# Crate API

[← README](../README.md)

## `rqmd-core`

The search engine. Key public types:

```rust
use rqmd_core::{SearchResult, Store, StoreConfig};
use rqmd_llm::InferenceBackend;

fn example(config: StoreConfig, backend: Box<dyn InferenceBackend>) -> anyhow::Result<()> {
    let mut store = Store::open(config, backend)?;

    // Index a document
    store.index_document("collection", "rel/path.md", "Title", "body text")?;

    // Hybrid search: BM25 + vector + RRF + rerank
    let results: Vec<SearchResult> =
        store.hybrid_query("search terms", None, 10, None, false, false)?;

    // BM25 keyword search
    let results = store.search_fts("keyword", 10, None)?;

    // Vector similarity search
    let results = store.search_vec("semantic query", 10, None)?;
    let _ = results;

    Ok(())
}
```

`hybrid_query`'s parameters, in order: `query`, `intent` (optional context for
expansion/reranking — `None` if unavailable), `limit`, `collection` (`None`
searches every default-included collection), `skip_rerank`, `no_expand` (skips
the LLM query-expansion round-trip). `Store::open` and every method that
mutates the index (`index_document`, `hybrid_query`, `search_vec`) take
`&mut self`; `search_fts` alone takes `&self`.

This example is kept honest by an identical `no_run` doctest on `Store` itself
(`crates/rqmd-core/src/store.rs`), compiled by `cargo test --doc` — a future
signature change fails that build before it can drift from this page.

## `rqmd-llm`

Inference backend abstraction. Implement `InferenceBackend` to add a new
backend. Only `embed`, `rerank`, `generate`, and the three `*_model_name`
methods are required; `capabilities`, `embed_batch`, `embed_query`,
`embed_passage`, `embed_batch_passage`, and `release_idle` all have default
implementations and only need overriding where a backend's behavior actually
differs from the default (e.g. `OrtBackend` overrides `capabilities` to report
embed-only support).

```rust
use rqmd_llm::{BackendCapabilities, BackendKind, InferenceBackend, create_backend};

struct EchoBackend;

impl InferenceBackend for EchoBackend {
    fn capabilities(&self) -> BackendCapabilities {
        // Override truthfully — this stub only supports embed.
        BackendCapabilities { embed: true, rerank: false, generate: false }
    }
    fn embed(&mut self, text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(vec![text.len() as f32])
    }
    fn rerank(&mut self, _query: &str, docs: &[&str]) -> anyhow::Result<Vec<f32>> {
        Ok(vec![0.0; docs.len()])
    }
    fn generate(&mut self, _prompt: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn embed_model_name(&self) -> &str { "echo" }
    fn rerank_model_name(&self) -> &str { "echo" }
    fn generate_model_name(&self) -> &str { "echo" }
}

// Factory: reads RQMD_INFERENCE_BACKEND + RQMD_ORT_EP from env
let backend: Box<dyn InferenceBackend> = create_backend(&BackendKind::from_env())?;
```

This example is a real, executable doctest on `InferenceBackend` itself
(`crates/rqmd-llm/src/lib.rs`) — if a future change adds a required method,
`EchoBackend` stops compiling and `cargo test --doc` catches it immediately,
rather than this page silently going stale.

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
