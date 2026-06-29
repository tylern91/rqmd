# rqmd

Rust-based mini CLI search engine for your docs, knowledge bases, meeting notes, whatever. Tracking current SOTA approaches while being all local.

Hybrid local document search in a single static binary. No Node. No Bun. No native-module rebuild per platform. Build once, run anywhere.

## Contents

- [Why Rust](#why-rust)
- [Status](#status)
- [Installation](#installation)
- [Quick start](#quick-start)
- [CLI reference](#cli-reference)
- [Inference backends](#inference-backends)
- [Environment variables](#environment-variables)
- [Workspace layout](#workspace-layout)
- [Crate API](#crate-api)
- [Design decisions](#design-decisions)
- [Troubleshooting](#troubleshooting)
- [Contributing](#contributing)
- [Acknowledgements](#acknowledgements)

---

## Why Rust

rqmd ships as a ~60MB self-contained binary with no runtime dependencies:

- SQLite bundled via `rusqlite` — no system SQLite dependency
- BM25 via Tantivy (pure Rust) — no FTS5 extension
- Vector search via usearch (HNSW, C++ header-only, statically linked)
- Inference via llama-cpp-2 (Metal on macOS, CPU on Linux/Windows)

---

## Status

rqmd is at **v0.1.0**. All core search functionality is implemented and tested:

| Feature | Status |
|---------|--------|
| BM25 keyword search | ✅ |
| Vector similarity search (HNSW) | ✅ |
| Hybrid BM25 + vector + RRF | ✅ |
| Cross-encoder reranking | ✅ |
| MCP server (stdio + HTTP) | ✅ |
| Query expansion (lex/vec/hyde via LLM) | 🔜 future phase |

The query expansion model (Qwen3-1.7B fine-tuned) requires a separate training
pipeline; the API is wired but the generate model is not yet loaded by default.
Until that phase ships, `rqmd query` uses BM25 + vector + RRF + rerank only.

---

## Installation

### From source (recommended while in development)

Requirements: Rust stable (≥1.78), cmake ≥3.14 (pinned to 3.x — see [Troubleshooting](#troubleshooting)), Xcode Command Line Tools (macOS) or `build-essential` (Linux).

```sh
# Clone the repo
git clone https://github.com/tylern91/rqmd
cd rqmd

# Development build (fast, debug symbols)
cargo build -p rqmd-cli

# Optimized release binary (~60MB, fat LTO + stripped)
cargo build --profile dist -p rqmd-cli
# → target/dist/rqmd

# Install to ~/.cargo/bin/
cargo install --path crates/rqmd-cli --profile dist
```

### With ONNX Runtime backend (CoreML / CUDA / DirectML)

```sh
cargo build --profile dist -p rqmd-cli --features ort-backend
```

This downloads the ONNX Runtime library at build time. The resulting binary
supports CoreML (Apple Neural Engine on macOS), CUDA (NVIDIA GPU), and DirectML
(Windows GPU) in addition to the CPU fallback.

### Linux

```sh
sudo apt-get install cmake build-essential
cargo build -p rqmd-cli
```

For a fully static MUSL binary (no glibc dependency):

```sh
rustup target add x86_64-unknown-linux-musl
RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --profile dist -p rqmd-cli --target x86_64-unknown-linux-musl
```

---

## Quick start

```sh
# Index a directory
rqmd collection add ~/notes --name notes
rqmd context add rqmd://notes "Personal notes and ideas"
rqmd embed                          # downloads GGUF models on first run (~900MB)

# Search
rqmd search "project timeline"      # BM25 keyword
rqmd vsearch "deployment process"   # vector similarity
rqmd query "quarterly planning"     # hybrid BM25 + vector + rerank (best quality)

# MCP server (for Claude, Cursor, etc.)
rqmd mcp                            # stdio transport
rqmd mcp --http --port 8181         # Streamable HTTP transport
```

---

## CLI reference

| Command | Description |
|---------|-------------|
| `rqmd query <text>` | Hybrid search: BM25 + vector + rerank |
| `rqmd search <text>` | BM25 keyword search only |
| `rqmd vsearch <text>` | Vector similarity only |
| `rqmd get <path\|#docid>` | Retrieve document by path or content hash |
| `rqmd multi-get <glob>` | Retrieve multiple documents |
| `rqmd ls [collection[/path]]` | List collections or files |
| `rqmd embed [-c collection]` | Generate embeddings |
| `rqmd update [-c collection]` | Re-index collections |
| `rqmd status` | Index health and collection summary |
| `rqmd doctor` | Diagnose config, index, model, and device issues |
| `rqmd bench [-n N]` | Embed throughput benchmark (default: 5 rounds) |
| `rqmd eval [--mode bm25\|vec\|hybrid] [--verbose]` | Search quality eval against synthetic fixtures |
| `rqmd mcp [--http] [--port N]` | Start MCP server |
| `rqmd collection add <path>` | Add a directory as a collection |
| `rqmd collection list` | List all collections |
| `rqmd collection remove <name>` | Remove a collection |
| `rqmd collection rename <old> <new>` | Rename a collection |
| `rqmd collection show <name>` | Show collection details |
| `rqmd collection update-cmd <name> [cmd]` | Set/clear pre-update hook |
| `rqmd collection include/exclude <name>` | Toggle from default queries |
| `rqmd context add [path] <text>` | Add context for a path |
| `rqmd context list` | List all contexts |
| `rqmd context rm <path>` | Remove context |
| `rqmd context check` | Find paths missing context |
| `rqmd init` | Create a project-local `.rqmd` index |

Global flags (before the subcommand):

```
--index-dir <path>       Override index directory ($RQMD_INDEX_DIR)
--backend llama|ort      Inference backend ($RQMD_INFERENCE_BACKEND)
--ort-ep auto|coreml|cuda|directml|cpu   ORT execution provider ($RQMD_ORT_EP)
```

---

## Inference backends

Two backends are available, selected at runtime via env var or `--backend` flag.

### LlamaCppBackend (default)

Uses [llama-cpp-2](https://github.com/utilityai/llama-cpp-rs) to run GGUF models locally.

| Role | Model | Size |
|------|-------|------|
| Embeddings | `ggml-org/embeddinggemma-300M-GGUF` | ~300MB |
| Reranking | `ggml-org/Qwen3-Reranker-0.6B-Q8_0-GGUF` | ~600MB |

Models are downloaded automatically from HuggingFace on first use and cached
at `~/.cache/huggingface/hub/`.

On macOS, llama.cpp uses Metal (Apple GPU) automatically. On Linux, CPU-only
unless CUDA is available.

```sh
rqmd embed                          # uses LlamaCppBackend by default
rqmd --backend llama embed          # explicit
RQMD_INFERENCE_BACKEND=llama rqmd embed
```

### OrtBackend (`ort-backend` feature)

Uses [ONNX Runtime](https://ort.pyke.io/) with pluggable execution providers.
Build with `--features ort-backend`.

| Role | Model | Size |
|------|-------|------|
| Embeddings | `BAAI/bge-base-en-v1.5` (ONNX) | ~440MB |
| Reranking | *(not supported — falls back to LlamaCppBackend)* | — |

Execution providers selected by `--ort-ep` or `RQMD_ORT_EP`:

| EP | Flag | Platform | Hardware |
|----|------|----------|----------|
| CoreML | `coreml` | macOS | Apple Neural Engine + GPU |
| CUDA | `cuda` | Linux / Windows | NVIDIA GPU |
| DirectML | `directml` | Windows | Any GPU via DirectML |
| CPU | `cpu` | All | CPU fallback |
| Auto | `auto` (default) | All | CoreML on macOS, CPU elsewhere |

```sh
# CoreML (Apple Neural Engine — fastest for embed-sized models on M-series)
RQMD_INFERENCE_BACKEND=ort RQMD_ORT_EP=coreml rqmd embed
rqmd --backend ort --ort-ep coreml embed
```

---

## Environment variables

| Variable | Values | Default | Description |
|----------|--------|---------|-------------|
| `RQMD_INDEX_DIR` | path | `~/.cache/rqmd/` | Index storage directory |
| `RQMD_INFERENCE_BACKEND` | `llama`, `ort` | `llama` | Inference backend |
| `RQMD_ORT_EP` | `auto`, `coreml`, `cuda`, `directml`, `cpu` | `auto` | ONNX Runtime EP |
| `RQMD_FORCE_CPU` | `1` | *(unset)* | Disable GPU layers in LlamaCppBackend |
| `RQMD_CI` | `1` | *(unset)* | Skip model downloads (CI / offline use) |

---

## Workspace layout

```
rqmd/                    # repo root = Cargo workspace
├── Cargo.toml           # workspace definition + release/dist profiles
├── .cargo/config.toml   # MUSL static build target config
├── crates/
│   ├── rqmd-core/       # engine: search, chunking, store, collections
│   ├── rqmd-llm/        # inference backends (LlamaCpp + ORT)
│   ├── rqmd-cli/        # CLI entry point (clap)
│   └── rqmd-mcp/        # MCP server (rmcp, stdio + HTTP)
├── docs/                # SYNTAX.md and other reference docs
└── assets/              # architecture diagram
```

### Build profiles

| Profile | Command | LTO | Strip | Use |
|---------|---------|-----|-------|-----|
| `dev` | `cargo build` | off | none | development |
| `release` | `cargo build --release` | thin | debuginfo | testing |
| `dist` | `cargo build --profile dist` | fat | symbols | release binary |

---

## Crate API

### `rqmd-core`

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

### `rqmd-llm`

Inference backend abstraction. Implement `InferenceBackend` to add a new backend:

```rust
use rqmd_llm::{InferenceBackend, BackendKind, create_backend};

pub trait InferenceBackend: Send {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>>;
    fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>>;
    fn generate_constrained(&mut self, prompt: &str, grammar: &str, root: &str) -> Result<String>;
    fn embed_model_name(&self) -> &str;
    fn rerank_model_name(&self) -> &str;
}

// Factory: reads RQMD_INFERENCE_BACKEND + RQMD_ORT_EP from env
let backend: Box<dyn InferenceBackend> = create_backend(&BackendKind::from_env())?;
```

All embeddings are returned as **unit-normalized f32 vectors** (L2 norm = 1.0).
Cosine similarity is therefore equivalent to dot product.

### `rqmd-mcp`

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

---

## Design decisions

| Aspect | Choice |
|--------|--------|
| Search backend | Tantivy (BM25) + usearch (HNSW) |
| Index location | `~/.cache/rqmd/` |
| Embed model | embeddinggemma-300M (GGUF, Metal/CPU) |
| Rerank model | Qwen3-Reranker-0.6B (GGUF) |
| ORT backend | ✓ CoreML / CUDA / DirectML (feature-gated) |
| Query expansion | Deferred — requires fine-tuned Qwen3-1.7B generate model |
| MlxBackend | Deferred — `mlx-rs` `Array: !Send` conflicts with parallel embed pool |
| Startup time | ~5ms (no JIT) |

The RRF fusion formula, BM25 field weights, chunking parameters (900 tokens /
15% overlap), and docid scheme (`first 6 hex chars of SHA-256(content)`) match
the original qmd design so search quality is preserved.

See [BENCHMARK.md](BENCHMARK.md) for the Phase 0 spike results (inference backend
+ DB bake-off) that drove the Tantivy+usearch and llama-cpp-2 decisions.

---

## Troubleshooting

### cmake 4.x breaks the llama.cpp build

If you have cmake 4.x installed, `llama-cpp-sys-2` fails to compile because
llama.cpp's `CMakeLists.txt` specifies `cmake_minimum_required(VERSION 3.14...3.28)`.

```sh
# macOS fix — install cmake 3.x and put it first on PATH
brew install cmake@3
export PATH="$(brew --prefix cmake@3)/bin:$PATH"
```

The CI workflow pins cmake@3 automatically via `pip install "cmake<4"`.

**Do not** add `target-cpu` flags to `.cargo/config.toml` — they change the
llama-cpp-sys fingerprint and force a cmake rebuild. Pass them at build time:

```sh
RUSTFLAGS="-C target-cpu=native" cargo build --profile dist -p rqmd-cli
```

### Model downloads are slow / fail

Models are fetched from HuggingFace on first `rqmd embed` and cached at
`~/.cache/huggingface/hub/`. Set `HF_ENDPOINT` for a mirror, or
`HF_HUB_OFFLINE=1` with pre-downloaded models.

### `rqmd embed` exits with "generate model not loaded"

This is expected — the query-expansion model is not loaded by default (see
[Status](#status)). `rqmd embed` only uses the embed model; `rqmd query` falls
back to raw-query BM25+vec+rerank when the generate model is absent.

### "OrtBackend: reranking not supported"

`OrtBackend` handles embeddings only. Reranking uses `LlamaCppBackend`
automatically as a fallback.

---

## Contributing

Before sending a PR:

1. Run `cargo fmt --all` and `cargo clippy --workspace -- -D warnings`
2. Run `cargo test --workspace --lib`
3. Check that `cargo build --workspace` and `cargo build -p rqmd-cli --features ort-backend` both pass

The search quality gate is `rqmd eval`:

```sh
# BM25 quality (no model, fast — run this always)
cargo run -p rqmd-cli -- eval --mode bm25 --verbose

# Full hybrid quality (requires models — run before search-path changes)
RQMD_INFERENCE_BACKEND=llama cargo run -p rqmd-cli -- eval --mode hybrid

# Embed throughput (compare backends)
cargo run -p rqmd-cli -- bench -n 5
```

The BM25 eval also runs in CI on every push.

---

## Acknowledgements

rqmd is a Rust port of **[tobi/qmd](https://github.com/tobi/qmd)** — the original
TypeScript hybrid-search CLI by [@tobi](https://github.com/tobi). The search
pipeline design, RRF fusion formula, BM25 field weights, chunking parameters, docid
scheme, and MCP tool surface are all derived from that project. See
[BENCHMARK.md](BENCHMARK.md) for the de-risking spikes that validated the Rust
technology choices.
