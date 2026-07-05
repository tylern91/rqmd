# rqmd

Rust-based mini CLI search engine for your docs, knowledge bases, meeting notes, whatever. Tracking current SOTA approaches while being all local.

Hybrid local document search in a single static binary. No Node. No Bun. No native-module rebuild per platform. Build once, run anywhere.

Built on the search pipeline and ideas of **[tobi/qmd](https://github.com/tobi/qmd)**. Coming from qmd? See [Migrating from qmd](#migrating-from-qmd).

## Contents

- [Why Rust](#why-rust)
- [Status](#status)
- [Installation](#installation)
- [Quick start](#quick-start)
- [CLI reference](#cli-reference)
- [Query syntax and expansion](#query-syntax-and-expansion)
- [Excluding files](#excluding-files)
- [Inference backends](#inference-backends)
- [Models](#models)
- [MCP server](#mcp-server)
- [Environment variables](#environment-variables)
- [Where data lives](#where-data-lives)
- [Workspace layout](#workspace-layout)
- [Crate API](#crate-api)
- [Design decisions](#design-decisions)
- [Differences from qmd](#differences-from-qmd)
- [Migrating from qmd](#migrating-from-qmd)
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

rqmd is at **v0.2.0**. All core search functionality is implemented and tested:

| Feature | Status |
|---------|--------|
| BM25 keyword search | ✅ |
| Vector similarity search (HNSW) | ✅ |
| Hybrid BM25 + vector + RRF | ✅ |
| Cross-encoder reranking | ✅ |
| MCP server (stdio + HTTP) | ✅ |
| Query expansion (lex/vec/hyde via LLM) | ✅ |

Plain-text queries are auto-expanded by a local Qwen3-1.7B model
(`ggml-org/Qwen3-1.7B-GGUF`, downloaded on first `rqmd query`) into `lex`/`vec`/`hyde`
sub-queries fused with the original via RRF (original query keeps 2× weight). Expansion
is best-effort and skipped when the top BM25 hit is a strong, unambiguous match.
Typed multi-line queries and the `--intent` flag are also supported — see
[Query syntax and expansion](#query-syntax-and-expansion) and [docs/SYNTAX.md](docs/SYNTAX.md).

---

## Installation

### Homebrew (macOS / Linux — prebuilt, no compile)

```sh
brew tap tylern91/rqmd
brew trust tylern91/rqmd  # required on Homebrew ≥4.5
brew install rqmd
```

> The formula downloads a prebuilt binary — no Rust toolchain, cmake, or C++ compiler required.
> macOS arm64 and Linux x86_64 are supported. Other platforms: use the source build below.

### cargo install (source build, cross-platform)

Requires Rust stable ≥1.78, cmake ≥3.14, and a C/C++ toolchain (builds llama.cpp from source).

```sh
cargo install --git https://github.com/tylern91/rqmd --locked rqmd-cli
```

On Linux, Metal is not available — prefix with `LLAMA_METAL=0`:

```sh
LLAMA_METAL=0 cargo install --git https://github.com/tylern91/rqmd --locked rqmd-cli
```

### Prebuilt binary (manual download)

Download from the [latest GitHub Release](https://github.com/tylern91/rqmd/releases/latest),
then verify and install:

```sh
# macOS arm64
curl -fLO https://github.com/tylern91/rqmd/releases/latest/download/rqmd-aarch64-apple-darwin.tar.gz
shasum -a 256 -c rqmd-aarch64-apple-darwin.tar.gz.sha256
tar -xf rqmd-aarch64-apple-darwin.tar.gz
install -m 0755 rqmd ~/.local/bin/rqmd   # or /usr/local/bin/rqmd

# Linux x86_64
curl -fLO https://github.com/tylern91/rqmd/releases/latest/download/rqmd-x86_64-unknown-linux-gnu.tar.gz
shasum -a 256 -c rqmd-x86_64-unknown-linux-gnu.tar.gz.sha256
tar -xf rqmd-x86_64-unknown-linux-gnu.tar.gz
install -m 0755 rqmd ~/.local/bin/rqmd
```

### From source (recommended while in development)

Requirements: Rust stable (≥1.78), cmake ≥3.14 (cmake 4.x supported), Xcode Command Line Tools (macOS) or `build-essential` (Linux).

> **Git LFS note:** binary assets (images in `assets/`) are stored in Git LFS.
> Run `brew install git-lfs && git lfs install` once before cloning if you need those files.
> A normal `git clone` without LFS still works — the PNG will be an LFS pointer file rather than the full image.

```sh
# Clone the repo
git clone https://github.com/tylern91/rqmd
cd rqmd

# Development build (fast, debug symbols)
cargo build -p rqmd-cli

# Optimized release binary (~60MB, fat LTO + stripped)
cargo build --profile dist -p rqmd-cli
# → target/dist/rqmd

# Install to ~/.cargo/bin/ (content-aware: rebuilds only when source changed)
./scripts/install.sh
```

> **Why not `cargo install --path`?** `cargo install` skips reinstalling when the crate version
> is unchanged, so source changes without a version bump are silently ignored. `scripts/install.sh`
> uses `cargo build`'s fingerprinting instead — it rebuilds only when something actually changed,
> then copies the fresh binary into `~/.cargo/bin/`. No `--force`, no manual version bump.

### With ONNX Runtime backend (CoreML / CUDA / DirectML)

```sh
cargo build --profile dist -p rqmd-cli --features ort-backend
# or install directly:
./scripts/install.sh --features ort-backend
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
rqmd query "quarterly planning"     # hybrid BM25 + vector + rerank + LLM expansion (best quality)

# MCP server (for Claude, Cursor, etc.)
rqmd mcp                            # stdio transport
rqmd mcp --http --port 8181         # Streamable HTTP transport
```

---

## CLI reference

| Command | Description |
|---------|-------------|
| `rqmd query <text>` | Hybrid search: BM25 + vector + rerank + LLM query expansion |
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
| `rqmd mcp [--http] [--port N] [--daemon]` | Start MCP server |
| `rqmd collection add <path> [--ignore PATTERN]` | Add a directory as a collection |
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
--index-dir <path>       Override index directory ($RRQMD_INDEX_DIR)
--backend llama|ort      Inference backend ($RRQMD_INFERENCE_BACKEND)
--ort-ep auto|coreml|cuda|directml|cpu   ORT execution provider ($RRQMD_ORT_EP)
```

---

## Query syntax and expansion

`rqmd query` (and the MCP `query` tool) auto-expand plain-text queries using a local
Qwen3-1.7B model, producing `lex`, `vec`, and `hyde` variants that are fused with the
original query via RRF. `rqmd search` and `rqmd vsearch` do **not** expand — they run
their respective single-mode search only.

**`rqmd query` flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `--intent <text>` | *(none)* | Background context to steer expansion and reranking |
| `-n <num>` | `10` | Number of results to return |
| `-c/--collection <name>` | *(all)* | Scope to a collection (repeatable, OR-matched) |
| `--no-rerank` | off | Skip cross-encoder reranking (expansion still runs) |
| `--full` | off | Return full document bodies instead of snippets |
| `--format cli\|json` | `cli` | Output format |

**Examples:**

```sh
# Plain text — auto-expanded into lex/vec/hyde by the LLM
rqmd query "how does authentication work"

# Explicit expand (equivalent to above)
rqmd query "expand: how does authentication work"

# Typed multi-line query (bypasses LLM; each line is a direct sub-query)
rqmd query $'lex: auth token -oauth\nvec: how does authentication work\nhyde: The auth system uses JWT tokens with a 15-minute TTL...'

# Intent flag — steers expansion and reranking toward web performance
rqmd query --intent "web page load times" "performance"

# Intent inline (query document)
rqmd query $'intent: web page load times\nlex: performance\nvec: how to improve page speed'

# Scope to a specific collection
rqmd query -c docs "deployment pipeline"
```

Full grammar (typed lines, lex phrase/negation operators, MCP `searches` array):
[docs/SYNTAX.md](docs/SYNTAX.md).

---

## Excluding files

By default rqmd indexes every file matching a collection's pattern (`**/*.md`).
It reads **no** ignore files — `.gitignore` and `.ignore` are never consulted.

**Built-in exclusions** (always skipped):

- Hidden files and directories (names starting with `.`)
- `node_modules`, `vendor`, `dist`, `build`, `target`, `.cache`

**Per-collection ignore patterns** (gitignore-style globs):

```sh
# Exclude patterns when adding a collection
rqmd collection add ~/notes --ignore '*.log' --ignore 'tmp/'

# Multiple patterns are combined with OR — any match excludes the file
rqmd collection add ~/docs --ignore 'drafts/**' --ignore '**/node_modules'
```

Ignore patterns are stored with the collection and apply on every subsequent
`rqmd update` run — you only need to specify them once.

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
RRQMD_INFERENCE_BACKEND=llama rqmd embed
```

### OrtBackend (`ort-backend` feature)

Uses [ONNX Runtime](https://ort.pyke.io/) with pluggable execution providers.
Build with `--features ort-backend`.

| Role | Model | Size |
|------|-------|------|
| Embeddings | `BAAI/bge-base-en-v1.5` (ONNX) | ~440MB |
| Reranking | *(not supported — falls back to LlamaCppBackend)* | — |

Execution providers selected by `--ort-ep` or `RRQMD_ORT_EP`:

| EP | Flag | Platform | Hardware |
|----|------|----------|----------|
| CoreML | `coreml` | macOS | Apple Neural Engine + GPU |
| CUDA | `cuda` | Linux / Windows | NVIDIA GPU |
| DirectML | `directml` | Windows | Any GPU via DirectML |
| CPU | `cpu` | All | CPU fallback |
| Auto | `auto` (default) | All | CoreML on macOS, CPU elsewhere |

```sh
# CoreML (Apple Neural Engine — fastest for embed-sized models on M-series)
RRQMD_INFERENCE_BACKEND=ort RRQMD_ORT_EP=coreml rqmd embed
rqmd --backend ort --ort-ep coreml embed
```

---

## Models

| Role | Backend | Model | Size |
|------|---------|-------|------|
| Embeddings | LlamaCpp (default) | `ggml-org/embeddinggemma-300M-GGUF` | ~300 MB |
| Reranking | LlamaCpp | `ggml-org/Qwen3-Reranker-0.6B-Q8_0-GGUF` | ~600 MB |
| Embeddings | ORT (`ort-backend` feature) | `BAAI/bge-base-en-v1.5` (ONNX) | ~440 MB |
| Query expansion | LlamaCpp | `ggml-org/Qwen3-1.7B-GGUF` (`Qwen3-1.7B-Q8_0.gguf`) | ~1.7 GB |

Models download automatically from HuggingFace on first use (~900 MB for embed + rerank; ~2.6 GB with query expansion) and are cached at `~/.cache/huggingface/hub/`.

Set `HF_ENDPOINT` to use a mirror, or `HF_HUB_OFFLINE=1` to disable downloads entirely (models must be pre-staged in the cache).

---

## MCP server

rqmd includes a built-in MCP server exposing its search index as tools for Claude, Cursor, and other MCP-aware clients.

| Tool | Description |
|------|-------------|
| `query` | Hybrid search: BM25 + vector + rerank + LLM expansion (recommended) |
| `search` | BM25 keyword search — no models required |
| `get` | Retrieve a document by path or content hash |
| `multi_get` | Retrieve multiple documents by glob pattern |
| `status` | Index health and collection summary |

```sh
rqmd mcp                        # stdio (Claude Desktop, Cursor, etc.)
rqmd mcp --http                 # Streamable HTTP on port 8181
rqmd mcp --http --port 9000     # custom port
rqmd mcp --daemon               # background HTTP (implies --http)
```

For Claude Desktop, add to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "rqmd": {
      "command": "rqmd",
      "args": ["mcp"]
    }
  }
}
```

---

## Environment variables

| Variable | Values | Default | Description |
|----------|--------|---------|-------------|
| `RRQMD_INDEX_DIR` | path | `~/.cache/rqmd/` | Index storage directory |
| `RRQMD_INFERENCE_BACKEND` | `llama`, `ort` | `llama` | Inference backend |
| `RRQMD_ORT_EP` | `auto`, `coreml`, `cuda`, `directml`, `cpu` | `auto` | ONNX Runtime EP |
| `RRQMD_FORCE_CPU` | `1` | *(unset)* | Disable GPU layers in LlamaCppBackend |
| `RRQMD_CI` | `1` | *(unset)* | Skip model downloads (CI / offline use) |

---

## Where data lives

| What | Path |
|------|------|
| Index + collections (SQLite) | `~/.cache/rqmd/index.sqlite` |
| BM25 index (Tantivy) | `~/.cache/rqmd/tantivy/` |
| Vector index (usearch) | `~/.cache/rqmd/vectors.usearch` |
| Model cache (HuggingFace) | `~/.cache/huggingface/hub/` |
| Project-local index | `.rqmd/` (created by `rqmd init`) |

Override the root index directory with `--index-dir <path>` or `$RRQMD_INDEX_DIR`.

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
    fn generate(&mut self, prompt: &str) -> Result<String>;
    fn embed_model_name(&self) -> &str;
    fn rerank_model_name(&self) -> &str;
}

// Factory: reads RRQMD_INFERENCE_BACKEND + RRQMD_ORT_EP from env
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
| Query expansion | ✓ LLM-generated lex/vec/hyde (stock Qwen3-1.7B), fused via RRF |
| MlxBackend | Deferred — `mlx-rs` `Array: !Send` conflicts with parallel embed pool |
| Startup time | ~5ms (no JIT) |

The RRF fusion formula, BM25 field weights, chunking parameters (900 tokens /
15% overlap), and docid scheme (`first 6 hex chars of SHA-256(content)`) match
the original qmd design so search quality is preserved.

See [BENCHMARK.md](BENCHMARK.md) for the de-risking spike results (inference backend
+ DB bake-off) that drove the Tantivy+usearch and llama-cpp-2 decisions.

---

## Differences from qmd

| Feature | qmd (TypeScript) | rqmd (Rust) |
|---------|-----------------|-------------|
| Runtime | Node.js required | Self-contained static binary |
| Startup | ~300 ms (JIT) | ~5 ms |
| Search pipeline | BM25 + vector + RRF + rerank | Same pipeline, same parameters |
| MCP server identity | `qmd` | `rqmd` |
| Chunking | tree-sitter AST-aware | Regex heuristic (headings, code fences, lists) |
| Index location | `~/.cache/qmd/` | `~/.cache/rqmd/` |
| File exclusion | `.gitignore` aware | Built-in exclusions + per-collection `ignore` list |

Search quality is equivalent — the RRF formula, BM25 field weights, chunk size (900 tokens / 15% overlap), and docid scheme are all ported verbatim from qmd.

---

## Migrating from qmd

rqmd uses its own index at `~/.cache/rqmd/` — existing qmd collections need to be re-added:

```sh
rqmd collection add ~/path/to/your/docs --name your-collection
rqmd embed
```

All environment variables are prefixed `RRQMD_` instead of `QMD_`:

| Old (qmd) | New (rqmd) |
|-----------|----------|
| `QMD_INDEX_DIR` | `RRQMD_INDEX_DIR` |
| `QMD_INFERENCE_BACKEND` | `RRQMD_INFERENCE_BACKEND` |
| `QMD_ORT_EP` | `RRQMD_ORT_EP` |
| `QMD_FORCE_CPU` | `RRQMD_FORCE_CPU` |
| `QMD_CI` | `RRQMD_CI` |

The MCP server now identifies as `rqmd` — update any `claude_desktop_config.json` entries accordingly.

---

## Troubleshooting

### cmake version requirements

cmake ≥3.14 is required. cmake 4.x is supported — the `llama-cpp-sys-2` crate
(which builds llama.cpp from source) builds correctly with cmake 4.x on macOS
and Linux. You do not need to pin or downgrade cmake.

**Do not** add `target-cpu` flags to `.cargo/config.toml` — they change the
llama-cpp-sys fingerprint and force a cmake rebuild. Pass them at build time:

```sh
RUSTFLAGS="-C target-cpu=native" cargo build --profile dist -p rqmd-cli
```

### Model downloads are slow / fail

Models are fetched from HuggingFace on first `rqmd embed` and cached at
`~/.cache/huggingface/hub/`. Set `HF_ENDPOINT` for a mirror, or
`HF_HUB_OFFLINE=1` with pre-downloaded models.

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
RRQMD_INFERENCE_BACKEND=llama cargo run -p rqmd-cli -- eval --mode hybrid

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
[BENCHMARK.md](BENCHMARK.md) for the de-risking spike results that validated the
Rust technology choices.

**Coming from qmd?** The quickest path:

```sh
# macOS / Linux — prebuilt binary, no compiler needed
brew tap tylern91/rqmd && brew trust tylern91/rqmd && brew install rqmd

# or build from source
git clone https://github.com/tylern91/rqmd && cd rqmd
./scripts/install.sh          # builds + installs rqmd to ~/.cargo/bin/

rqmd collection add ~/notes   # same pattern as qmd
rqmd embed                    # downloads models on first run (~900 MB)
```

Your existing collections need to be re-added (rqmd uses its own index at
`~/.cache/rqmd/`), but the search commands and MCP surface work the same way.
See [Migrating from qmd](#migrating-from-qmd) for the full env-var mapping.
